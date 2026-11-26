#!/usr/bin/env bash
set -euo pipefail

BUNDLE_DIR="$(cd "$(dirname "$0")" && pwd)"
EXPLICIT_PREFIX=""

usage() {
    cat <<'EOF'
Install Quarry desktop app, CLI, launcher entry, icon, and license notices on Linux.

Usage:
  ./install.sh [--prefix DIR]

Options:
  --prefix DIR   Installation prefix (default: ~/.local/bin and $XDG_DATA_HOME or ~/.local/share)
  -h, --help     Show this help message
EOF
}

fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

host_arch_label() {
    case "$(uname -m)" in
        x86_64|amd64) printf 'x86_64\n' ;;
        aarch64|arm64) printf 'arm64\n' ;;
        *) printf '%s\n' "$(uname -m)" ;;
    esac
}

validate_bundle_arch() {
    [[ "${QUARRY_SKIP_ARCH_CHECK:-0}" == "1" ]] && return 0
    local meta="$BUNDLE_DIR/share/doc/quarry/RELEASE_METADATA.json"
    [[ -f "$meta" ]] || return 0
    local bundle_arch host_arch
    bundle_arch="$(sed -n 's/.*"architecture"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$meta" | head -n 1)"
    [[ -n "$bundle_arch" ]] || return 0
    host_arch="$(host_arch_label)"
    if [[ "$bundle_arch" != "$host_arch" ]]; then
        fail "bundle architecture ($bundle_arch) does not match this machine ($host_arch); download the matching Quarry archive."
    fi
}

validate_linux_binaries() {
    [[ "${QUARRY_SKIP_LDD_CHECK:-0}" == "1" ]] && return 0
    [[ "$(uname -s)" == "Linux" ]] || return 0
    command -v ldd >/dev/null 2>&1 || return 0

    local bin out rc missing
    for bin in "$BUNDLE_DIR/bin/quarry-gpui" "$BUNDLE_DIR/bin/quarry"; do
        set +e
        out="$(ldd "$bin" 2>&1)"
        rc=$?
        set -e
        if [[ $rc -ne 0 ]]; then
            printf '%s\n' "$out" >&2
            fail "binary loader check failed for $(basename "$bin") (wrong architecture or incompatible runtime)."
        fi
        missing="$(printf '%s\n' "$out" | grep '=> not found' || true)"
        if [[ -n "$missing" ]]; then
            printf 'error: missing runtime shared libraries for %s:\n%s\n' "$(basename "$bin")" "$missing" >&2
            cat >&2 <<'EOF'
Install the required system libraries for your distribution and re-run ./install.sh:
  Arch / Omarchy:
    sudo pacman -S --needed wayland libxkbcommon vulkan-icd-loader fontconfig freetype2 libxcb libx11 libxcursor libxrandr libxi libglvnd
  Ubuntu / Debian:
    sudo apt-get install -y libwayland-client0 libxkbcommon0 libxkbcommon-x11-0 libvulkan1 libfontconfig1 libfreetype6 libxcb1
EOF
            exit 1
        fi
    done
}

# Encode an executable path for the Desktop Entry Spec `Exec` key.
# Two layers apply: (1) Exec argument quoting for reserved chars, then
# (2) Desktop Entry `string` escaping (\ -> \\). Literal `%` is escaped as `%%`.
encode_desktop_exec_arg() {
    local raw="$1"
    local escaped="$raw"
    # Escape % -> %% for field-code safety.
    escaped="${escaped//%/%%}"
    # Quote whenever the path contains any character outside the safe unquoted set.
    if [[ "$raw" == *[^A-Za-z0-9_./:-]* ]]; then
        # Inside double quotes, spec requires escaping \, ", $, ` at the Exec layer,
        # then escaping every \ again at the Desktop Entry string layer:
        #   \ -> \\  (Exec) -> \\\\ (string)
        #   " -> \"  (Exec) -> \\"  (string)
        #   $ -> \$  (Exec) -> \\$  (string)
        #   ` -> \`  (Exec) -> \\`  (string)
        escaped="${escaped//\\/\\\\\\\\}"
        escaped="${escaped//\"/\\\\\"}"
        escaped="${escaped//\$/\\\\\$}"
        escaped="${escaped//\`/\\\\\`}"
        printf '"%s"' "$escaped"
    else
        printf '%s' "$escaped"
    fi
}

