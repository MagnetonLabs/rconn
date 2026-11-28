#!/usr/bin/env bash
# Build rconn-mobile for iPhone/iPad and the Apple-silicon simulator, bundle both into
# ios/Generated/RconnMobileFFI.xcframework, and generate the Swift bindings into
# ios/Generated/Swift. Run before building the Xcode project (and after Rust changes).
# Also writes ios/Generated/Version.xcconfig: the app's version is the workspace's.
#
#   ios/build-rust.sh            release build (what goes on a phone)
#   ios/build-rust.sh --debug    faster, unoptimised
#   ios/build-rust.sh --sim      the simulator only (with or after --debug)
#
# RCONN_IOS_BUILD overrides the build number (CFBundleVersion, the version by default):
# App Store Connect wants a new one for every upload of the same version.
set -euo pipefail
cd "$(dirname "$0")/.."

profile=release
flag=--release
targets=(aarch64-apple-ios aarch64-apple-ios-sim)
for arg in "$@"; do
  case "$arg" in
    --debug) profile=debug; flag= ;;
    --sim) targets=(aarch64-apple-ios-sim) ;;
    *) echo "unknown option $arg" >&2; exit 2 ;;
  esac
done

# Match the app's deployment target (project.yml), so the linker doesn't warn about
# objects built for a newer iOS.
export IPHONEOS_DEPLOYMENT_TARGET=17.0

for t in "${targets[@]}"; do
  rustup target list --installed | grep -qx "$t" || rustup target add "$t"
  # shellcheck disable=SC2086 # $flag is empty or one word
  cargo rustc -p rconn-mobile --lib $flag --target "$t" --crate-type staticlib
done

out=ios/Generated
rm -rf "$out/RconnMobileFFI.xcframework" "$out/Swift" "$out/include"
mkdir -p "$out/Swift" "$out/include"

cargo run -q -p rconn-mobile --features bindgen --bin uniffi-bindgen -- generate \
  --library "target/${targets[0]}/$profile/librconn_mobile.a" \
  --language swift --out-dir "$out/Swift"

# The C half of the bindings becomes a clang module inside the XCFramework.
mv "$out/Swift/rconn_mobileFFI.h" "$out/include/"
mv "$out/Swift/rconn_mobileFFI.modulemap" "$out/include/module.modulemap"

libs=()
for t in "${targets[@]}"; do
  libs+=(-library "target/$t/$profile/librconn_mobile.a" -headers "$out/include")
done
xcodebuild -create-xcframework "${libs[@]}" -output "$out/RconnMobileFFI.xcframework" >/dev/null
rm -rf "$out/include"

# The workspace version (Cargo.toml) is the app's.
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
cat >"$out/Version.xcconfig" <<EOF
// Written by build-rust.sh from Cargo.toml.
MARKETING_VERSION = $version
CURRENT_PROJECT_VERSION = ${RCONN_IOS_BUILD:-$version}
EOF
echo "built $out/RconnMobileFFI.xcframework ($profile, ${targets[*]}) and $out/Swift/rconn_mobile.swift; version $version"
