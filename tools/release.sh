#!/bin/sh
# Build, sign, package, and (when credentials exist) notarize a release into dist/.
#
# Signing (see tools/sign-app.sh):
#   ATRIUM_SIGN_IDENTITY        Developer ID Application identity; unset signs ad hoc.
#   ATRIUM_SIGN_KEYCHAIN        keychain holding that identity (CI).
# Notarization, used only with a real identity and one complete credential set:
#   ATRIUM_NOTARY_PROFILE                                       notarytool keychain profile, or
#   ATRIUM_NOTARY_KEY + ATRIUM_NOTARY_KEY_ID + ATRIUM_NOTARY_ISSUER  App Store Connect API key (.p8 path), or
#   ATRIUM_NOTARY_APPLE_ID + ATRIUM_NOTARY_TEAM_ID + ATRIUM_NOTARY_PASSWORD  app-specific password.
# Update signing, used when set:
#   ATRIUM_UPDATE_SECRET_KEY    path to the Ed25519 secret key, or
#   ATRIUM_UPDATE_SECRET_KEY_B64  the key itself (CI secret).
# Build:
#   ATRIUM_SKIP_BUILD=1         package the existing target/bundle/atrium.app.
#   Any other arguments go to tools/build-chromium.sh (e.g. --offline).
set -eu
cd "$(dirname "$0")/.."
ROOT=$PWD

if [ "${ATRIUM_SKIP_BUILD:-0}" != "1" ]; then
    ./tools/build-chromium.sh --release "$@"
fi
APP_SRC=target/bundle/atrium.app
[ -d "$APP_SRC" ] || { echo "missing $APP_SRC" >&2; exit 1; }
VERSION=$(/usr/bin/plutil -extract CFBundleVersion raw "$APP_SRC/Contents/Info.plist")
case "$(uname -m)" in arm64) ARCH=arm64 ;; *) ARCH=x86_64 ;; esac
IDENTITY=${ATRIUM_SIGN_IDENTITY:--}
[ -n "$IDENTITY" ] || IDENTITY=-

DIST=dist
STAGE=$DIST/stage
rm -rf "$DIST"
mkdir -p "$STAGE"
APP=$STAGE/Atrium.app
/usr/bin/ditto "$APP_SRC" "$APP"
# The build already signed with the same settings; re-signing keeps dist/ consistent
# when ATRIUM_SIGN_IDENTITY differs from the one used at build time.
./tools/sign-app.sh "$APP"
/usr/bin/codesign --verify --deep --strict --verbose=2 "$APP"
SIGNED_BY=$(/usr/bin/codesign -dv "$APP" 2>&1 | sed -n 's/^Authority=//p' | head -1)
echo "signed: ${SIGNED_BY:-ad hoc}"

notary_args() {
    if [ -n "${ATRIUM_NOTARY_PROFILE:-}" ]; then
        echo "--keychain-profile|$ATRIUM_NOTARY_PROFILE"
    elif [ -n "${ATRIUM_NOTARY_KEY:-}" ] && [ -n "${ATRIUM_NOTARY_KEY_ID:-}" ] && [ -n "${ATRIUM_NOTARY_ISSUER:-}" ]; then
        echo "--key|$ATRIUM_NOTARY_KEY|--key-id|$ATRIUM_NOTARY_KEY_ID|--issuer|$ATRIUM_NOTARY_ISSUER"
    elif [ -n "${ATRIUM_NOTARY_APPLE_ID:-}" ] && [ -n "${ATRIUM_NOTARY_TEAM_ID:-}" ] && [ -n "${ATRIUM_NOTARY_PASSWORD:-}" ]; then
        echo "--apple-id|$ATRIUM_NOTARY_APPLE_ID|--team-id|$ATRIUM_NOTARY_TEAM_ID|--password|$ATRIUM_NOTARY_PASSWORD"
    fi
}
NOTARY=$(notary_args)
NOTARIZE=0
if [ -z "$NOTARY" ]; then
    echo "notarization: skipped (no credentials)"
elif [ "$IDENTITY" = "-" ]; then
    echo "notarization: skipped (ad hoc signatures cannot be notarized; set ATRIUM_SIGN_IDENTITY)"
