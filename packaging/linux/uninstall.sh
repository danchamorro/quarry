#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
EXPLICIT_PREFIX=""

usage() {
    cat <<'EOF'
Uninstall Quarry desktop app, CLI, launcher entry, icon, and bundled docs on Linux.
Only files recorded in Quarry's installation manifest are removed; user settings under
~/.config/quarry and any unrelated files are preserved.

Usage:
  ./uninstall.sh [--prefix DIR]

Options:
  --prefix DIR   Installation prefix (default: detected prefix or $XDG_DATA_HOME / ~/.local)
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
    if [[ ! -d "$EXPLICIT_PREFIX" ]]; then
        printf 'Nothing to remove: prefix does not exist (%s)\n' "$EXPLICIT_PREFIX"
        exit 0
    fi
    PREFIX="$(cd "$EXPLICIT_PREFIX" && pwd)"
    DATA_DIR="$PREFIX/share"
    DOC_DIR="$DATA_DIR/doc/quarry"
elif [[ "$(basename "$SCRIPT_DIR")" == "quarry" ]] \
    && [[ "$(basename "$(dirname "$SCRIPT_DIR")")" == "doc" ]] \
    && [[ -f "$SCRIPT_DIR/installed-files.txt" ]]; then
    DOC_DIR="$SCRIPT_DIR"
    DATA_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
else
    DATA_DIR="${XDG_DATA_HOME:-${HOME}/.local/share}"
    if [[ -d "$DATA_DIR" ]]; then
        DATA_DIR="$(cd "$DATA_DIR" && pwd)"
    fi
    DOC_DIR="$DATA_DIR/doc/quarry"
fi

if [[ ! -d "$DATA_DIR" ]]; then
    printf 'No Quarry installation found under %s; nothing removed.\n' "$DATA_DIR"
    exit 0
fi

LOCK_PATH="$DATA_DIR/.quarry-install.lock"
LOCK_MODE=""
LOCK_HELD=0
release_lock() {
    if [[ "$LOCK_HELD" -eq 1 ]]; then
        if [[ "$LOCK_MODE" == "flock" ]]; then
            exec 9>&- 2>/dev/null || true
        elif [[ "$LOCK_MODE" == "dir" ]]; then
            rm -f -- "${LOCK_PATH}.d/pid"
            rmdir "${LOCK_PATH}.d" 2>/dev/null || true
        fi
        LOCK_HELD=0
    fi
}
trap release_lock EXIT

if command -v flock >/dev/null 2>&1; then
    exec 9>"$LOCK_PATH"
    if ! flock -n 9; then
        fail "another Quarry install or uninstall is currently in progress (lock: $LOCK_PATH)"
    fi
    LOCK_MODE="flock"
    LOCK_HELD=1
else
    lock_dir="${LOCK_PATH}.d"
    if ! mkdir "$lock_dir" 2>/dev/null; then
        owner_pid=""
        if [[ -f "$lock_dir/pid" ]]; then
            owner_pid="$(tr -d '[:space:]' < "$lock_dir/pid" 2>/dev/null || true)"
        fi
        if [[ "$owner_pid" =~ ^[0-9]+$ ]] && ! kill -0 "$owner_pid" 2>/dev/null; then
            rm -f -- "$lock_dir/pid"
            rmdir "$lock_dir" 2>/dev/null || true
        fi
        if ! mkdir "$lock_dir" 2>/dev/null; then
            fail "another Quarry install or uninstall is currently in progress (lock: $lock_dir)"
        fi
    fi
    printf '%s\n' "$$" > "$lock_dir/pid"
    LOCK_MODE="dir"
    LOCK_HELD=1
fi

MANIFEST_FILE="$DOC_DIR/installed-files.txt"
if [[ ! -f "$MANIFEST_FILE" ]]; then
    printf 'No Quarry installation manifest found at %s; nothing removed.\n' "$MANIFEST_FILE"
    exit 0
fi

RECORDED_FILES=()
while IFS= read -r path; do
    [[ -n "$path" ]] || continue
    case "$path" in
        /*) ;;
        *) fail "invalid non-absolute path in installation manifest: $path" ;;
    esac
    case "/$path/" in
        *"/../"*) fail "invalid traversal path in installation manifest: $path" ;;
    esac
    RECORDED_FILES+=("$path")
done < "$MANIFEST_FILE"

for path in "${RECORDED_FILES[@]}"; do
    rm -f -- "$path"
done

# Remove only empty Quarry-owned directories; never recursively delete unknown files.
rmdir "$DOC_DIR/Rust" 2>/dev/null || true
rmdir "$DOC_DIR" 2>/dev/null || true
rmdir "$DATA_DIR/doc" 2>/dev/null || true

if command -v update-desktop-database >/dev/null 2>&1 && [[ -d "$DATA_DIR/applications" ]]; then
    update-desktop-database -q "$DATA_DIR/applications" 2>/dev/null || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1 && [[ -d "$DATA_DIR/icons/hicolor" ]]; then
    gtk-update-icon-cache -q -t -f "$DATA_DIR/icons/hicolor" 2>/dev/null || true
fi

printf 'Uninstalled Quarry (recorded files removed; ~/.config/quarry and any extra files preserved).\n'
