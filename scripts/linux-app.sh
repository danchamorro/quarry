#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP_NAME="Quarry"
ICON_SOURCE="$ROOT/assets/quarry-logo-v3.png"
DESKTOP_SOURCE="$ROOT/packaging/linux/dev.quarry.gpui.desktop"
INSTALL_SCRIPT="$ROOT/packaging/linux/install.sh"
UNINSTALL_SCRIPT="$ROOT/packaging/linux/uninstall.sh"
RUST_LICENSES_DIR="$ROOT/packaging/licenses/rust"

STAGE_DIR=""
cleanup() {
    if [[ -n "$STAGE_DIR" && -d "$STAGE_DIR" ]]; then
        rm -rf "$STAGE_DIR"
    fi
}
trap cleanup EXIT

usage() {
    cat <<'EOF'
Package, install, uninstall, release, and self-test Quarry on Linux (including Omarchy).

Usage:
  ./scripts/linux-app.sh package
  ./scripts/linux-app.sh install [--prefix DIR]
  ./scripts/linux-app.sh uninstall [--prefix DIR]
  ./scripts/linux-app.sh release [--tag TAG] [--out-dir DIR]
  ./scripts/linux-app.sh self-test
EOF
}

fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

host_target() {
    local machine
    machine="$(uname -m)"
    case "$machine" in
        x86_64|amd64)
            printf 'x86_64-unknown-linux-gnu\n'
            ;;
        aarch64|arm64)
            printf 'aarch64-unknown-linux-gnu\n'
            ;;
        *)
            fail "unsupported Linux architecture: $machine"
            ;;
    esac
}

host_arch_label() {
    local machine
    machine="$(uname -m)"
    case "$machine" in
        x86_64|amd64)
            printf 'x86_64\n'
            ;;
        aarch64|arm64)
            printf 'arm64\n'
            ;;
        *)
            fail "unsupported Linux architecture: $machine"
            ;;
    esac
}

rust_release_version() {
    local version
    version="$("${RUSTC:-rustc}" -vV | awk '/^release: / {print $2; exit}')"
    [[ -n "$version" ]] || fail "could not determine rustc release version"
    printf '%s\n' "$version"
}

