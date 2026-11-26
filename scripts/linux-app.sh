#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP_NAME="Quarry"
ICON_SOURCE="$ROOT/assets/quarry-logo-v3.png"
DESKTOP_SOURCE="$ROOT/packaging/linux/dev.quarry.gpui.desktop"
INSTALL_SCRIPT="$ROOT/packaging/linux/install.sh"
UNINSTALL_SCRIPT="$ROOT/packaging/linux/uninstall.sh"
RUST_LICENSES_DIR="$ROOT/packaging/licenses/rust"
BUILD_TARGET_DIR="$ROOT/target/linux-package-build"

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
        local out rc missing
        set +e
        out="$(ldd "$bin" 2>&1)"
        rc=$?
        set -e
        [[ $rc -eq 0 ]] || fail "ldd failed for $bin:\n$out"
        missing="$(printf '%s\n' "$out" | grep '=> not found' || true)"
        [[ -z "$missing" ]] || fail "missing shared libraries in $bin:\n$missing"
    fi
}

build_release_binaries() {
    local target="$1"
    # Pass explicit --target and --target-dir so ambient CARGO_TARGET_DIR or
    # build.target settings can never cause staging to pick up stale binaries.
    cargo build --release --locked \
        --target "$target" \
        --target-dir "$BUILD_TARGET_DIR" \
        --manifest-path "$ROOT/Cargo.toml" \
        -p quarry-gpui -p quarry-cli
}

