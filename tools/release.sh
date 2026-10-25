#!/bin/sh
# Build, sign, package, and (when credentials exist) notarize a release into dist/.
#
# Signing (see tools/sign-app.sh):
#   ASIDE_SIGN_IDENTITY        Developer ID Application identity; unset signs ad hoc.
#   ASIDE_SIGN_KEYCHAIN        keychain holding that identity (CI).
# Notarization, used only with a real identity and one complete credential set:
#   ASIDE_NOTARY_PROFILE                                       notarytool keychain profile, or
#   ASIDE_NOTARY_KEY + ASIDE_NOTARY_KEY_ID + ASIDE_NOTARY_ISSUER  App Store Connect API key (.p8 path), or
#   ASIDE_NOTARY_APPLE_ID + ASIDE_NOTARY_TEAM_ID + ASIDE_NOTARY_PASSWORD  app-specific password.
# Update signing, used when set:
#   ASIDE_UPDATE_SECRET_KEY    path to the Ed25519 secret key, or
#   ASIDE_UPDATE_SECRET_KEY_B64  the key itself (CI secret).
# Build:
#   ASIDE_SKIP_BUILD=1         package the existing target/bundle/aside.app.
#   Any other arguments go to tools/build-chromium.sh (e.g. --offline).
set -eu
cd "$(dirname "$0")/.."
ROOT=$PWD

if [ "${ASIDE_SKIP_BUILD:-0}" != "1" ]; then
    ./tools/build-chromium.sh --release "$@"
fi
APP_SRC=target/bundle/aside.app
[ -d "$APP_SRC" ] || { echo "missing $APP_SRC" >&2; exit 1; }
VERSION=$(/usr/bin/plutil -extract CFBundleVersion raw "$APP_SRC/Contents/Info.plist")
case "$(uname -m)" in arm64) ARCH=arm64 ;; *) ARCH=x86_64 ;; esac
IDENTITY=${ASIDE_SIGN_IDENTITY:--}
[ -n "$IDENTITY" ] || IDENTITY=-

DIST=dist
STAGE=$DIST/stage
rm -rf "$DIST"
mkdir -p "$STAGE"
APP=$STAGE/Aside.app
/usr/bin/ditto "$APP_SRC" "$APP"
# The build already signed with the same settings; re-signing keeps dist/ consistent
# when ASIDE_SIGN_IDENTITY differs from the one used at build time.
./tools/sign-app.sh "$APP"
/usr/bin/codesign --verify --deep --strict --verbose=2 "$APP"
SIGNED_BY=$(/usr/bin/codesign -dv "$APP" 2>&1 | sed -n 's/^Authority=//p' | head -1)
echo "signed: ${SIGNED_BY:-ad hoc}"

notary_args() {
    if [ -n "${ASIDE_NOTARY_PROFILE:-}" ]; then
        echo "--keychain-profile|$ASIDE_NOTARY_PROFILE"
    elif [ -n "${ASIDE_NOTARY_KEY:-}" ] && [ -n "${ASIDE_NOTARY_KEY_ID:-}" ] && [ -n "${ASIDE_NOTARY_ISSUER:-}" ]; then
        echo "--key|$ASIDE_NOTARY_KEY|--key-id|$ASIDE_NOTARY_KEY_ID|--issuer|$ASIDE_NOTARY_ISSUER"
    elif [ -n "${ASIDE_NOTARY_APPLE_ID:-}" ] && [ -n "${ASIDE_NOTARY_TEAM_ID:-}" ] && [ -n "${ASIDE_NOTARY_PASSWORD:-}" ]; then
        echo "--apple-id|$ASIDE_NOTARY_APPLE_ID|--team-id|$ASIDE_NOTARY_TEAM_ID|--password|$ASIDE_NOTARY_PASSWORD"
    fi
}
NOTARY=$(notary_args)
NOTARIZE=0
if [ -z "$NOTARY" ]; then
    echo "notarization: skipped (no credentials)"
