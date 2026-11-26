#!/usr/bin/env bash
# Assemble a .deb for Ubuntu 24.04+ from release binaries. Runs inside the Linux build
# image (deploy/linux/Dockerfile.build), from the repo root:
#   deploy/linux/make-deb.sh <dir-with-binaries> <openh264.so> <version> <arch: arm64|amd64> <out.deb>
set -euo pipefail
bins="$1" openh264="$2" version="$3" arch="$4" out="$5"
root="$(mktemp -d)"
trap 'rm -rf "$root"' EXIT
lib="$root/usr/lib/rconn"
mkdir -p "$lib" "$root/usr/bin" "$root/usr/share/applications" \
  "$root/usr/share/icons/hicolor/256x256/apps" "$root/DEBIAN"
for b in rconn rconn-viewer rconn-host-agent; do
  install -m 0755 "$bins/$b" "$lib/$b"
  strip "$lib/$b"
done
# Loaded at runtime from beside the binaries (Cisco's own build: their patent licence).
install -m 0644 "$openh264" "$lib/$(basename "$openh264")"
ln -s ../lib/rconn/rconn "$root/usr/bin/rconn"
ln -s ../lib/rconn/rconn-viewer "$root/usr/bin/rconn-viewer"
install -m 0644 crates/rconn-app/assets/icon-256.png "$root/usr/share/icons/hicolor/256x256/apps/rconn.png"
cat > "$root/usr/share/applications/rconn.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Rconn
Comment=Connect to your computers, and let them connect to this one
Exec=/usr/lib/rconn/rconn
Icon=rconn
Terminal=false
Categories=Network;RemoteAccess;
StartupWMClass=rconn
DESKTOP
size=$(du -sk "$root/usr" | cut -f1)
cat > "$root/DEBIAN/control" <<CONTROL
Package: rconn
Version: $version
Architecture: $arch
Maintainer: Magneton Labs <support@rconn.app>
Installed-Size: $size
Depends: libc6 (>= 2.36), libasound2t64 | libasound2, libx11-6, libxcursor1, libxrandr2, libxi6, libxkbcommon0, libxkbcommon-x11-0, libwayland-client0, libegl1, libpipewire-0.3-0t64 | libpipewire-0.3-0, libgtk-3-0t64 | libgtk-3-0, libayatana-appindicator3-1
Recommends: xdg-desktop-portal-gnome | xdg-desktop-portal
Section: net
Priority: optional
Description: Rconn remote desktop
 Connect to your own computers and control them, or let them connect to this one.
CONTROL
dpkg-deb --root-owner-group --build "$root" "$out" >/dev/null
echo "$out"