stage_bundle() {
    local dest="$1"
    local target="$2"
    local arch="$3"
    local bin_src_dir="$4"
    local tag="${5:-}"
    local version revision status notices_html

    version="$(app_version)"
    revision="$(git -C "$ROOT" rev-parse HEAD)"
    status="$(source_status)"
    notices_html="$(notice_html_for_target "$target")"

    [[ -x "$bin_src_dir/quarry-gpui" ]] || fail "missing built desktop binary: $bin_src_dir/quarry-gpui"
    [[ -x "$bin_src_dir/quarry" ]] || fail "missing built CLI binary: $bin_src_dir/quarry"
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

    install -m 755 "$bin_src_dir/quarry-gpui" "$dest/bin/quarry-gpui"
    install -m 755 "$bin_src_dir/quarry" "$dest/bin/quarry"
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
    local target arch rust_release out_bundle bin_src_dir
    target="$(host_target)"
    arch="$(host_arch_label)"
    rust_release="$(rust_release_version)"

    "$ROOT/scripts/generate-notices.sh" --check --target "$target" --rust-release "$rust_release"
    build_release_binaries "$target"
    bin_src_dir="$BUILD_TARGET_DIR/$target/release"

    out_bundle="$ROOT/target/package/linux-$arch/$APP_NAME"
    rm -rf "$out_bundle"
    stage_bundle "$out_bundle" "$target" "$arch" "$bin_src_dir"
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

    local target arch rust_release version revision tag_base_version tag_commit bin_src_dir
    target="$(host_target)"
    arch="$(host_arch_label)"
    rust_release="$(rust_release_version)"

    "$ROOT/scripts/generate-notices.sh" --release-check --target "$target" --rust-release "$rust_release"
    build_release_binaries "$target"
    bin_src_dir="$BUILD_TARGET_DIR/$target/release"

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

    stage_bundle "$bundle_dir" "$target" "$arch" "$bin_src_dir" "$tag"
    rm -f "$tarball"
    tar -C "$STAGE_DIR" -czf "$tarball" "$bundle_name"

    # Verify tarball round-trip extraction, install, CLI execution, and clean uninstall.
    local verify_dir="$STAGE_DIR/verify"
    local verify_prefix="$STAGE_DIR/verify prefix"
    mkdir -p "$verify_dir"
    tar -C "$verify_dir" -xzf "$tarball"
    "$verify_dir/$bundle_name/install.sh" --prefix "$verify_prefix"
    [[ -x "$verify_prefix/bin/quarry-gpui" ]] || fail "extracted tarball install missing quarry-gpui"
    [[ -x "$verify_prefix/bin/quarry" ]] || fail "extracted tarball install missing quarry"
    [[ -f "$verify_prefix/share/applications/dev.quarry.gpui.desktop" ]] || fail "extracted tarball install missing desktop file"
    "$verify_prefix/bin/quarry" --help >/dev/null || fail "extracted CLI --help check failed"
    "$verify_prefix/share/doc/quarry/uninstall.sh"
    [[ ! -e "$verify_prefix/bin/quarry-gpui" ]] || fail "uninstall did not remove quarry-gpui"
    [[ ! -e "$verify_prefix/bin/quarry" ]] || fail "uninstall did not remove quarry"

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
    STAGE_DIR="$(cd "$STAGE_DIR" && pwd)"
    local fake_bundle="$STAGE_DIR/Quarry-v0.2.0-linux-x86_64"
    # Test spaces, apostrophe, and literal '%' in prefix path!
    local prefix="$STAGE_DIR/o'brien 100% prefix"
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

    # 1. Wrong-architecture bundle must be rejected before modifying prefix.
    printf '{"architecture": "mips64"}\n' > "$fake_bundle/share/doc/quarry/RELEASE_METADATA.json"
    if QUARRY_SKIP_LDD_CHECK=1 "$fake_bundle/install.sh" --prefix "$prefix" >/dev/null 2>&1; then
        fail "self-test: wrong-architecture bundle should have been rejected"
    fi
    [[ ! -e "$prefix/bin/quarry-gpui" ]] || fail "self-test: rejected wrong-arch install wrote files"
    rm -f "$fake_bundle/share/doc/quarry/RELEASE_METADATA.json"

    # 2. Unrepresentable '=' in prefix must be rejected before modifying prefix.
    local bad_prefix="$STAGE_DIR/bad=prefix"
    if QUARRY_SKIP_ARCH_CHECK=1 QUARRY_SKIP_LDD_CHECK=1 "$fake_bundle/install.sh" --prefix "$bad_prefix" >/dev/null 2>&1; then
        fail "self-test: '=' prefix should have been rejected"
    fi
    [[ ! -e "$bad_prefix/bin/quarry-gpui" ]] || fail "self-test: '=' prefix wrote files"

    # 3. Unowned pre-existing bin/quarry must block install and remain intact.
    printf 'unrelated-quarry-tool\n' > "$prefix/bin/quarry"
    if QUARRY_SKIP_ARCH_CHECK=1 QUARRY_SKIP_LDD_CHECK=1 "$fake_bundle/install.sh" --prefix "$prefix" >/dev/null 2>&1; then
        fail "self-test: collision with unowned bin/quarry should have failed"
    fi
    [[ "$(cat "$prefix/bin/quarry")" == "unrelated-quarry-tool" ]] || fail "self-test: unowned bin/quarry was overwritten"
    [[ ! -e "$prefix/bin/quarry-gpui" ]] || fail "self-test: failed collision install wrote quarry-gpui"

    # 4. Uninstall on a prefix where Quarry was never installed must delete nothing.
    "$UNINSTALL_SCRIPT" --prefix "$prefix" >/dev/null
    [[ "$(cat "$prefix/bin/quarry")" == "unrelated-quarry-tool" ]] || fail "self-test: uninstall deleted unowned bin/quarry"
    rm -f "$prefix/bin/quarry"

    # 5. Clean install into prefix with spaces, apostrophe, and '%'; verify XDG Exec/TryExec parsing.
    printf 'keep\n' > "$prefix/bin/other-tool"
    QUARRY_SKIP_ARCH_CHECK=1 QUARRY_SKIP_LDD_CHECK=1 "$fake_bundle/install.sh" --prefix "$prefix" >/dev/null
    [[ -x "$prefix/bin/quarry-gpui" ]] || fail "self-test: quarry-gpui not installed"
    [[ -x "$prefix/bin/quarry" ]] || fail "self-test: quarry CLI not installed"
    [[ -f "$prefix/share/icons/hicolor/512x512/apps/dev.quarry.gpui.png" ]] || fail "self-test: icon not installed"
    [[ -f "$prefix/share/doc/quarry/THIRD_PARTY_NOTICES.html" ]] || fail "self-test: notices not installed"
    [[ -f "$prefix/share/doc/quarry/installed-files.txt" ]] || fail "self-test: ownership manifest missing"
    [[ -x "$prefix/share/doc/quarry/uninstall.sh" ]] || fail "self-test: uninstall.sh not installed"

    local desktop="$prefix/share/applications/dev.quarry.gpui.desktop"
    python3 -B - "$desktop" "$prefix/bin/quarry-gpui" <<'PY'
import pathlib, sys

desktop_path, expected_bin = pathlib.Path(sys.argv[1]), sys.argv[2]
kv = {}
for line in desktop_path.read_text(encoding="utf-8").splitlines():
    if "=" in line and not line.startswith("["):
        k, v = line.split("=", 1)
        kv[k] = v

def unescape_desktop_string(s: str) -> str:
    out, i = [], 0
    while i < len(s):
        if s[i] == "\\" and i + 1 < len(s):
            nxt = s[i + 1]
            out.append({"s": " ", "n": "\n", "t": "\t", "r": "\r", "\\": "\\"}.get(nxt, "\\" + nxt))
            i += 2
        else:
            out.append(s[i])
            i += 1
    return "".join(out)

assert kv["StartupWMClass"] == "dev.quarry.gpui"
assert unescape_desktop_string(kv["TryExec"]) == expected_bin, (kv["TryExec"], expected_bin)

exec_str = unescape_desktop_string(kv["Exec"])
# Parse Desktop Entry Exec quoting (double quotes with \, ", $, ` escapes) and %% field code.
tokens, cur, in_q, i = [], [], False, 0
while i < len(exec_str):
    ch = exec_str[i]
    if in_q:
        if ch == "\\" and i + 1 < len(exec_str):
            cur.append(exec_str[i + 1])
            i += 2
        elif ch == '"':
            in_q = False
            i += 1
        else:
            cur.append(ch)
            i += 1
    else:
        if ch.isspace():
            if cur:
                tokens.append("".join(cur))
                cur = []
            i += 1
        elif ch == '"':
            in_q = True
            i += 1
        else:
            cur.append(ch)
            i += 1
if cur:
    tokens.append("".join(cur))
assert not in_q, f"Unclosed quote in Exec: {kv['Exec']}"
assert len(tokens) == 2 and tokens[1] == "%F", tokens
assert tokens[0].replace("%%", "%") == expected_bin, (tokens[0], expected_bin)
PY

    # 6. Extra user file inside doc dir must survive reinstall AND uninstall.
    printf 'user-notes\n' > "$prefix/share/doc/quarry/my-notes.txt"
    QUARRY_SKIP_ARCH_CHECK=1 QUARRY_SKIP_LDD_CHECK=1 "$fake_bundle/install.sh" --prefix "$prefix" >/dev/null
    [[ "$(cat "$prefix/share/doc/quarry/my-notes.txt")" == "user-notes" ]] || fail "self-test: reinstall deleted user doc file"

    "$prefix/share/doc/quarry/uninstall.sh" >/dev/null
    [[ ! -e "$prefix/bin/quarry-gpui" ]] || fail "self-test: quarry-gpui not removed"
    [[ ! -e "$prefix/bin/quarry" ]] || fail "self-test: quarry CLI not removed"
    [[ ! -e "$desktop" ]] || fail "self-test: desktop entry not removed"
    [[ ! -e "$prefix/share/icons/hicolor/512x512/apps/dev.quarry.gpui.png" ]] || fail "self-test: icon not removed"
    [[ ! -e "$prefix/share/doc/quarry/installed-files.txt" ]] || fail "self-test: manifest not removed"
    [[ "$(cat "$prefix/share/doc/quarry/my-notes.txt")" == "user-notes" ]] || fail "self-test: uninstall deleted user doc file"
    [[ -f "$prefix/bin/other-tool" ]] || fail "self-test: unrelated file was modified or removed"

    # 7. Default user install honors custom XDG_DATA_HOME and uninstalls cleanly.
    local fake_home="$STAGE_DIR/home"
    local custom_xdg="$STAGE_DIR/xdg-data"
    mkdir -p "$fake_home" "$custom_xdg"
    HOME="$fake_home" XDG_DATA_HOME="$custom_xdg" QUARRY_SKIP_ARCH_CHECK=1 QUARRY_SKIP_LDD_CHECK=1 \
        "$fake_bundle/install.sh" >/dev/null
    [[ -x "$fake_home/.local/bin/quarry-gpui" ]] || fail "self-test: XDG default install missing bin"
    [[ -f "$custom_xdg/applications/dev.quarry.gpui.desktop" ]] || fail "self-test: XDG_DATA_HOME desktop missing"
    HOME="$fake_home" XDG_DATA_HOME="$custom_xdg" "$custom_xdg/doc/quarry/uninstall.sh" >/dev/null
    [[ ! -e "$fake_home/.local/bin/quarry-gpui" ]] || fail "self-test: XDG uninstall left binary"
    [[ ! -e "$custom_xdg/applications/dev.quarry.gpui.desktop" ]] || fail "self-test: XDG uninstall left desktop"

    # 8. Failed first install mid-copy rolls back newly created files so retry succeeds.
    local fail_prefix="$STAGE_DIR/failed-first-install"
    mkdir -p "$fail_prefix/share/applications"
    chmod 555 "$fail_prefix/share/applications"
    if QUARRY_SKIP_ARCH_CHECK=1 QUARRY_SKIP_LDD_CHECK=1 "$fake_bundle/install.sh" --prefix "$fail_prefix" >/dev/null 2>&1; then
        chmod 755 "$fail_prefix/share/applications"
        fail "self-test: read-only applications dir should have failed first install"
    fi
    chmod 755 "$fail_prefix/share/applications"
    [[ ! -e "$fail_prefix/bin/quarry-gpui" ]] || fail "self-test: failed first install left untracked quarry-gpui"
    [[ ! -e "$fail_prefix/bin/quarry" ]] || fail "self-test: failed first install left untracked quarry"
    [[ ! -e "$fail_prefix/share/doc/quarry/installed-files.txt" ]] || fail "self-test: failed first install left stale manifest"
    QUARRY_SKIP_ARCH_CHECK=1 QUARRY_SKIP_LDD_CHECK=1 "$fake_bundle/install.sh" --prefix "$fail_prefix" >/dev/null
    [[ -x "$fail_prefix/bin/quarry-gpui" ]] || fail "self-test: retry after failed first install did not succeed"
    "$fail_prefix/share/doc/quarry/uninstall.sh" >/dev/null

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
