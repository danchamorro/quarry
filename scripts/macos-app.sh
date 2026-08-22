#!/bin/bash

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP_NAME="Quarry"
EXECUTABLE="Quarry"
BUNDLE_ID="io.github.danchamorro.quarry"
PACKAGE_APP="$ROOT/target/package/$APP_NAME.app"
INSTALL_APP="/Applications/$APP_NAME.app"
LEGACY_INSTALL_APP="/Applications/Quarry Egui.app"
BACKUP_DIR="$HOME/Library/Application Support/Quarry/Backups"
BACKUP_ARCHIVE="$BACKUP_DIR/Quarry-previous.zip"
LEGACY_BACKUP_ARCHIVE="$BACKUP_DIR/Quarry-Egui-legacy.zip"
PLIST_TEMPLATE="$ROOT/packaging/macos/Info.plist"
ICON_SOURCE="$ROOT/assets/quarry-logo-v3.png"
LOCK_FILE="/private/tmp/$BUNDLE_ID.$UID.lock"
APP_INSTALL_LOCK_FILE="/private/tmp/$BUNDLE_ID.$UID.install.lock"
LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
PACKAGE_STAGE=""
INSTALL_STAGE=""
INSTALL_PREVIOUS=""
INSTALL_ACTIVATED=false
INSTALL_REPLACED=false
INSTALL_COMMITTED=false
BACKUP_TEMP=""
BACKUP_VERIFY_DIR=""

fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

require_macos() {
    [[ "$(uname -s)" == "Darwin" ]] || fail "macOS packaging must run on macOS."
    [[ -x "$LSREGISTER" ]] || fail "LaunchServices registration tool is unavailable."
    for command in cargo codesign git lipo lockf plutil rustc sips unzip; do
        command -v "$command" >/dev/null || fail "required command is unavailable: $command"
    done
}

plist_value() {
    /usr/bin/plutil -extract "$1" raw "$2"
}

verify_signed_app() {
    local app="$1"
    local plist="$app/Contents/Info.plist"
    local executable binary

    [[ -d "$app" ]] || { printf 'error: app bundle not found: %s\n' "$app" >&2; return 1; }
    /usr/bin/plutil -lint "$plist" >/dev/null || return 1
    executable="$(plist_value CFBundleExecutable "$plist")" || return 1
    [[ -n "$executable" ]] || return 1
    binary="$app/Contents/MacOS/$executable"
    [[ -x "$binary" ]] || { printf 'error: app executable is missing: %s\n' "$binary" >&2; return 1; }
    /usr/bin/codesign --verify --deep --strict --verbose=2 "$app"
}

verify_app_identity() {
    local app="$1"
    local plist="$app/Contents/Info.plist"

    verify_signed_app "$app" || return 1
    [[ "$(plist_value CFBundleIdentifier "$plist")" == "$BUNDLE_ID" ]] || {
        printf 'error: unexpected bundle identifier in %s\n' "$app" >&2
        return 1
    }
    [[ "$(plist_value CFBundleExecutable "$plist")" == "$EXECUTABLE" ]] || {
        printf 'error: unexpected bundle executable in %s\n' "$app" >&2
        return 1
    }
    for key in CFBundleShortVersionString CFBundleVersion QuarryArchitecture QuarryGitRevision QuarrySourceStatus; do
        [[ -n "$(plist_value "$key" "$plist")" ]] || return 1
    done
    [[ -f "$app/Contents/Resources/Quarry.icns" ]] || {
        printf 'error: app icon is missing from %s\n' "$app" >&2
        return 1
    }
}

verify_app() {
    local app="$1"
    local plist="$app/Contents/Info.plist"

    verify_app_identity "$app" || return 1
    [[ "$(plist_value CFBundleDocumentTypes.0.CFBundleTypeRole "$plist")" == Editor ]] || {
        printf 'error: CSV editor role is missing from %s\n' "$app" >&2
        return 1
    }
    [[ "$(plist_value CFBundleDocumentTypes.0.LSItemContentTypes.0 "$plist")" == public.comma-separated-values-text ]] || {
        printf 'error: CSV document support is missing from %s\n' "$app" >&2
        return 1
    }
}

register_app() {
    "$LSREGISTER" -f "$1"
}

make_icon() {
    /usr/bin/sips -s format icns "$ICON_SOURCE" --out "$1" >/dev/null
}

