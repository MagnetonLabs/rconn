#!/usr/bin/env bash
# Build rconn.app and rconn-<version>.dmg from the universal binaries in a staging dir.
#   deploy/macos/make-dmg.sh <stage dir with rconn, rconn-viewer, *.dylib> <version> <out.dmg>
# Signing: ad-hoc only. With a Developer ID, sign with it and notarize instead.
set -euo pipefail
stage="$1"; version="$2"; out="$3"
here="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

app="$work/dmg/rconn.app"
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
  <key>CFBundleName</key><string>rconn tray</string>
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
  <key>CFBundleName</key><string>rconn</string>
  <key>CFBundleDisplayName</key><string>rconn</string>
  <key>CFBundleIdentifier</key><string>app.rconn.client</string>
  <key>CFBundleExecutable</key><string>rconn</string>
  <key>CFBundleIconFile</key><string>rconn</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${version}</string>
  <key>CFBundleVersion</key><string>${version}</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSLocalNetworkUsageDescription</key><string>rconn connects directly to computers on your network for remote sessions.</string>
</dict>
</plist>
PLIST
codesign --force --sign - "$helper" 2>/dev/null
codesign --force --sign - "$app/Contents/MacOS/rconn-viewer" 2>/dev/null
codesign --force --sign - --identifier app.rconn.host-agent "$app/Contents/MacOS/rconn-host-agent" 2>/dev/null
codesign --force --sign - "$app" 2>/dev/null
codesign --verify --strict "$app"
for lib in "$stage"/libopenh264-*.dylib; do
  cmp -s "$lib" "$app/Contents/Resources/$(basename "$lib")" || { echo "openh264 was modified: $lib" >&2; exit 1; }
done
ln -s /Applications "$work/dmg/Applications"
rm -f "$out"
hdiutil create -quiet -volname "rconn $version" -srcfolder "$work/dmg" -fs HFS+ -format UDZO "$out"
echo "$out"
