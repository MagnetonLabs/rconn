#!/usr/bin/env bash
# Download Cisco's prebuilt openh264 for a platform into target/openh264/ and verify its
# SHA-256 against the list built into the openh264-sys2 crate. Cisco's own binary is what
# Cisco's H.264 patent license covers, so we never ship one we compiled.
#
#   scripts/fetch-openh264.sh            # this machine
#   scripts/fetch-openh264.sh win-arm64  # also: win64, win32, mac-arm64, mac-x64, linux64, linux-arm64
set -euo pipefail
VERSION=2.7.0
cd "$(dirname "$0")/.."
case "${1:-}" in
  "") case "$(uname -s)-$(uname -m)" in
        Darwin-arm64) plat=mac-arm64 ;; Darwin-x86_64) plat=mac-x64 ;;
        Linux-x86_64) plat=linux64 ;; Linux-aarch64) plat=linux-arm64 ;;
        *) echo "unknown platform; pass one explicitly" >&2; exit 1 ;;
      esac ;;
  *) plat="$1" ;;
esac
case "$plat" in
  win64|win32|win-arm64) file="openh264-$VERSION-$plat.dll" ;;
  mac-arm64|mac-x64) file="libopenh264-$VERSION-$plat.dylib" ;;
  linux64|linux-arm64) file="libopenh264-$VERSION-$plat.8.so" ;;
  *) echo "unknown platform $plat" >&2; exit 1 ;;
esac
hashes=$(find "${CARGO_HOME:-$HOME/.cargo}/registry/src" -path "*openh264-sys2-*/src/blobs/hashes-$VERSION.txt" | head -1)
[ -n "$hashes" ] || { echo "openh264-sys2 sources not found; run cargo fetch" >&2; exit 1; }
want=$(awk -v f="$file" '$2==f {print $1}' "$hashes")
mkdir -p target/openh264
out="target/openh264/$file"
if [ ! -f "$out" ]; then
  curl -fsSL "http://ciscobinary.openh264.org/$file.bz2" | bunzip2 > "$out.tmp"
  mv "$out.tmp" "$out"
fi
got=$( (command -v sha256sum >/dev/null && sha256sum "$out" || shasum -a 256 "$out") | awk '{print $1}')
[ "$got" = "$want" ] || { echo "hash mismatch for $file: $got" >&2; rm -f "$out"; exit 1; }
echo "$out"
