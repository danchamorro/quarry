#!/usr/bin/env bash
set -euo pipefail

BUNDLE_DIR="$(cd "$(dirname "$0")" && pwd)"
PREFIX="${HOME}/.local"

usage() {
    cat <<'EOF'
Install Quarry desktop app, CLI, launcher entry, icon, and license notices on Linux.

Usage:
  ./install.sh [--prefix DIR]

Options:
  --prefix DIR   Installation prefix (default: ~/.local)
  -h, --help     Show this help message
EOF
}

fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --prefix)
            [[ $# -ge 2 ]] || fail "--prefix requires a directory argument"
            PREFIX="$2"
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

[[ -n "$PREFIX" ]] || fail "--prefix must not be empty"
mkdir -p "$PREFIX"
PREFIX="$(cd "$PREFIX" && pwd)"

BIN_DIR="$PREFIX/bin"
APPS_DIR="$PREFIX/share/applications"
ICON_512_DIR="$PREFIX/share/icons/hicolor/512x512/apps"
DOC_DIR="$PREFIX/share/doc/quarry"
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

mkdir -p "$BIN_DIR" "$APPS_DIR" "$ICON_512_DIR" "$DOC_DIR"

install -m 755 "$BUNDLE_DIR/bin/quarry-gpui" "$BIN_DIR/quarry-gpui"
install -m 755 "$BUNDLE_DIR/bin/quarry" "$BIN_DIR/quarry"
install -m 644 "$ICON_512_SRC" "$ICON_512_DIR/dev.quarry.gpui.png"

if [[ -f "$BUNDLE_DIR/share/icons/hicolor/256x256/apps/dev.quarry.gpui.png" ]]; then
    mkdir -p "$PREFIX/share/icons/hicolor/256x256/apps"
    install -m 644 \
        "$BUNDLE_DIR/share/icons/hicolor/256x256/apps/dev.quarry.gpui.png" \
        "$PREFIX/share/icons/hicolor/256x256/apps/dev.quarry.gpui.png"
fi

rm -rf "$DOC_DIR/Rust"
for doc in LICENSE-MIT LICENSE-APACHE THIRD_PARTY_NOTICES.html RELEASE_METADATA.json; do
    if [[ -f "$BUNDLE_DIR/share/doc/quarry/$doc" ]]; then
        install -m 644 "$BUNDLE_DIR/share/doc/quarry/$doc" "$DOC_DIR/$doc"
    fi
done
if [[ -d "$BUNDLE_DIR/share/doc/quarry/Rust" ]]; then
    mkdir -p "$DOC_DIR/Rust"
    for rust_doc in "$BUNDLE_DIR/share/doc/quarry/Rust/"*; do
        [[ -f "$rust_doc" ]] || continue
        install -m 644 "$rust_doc" "$DOC_DIR/Rust/$(basename "$rust_doc")"
    done
fi
install -m 755 "$BUNDLE_DIR/uninstall.sh" "$DOC_DIR/uninstall.sh"

# Write installed .desktop entry with exact binary path so Wayland launchers
# (Omarchy Walker, Hyprland, GNOME, KDE) find Quarry even when ~/.local/bin is
# not in the compositor's login PATH.
QUARRY_BIN="$BIN_DIR/quarry-gpui"
# Escape backslashes and double quotes for XDG Desktop Entry Exec quoting.
ESCAPED_EXEC="$QUARRY_BIN"
ESCAPED_EXEC="${ESCAPED_EXEC//\\/\\\\}"
ESCAPED_EXEC="${ESCAPED_EXEC//\"/\\\"}"
ESCAPED_EXEC="${ESCAPED_EXEC//\$/\\\$}"
ESCAPED_EXEC="${ESCAPED_EXEC//\`/\\\`}"
if [[ "$QUARRY_BIN" == *[[:space:]]* ]]; then
    EXEC_VALUE="\"$ESCAPED_EXEC\" %F"
else
    EXEC_VALUE="$ESCAPED_EXEC %F"
fi

DESKTOP_TMP="$(mktemp "$APPS_DIR/.dev.quarry.gpui.desktop.XXXXXX")"
trap 'rm -f "$DESKTOP_TMP"' EXIT
while IFS= read -r line || [[ -n "$line" ]]; do
    case "$line" in
        Exec=*)
            printf 'Exec=%s\n' "$EXEC_VALUE"
            ;;
        TryExec=*)
            printf 'TryExec=%s\n' "$QUARRY_BIN"
            ;;
        *)
            printf '%s\n' "$line"
            ;;
    esac
done < "$DESKTOP_SRC" > "$DESKTOP_TMP"
chmod 644 "$DESKTOP_TMP"
mv -f "$DESKTOP_TMP" "$APPS_DIR/dev.quarry.gpui.desktop"
trap - EXIT

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q "$APPS_DIR" 2>/dev/null || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t -f "$PREFIX/share/icons/hicolor" 2>/dev/null || true
fi

if [[ "$(uname -s)" == "Linux" ]] && command -v ldd >/dev/null 2>&1; then
    missing_libs="$(ldd "$BIN_DIR/quarry-gpui" 2>/dev/null | grep '=> not found' || true)"
    if [[ -n "$missing_libs" ]]; then
        printf '\nwarning: missing runtime shared libraries detected for quarry-gpui:\n%s\n' "$missing_libs" >&2
        cat >&2 <<'EOF'
Install the required system libraries for your distribution:
  Arch / Omarchy:
    sudo pacman -S --needed wayland libxkbcommon vulkan-icd-loader fontconfig freetype2 libxcb libx11 libxcursor libxrandr libxi libglvnd
  Ubuntu / Debian:
    sudo apt-get install -y libwayland-client0 libxkbcommon0 libxkbcommon-x11-0 libvulkan1 libfontconfig1 libfreetype6 libxcb1
EOF
    fi
fi

printf 'Installed Quarry into %s\n' "$PREFIX"
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
