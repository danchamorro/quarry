#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
DEFAULT_PREFIX="${HOME}/.local"
if [[ "$(basename "$SCRIPT_DIR")" == "quarry" ]] \
    && [[ "$(basename "$(dirname "$SCRIPT_DIR")")" == "doc" ]] \
    && [[ "$(basename "$(dirname "$(dirname "$SCRIPT_DIR")")")" == "share" ]]; then
    DEFAULT_PREFIX="$(cd "$SCRIPT_DIR/../../.." && pwd)"
fi

PREFIX="$DEFAULT_PREFIX"

usage() {
    cat <<'EOF'
Uninstall Quarry desktop app, CLI, launcher entry, icon, and bundled docs on Linux.
User settings under ~/.config/quarry are preserved.

Usage:
  ./uninstall.sh [--prefix DIR]

Options:
  --prefix DIR   Installation prefix (default: detected prefix or ~/.local)
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
if [[ ! -d "$PREFIX" ]]; then
    printf 'Nothing to remove: prefix does not exist (%s)\n' "$PREFIX"
    exit 0
fi
PREFIX="$(cd "$PREFIX" && pwd)"

rm -f \
    "$PREFIX/bin/quarry-gpui" \
    "$PREFIX/bin/quarry" \
    "$PREFIX/share/applications/dev.quarry.gpui.desktop" \
    "$PREFIX/share/icons/hicolor/512x512/apps/dev.quarry.gpui.png" \
    "$PREFIX/share/icons/hicolor/256x256/apps/dev.quarry.gpui.png"
rm -rf "$PREFIX/share/doc/quarry"

if command -v update-desktop-database >/dev/null 2>&1 && [[ -d "$PREFIX/share/applications" ]]; then
    update-desktop-database -q "$PREFIX/share/applications" 2>/dev/null || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1 && [[ -d "$PREFIX/share/icons/hicolor" ]]; then
    gtk-update-icon-cache -q -t -f "$PREFIX/share/icons/hicolor" 2>/dev/null || true
fi

printf 'Uninstalled Quarry from %s (user config in ~/.config/quarry preserved).\n' "$PREFIX"
