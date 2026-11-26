#!/bin/sh
# Rconn for Linux, just for you: no sudo. Unpack the .tar.gz, then run
#   ./install.sh               install or upgrade into ~/.local and open Rconn
#   ./install.sh --uninstall   remove it (and this computer's Rconn ID for your user)
# The computer gets an ID at Rconn's first start and takes connections while you're
# signed in (a systemd user service). For every user on this computer, use the .deb.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
lib="$HOME/.local/lib/rconn"
bin="$HOME/.local/bin"
share="${XDG_DATA_HOME:-$HOME/.local/share}"
config="${XDG_CONFIG_HOME:-$HOME/.config}"
desktop="$share/applications/rconn.desktop"
icon="$share/icons/hicolor/256x256/apps/rconn.png"
unit="$config/systemd/user/rconn-host.service"

if [ "${1:-}" = "--uninstall" ]; then
  systemctl --user disable --now rconn-host.service 2>/dev/null || true
  rm -f "$unit"
  systemctl --user daemon-reload 2>/dev/null || true
  pkill -x -u "$(id -u)" rconn 2>/dev/null || true
  rm -rf "$lib" "$config/rconn/host"
  rm -f "$bin/rconn" "$bin/rconn-viewer" "$desktop" "$icon"
  echo "Rconn removed. Your recent computers are kept in $config/rconn (delete it to forget them)."
  exit 0
fi

if [ -x /usr/lib/rconn/rconn ]; then
  echo "Rconn is already installed for everyone on this computer (the .deb). Open it from your apps."
  exit 1
fi

mkdir -p "$lib" "$bin" "$share/applications" "$(dirname "$icon")"
# Copy then rename: a running Rconn keeps its old file and the new one takes its place.
for f in "$here"/rconn "$here"/rconn-viewer "$here"/rconn-host-agent "$here"/libopenh264-*; do
  cp "$f" "$lib/.$(basename "$f").new"
  mv -f "$lib/.$(basename "$f").new" "$lib/$(basename "$f")"
done
ln -sf "$lib/rconn" "$bin/rconn"
ln -sf "$lib/rconn-viewer" "$bin/rconn-viewer"
cp "$here/rconn.png" "$icon"
cat > "$desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Rconn
Comment=Connect to your computers, and let them connect to this one
Exec=$lib/rconn
Icon=rconn
Terminal=false
Categories=Network;RemoteAccess;
StartupWMClass=rconn
DESKTOP
command -v update-desktop-database >/dev/null && update-desktop-database "$share/applications" 2>/dev/null || true

# Libraries an administrator would have to add (a standard Ubuntu desktop has them all).
missing=$(for b in rconn rconn-viewer rconn-host-agent; do ldd "$lib/$b"; done 2>/dev/null | awk '/not found/ {print $1}' | sort -u)
if [ -n "$missing" ]; then
  echo "Rconn needs these libraries, which only an administrator can install:"
  echo "$missing" | sed 's/^/  /'
  echo "On Ubuntu: sudo apt install libgtk-3-0t64 libayatana-appindicator3-1 libpipewire-0.3-0t64 libxkbcommon-x11-0"
  exit 1
fi

# Upgrade: the host runs the new version now.
systemctl --user try-restart rconn-host.service 2>/dev/null || true
if [ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]; then
  echo "Rconn installed in $lib. Opening it: this computer gets its ID in a few seconds."
  nohup "$lib/rconn" >/dev/null 2>&1 &
else
  echo "Rconn installed in $lib. Open Rconn from your apps to get this computer's ID."
fi