process_is_running() {
    local status

    if /usr/bin/pgrep -x "$1" >/dev/null 2>&1; then
        return 0
    else
        status=$?
    fi
    [[ "$status" -eq 1 ]] && return 1
    fail "could not inspect running processes before installation."
}

require_app_closed() {
    if process_is_running "$EXECUTABLE" \
        || process_is_running QuarryEgui \
        || process_is_running quarry-egui
    then
        fail "Quit Quarry before installing an update."
    fi
}

run_with_lock() {
    [[ "${QUARRY_PACKAGE_LOCKED:-}" == 1 ]] && return
    exec /usr/bin/lockf -k -t 0 "$LOCK_FILE" \
        /usr/bin/env QUARRY_PACKAGE_LOCKED=1 "$0" "$@"
}

acquire_app_install_lock() {
    exec 9>>"$APP_INSTALL_LOCK_FILE" \
        || fail "could not open the application installation lock."
    /usr/bin/lockf -s -t 0 9 || fail "Quit Quarry before installing an update."
}

backup_app() {
    local app="$1"
    local archive="$2"
    local extracted

    /bin/mkdir -p "$BACKUP_DIR"
    BACKUP_TEMP="$archive.tmp.$$"
    BACKUP_VERIFY_DIR="$(mktemp -d "$BACKUP_DIR/.verify.XXXXXX")"
    extracted="$BACKUP_VERIFY_DIR/$(basename "$app")"
    /bin/rm -f "$BACKUP_TEMP"
    if ! /usr/bin/ditto -c -k --sequesterRsrc --keepParent "$app" "$BACKUP_TEMP" \
        || ! /usr/bin/unzip -tq "$BACKUP_TEMP" >/dev/null \
        || ! /usr/bin/ditto -x -k "$BACKUP_TEMP" "$BACKUP_VERIFY_DIR" \
        || ! verify_signed_app "$extracted"
    then
        fail "could not create a verified rollback archive for $app"
    fi
    /bin/mv -f "$BACKUP_TEMP" "$archive"
    BACKUP_TEMP=""
    /bin/rm -rf "$BACKUP_VERIFY_DIR"
    BACKUP_VERIFY_DIR=""
    printf 'Saved rollback archive %s\n' "$archive"
}