else
    NOTARIZE=1
fi
submit() {
    file=$1
    old_ifs=$IFS
    IFS='|'
    # shellcheck disable=SC2086
    set -- $NOTARY
    IFS=$old_ifs
    echo "notarizing $file"
    xcrun notarytool submit "$file" --wait --timeout 2h "$@"
}

ZIP=$DIST/Atrium-$VERSION-macos-$ARCH.zip
if [ "$NOTARIZE" = "1" ]; then
    /usr/bin/ditto -c -k --keepParent "$APP" "$DIST/notarize-app.zip"
    submit "$DIST/notarize-app.zip"
    rm -f "$DIST/notarize-app.zip"
    xcrun stapler staple "$APP"
    xcrun stapler validate "$APP"
fi
/usr/bin/ditto -c -k --keepParent "$APP" "$ZIP"

SIGN_TOOL=target/release/atrium_sign_update
if [ -n "${ATRIUM_UPDATE_SECRET_KEY:-}" ] || [ -n "${ATRIUM_UPDATE_SECRET_KEY_B64:-}" ]; then
    [ -x "$SIGN_TOOL" ] || cargo build --locked --release --bin atrium_sign_update
    if [ -n "${ATRIUM_UPDATE_SECRET_KEY:-}" ]; then
        "$SIGN_TOOL" sign --secret "$ATRIUM_UPDATE_SECRET_KEY" --version "$VERSION" "$ZIP" >/dev/null
    else
        "$SIGN_TOOL" sign --secret-env ATRIUM_UPDATE_SECRET_KEY_B64 --version "$VERSION" "$ZIP" >/dev/null
    fi
    echo "update archive signed: $ZIP.sig"
else
    echo "update signing: skipped (no ATRIUM_UPDATE_SECRET_KEY); the updater will not accept this archive"
fi

DMG=$DIST/Atrium-$VERSION-macos-$ARCH.dmg
DMG_ROOT=$DIST/dmg
mkdir -p "$DMG_ROOT"
/usr/bin/ditto "$APP" "$DMG_ROOT/Atrium.app"
ln -s /Applications "$DMG_ROOT/Applications"
/usr/bin/hdiutil create -quiet -volname "Atrium $VERSION" -srcfolder "$DMG_ROOT" -fs APFS -format UDZO -ov "$DMG"
rm -rf "$DMG_ROOT"
if [ "$IDENTITY" != "-" ]; then
    set -- --force --sign "$IDENTITY" --timestamp
    [ -n "${ATRIUM_SIGN_KEYCHAIN:-}" ] && set -- "$@" --keychain "$ATRIUM_SIGN_KEYCHAIN"
    /usr/bin/codesign "$@" "$DMG"
fi
if [ "$NOTARIZE" = "1" ]; then
    submit "$DMG"
    xcrun stapler staple "$DMG"
    xcrun stapler validate "$DMG"
fi

# Mount without Finder (-nobrowse) at a private mount point and check the app inside.
MOUNT=$(mktemp -d /private/tmp/atrium-dmg.XXXXXX)
/usr/bin/hdiutil attach -quiet -nobrowse -noverify -noautoopen -readonly -mountpoint "$MOUNT" "$DMG"
status=0
if [ ! -d "$MOUNT/Atrium.app" ] || [ ! -L "$MOUNT/Applications" ]; then
    echo "DMG layout is wrong" >&2
    status=1
elif ! /usr/bin/codesign --verify --deep --strict "$MOUNT/Atrium.app"; then
    echo "the app inside the DMG fails signature verification" >&2
    status=1
fi
/usr/bin/hdiutil detach -quiet "$MOUNT" || /usr/bin/hdiutil detach -quiet -force "$MOUNT"
rmdir "$MOUNT" 2>/dev/null || true
[ "$status" = "0" ] || exit 1
if [ "$NOTARIZE" = "1" ]; then
    /usr/sbin/spctl --assess --type execute --verbose=2 "$APP"
fi

(cd "$DIST" && /usr/bin/shasum -a 256 Atrium-* > SHA256SUMS)
rm -rf "$STAGE"
echo "release $VERSION ($ARCH) in $ROOT/$DIST:"
ls -l "$DIST"
