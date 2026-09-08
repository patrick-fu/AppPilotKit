#!/bin/zsh
set -euo pipefail

script_dir=${0:A:h}
host_dir=${script_dir:h}
repo_dir=${host_dir:h:h}
ffi_manifest="$repo_dir/transport/crypto-core/ffi/Cargo.toml"
rustup_root=/Volumes/WD/Toolchains/AppPilotKit/rustup
cargo_root=/Volumes/WD/Toolchains/AppPilotKit/cargo
work_parent=/Volumes/WD/Toolchains/AppPilotKit/tmp
simulator_id=${1:-}

[[ -f "$ffi_manifest" ]] || { print -u2 "missing accepted transport FFI manifest"; exit 2; }
[[ -d "$rustup_root" && -d "$cargo_root" ]] || {
  print -u2 "missing isolated AppPilotKit Rust toolchain"
  exit 2
}

if [[ -z "$simulator_id" ]]; then
  simulator_id=$(xcrun simctl list devices available | \
    sed -nE 's/.*\(([0-9A-F-]{36})\) \((Shutdown|Booted)\).*/\1/p' | head -n 1)
fi
[[ -n "$simulator_id" ]] || { print -u2 "no available iOS Simulator"; exit 2; }

mkdir -p "$work_parent"
work_root=$(mktemp -d "$work_parent/apppilotkit-acceptance-ios.XXXXXX")
cleanup() {
  find "$work_root" -depth -delete 2>/dev/null || true
}
trap cleanup EXIT INT TERM

export RUSTUP_HOME="$rustup_root"
export CARGO_HOME="$cargo_root"
export CARGO_TARGET_DIR="$work_root/cargo-target"
"$cargo_root/bin/cargo" +1.94.0 build \
  --locked \
  --release \
  --manifest-path "$ffi_manifest" \
  --target aarch64-apple-ios-sim

ffi_dir="$CARGO_TARGET_DIR/aarch64-apple-ios-sim/release"
[[ -s "$ffi_dir/libapppilotkit_transport_ffi.a" ]] || {
  print -u2 "missing iOS Simulator transport FFI library"
  exit 3
}

cd "$host_dir"
xcodebuild test \
  -jobs 1 \
  -parallel-testing-enabled NO \
  -scheme AppPilotKitAcceptanceHost \
  -destination "platform=iOS Simulator,id=$simulator_id" \
  -derivedDataPath "$work_root/derived-data" \
  CODE_SIGNING_ALLOWED=NO \
  PRODUCT_BUNDLE_IDENTIFIER=dev.apppilotkit.acceptancehost.ios \
  "OTHER_LDFLAGS=-L$ffi_dir"
