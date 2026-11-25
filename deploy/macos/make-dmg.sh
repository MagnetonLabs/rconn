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
cp "$stage/rconn" "$stage/rconn-viewer" "$stage"/libopenh264-*.dylib "$app/Contents/MacOS/"
cp "$here/rconn.icns" "$app/Contents/Resources/"
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
codesign --force --deep --sign - "$app" >/dev/null
ln -s /Applications "$work/dmg/Applications"
rm -f "$out"
hdiutil create -quiet -volname "rconn $version" -srcfolder "$work/dmg" -fs HFS+ -format UDZO "$out"
echo "$out"