encode_desktop_string() {
    local raw="$1"
    printf '%s' "${raw//\\/\\\\}"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --prefix)
            [[ $# -ge 2 ]] || fail "--prefix requires a directory argument"
            EXPLICIT_PREFIX="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            fail "unknown argument: $1"
            ;;
    esac
done

if [[ -n "$EXPLICIT_PREFIX" ]]; then
    mkdir -p "$EXPLICIT_PREFIX"
    PREFIX="$(cd "$EXPLICIT_PREFIX" && pwd)"
    BIN_DIR="$PREFIX/bin"
    DATA_DIR="$PREFIX/share"
else
    BIN_DIR="${HOME}/.local/bin"
    DATA_DIR="${XDG_DATA_HOME:-${HOME}/.local/share}"
    mkdir -p "$BIN_DIR" "$DATA_DIR"
    BIN_DIR="$(cd "$BIN_DIR" && pwd)"
    DATA_DIR="$(cd "$DATA_DIR" && pwd)"
fi

QUARRY_BIN="$BIN_DIR/quarry-gpui"
# Reject control characters or '=' in paths (unrepresentable in XDG Desktop Entry or line manifests).
if [[ "$QUARRY_BIN" == *$'\n'* || "$QUARRY_BIN" == *$'\r'* || "$QUARRY_BIN" == *=* ]]; then
    fail "installation path contains unsupported characters (newline or '='): $QUARRY_BIN"
fi
if [[ "$DATA_DIR" == *$'\n'* || "$DATA_DIR" == *$'\r'* ]]; then
    fail "data directory path contains unsupported newline characters: $DATA_DIR"
fi

APPS_DIR="$DATA_DIR/applications"
ICON_512_DIR="$DATA_DIR/icons/hicolor/512x512/apps"
ICON_256_DIR="$DATA_DIR/icons/hicolor/256x256/apps"
DOC_DIR="$DATA_DIR/doc/quarry"
MANIFEST_FILE="$DOC_DIR/installed-files.txt"
DESKTOP_SRC="$BUNDLE_DIR/share/applications/dev.quarry.gpui.desktop"
ICON_512_SRC="$BUNDLE_DIR/share/icons/hicolor/512x512/apps/dev.quarry.gpui.png"

for required in \
    "$BUNDLE_DIR/bin/quarry-gpui" \
    "$BUNDLE_DIR/bin/quarry" \
    "$DESKTOP_SRC" \
    "$ICON_512_SRC" \
    "$BUNDLE_DIR/share/doc/quarry/LICENSE-MIT" \
    "$BUNDLE_DIR/share/doc/quarry/LICENSE-APACHE" \
    "$BUNDLE_DIR/share/doc/quarry/THIRD_PARTY_NOTICES.html" \
    "$BUNDLE_DIR/uninstall.sh"; do
    [[ -e "$required" ]] || fail "missing bundle file: $required"
done

validate_bundle_arch
validate_linux_binaries

# Build exact list of target files before writing anything.
NEW_FILES=(
    "$BIN_DIR/quarry-gpui"
    "$BIN_DIR/quarry"
    "$APPS_DIR/dev.quarry.gpui.desktop"
    "$ICON_512_DIR/dev.quarry.gpui.png"
    "$DOC_DIR/LICENSE-MIT"
    "$DOC_DIR/LICENSE-APACHE"
    "$DOC_DIR/THIRD_PARTY_NOTICES.html"
    "$DOC_DIR/uninstall.sh"
    "$MANIFEST_FILE"
)
if [[ -f "$BUNDLE_DIR/share/icons/hicolor/256x256/apps/dev.quarry.gpui.png" ]]; then
    NEW_FILES+=("$ICON_256_DIR/dev.quarry.gpui.png")
fi
if [[ -f "$BUNDLE_DIR/share/doc/quarry/RELEASE_METADATA.json" ]]; then
    NEW_FILES+=("$DOC_DIR/RELEASE_METADATA.json")
fi
if [[ -d "$BUNDLE_DIR/share/doc/quarry/Rust" ]]; then
    for rust_doc in "$BUNDLE_DIR/share/doc/quarry/Rust/"*; do
        [[ -f "$rust_doc" ]] || continue
        NEW_FILES+=("$DOC_DIR/Rust/$(basename "$rust_doc")")
    done
fi

# Load previously Quarry-owned files (if upgrading an existing Quarry install).
OLD_FILES=()
if [[ -f "$MANIFEST_FILE" ]]; then
    while IFS= read -r line; do
        [[ -n "$line" ]] && OLD_FILES+=("$line")
    done < "$MANIFEST_FILE"
fi

is_in_list() {
    local needle="$1"
    shift
    local item
    for item in "$@"; do
        [[ "$item" == "$needle" ]] && return 0
    done
    return 1
}

# Refuse to overwrite any pre-existing file that Quarry does not own.
for dest in "${NEW_FILES[@]}"; do
    if [[ -e "$dest" || -L "$dest" ]]; then
        if [[ ${#OLD_FILES[@]} -eq 0 ]] || ! is_in_list "$dest" "${OLD_FILES[@]}"; then
            fail "refusing to overwrite existing unowned file: $dest (remove or move it, or pass a different --prefix)"
        fi
    fi
done

mkdir -p "$BIN_DIR" "$APPS_DIR" "$ICON_512_DIR" "$DOC_DIR"

# Write pre-commit ownership journal (OLD_FILES union NEW_FILES) before copying
# any files so even an uncatchable crash (SIGKILL) remains recoverable via retry
# or uninstall.sh, and register an EXIT rollback trap to clean newly created
# files on ordinary errors while preserving prior upgrade installations.
INSTALL_COMMITTED=0
CREATED_NEW_FILES=()
DESKTOP_TMP=""
MANIFEST_TMP=""

write_manifest() {
    MANIFEST_TMP="$(mktemp "$DOC_DIR/.installed-files.XXXXXX")"
    printf '%s\n' "$@" > "$MANIFEST_TMP"
    chmod 644 "$MANIFEST_TMP"
    mv -f "$MANIFEST_TMP" "$MANIFEST_FILE"
    MANIFEST_TMP=""
}

rollback_on_error() {
    [[ -n "$DESKTOP_TMP" ]] && rm -f -- "$DESKTOP_TMP"
    [[ -n "$MANIFEST_TMP" ]] && rm -f -- "$MANIFEST_TMP"
    if [[ "$INSTALL_COMMITTED" -eq 0 ]]; then
        if [[ ${#CREATED_NEW_FILES[@]} -gt 0 ]]; then
            local f
            for f in "${CREATED_NEW_FILES[@]}"; do
                rm -f -- "$f"
            done
        fi
        if [[ ${#OLD_FILES[@]} -eq 0 ]]; then
            rm -f -- "$MANIFEST_FILE"
            rmdir "$DOC_DIR/Rust" 2>/dev/null || true
            rmdir "$DOC_DIR" 2>/dev/null || true
            rmdir "$DATA_DIR/doc" 2>/dev/null || true
        fi
    fi
}
trap rollback_on_error EXIT

JOURNAL_FILES=()
if [[ ${#OLD_FILES[@]} -gt 0 ]]; then
    JOURNAL_FILES=("${OLD_FILES[@]}")
fi
for dest in "${NEW_FILES[@]}"; do
    if [[ ${#JOURNAL_FILES[@]} -eq 0 ]] || ! is_in_list "$dest" "${JOURNAL_FILES[@]}"; then
        JOURNAL_FILES+=("$dest")
    fi
done
write_manifest "${JOURNAL_FILES[@]}"

record_created_if_new() {
    local dest="$1"
    if [[ ${#OLD_FILES[@]} -eq 0 ]] || ! is_in_list "$dest" "${OLD_FILES[@]}"; then
        CREATED_NEW_FILES+=("$dest")
    fi
}

install_tracked() {
    local mode="$1" src="$2" dest="$3"
    record_created_if_new "$dest"
    install -m "$mode" "$src" "$dest"
}

install_tracked 755 "$BUNDLE_DIR/bin/quarry-gpui" "$BIN_DIR/quarry-gpui"
install_tracked 755 "$BUNDLE_DIR/bin/quarry" "$BIN_DIR/quarry"
install_tracked 644 "$ICON_512_SRC" "$ICON_512_DIR/dev.quarry.gpui.png"

if [[ -f "$BUNDLE_DIR/share/icons/hicolor/256x256/apps/dev.quarry.gpui.png" ]]; then
    mkdir -p "$ICON_256_DIR"
    install_tracked 644 \
        "$BUNDLE_DIR/share/icons/hicolor/256x256/apps/dev.quarry.gpui.png" \
        "$ICON_256_DIR/dev.quarry.gpui.png"
fi

for doc in LICENSE-MIT LICENSE-APACHE THIRD_PARTY_NOTICES.html RELEASE_METADATA.json; do
    if [[ -f "$BUNDLE_DIR/share/doc/quarry/$doc" ]]; then
        install_tracked 644 "$BUNDLE_DIR/share/doc/quarry/$doc" "$DOC_DIR/$doc"
    fi
done
if [[ -d "$BUNDLE_DIR/share/doc/quarry/Rust" ]]; then
    mkdir -p "$DOC_DIR/Rust"
    for rust_doc in "$BUNDLE_DIR/share/doc/quarry/Rust/"*; do
        [[ -f "$rust_doc" ]] || continue
        install_tracked 644 "$rust_doc" "$DOC_DIR/Rust/$(basename "$rust_doc")"
    done
fi
install_tracked 755 "$BUNDLE_DIR/uninstall.sh" "$DOC_DIR/uninstall.sh"

# Write installed .desktop entry with exact spec-encoded binary path so Wayland
# launchers (Omarchy Walker, Hyprland, GNOME, KDE) find Quarry regardless of PATH.
EXEC_VALUE="$(encode_desktop_exec_arg "$QUARRY_BIN") %F"
TRY_EXEC_VALUE="$(encode_desktop_string "$QUARRY_BIN")"

DESKTOP_TMP="$(mktemp "$APPS_DIR/.dev.quarry.gpui.desktop.XXXXXX")"
while IFS= read -r line || [[ -n "$line" ]]; do
    case "$line" in
        Exec=*)
            printf 'Exec=%s\n' "$EXEC_VALUE"
            ;;
        TryExec=*)
            printf 'TryExec=%s\n' "$TRY_EXEC_VALUE"
            ;;
        *)
            printf '%s\n' "$line"
            ;;
    esac
done < "$DESKTOP_SRC" > "$DESKTOP_TMP"
chmod 644 "$DESKTOP_TMP"
record_created_if_new "$APPS_DIR/dev.quarry.gpui.desktop"
mv -f "$DESKTOP_TMP" "$APPS_DIR/dev.quarry.gpui.desktop"
DESKTOP_TMP=""

# Remove only previously Quarry-owned files that are no longer part of this install.
if [[ ${#OLD_FILES[@]} -gt 0 ]]; then
    for old in "${OLD_FILES[@]}"; do
        if ! is_in_list "$old" "${NEW_FILES[@]}"; then
            rm -f -- "$old"
        fi
    done
    rmdir "$DOC_DIR/Rust" 2>/dev/null || true
fi

# Commit exact final ownership manifest.
write_manifest "${NEW_FILES[@]}"
INSTALL_COMMITTED=1
trap - EXIT

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q "$APPS_DIR" 2>/dev/null || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t -f "$DATA_DIR/icons/hicolor" 2>/dev/null || true
fi

printf 'Installed Quarry:\n'
printf '  Desktop app:  %s\n' "$BIN_DIR/quarry-gpui"
printf '  CLI:          %s\n' "$BIN_DIR/quarry"
printf '  Launcher:     %s\n' "$APPS_DIR/dev.quarry.gpui.desktop"
printf '  Uninstall:    %s\n' "$DOC_DIR/uninstall.sh"

case ":${PATH:-}:" in
    *":$BIN_DIR:"*)
        ;;
    *)
        printf '\nNote: %s is not currently in your PATH.\n' "$BIN_DIR"
        printf 'Add this line to your shell config (~/.bashrc or ~/.zshrc) to run `quarry` from any terminal:\n'
        printf '  export PATH="%s:$PATH"\n' "$BIN_DIR"
        ;;
esac

if [[ -n "$EXPLICIT_PREFIX" ]]; then
    default_data="${XDG_DATA_HOME:-${HOME}/.local/share}"
    if [[ "$DATA_DIR" != "$default_data" && "$DATA_DIR" != "/usr/share" && "$DATA_DIR" != "/usr/local/share" ]]; then
        printf '\nNote: installed desktop entry under custom data dir %s.\n' "$DATA_DIR"
        printf 'Ensure %s is in XDG_DATA_DIRS so your application launcher discovers Quarry.\n' "$DATA_DIR"
    fi
fi
