#!/usr/bin/env bash
#
# Package a built binary as CloudBridge.app inside a .dmg.
#
# A bare executable in a zip is not what macOS expects: Finder shows it as
# a document, it arrives without the execute bit, and it has no bundle to
# hang an identity, a version or an icon on. A .dmg with an app bundle and
# an Applications symlink is the drag-to-install shape people know.
#
# Usage: scripts/package-macos.sh <binary> <output.dmg> <version>
# Run from the repository root; writes CloudBridge.app and the dmg into the
# working directory.
set -euo pipefail

BINARY="$1"
DMG="$2"
VERSION="$3"
APP="CloudBridge.app"

rm -rf "$APP" dmg-root "$DMG"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BINARY" "$APP/Contents/MacOS/CloudBridge"
chmod +x "$APP/Contents/MacOS/CloudBridge"

# An icon is optional: without one the app shows the generic bundle icon.
if [ -f assets/icon.icns ]; then
  cp assets/icon.icns "$APP/Contents/Resources/CloudBridge.icns"
fi

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>CloudBridge</string>
    <key>CFBundleDisplayName</key>
    <string>CloudBridge</string>
    <key>CFBundleIdentifier</key>
    <string>io.github.jetsquirrel.cloudbridge</string>
    <key>CFBundleExecutable</key>
    <string>CloudBridge</string>
    <key>CFBundleIconFile</key>
    <string>CloudBridge</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>
    <key>CFBundleVersion</key>
    <string>${VERSION}</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.finance</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST

# Ad-hoc signature. There is no Developer ID to sign with, and an arm64
# bundle with no signature at all is refused outright rather than merely
# warned about. This still is not notarized: first launch needs a
# right-click -> Open.
codesign --force --deep --sign - "$APP"
codesign --verify --strict "$APP"

mkdir -p dmg-root
cp -R "$APP" dmg-root/
ln -s /Applications dmg-root/Applications
hdiutil create -volname CloudBridge -srcfolder dmg-root -ov -format UDZO "$DMG" >/dev/null
rm -rf dmg-root
