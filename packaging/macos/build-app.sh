#!/usr/bin/env bash
# Builds CrossCopy.app (universal), signs and notarizes it when credentials
# are present, and produces:
#   dist/CrossCopy-<version>-macos.zip   (used by the in-app updater)
#   dist/CrossCopy-<version>-macos.dmg   (for first-time installs)
#
# Usage: packaging/macos/build-app.sh <version>
# Optional env:
#   MACOS_SIGNING_IDENTITY  "Developer ID Application: Name (TEAMID)"; ad-hoc if unset
#   APPLE_ID, APPLE_APP_PASSWORD, APPLE_TEAM_ID  notarize when all are set
set -euo pipefail

VERSION="${1:?usage: build-app.sh <version>}"
cd "$(dirname "$0")/../.."

TARGETS=(aarch64-apple-darwin x86_64-apple-darwin)
for target in "${TARGETS[@]}"; do
    cargo build --release --locked -p crosscopy-tray -p crosscopy --target "$target"
done

APP=dist/CrossCopy.app
rm -rf "$APP" dist/iconset.iconset
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

for bin in crosscopy-tray crosscopy; do
    lipo -create -output "$APP/Contents/MacOS/$bin" \
        "target/aarch64-apple-darwin/release/$bin" \
        "target/x86_64-apple-darwin/release/$bin"
done
sed "s/__VERSION__/$VERSION/g" packaging/macos/Info.plist > "$APP/Contents/Info.plist"

# App icon from the rendered tile.
ICONSET=dist/iconset.iconset
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
    sips -z "$size" "$size" assets/app-icon.png --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -z "$double" "$double" assets/app-icon.png --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"
rm -rf "$ICONSET"

IDENTITY="${MACOS_SIGNING_IDENTITY:--}"
sign() {
    codesign --force --timestamp --options runtime --sign "$IDENTITY" "$@"
}
if [[ "$IDENTITY" == "-" ]]; then
    echo "warning: MACOS_SIGNING_IDENTITY not set; ad-hoc signing (no updates, Gatekeeper warnings)"
    sign() { codesign --force --sign - "$@"; }
fi
# Sign nested code first, then the bundle.
sign "$APP/Contents/MacOS/crosscopy"
sign "$APP"
codesign --verify --deep --strict "$APP"

ZIP="dist/CrossCopy-$VERSION-macos.zip"
DMG="dist/CrossCopy-$VERSION-macos.dmg"
notarize() {
    xcrun notarytool submit "$1" --apple-id "$APPLE_ID" --password "$APPLE_APP_PASSWORD" \
        --team-id "$APPLE_TEAM_ID" --wait
}
CAN_NOTARIZE=false
if [[ -n "${APPLE_ID:-}" && -n "${APPLE_APP_PASSWORD:-}" && -n "${APPLE_TEAM_ID:-}" && "$IDENTITY" != "-" ]]; then
    CAN_NOTARIZE=true
fi

ditto -c -k --keepParent "$APP" "$ZIP"
if $CAN_NOTARIZE; then
    notarize "$ZIP"
    xcrun stapler staple "$APP"
    # Re-zip so the updater ships the stapled app.
    rm "$ZIP"
    ditto -c -k --keepParent "$APP" "$ZIP"
else
    echo "warning: notarization credentials not set; skipping notarization"
fi

# Drag-to-Applications disk image.
STAGE=dist/dmg
rm -rf "$STAGE" "$DMG"
mkdir -p "$STAGE"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
hdiutil create -volname CrossCopy -srcfolder "$STAGE" -ov -format UDZO "$DMG"
rm -rf "$STAGE"
if [[ "$IDENTITY" != "-" ]]; then
    codesign --force --timestamp --sign "$IDENTITY" "$DMG"
fi
if $CAN_NOTARIZE; then
    notarize "$DMG"
    xcrun stapler staple "$DMG"
fi

echo "Built $ZIP and $DMG"
