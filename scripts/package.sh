#!/usr/bin/env bash
# Build release binaries and assemble dist/*.zip. Run on macOS with cargo-xwin installed.
set -euo pipefail
cd "$(dirname "$0")/.."
LLVM_PREFIX="$(brew --prefix llvm 2>/dev/null || true)"
export PATH="$LLVM_PREFIX/bin:$PATH"
VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
# Only remove what this script builds: dist/ also holds local server credentials.
mkdir -p dist && rm -rf dist/rconn-windows-* dist/rconn-macos* dist/rconn-host-setup-* dist/rconn-*.dmg dist/rconn_*.deb dist/SHA256SUMS

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
if docker image inspect rconn-linux-build >/dev/null 2>&1; then
  lib="$(scripts/fetch-openh264.sh linux-arm64)"
  docker run --rm -v "$PWD":/src -v rconn-cargo:/usr/local/cargo/registry \
    -v rconn-rustup:/usr/local/rustup -v rconn-linux-target:/target rconn-linux-build sh -c \
    "cargo build -q --release -p rconn-app -p rconn-viewer -p rconn-host-agent &&
     deploy/linux/make-deb.sh /target/release $lib $VERSION arm64 dist/rconn_${VERSION}_arm64.deb"
fi

(cd dist && shasum -a 256 ./*.zip ./*.exe ./*.dmg ./*.deb > SHA256SUMS)
ls -lh dist/*.zip dist/*.exe dist/*.dmg dist/*.deb