package_app() {
    local package_id version build revision source_status source_changes
    local architecture host_target binary app plist post_revision post_changes

    [[ -f "$PLIST_TEMPLATE" ]] || fail "Info.plist template is missing."
    [[ -f "$ICON_SOURCE" ]] || fail "icon source is missing."

    package_id="$(cd "$ROOT" && cargo pkgid --offline -p quarry-egui)"
    version="${package_id##*#}"
    [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "app version must be numeric x.y.z: $version"

    git -C "$ROOT" rev-parse --is-inside-work-tree >/dev/null 2>&1 \
        || fail "packaging requires a Git checkout."
    [[ "$(git -C "$ROOT" rev-parse --is-shallow-repository)" == false ]] \
        || fail "packaging requires full Git history."
    revision="$(git -C "$ROOT" rev-parse --verify HEAD)"
    build="$(git -C "$ROOT" rev-list --count HEAD)"
    [[ "$build" =~ ^[1-9][0-9]{0,3}$ ]] \
        || fail "Git commit count is not a valid macOS build number: $build"
    if ! source_changes="$(git -C "$ROOT" status --porcelain --untracked-files=normal)"; then
        fail "could not determine whether the source checkout is clean."
    fi
    source_status=clean
    [[ -z "$source_changes" ]] || source_status=dirty
    host_target="$(rustc -vV | /usr/bin/sed -n 's/^host: //p')"
    [[ -n "$host_target" ]] || fail "could not determine the native Rust target."
    (
        cd "$ROOT"
        cargo build --release --locked --target "$host_target" --target-dir "$ROOT/target" -p quarry-egui
    )
    post_revision="$(git -C "$ROOT" rev-parse --verify HEAD)"
    [[ "$post_revision" == "$revision" ]] || fail "source revision changed during the build."
    if ! post_changes="$(git -C "$ROOT" status --porcelain --untracked-files=normal)"; then
        fail "could not recheck the source checkout after the build."
    fi
    [[ "$post_changes" == "$source_changes" ]] || fail "source checkout changed during the build."
    binary="$ROOT/target/$host_target/release/quarry-egui"
    architecture="$(/usr/bin/lipo -archs "$binary")"

    /bin/mkdir -p "$ROOT/target"
    PACKAGE_STAGE="$(mktemp -d "$ROOT/target/.quarry-package.XXXXXX")"
    app="$PACKAGE_STAGE/$APP_NAME.app"
    plist="$app/Contents/Info.plist"

    /bin/mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
    /usr/bin/install -m 0755 "$binary" "$app/Contents/MacOS/$EXECUTABLE"
    /bin/cp "$PLIST_TEMPLATE" "$plist"
    /usr/bin/plutil -replace CFBundleShortVersionString -string "$version" "$plist"
    /usr/bin/plutil -replace CFBundleVersion -string "$build" "$plist"
    /usr/bin/plutil -replace QuarryArchitecture -string "$architecture" "$plist"
    /usr/bin/plutil -replace QuarryGitRevision -string "$revision" "$plist"
    /usr/bin/plutil -replace QuarrySourceStatus -string "$source_status" "$plist"
    make_icon "$app/Contents/Resources/Quarry.icns"

    /usr/bin/codesign --force --deep --sign - --timestamp=none "$app"
    verify_app "$app" || fail "packaged app verification failed."

    /bin/mkdir -p "$(dirname "$PACKAGE_APP")"
    /bin/rm -rf "$PACKAGE_APP"
    /bin/mv "$app" "$PACKAGE_APP"
    /bin/rm -rf "$PACKAGE_STAGE"
    PACKAGE_STAGE=""

    printf 'Packaged %s %s (%s, %s, %s source)\n' \
        "$APP_NAME" "$version" "$revision" "$architecture" "$source_status"
    printf '%s\n' "$PACKAGE_APP"
}

install_app() {
    local candidate

    acquire_app_install_lock
    require_app_closed
    [[ -d /Applications && -w /Applications ]] || fail "/Applications is not writable for this account."
    package_app
    require_app_closed

    if [[ -e "$INSTALL_APP" ]]; then
        verify_app_identity "$INSTALL_APP" || fail "the current installed app is not a valid rollback source."
        backup_app "$INSTALL_APP" "$BACKUP_ARCHIVE"
    fi
    if [[ -e "$LEGACY_INSTALL_APP" ]]; then
        verify_signed_app "$LEGACY_INSTALL_APP" \
            || fail "the legacy installed app is not a valid rollback source."
        backup_app "$LEGACY_INSTALL_APP" "$LEGACY_BACKUP_ARCHIVE"
    fi

    require_app_closed
    INSTALL_STAGE="$(mktemp -d "/Applications/.quarry-install.XXXXXX")"
    candidate="$INSTALL_STAGE/candidate.app"
    INSTALL_PREVIOUS="$INSTALL_STAGE/previous.app"
    /usr/bin/ditto "$PACKAGE_APP" "$candidate"
    verify_app "$candidate" || fail "staged installation verification failed."
    require_app_closed

    if [[ -e "$INSTALL_APP" ]]; then
        INSTALL_REPLACED=true
        /bin/mv "$INSTALL_APP" "$INSTALL_PREVIOUS"
    fi
    INSTALL_ACTIVATED=true
    if ! /bin/mv "$candidate" "$INSTALL_APP"; then
        fail "installation failed; the previous app will be restored."
    fi
    if ! verify_app "$INSTALL_APP" \
        || ! /usr/bin/cmp -s "$PACKAGE_APP/Contents/Info.plist" "$INSTALL_APP/Contents/Info.plist" \
        || ! /usr/bin/cmp -s "$PACKAGE_APP/Contents/MacOS/$EXECUTABLE" "$INSTALL_APP/Contents/MacOS/$EXECUTABLE" \
        || ! register_app "$INSTALL_APP"
    then
        fail "installation verification failed; the previous app will be restored."
    fi

    INSTALL_COMMITTED=true
    /bin/rm -rf "$INSTALL_STAGE"
    INSTALL_STAGE=""
    INSTALL_PREVIOUS=""
    INSTALL_ACTIVATED=false
    INSTALL_REPLACED=false
    INSTALL_COMMITTED=false
    if [[ -e "$LEGACY_INSTALL_APP" ]]; then
        /bin/rm -rf "$LEGACY_INSTALL_APP"
        printf 'Removed legacy app %s\n' "$LEGACY_INSTALL_APP"
    fi
    /bin/rm -rf "$PACKAGE_APP"
    printf 'Installed and verified %s\n' "$INSTALL_APP"
}

rollback_install() {
    local restored=true

    [[ -n "$INSTALL_STAGE" ]] || return 0
    if [[ "$INSTALL_COMMITTED" != true ]]; then
        if [[ "$INSTALL_ACTIVATED" == true && -e "$INSTALL_APP" ]]; then
            /bin/rm -rf "$INSTALL_APP" || restored=false
        fi
        if [[ "$INSTALL_REPLACED" == true && -e "$INSTALL_PREVIOUS" ]]; then
            if [[ -e "$INSTALL_APP" ]] || ! /bin/mv "$INSTALL_PREVIOUS" "$INSTALL_APP"; then
                restored=false
            fi
        fi
        if [[ -e "$INSTALL_APP" ]] && ! register_app "$INSTALL_APP"; then
            restored=false
        fi
    fi
    if [[ "$restored" == true ]]; then
        case "$INSTALL_STAGE" in
            /Applications/.quarry-install.*) /bin/rm -rf "$INSTALL_STAGE" ;;
        esac
    else
        printf 'error: rollback app remains at %s\n' "$INSTALL_PREVIOUS" >&2
    fi
    INSTALL_STAGE=""
    INSTALL_PREVIOUS=""
    INSTALL_ACTIVATED=false
    INSTALL_REPLACED=false
    INSTALL_COMMITTED=false
    [[ "$restored" == true ]]
}

