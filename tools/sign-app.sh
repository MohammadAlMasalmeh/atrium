#!/bin/sh
# Sign an Aside.app bundle inside-out with per-process entitlements.
#
#   ASIDE_SIGN_IDENTITY     "Developer ID Application: ..." (or its SHA-1). Unset or "-" signs ad hoc.
#   ASIDE_SIGN_KEYCHAIN     optional keychain to search for the identity (CI).
#   ASIDE_HARDENED_RUNTIME  1 to use the hardened runtime for an ad hoc signature (local testing).
#
# A real identity always gets the hardened runtime and a secure timestamp, as notarization
# requires. Ad hoc code has no Team ID, so under the hardened runtime library validation
# rejects the ad hoc CEF framework; the ad hoc test mode therefore also disables library
# validation. That entitlement is never added to Developer ID builds.
set -eu
usage() { echo "usage: $0 path/to/App.app" >&2; exit 2; }
[ $# -eq 1 ] || usage
APP=${1%/}
[ -d "$APP/Contents/MacOS" ] || usage
ENTITLEMENTS="$(cd "$(dirname "$0")" && pwd)/entitlements"
IDENTITY=${ASIDE_SIGN_IDENTITY:--}
[ -n "$IDENTITY" ] || IDENTITY=-

WORK=$(mktemp -d /private/tmp/aside-sign.XXXXXX)
trap 'rm -rf "$WORK"' EXIT
set -- --force --sign "$IDENTITY"
ADHOC_RUNTIME=0
if [ "$IDENTITY" != "-" ]; then
    set -- "$@" --options runtime --timestamp
elif [ "${ASIDE_HARDENED_RUNTIME:-0}" = "1" ]; then
    set -- "$@" --options runtime --timestamp=none
    ADHOC_RUNTIME=1
else
    set -- "$@" --timestamp=none
fi
if [ -n "${ASIDE_SIGN_KEYCHAIN:-}" ]; then
    set -- "$@" --keychain "$ASIDE_SIGN_KEYCHAIN"
fi

# Prints the entitlements file for a profile, or nothing for none.
entitlements() {
    if [ "$ADHOC_RUNTIME" = "1" ]; then
        out="$WORK/${1:-none}.plist"
        if [ -n "$1" ]; then cp "$ENTITLEMENTS/$1.plist" "$out"; else /usr/bin/plutil -create xml1 "$out"; fi
        /usr/bin/plutil -replace 'com\.apple\.security\.cs\.disable-library-validation' -bool YES "$out" || return 1
        echo "$out"
    elif [ -n "$1" ]; then
        echo "$ENTITLEMENTS/$1.plist"
    fi
}
sign_with() {
    profile=$1
    shift
    file=$(entitlements "$profile") || exit 1
    if [ -n "$file" ]; then
        /usr/bin/codesign "$@" --entitlements "$file"
    else
        /usr/bin/codesign "$@"
    fi
}

FRAMEWORKS="$APP/Contents/Frameworks"
CEF="$FRAMEWORKS/Chromium Embedded Framework.framework"
for library in "$CEF/Libraries/"*.dylib; do
    [ -e "$library" ] && /usr/bin/codesign "$@" "$library"
done
/usr/bin/codesign "$@" "$CEF"

MAIN=$(/usr/bin/plutil -extract CFBundleExecutable raw "$APP/Contents/Info.plist")
for helper in "$FRAMEWORKS/$MAIN Helper"*.app; do
    [ -d "$helper" ] || continue
    case "$helper" in
        *"Helper (Renderer).app") profile=helper-renderer ;;
        *"Helper (GPU).app") profile=helper-gpu ;;
        *"Helper (Plugin).app") profile=helper-plugin ;;
        *"Helper (Alerts).app") profile= ;;
        *) profile=helper ;;
    esac
    sign_with "$profile" "$@" "$helper"
done
sign_with app "$@" "$APP"
/usr/bin/codesign --verify --deep --strict "$APP"
