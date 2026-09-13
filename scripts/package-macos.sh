#!/usr/bin/env bash
#
# Package a built binary as CloudBridge.app inside a .dmg, signed with a
# Developer ID and notarized by Apple.
#
# A bare executable in a zip is not what macOS expects: Finder shows it as
# a document, it arrives without the execute bit, and it has no bundle to
# hang an identity, a version or an icon on. A .dmg with an app bundle and
# an Applications symlink is the drag-to-install shape people know.
#
# Signing and notarization are *required*, not optional. An ad-hoc signed
# build is refused by Gatekeeper on every current macOS — and since 15
# (Sequoia) there is no Control-click bypass left, so an unnotarized
# artifact is not something a user can reasonably open. Rather than let one
# ship by accident, this script fails when the credentials are absent.
#
# Usage: scripts/package-macos.sh <binary> <output.dmg> <version>
# Run from the repository root; writes CloudBridge.app and the dmg into the
# working directory.
#
# Required environment:
#   NOTARY_KEY        Path to the App Store Connect API key (.p8)
#   NOTARY_KEY_ID     That key's Key ID
#   NOTARY_ISSUER     The issuer UUID the key belongs to
#
# Optional:
#   MACOS_SIGN_IDENTITY  Signing identity. Defaults to the one
#                        "Developer ID Application" identity in the
#                        keychain, which is what CI imports.
set -euo pipefail

BINARY="$1"
DMG="$2"
VERSION="$3"
APP="CloudBridge.app"

die() {
  echo "package-macos: $*" >&2
  exit 1
}

# ---------------------------------------------------------------------------
# Credentials, checked before anything is built: a failure here is a
# configuration mistake, and finding out after a five-minute build is worse
# than finding out now.
# ---------------------------------------------------------------------------

if [ -z "${MACOS_SIGN_IDENTITY:-}" ]; then
  # `security find-identity` prints one indented line per identity:
  #   1) <40 hex> "Developer ID Application: Name (TEAMID)"
  # No mapfile: the macOS runners (and /bin/bash on every shipping macOS)
  # are bash 3.2.
  IDENTITIES=()
  while IFS= read -r line; do
    IDENTITIES+=("$line")
  done < <(
    security find-identity -v -p codesigning |
      sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p'
  )
  case ${#IDENTITIES[@]} in
    1) MACOS_SIGN_IDENTITY="${IDENTITIES[0]}" ;;
    0) die "no 'Developer ID Application' identity in the keychain.

An 'Apple Development' certificate cannot sign for distribution. Create a
Developer ID Application certificate at
https://developer.apple.com/account/resources/certificates, install it, and
re-run. To pick an identity explicitly, set MACOS_SIGN_IDENTITY." ;;
    *) die "${#IDENTITIES[@]} Developer ID Application identities found; set \
MACOS_SIGN_IDENTITY to the one to use:
$(printf '  %s\n' "${IDENTITIES[@]}")" ;;
  esac
fi

for var in NOTARY_KEY NOTARY_KEY_ID NOTARY_ISSUER; do
  [ -n "${!var:-}" ] || die "$var is not set.

Notarization needs an App Store Connect API key. Create one under
App Store Connect -> Users and Access -> Integrations -> Keys, then set
NOTARY_KEY (path to the .p8), NOTARY_KEY_ID and NOTARY_ISSUER."
done
[ -f "$NOTARY_KEY" ] || die "NOTARY_KEY points at no file: $NOTARY_KEY"

echo "package-macos: signing as $MACOS_SIGN_IDENTITY"

# ---------------------------------------------------------------------------
# The bundle
# ---------------------------------------------------------------------------

rm -rf "$APP" dmg-root "$DMG"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BINARY" "$APP/Contents/MacOS/CloudBridge"
chmod +x "$APP/Contents/MacOS/CloudBridge"

# An icon is optional: without one the app shows the generic bundle icon.
if [ -f assets/icon.icns ]; then
  cp assets/icon.icns "$APP/Contents/Resources/CloudBridge.icns"
fi

# Theme JSON files: main.rs looks for ../Resources/themes relative to the
# executable when ./themes does not exist in the working directory.
if [ -d themes ]; then
  cp -R themes "$APP/Contents/Resources/themes"
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

# ---------------------------------------------------------------------------
# Signing
#
# `--options runtime` (the hardened runtime) and `--timestamp` are both
# preconditions for notarization, not preferences.
#
# No entitlements file: nothing here needs an exception from the hardened
# runtime. The binary is statically linked, loads no plug-ins, and reaches
# the keychain through the Security framework, which needs no entitlement.
# A dependency that wants JIT or an unsigned dylib would need one — this is
# where it would go.
#
# Inside out, and no `--deep`: Apple discourages it, and the only nested
# code here is the main executable.
# ---------------------------------------------------------------------------

sign() {
  codesign --force --timestamp --options runtime \
    --sign "$MACOS_SIGN_IDENTITY" "$1"
}

sign "$APP/Contents/MacOS/CloudBridge"
sign "$APP"
codesign --verify --deep --strict --verbose=2 "$APP"

# ---------------------------------------------------------------------------
# Notarization
#
# One submission, not two. The notary service scans the .dmg's contents, so
# the enclosed .app gets its ticket from the same submission. The trade-off:
# the .app inside carries no stapled ticket of its own, so Gatekeeper's
# first-launch check needs network. That is the shape most Mac apps ship in,
# and it halves our exposure to the notary queue — fresh teams see hours of
# queue latency per submission, and two sequential hour-long waits do not
# fit in a CI job. If an offline-first-launch guarantee ever matters, add a
# second submission for the zipped .app here.
#
# The wait budget is 5 hours for the same reason: a new team's first
# submissions routinely sit In Progress for several hours. The client
# timing out does not stop the server-side submission, but the run needs
# the verdict to staple, so the budget has to outlast the queue.
# ---------------------------------------------------------------------------

mkdir -p dmg-root
cp -R "$APP" dmg-root/
ln -s /Applications dmg-root/Applications
hdiutil create -volname CloudBridge -srcfolder dmg-root -ov -format UDZO "$DMG" >/dev/null
rm -rf dmg-root

echo "package-macos: notarizing $DMG"
SUBMIT_LOG=$(mktemp -t notarytool)
if ! xcrun notarytool submit "$DMG" \
    --key "$NOTARY_KEY" \
    --key-id "$NOTARY_KEY_ID" \
    --issuer "$NOTARY_ISSUER" \
    --wait --timeout 300m | tee "$SUBMIT_LOG"; then
  # Rejected or timed out: Apple's log says exactly which and why.
  SUBMISSION_ID=$(sed -n 's/^  id: //p' "$SUBMIT_LOG" | head -1)
  if [ -n "$SUBMISSION_ID" ]; then
    xcrun notarytool log "$SUBMISSION_ID" \
      --key "$NOTARY_KEY" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER" || true
  fi
  die "notarization did not complete cleanly (see log above)"
fi
xcrun stapler staple "$DMG"

# ---------------------------------------------------------------------------
# What a user's Mac will conclude. `spctl` here is the whole point of the
# exercise: it is the check that rejected every build before this one. The
# .app assessment works only online: its ticket lives on Apple's servers,
# not stapled to the bundle (see the notarization comment above).
# ---------------------------------------------------------------------------

xcrun stapler validate "$DMG"
spctl --assess --type execute --verbose=2 "$APP"
spctl --assess --type open --context context:primary-signature --verbose=2 "$DMG"

echo "package-macos: $DMG is signed, notarized and stapled"
