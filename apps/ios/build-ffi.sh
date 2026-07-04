#!/usr/bin/env bash
# Regenerates apps/ios/Packages/MurmurCoreFFI's binary artifacts:
#   - crates/ffi built for aarch64-apple-ios-sim and aarch64-apple-ios
#   - Swift bindings generated via the uniffi-bindgen dev binary
#   - Frameworks/ffiFFI.xcframework (device + sim slices)
#
# These artifacts are GITIGNORED (large binaries); Sources/MurmurCoreFFI/ffi.swift
# and Package.swift ARE committed. Run this after any crates/ffi surface change,
# or any time Frameworks/ffiFFI.xcframework is missing.
#
# Why this isn't `nix develop -c cargo build --target ...` alone (Plan 07
# Task 9 / flake.nix multi-target toolchain): the nix-wrapped clang/cc-wrapper
# hardcodes macOS SDK paths and a `-mmacos-version-min` flag that conflicts
# with `-target arm64-apple-ios...-simulator`, and its default library search
# resolves against the MacOSX SDK's libiconv.tbd even when --target is iOS,
# which fails at the final link step ("building for iOS Simulator, but
# linking in .tbd built for macOS/Mac Catalyst"). The fix used here: keep the
# nix devShell's rust-overlay toolchain (cargo/rustc/clippy — unchanged, still
# the single source for host builds and `cargo test --workspace`), but for the
# iOS cross builds only, point CC/AR/the linker at the *system* Xcode
# toolchain (`/usr/bin/clang`, `/usr/bin/ar`) and SDKROOT at the real
# iphoneos/iphonesimulator SDK via `xcrun`. This is the "system Xcode
# fallback" path referenced in the Plan 07 Task 9 report — the pure-nix path
# (unset SDKROOT/NIX_*FLAGS only) still fails on SDK linkage.
set -euo pipefail

cd "$(dirname "$0")/../.."   # repo root
FFI_DIR="apps/ios/Packages/MurmurCoreFFI"
BINDINGS_DIR="$(mktemp -d)"
trap 'rm -rf "$BINDINGS_DIR"' EXIT

echo "==> building crates/ffi for aarch64-apple-ios-sim"
nix develop -c bash -c '
  set -euo pipefail
  export DEVELOPER_DIR=/Applications/Xcode-26.2.0.app/Contents/Developer
  export SDKROOT=$(/usr/bin/xcrun --sdk iphonesimulator --show-sdk-path)
  export CC_aarch64_apple_ios_sim=/usr/bin/clang
  export CXX_aarch64_apple_ios_sim=/usr/bin/clang++
  export AR_aarch64_apple_ios_sim=/usr/bin/ar
  export CARGO_TARGET_AARCH64_APPLE_IOS_SIM_LINKER=/usr/bin/clang
  unset NIX_CFLAGS_COMPILE NIX_LDFLAGS NIX_CFLAGS_COMPILE_FOR_BUILD NIX_LDFLAGS_FOR_BUILD
  cargo build -p ffi --release --target aarch64-apple-ios-sim
'

echo "==> building crates/ffi for aarch64-apple-ios (device)"
nix develop -c bash -c '
  set -euo pipefail
  export DEVELOPER_DIR=/Applications/Xcode-26.2.0.app/Contents/Developer
  export SDKROOT=$(/usr/bin/xcrun --sdk iphoneos --show-sdk-path)
  export CC_aarch64_apple_ios=/usr/bin/clang
  export CXX_aarch64_apple_ios=/usr/bin/clang++
  export AR_aarch64_apple_ios=/usr/bin/ar
  export CARGO_TARGET_AARCH64_APPLE_IOS_LINKER=/usr/bin/clang
  unset NIX_CFLAGS_COMPILE NIX_LDFLAGS NIX_CFLAGS_COMPILE_FOR_BUILD NIX_LDFLAGS_FOR_BUILD
  cargo build -p ffi --release --target aarch64-apple-ios
'

echo "==> generating Swift bindings (uniffi-bindgen, host build)"
nix develop -c cargo run -p ffi --features uniffi-bindgen-cli --bin uniffi-bindgen -- \
  generate --library target/aarch64-apple-ios-sim/release/libffi.a \
  --language swift --out-dir "$BINDINGS_DIR"

cp "$BINDINGS_DIR/ffi.swift" "$FFI_DIR/Sources/MurmurCoreFFI/ffi.swift"

echo "==> assembling ffiFFI.xcframework"
rm -rf "$FFI_DIR/Frameworks/ffiFFI.xcframework"
for slice in sim device; do
  hdir="$BINDINGS_DIR/headers-$slice"
  mkdir -p "$hdir"
  cp "$BINDINGS_DIR/ffiFFI.h" "$hdir/"
  cp "$BINDINGS_DIR/ffiFFI.modulemap" "$hdir/module.modulemap"
done

xcodebuild -create-xcframework \
  -library target/aarch64-apple-ios-sim/release/libffi.a -headers "$BINDINGS_DIR/headers-sim" \
  -library target/aarch64-apple-ios/release/libffi.a -headers "$BINDINGS_DIR/headers-device" \
  -output "$FFI_DIR/Frameworks/ffiFFI.xcframework"

echo "==> done. Run 'cd apps/ios && xcodegen generate' next."