elif [ "$IDENTITY" = "-" ]; then
    echo "notarization: skipped (ad hoc signatures cannot be notarized; set ASIDE_SIGN_IDENTITY)"
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

ZIP=$DIST/Aside-$VERSION-macos-$ARCH.zip
if [ "$NOTARIZE" = "1" ]; then
    /usr/bin/ditto -c -k --keepParent "$APP" "$DIST/notarize-app.zip"
    submit "$DIST/notarize-app.zip"
    rm -f "$DIST/notarize-app.zip"
    xcrun stapler staple "$APP"
    xcrun stapler validate "$APP"
fi
/usr/bin/ditto -c -k --keepParent "$APP" "$ZIP"

SIGN_TOOL=target/release/aside_sign_update
if [ -n "${ASIDE_UPDATE_SECRET_KEY:-}" ] || [ -n "${ASIDE_UPDATE_SECRET_KEY_B64:-}" ]; then
    [ -x "$SIGN_TOOL" ] || cargo build --locked --release --bin aside_sign_update
    if [ -n "${ASIDE_UPDATE_SECRET_KEY:-}" ]; then
        "$SIGN_TOOL" sign --secret "$ASIDE_UPDATE_SECRET_KEY" --version "$VERSION" "$ZIP" >/dev/null
    else
        "$SIGN_TOOL" sign --secret-env ASIDE_UPDATE_SECRET_KEY_B64 --version "$VERSION" "$ZIP" >/dev/null
    fi
    echo "update archive signed: $ZIP.sig"
else
    echo "update signing: skipped (no ASIDE_UPDATE_SECRET_KEY); the updater will not accept this archive"
fi

DMG=$DIST/Aside-$VERSION-macos-$ARCH.dmg
DMG_ROOT=$DIST/dmg
mkdir -p "$DMG_ROOT"
/usr/bin/ditto "$APP" "$DMG_ROOT/Aside.app"
ln -s /Applications "$DMG_ROOT/Applications"
/usr/bin/hdiutil create -quiet -volname "Aside $VERSION" -srcfolder "$DMG_ROOT" -fs APFS -format UDZO -ov "$DMG"
rm -rf "$DMG_ROOT"
if [ "$IDENTITY" != "-" ]; then
    set -- --force --sign "$IDENTITY" --timestamp
    [ -n "${ASIDE_SIGN_KEYCHAIN:-}" ] && set -- "$@" --keychain "$ASIDE_SIGN_KEYCHAIN"
    /usr/bin/codesign "$@" "$DMG"
fi
if [ "$NOTARIZE" = "1" ]; then
    submit "$DMG"
    xcrun stapler staple "$DMG"
    xcrun stapler validate "$DMG"
fi

# Mount without Finder (-nobrowse) at a private mount point and check the app inside.
MOUNT=$(mktemp -d /private/tmp/aside-dmg.XXXXXX)
/usr/bin/hdiutil attach -quiet -nobrowse -noverify -noautoopen -readonly -mountpoint "$MOUNT" "$DMG"
status=0
if [ ! -d "$MOUNT/Aside.app" ] || [ ! -L "$MOUNT/Applications" ]; then
    echo "DMG layout is wrong" >&2
    status=1
elif ! /usr/bin/codesign --verify --deep --strict "$MOUNT/Aside.app"; then
    echo "the app inside the DMG fails signature verification" >&2
    status=1
fi
/usr/bin/hdiutil detach -quiet "$MOUNT" || /usr/bin/hdiutil detach -quiet -force "$MOUNT"
rmdir "$MOUNT" 2>/dev/null || true
[ "$status" = "0" ] || exit 1
if [ "$NOTARIZE" = "1" ]; then
    /usr/sbin/spctl --assess --type execute --verbose=2 "$APP"
fi

(cd "$DIST" && /usr/bin/shasum -a 256 Aside-* > SHA256SUMS)
rm -rf "$STAGE"
echo "release $VERSION ($ARCH) in $ROOT/$DIST:"
ls -l "$DIST"
