#!/usr/bin/env bash
# Build rconn-mobile for iPhone/iPad and the Apple-silicon simulator, bundle both into
# ios/Generated/RconnMobileFFI.xcframework, and generate the Swift bindings into
# ios/Generated/Swift. Run before building the Xcode project (and after Rust changes).
#
#   ios/build-rust.sh            release build (what goes on a phone)
#   ios/build-rust.sh --debug    faster, unoptimised
set -euo pipefail
cd "$(dirname "$0")/.."

profile=release
flag=--release
if [[ "${1:-}" == "--debug" ]]; then
  profile=debug
  flag=
fi

# Match the app's deployment target (project.yml), so the linker doesn't warn about
# objects built for a newer iOS.
export IPHONEOS_DEPLOYMENT_TARGET=17.0

targets=(aarch64-apple-ios aarch64-apple-ios-sim)
for t in "${targets[@]}"; do
  rustup target list --installed | grep -qx "$t" || rustup target add "$t"
  # shellcheck disable=SC2086 # $flag is empty or one word
  cargo build -p rconn-mobile --lib $flag --target "$t"
done

out=ios/Generated
rm -rf "$out/RconnMobileFFI.xcframework" "$out/Swift" "$out/include"
mkdir -p "$out/Swift" "$out/include"

cargo run -q -p rconn-mobile --features bindgen --bin uniffi-bindgen -- generate \
  --library "target/aarch64-apple-ios/$profile/librconn_mobile.a" \
  --language swift --out-dir "$out/Swift"

# The C half of the bindings becomes a clang module inside the XCFramework.
mv "$out/Swift/rconn_mobileFFI.h" "$out/include/"
mv "$out/Swift/rconn_mobileFFI.modulemap" "$out/include/module.modulemap"

xcodebuild -create-xcframework \
  -library "target/aarch64-apple-ios/$profile/librconn_mobile.a" -headers "$out/include" \
  -library "target/aarch64-apple-ios-sim/$profile/librconn_mobile.a" -headers "$out/include" \
  -output "$out/RconnMobileFFI.xcframework" >/dev/null
rm -rf "$out/include"
echo "built $out/RconnMobileFFI.xcframework ($profile) and $out/Swift/rconn_mobile.swift"
