#!/usr/bin/env bash
# Build release binaries and assemble dist/*.zip. Run on macOS with cargo-xwin installed.
set -euo pipefail
cd "$(dirname "$0")/.."
LLVM_PREFIX="$(brew --prefix llvm 2>/dev/null || true)"
export PATH="$LLVM_PREFIX/bin:$PATH"
VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
# Only remove what this script builds: dist/ also holds local server credentials.
mkdir -p dist && rm -rf dist/rconn-windows-* dist/rconn-macos* dist/rconn-host-setup-* dist/rconn-setup-user-* \
  dist/Rconn-*.dmg dist/rconn_*.deb dist/rconn-*-linux-*.tar.gz dist/SHA256SUMS

for arch in x64 arm64; do
  case $arch in
    x64) target=x86_64-pc-windows-msvc; dll=win64; extra=() ;;
    arm64) target=aarch64-pc-windows-msvc; dll=win-arm64; extra=(--cross-compiler clang) ;;
  esac
  cargo xwin build -q --release -p rconn-host-agent -p rconn-host-service -p rconn-host-settings -p rconn-viewer -p rconn-app --target "$target" "${extra[@]}"
  lib="$(scripts/fetch-openh264.sh "$dll")"
  stage="dist/rconn-windows-$arch"
  mkdir -p "$stage/host" "$stage/viewer"
  cp "target/$target/release/rconn-host-agent.exe" "target/$target/release/rconn-host-service.exe" \
    "target/$target/release/rconn-settings.exe" "$lib" "$stage/host/"
  cp "target/$target/release/rconn-viewer.exe" "target/$target/release/rconn.exe" "$lib" "$stage/viewer/"
  cp deploy/windows/install-host.ps1 deploy/windows/uninstall-host.ps1 "$stage/"
  cp deploy/README-package.txt "$stage/README.txt"
  (cd dist && zip -qr "rconn-windows-$arch-$VERSION.zip" "rconn-windows-$arch")
  makensis -V2 -DARCH="$arch" -DVERSION="$VERSION" -DSRC="$PWD/$stage" \
    -DOUT="$PWD/dist/rconn-host-setup-$arch-$VERSION.exe" deploy/windows/rconn-host.nsi
  # Just for me: no administrator, no service.
  makensis -V2 -DARCH="$arch" -DVERSION="$VERSION" -DSRC="$PWD/$stage" \
    -DOUT="$PWD/dist/rconn-setup-user-$arch-$VERSION.exe" deploy/windows/rconn-user.nsi
done

cargo build -q --release -p rconn-viewer -p rconn-app -p rconn-host-agent
cargo build -q --release -p rconn-viewer -p rconn-app -p rconn-host-agent --target x86_64-apple-darwin
stage=dist/rconn-macos
mkdir -p "$stage"
for bin in rconn-viewer rconn rconn-tray rconn-host-agent; do
  lipo -create -output "$stage/$bin" "target/release/$bin" "target/x86_64-apple-darwin/release/$bin"
done
cp "$(scripts/fetch-openh264.sh mac-arm64)" "$(scripts/fetch-openh264.sh mac-x64)" "$stage/"
cp deploy/README-package.txt "$stage/README.txt"
(cd dist && zip -qr "rconn-macos-$VERSION.zip" rconn-macos)
deploy/macos/make-dmg.sh "$stage" "$VERSION" "dist/Rconn-$VERSION.dmg" >/dev/null

# Linux (Ubuntu 24.04+): built in the Docker image from deploy/linux/Dockerfile.build.
# Each arch: the .deb (everyone, sudo) and the .tar.gz (just me, no sudo). amd64 is
# emulated on Apple silicon (slow) and built in parallel.
linux() { # <docker image> <volume suffix> <deb arch> <openh264 platform> [--platform ...]
  local image="$1" sfx="$2" arch="$3" lib
  docker image inspect "$image" >/dev/null 2>&1 || { echo "skipping Linux $arch: no $image"; return; }
  lib="$(scripts/fetch-openh264.sh "$4")"
  docker run --rm "${@:5}" -v "$PWD":/src -v "rconn-cargo$sfx":/usr/local/cargo/registry \
    -v "rconn-rustup$sfx":/usr/local/rustup -v "rconn-linux-target$sfx":/target "$image" sh -c \
    "cargo build -q --release -p rconn-app -p rconn-viewer -p rconn-host-agent &&
     deploy/linux/make-deb.sh /target/release $lib $VERSION $arch dist/rconn_${VERSION}_$arch.deb &&
     deploy/linux/make-tarball.sh /target/release $lib $VERSION $arch dist/rconn-$VERSION-linux-$arch.tar.gz"
}
linux rconn-linux-build:amd64 -amd64 amd64 linux64 --platform linux/amd64 &
amd64=$!
linux rconn-linux-build "" arm64 linux-arm64
wait "$amd64"

(cd dist && shasum -a 256 ./*-"$VERSION"*.exe ./*-"$VERSION".dmg ./*_"$VERSION"_*.deb ./*-"$VERSION"-linux-*.tar.gz > SHA256SUMS)
ls -lh dist/*"$VERSION"*