cleanup() {
    local status=$?

    set +e
    rollback_install
    case "$PACKAGE_STAGE" in
        "$ROOT"/target/.quarry-package.*) /bin/rm -rf "$PACKAGE_STAGE" ;;
    esac
    case "$BACKUP_VERIFY_DIR" in
        "$BACKUP_DIR"/.verify.*) /bin/rm -rf "$BACKUP_VERIFY_DIR" ;;
    esac
    case "$BACKUP_TEMP" in
        "$BACKUP_DIR"/*.tmp.*) /bin/rm -f "$BACKUP_TEMP" ;;
    esac
    return "$status"
}

self_test_rollback() {
    local test_root

    test_root="$(mktemp -d /private/tmp/quarry-rollback-test.XXXXXX)"
    LSREGISTER=/usr/bin/true
    INSTALL_APP="$test_root/Quarry.app"
    INSTALL_STAGE="$test_root/.quarry-install.test"
    INSTALL_PREVIOUS="$INSTALL_STAGE/previous.app"
    /bin/mkdir -p "$INSTALL_APP" "$INSTALL_PREVIOUS"
    /usr/bin/touch "$INSTALL_APP/new.marker" "$INSTALL_PREVIOUS/old.marker"
    INSTALL_ACTIVATED=true
    INSTALL_REPLACED=true
    INSTALL_COMMITTED=false
    rollback_install
    if [[ ! -f "$INSTALL_APP/old.marker" || -e "$INSTALL_APP/new.marker" ]]; then
        fail "rollback self-test failed; test data remains at $test_root"
    fi
    /bin/rm -rf "$test_root"
    printf 'Rollback self-test passed.\n'
}

usage() {
    printf 'Usage: %s package|install|verify [app-path] | self-test\n' "$0"
}

trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

require_macos
case "${1:-package}" in
    package | install) run_with_lock "$@" ;;
esac
case "${1:-package}" in
    package)
        [[ "$#" -eq 0 || "$#" -eq 1 ]] || { usage >&2; exit 2; }
        package_app
        ;;
    install)
        [[ "$#" -eq 1 ]] || { usage >&2; exit 2; }
        install_app
        ;;
    verify)
        [[ "$#" -le 2 ]] || { usage >&2; exit 2; }
        verify_app "${2:-$INSTALL_APP}" || fail "app verification failed."
        printf 'Verified %s\n' "${2:-$INSTALL_APP}"
        ;;
    self-test)
        [[ "$#" -eq 1 ]] || { usage >&2; exit 2; }
        self_test_rollback
        ;;
    *)
        usage >&2
        exit 2
        ;;
esac
