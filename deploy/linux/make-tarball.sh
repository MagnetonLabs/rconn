#!/usr/bin/env bash
# Assemble the "just for me" Linux package (no sudo): a .tar.gz with the binaries and an
# install.sh that puts them under ~/.local. Runs inside the Linux build image, from the repo root:
#   deploy/linux/make-tarball.sh <dir-with-binaries> <openh264.so> <version> <arch: arm64|amd64> <out.tar.gz>
set -euo pipefail
bins="$1" openh264="$2" version="$3" arch="$4" out="$5"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
name="rconn-$version-linux-$arch"
dir="$work/$name"
mkdir -p "$dir"
for b in rconn rconn-viewer rconn-host-agent; do
  install -m 0755 "$bins/$b" "$dir/$b"
  strip "$dir/$b"
done
# Loaded at runtime from beside the binaries (Cisco's own build: their patent licence).
install -m 0644 "$openh264" "$dir/$(basename "$openh264")"
install -m 0644 crates/rconn-app/assets/icon-256.png "$dir/rconn.png"
install -m 0755 deploy/linux/install-user.sh "$dir/install.sh"
tar -C "$work" -czf "$out" "$name"
echo "$out"
