#!/bin/sh
# Download the latest Atrium release and install it into /Applications (or ~/Applications).
#
#   curl -fsSL https://raw.githubusercontent.com/MohammadAlMasalmeh/atrium/main/tools/install.sh | sh
#
# Release builds are ad hoc signed and not notarized, so Gatekeeper would refuse to open a
# downloaded copy. This script removes the quarantine flag from the installed app only.
#   ATRIUM_REPO      owner/name to install from (default MohammadAlMasalmeh/atrium)
#   ATRIUM_VERSION   tag to install, e.g. v0.1.0 (default: latest release)
set -eu
REPO=${ATRIUM_REPO:-MohammadAlMasalmeh/atrium}
[ "$(uname -s)" = "Darwin" ] || { echo "Atrium runs on macOS only" >&2; exit 1; }
case "$(uname -m)" in arm64) ARCH=arm64 ;; *) ARCH=x86_64 ;; esac

if [ -n "${ATRIUM_VERSION:-}" ]; then
    API=https://api.github.com/repos/$REPO/releases/tags/$ATRIUM_VERSION
else
    API=https://api.github.com/repos/$REPO/releases/latest
fi
URL=$(curl -fsSL "$API" | sed -n 's/.*"browser_download_url": *"\([^"]*-macos-'"$ARCH"'\.zip\)".*/\1/p' | head -1)
[ -n "$URL" ] || { echo "no macOS $ARCH build in $API" >&2; exit 1; }

TMP=$(mktemp -d /private/tmp/atrium-install.XXXXXX)
trap 'rm -rf "$TMP"' EXIT
echo "downloading $URL"
curl -fL --progress-bar -o "$TMP/Atrium.zip" "$URL"
SUMS=$(dirname "$URL")/SHA256SUMS
if curl -fsSL -o "$TMP/SHA256SUMS" "$SUMS"; then
    expected=$(grep " $(basename "$URL")\$" "$TMP/SHA256SUMS" | cut -d' ' -f1)
    actual=$(/usr/bin/shasum -a 256 "$TMP/Atrium.zip" | cut -d' ' -f1)
    [ "$expected" = "$actual" ] || { echo "checksum mismatch for $(basename "$URL")" >&2; exit 1; }
fi
/usr/bin/ditto -x -k "$TMP/Atrium.zip" "$TMP"
/usr/bin/codesign --verify --deep --strict "$TMP/Atrium.app"

DEST=/Applications
[ -w "$DEST" ] || { DEST=$HOME/Applications; mkdir -p "$DEST"; }
if pgrep -xq atrium; then
    echo "quit Atrium before installing" >&2
    exit 1
fi
rm -rf "$DEST/Atrium.app"
/usr/bin/ditto "$TMP/Atrium.app" "$DEST/Atrium.app"
/usr/bin/xattr -dr com.apple.quarantine "$DEST/Atrium.app" 2>/dev/null || true
echo "installed $DEST/Atrium.app"
echo "open it with: open \"$DEST/Atrium.app\""
