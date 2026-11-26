#!/usr/bin/env bash
# Build Rconn.app and Rconn-<version>.dmg from the universal binaries in a staging dir.
#   deploy/macos/make-dmg.sh <stage dir with rconn, rconn-viewer, *.dylib> <version> <out.dmg>
# Signing: ad-hoc only. With a Developer ID, sign with it and notarize instead.
set -euo pipefail
stage="$1"; version="$2"; out="$3"
here="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

app="$work/dmg/Rconn.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$stage/rconn" "$stage/rconn-viewer" "$stage/rconn-host-agent" "$app/Contents/MacOS/"
# Menu-bar icon: its own helper app (own bundle id, no Dock icon), so macOS still opens the
# window when rconn is launched while the icon runs.
helper="$app/Contents/Helpers/rconn-tray.app"
mkdir -p "$helper/Contents/MacOS"
cp "$stage/rconn-tray" "$helper/Contents/MacOS/"
cat > "$helper/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Rconn</string>
  <key>CFBundleIdentifier</key><string>app.rconn.tray</string>
  <key>CFBundleExecutable</key><string>rconn-tray</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${version}</string>
  <key>CFBundleVersion</key><string>${version}</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSUIElement</key><true/>
</dict>
</plist>
PLIST
# Cisco's openh264 goes in Resources, untouched: it is Cisco-signed, and the loader checks
# its hash, so it must never be re-signed (no `codesign --deep`).
cp "$stage"/libopenh264-*.dylib "$here/rconn.icns" "$app/Contents/Resources/"
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Rconn</string>
  <key>CFBundleDisplayName</key><string>Rconn</string>
  <key>CFBundleIdentifier</key><string>app.rconn.client</string>
  <key>CFBundleExecutable</key><string>rconn</string>
  <key>CFBundleIconFile</key><string>rconn</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${version}</string>
  <key>CFBundleVersion</key><string>${version}</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSLocalNetworkUsageDescription</key><string>Rconn connects directly to computers on your network for remote sessions.</string>
</dict>
</plist>
PLIST
# A real signing identity keeps the app's identity stable across builds, so macOS keeps
# its Screen Recording / Accessibility grants after an update. Ad-hoc signing ("-") ties
# the grants to one build's hash: every update would ask again.
sign_id="${RCONN_SIGN_ID:-$(security find-identity -v -p codesigning 2>/dev/null \
  | sed -n 's/.*\([0-9A-F]\{40\}\) "Developer ID Application.*/\1/p' | head -1)}"
sign_id="${sign_id:--}"
[ "$sign_id" = "-" ] && echo "warning: ad-hoc signing; macOS permissions reset on every update" >&2
# Notarization needs the hardened runtime and a secure timestamp on everything we sign.
# The runtime refuses libraries signed by another team, so the programs that load Cisco's
# openh264 (signed by Cisco) may load other teams' libraries; nothing else is relaxed.
ents="$work/entitlements.plist"
cat > "$ents" <<'ENTS'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>com.apple.security.cs.disable-library-validation</key><true/>
</dict>
</plist>
ENTS
if [ "$sign_id" = "-" ]; then
  sign() { codesign --force --sign - "$@"; }
else
  sign() { codesign --force --sign "$sign_id" --options runtime --timestamp "$@"; }
fi
sign --identifier app.rconn.tray "$helper"
sign --identifier app.rconn.viewer --entitlements "$ents" "$app/Contents/MacOS/rconn-viewer"
sign --identifier app.rconn.host-agent --entitlements "$ents" "$app/Contents/MacOS/rconn-host-agent"
sign --entitlements "$ents" "$app"
codesign --verify --strict "$app"
for lib in "$stage"/libopenh264-*.dylib; do
  cmp -s "$lib" "$app/Contents/Resources/$(basename "$lib")" || { echo "openh264 was modified: $lib" >&2; exit 1; }
done
ln -s /Applications "$work/dmg/Applications"
rm -f "$out"
hdiutil create -quiet -volname "Rconn $version" -srcfolder "$work/dmg" -fs HFS+ -format UDZO "$out"
[ "$sign_id" = "-" ] || codesign --force --sign "$sign_id" --timestamp "$out"
# Notarize with a notarytool keychain profile (`xcrun notarytool store-credentials`), so
# macOS opens the downloaded app without warnings. Skipped when the profile isn't there.
profile="${RCONN_NOTARY_PROFILE:-AC_PASSWORD}"
if [ "$sign_id" != "-" ] && xcrun notarytool history --keychain-profile "$profile" >/dev/null 2>&1; then
  xcrun notarytool submit "$out" --keychain-profile "$profile" --wait >&2
  xcrun stapler staple "$out" >&2
  spctl --assess --type open --context context:primary-signature -v "$out" >&2
else
  echo "warning: not notarized (no notarytool profile \"$profile\")" >&2
fi
echo "$out"