app_version() {
    local pkgid version
    pkgid="$(cargo pkgid --offline -p quarry-gpui --manifest-path "$ROOT/Cargo.toml")"
    version="${pkgid##*[@#]}"
    [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "unexpected quarry-gpui version: $version"
    printf '%s\n' "$version"
}

source_status() {
    if [[ -n "$(git -C "$ROOT" status --porcelain --untracked-files=normal)" ]]; then
        printf 'dirty\n'
    else
        printf 'clean\n'
    fi
}

notice_html_for_target() {
    local target="$1"
    printf '%s/packaging/licenses/targets/%s/THIRD_PARTY_NOTICES.html\n' "$ROOT" "$target"
}

verify_ldd_binary() {
    local bin="$1"
    if command -v ldd >/dev/null 2>&1; then
        local missing
        missing="$(ldd "$bin" | grep '=> not found' || true)"
        [[ -z "$missing" ]] || fail "missing shared libraries in $bin:\n$missing"
    fi
}

stage_bundle() {
    local dest="$1"
    local target="$2"
    local arch="$3"
    local tag="${4:-}"
    local version revision status notices_html

    version="$(app_version)"
    revision="$(git -C "$ROOT" rev-parse HEAD)"
    status="$(source_status)"
    notices_html="$(notice_html_for_target "$target")"

    [[ -f "$notices_html" ]] || fail "missing third-party notices for $target: $notices_html"
    [[ -f "$ICON_SOURCE" ]] || fail "missing icon asset: $ICON_SOURCE"
    [[ -f "$DESKTOP_SOURCE" ]] || fail "missing desktop file: $DESKTOP_SOURCE"
    [[ -f "$INSTALL_SCRIPT" ]] || fail "missing install script: $INSTALL_SCRIPT"
    [[ -f "$UNINSTALL_SCRIPT" ]] || fail "missing uninstall script: $UNINSTALL_SCRIPT"

    mkdir -p \
        "$dest/bin" \
        "$dest/share/applications" \
        "$dest/share/icons/hicolor/512x512/apps" \
        "$dest/share/doc/quarry/Rust"

    install -m 755 "$ROOT/target/release/quarry-gpui" "$dest/bin/quarry-gpui"
    install -m 755 "$ROOT/target/release/quarry" "$dest/bin/quarry"
    install -m 644 "$DESKTOP_SOURCE" "$dest/share/applications/dev.quarry.gpui.desktop"
    install -m 644 "$ICON_SOURCE" "$dest/share/icons/hicolor/512x512/apps/dev.quarry.gpui.png"
    install -m 644 "$ROOT/LICENSE-MIT" "$dest/share/doc/quarry/LICENSE-MIT"
    install -m 644 "$ROOT/LICENSE-APACHE" "$dest/share/doc/quarry/LICENSE-APACHE"
    install -m 644 "$notices_html" "$dest/share/doc/quarry/THIRD_PARTY_NOTICES.html"
    for rust_doc in "$RUST_LICENSES_DIR/"*; do
        [[ -f "$rust_doc" ]] || continue
        install -m 644 "$rust_doc" "$dest/share/doc/quarry/Rust/$(basename "$rust_doc")"
    done
    install -m 755 "$INSTALL_SCRIPT" "$dest/install.sh"
    install -m 755 "$UNINSTALL_SCRIPT" "$dest/uninstall.sh"

    python3 -B - "$dest/share/doc/quarry/RELEASE_METADATA.json" \
        "$version" "$tag" "$revision" "$target" "$arch" "$status" <<'PY'
import json, pathlib, sys
out, version, tag, revision, target, arch, status = sys.argv[1:]
payload = {
    "name": "Quarry",
    "version": version,
    "tag": tag or None,
    "git_revision": revision,
    "target": target,
    "architecture": arch,
    "source_status": status,
}
pathlib.Path(out).write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
PY

    verify_ldd_binary "$dest/bin/quarry-gpui"
    verify_ldd_binary "$dest/bin/quarry"
}

package_app() {
    [[ "$(uname -s)" == "Linux" ]] || fail "linux-app.sh package must run on Linux."
    local target arch rust_release out_bundle
    target="$(host_target)"
    arch="$(host_arch_label)"
    rust_release="$(rust_release_version)"

    "$ROOT/scripts/generate-notices.sh" --check --target "$target" --rust-release "$rust_release"
    cargo build --release --locked --manifest-path "$ROOT/Cargo.toml" -p quarry-gpui -p quarry-cli

    out_bundle="$ROOT/target/package/linux-$arch/$APP_NAME"
    rm -rf "$out_bundle"
    stage_bundle "$out_bundle" "$target" "$arch"
    printf 'Packaged Linux Quarry (%s, %s)\n%s\n' "$target" "$(source_status)" "$out_bundle"
}

install_app() {
    [[ "$(uname -s)" == "Linux" ]] || fail "linux-app.sh install must run on Linux."
    local arch bundle
    package_app
    arch="$(host_arch_label)"
    bundle="$ROOT/target/package/linux-$arch/$APP_NAME"
    "$bundle/install.sh" "$@"
}

uninstall_app() {
    "$UNINSTALL_SCRIPT" "$@"
}

release_app() {
    [[ "$(uname -s)" == "Linux" ]] || fail "linux-app.sh release must run on Linux."
    local tag="" out_dir=""
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --tag)
                [[ $# -ge 2 ]] || fail "--tag requires a value (e.g. v0.2.0-beta.2)"
                tag="$2"
                shift 2
                ;;
            --out-dir)
                [[ $# -ge 2 ]] || fail "--out-dir requires a directory path"
                out_dir="$2"
                shift 2
                ;;
            -h|--help)
                usage
                exit 0
                ;;
            *)
                fail "unknown release argument: $1"
                ;;
        esac
    done

    if [[ -n "$(git -C "$ROOT" status --porcelain --untracked-files=normal)" ]]; then
        fail "release packaging requires a clean Git working tree."
    fi

    local target arch rust_release version revision tag_base_version tag_commit
    target="$(host_target)"
    arch="$(host_arch_label)"
    rust_release="$(rust_release_version)"

    "$ROOT/scripts/generate-notices.sh" --release-check --target "$target" --rust-release "$rust_release"
    cargo build --release --locked --manifest-path "$ROOT/Cargo.toml" -p quarry-gpui -p quarry-cli

    version="$(app_version)"
    revision="$(git -C "$ROOT" rev-parse HEAD)"
    if [[ -z "$tag" ]]; then
        tag="v$version"
    fi
    [[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] \
        || fail "release tag must match vX.Y.Z or vX.Y.Z-prerelease: $tag"
    tag_base_version="${tag#v}"
    tag_base_version="${tag_base_version%%-*}"
    [[ "$tag_base_version" == "$version" ]] \
        || fail "release tag $tag base version ($tag_base_version) does not match packaged app version ($version)."

    if tag_commit="$(git -C "$ROOT" rev-parse --verify "refs/tags/$tag^{commit}" 2>/dev/null)"; then
        [[ "$tag_commit" == "$revision" ]] \
            || fail "tag $tag points to $tag_commit, not packaged HEAD $revision (check out $tag first)."
    else
        fail "git tag $tag does not exist locally; create tag $tag on $revision before cutting a release."
    fi

    if [[ -z "$out_dir" ]]; then
        out_dir="$ROOT/target/release-artifacts/$tag"
    fi
    mkdir -p "$out_dir"
    out_dir="$(cd "$out_dir" && pwd)"

    STAGE_DIR="$(mktemp -d "$ROOT/target/.quarry-linux-release.XXXXXX")"
    local bundle_name="$APP_NAME-$tag-linux-$arch"
    local bundle_dir="$STAGE_DIR/$bundle_name"
    local tarball="$out_dir/$bundle_name.tar.gz"
    local sums_file="$out_dir/SHA256SUMS-linux-$arch.txt"
    local manifest_file="$out_dir/release-manifest-linux-$arch.json"

    stage_bundle "$bundle_dir" "$target" "$arch" "$tag"
    rm -f "$tarball"
    tar -C "$STAGE_DIR" -czf "$tarball" "$bundle_name"

    # Verify tarball round-trip extraction and install/uninstall into a temp prefix.
    local verify_dir="$STAGE_DIR/verify"
    local verify_prefix="$STAGE_DIR/verify prefix"
    mkdir -p "$verify_dir"
    tar -C "$verify_dir" -xzf "$tarball"
    "$verify_dir/$bundle_name/install.sh" --prefix "$verify_prefix"
    [[ -x "$verify_prefix/bin/quarry-gpui" ]] || fail "extracted tarball install missing quarry-gpui"
    [[ -x "$verify_prefix/bin/quarry" ]] || fail "extracted tarball install missing quarry"
    [[ -f "$verify_prefix/share/applications/dev.quarry.gpui.desktop" ]] || fail "extracted tarball install missing desktop file"
    "$verify_prefix/share/doc/quarry/uninstall.sh"
    [[ ! -e "$verify_prefix/bin/quarry-gpui" ]] || fail "uninstall did not remove quarry-gpui"

    if command -v sha256sum >/dev/null 2>&1; then
        (cd "$out_dir" && sha256sum "$(basename "$tarball")" > "$sums_file")
    else
        (cd "$out_dir" && shasum -a 256 "$(basename "$tarball")" > "$sums_file")
    fi

    python3 -B - "$manifest_file" "$tag" "$version" "$revision" "$target" "$arch" "$tarball" <<'PY'
import hashlib, json, pathlib, sys
manifest_path, tag, version, revision, target, arch, tarball = sys.argv[1:]
p = pathlib.Path(tarball)
payload = {
    "tag": tag,
    "version": version,
    "git_revision": revision,
    "target": target,
    "architecture": arch,
    "artifacts": [
        {
            "file": p.name,
            "bytes": p.stat().st_size,
            "sha256": hashlib.sha256(p.read_bytes()).hexdigest(),
        }
    ],
}
text = json.dumps(payload, indent=2) + "\n"
pathlib.Path(manifest_path).write_text(text, encoding="utf-8")
print(text, end="")
PY

    printf 'Linux release artifacts ready in %s\n' "$out_dir"
    cat "$sums_file"
}

self_test() {
    STAGE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/quarry-linux-selftest.XXXXXX")"
    local fake_bundle="$STAGE_DIR/Quarry-v0.2.0-linux-x86_64"
    local prefix="$STAGE_DIR/user prefix"
    mkdir -p \
        "$fake_bundle/bin" \
        "$fake_bundle/share/applications" \
        "$fake_bundle/share/icons/hicolor/512x512/apps" \
        "$fake_bundle/share/doc/quarry/Rust" \
        "$prefix/bin"

    printf '#!/usr/bin/env bash\nexit 0\n' > "$fake_bundle/bin/quarry-gpui"
    printf '#!/usr/bin/env bash\nexit 0\n' > "$fake_bundle/bin/quarry"
    chmod 755 "$fake_bundle/bin/quarry-gpui" "$fake_bundle/bin/quarry"
    cp "$DESKTOP_SOURCE" "$fake_bundle/share/applications/dev.quarry.gpui.desktop"
    cp "$ICON_SOURCE" "$fake_bundle/share/icons/hicolor/512x512/apps/dev.quarry.gpui.png"
    cp "$ROOT/LICENSE-MIT" "$fake_bundle/share/doc/quarry/LICENSE-MIT"
    cp "$ROOT/LICENSE-APACHE" "$fake_bundle/share/doc/quarry/LICENSE-APACHE"
    cp "$(notice_html_for_target x86_64-unknown-linux-gnu)" "$fake_bundle/share/doc/quarry/THIRD_PARTY_NOTICES.html"
    cp "$RUST_LICENSES_DIR/LICENSE-MIT" "$fake_bundle/share/doc/quarry/Rust/LICENSE-MIT"
    cp "$INSTALL_SCRIPT" "$fake_bundle/install.sh"
    cp "$UNINSTALL_SCRIPT" "$fake_bundle/uninstall.sh"
    chmod 755 "$fake_bundle/install.sh" "$fake_bundle/uninstall.sh"

    # Unrelated file in prefix must remain untouched across install/uninstall.
    printf 'keep\n' > "$prefix/bin/other-tool"

    "$fake_bundle/install.sh" --prefix "$prefix" >/dev/null
    [[ -x "$prefix/bin/quarry-gpui" ]] || fail "self-test: quarry-gpui not installed"
    [[ -x "$prefix/bin/quarry" ]] || fail "self-test: quarry CLI not installed"
    [[ -f "$prefix/share/icons/hicolor/512x512/apps/dev.quarry.gpui.png" ]] || fail "self-test: icon not installed"
    [[ -f "$prefix/share/doc/quarry/THIRD_PARTY_NOTICES.html" ]] || fail "self-test: notices not installed"
    [[ -x "$prefix/share/doc/quarry/uninstall.sh" ]] || fail "self-test: uninstall.sh not installed"

    local desktop="$prefix/share/applications/dev.quarry.gpui.desktop"
    grep -Fq "Exec=\"$prefix/bin/quarry-gpui\" %F" "$desktop" || fail "self-test: Exec line mismatch in $desktop"
    grep -Fq "TryExec=$prefix/bin/quarry-gpui" "$desktop" || fail "self-test: TryExec line mismatch in $desktop"
    grep -Fq "StartupWMClass=dev.quarry.gpui" "$desktop" || fail "self-test: StartupWMClass missing"

    # Self-detecting uninstall from installed doc location.
    "$prefix/share/doc/quarry/uninstall.sh" >/dev/null
    [[ ! -e "$prefix/bin/quarry-gpui" ]] || fail "self-test: quarry-gpui not removed"
    [[ ! -e "$prefix/bin/quarry" ]] || fail "self-test: quarry CLI not removed"
    [[ ! -e "$desktop" ]] || fail "self-test: desktop entry not removed"
    [[ ! -e "$prefix/share/icons/hicolor/512x512/apps/dev.quarry.gpui.png" ]] || fail "self-test: icon not removed"
    [[ ! -e "$prefix/share/doc/quarry" ]] || fail "self-test: doc dir not removed"
    [[ -f "$prefix/bin/other-tool" ]] || fail "self-test: unrelated file was modified or removed"

    printf 'Linux packaging self-test passed.\n'
}

main() {
    local cmd="${1:-}"
    shift || true
    case "$cmd" in
        package)
            package_app "$@"
            ;;
        install)
            install_app "$@"
            ;;
        uninstall)
            uninstall_app "$@"
            ;;
        release)
            release_app "$@"
            ;;
        self-test)
            self_test "$@"
            ;;
        -h|--help|"")
            usage
            ;;
        *)
            fail "unknown subcommand: $cmd"
            ;;
    esac
}

main "$@"
