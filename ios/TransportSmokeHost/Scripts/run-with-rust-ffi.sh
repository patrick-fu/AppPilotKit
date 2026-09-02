#!/bin/zsh
set -euo pipefail

script_dir=${0:A:h}
package_dir=${script_dir:h}
repo_dir=${package_dir:h:h}
ffi_manifest="$repo_dir/transport/crypto-core/ffi/Cargo.toml"
test_broker_manifest="$package_dir/Tests/RustBroker/Cargo.toml"
rustup_root=/Volumes/WD/Toolchains/AppPilotKit/rustup
cargo_root=/Volumes/WD/Toolchains/AppPilotKit/cargo

if [[ ! -f "$ffi_manifest" ]]; then
  print -u2 "missing accepted transport FFI manifest"
  exit 2
fi
if [[ ! -d "$rustup_root" || ! -d "$cargo_root" ]]; then
  print -u2 "missing isolated AppPilotKit Rust toolchain"
  exit 2
fi

work_parent=/Volumes/WD/Toolchains/AppPilotKit/tmp
mkdir -p "$work_parent"
work_root=$(mktemp -d "$work_parent/apppilotkit-d5-ffi.XXXXXX")
cleanup() {
  find "$work_root" -depth -delete 2>/dev/null || true
}
trap cleanup EXIT INT TERM

export RUSTUP_HOME="$rustup_root"
export CARGO_HOME="$cargo_root"
export CARGO_TARGET_DIR="$work_root/cargo-target"

build_ffi() {
  local triple=$1
  "$cargo_root/bin/cargo" +1.94.0 build \
    --locked \
    --release \
    --manifest-path "$ffi_manifest" \
    --target "$triple"
  local library="$CARGO_TARGET_DIR/$triple/release/libapppilotkit_transport_ffi.a"
  [[ -s "$library" ]] || {
    print -u2 "missing FFI static library for $triple"
    exit 3
  }
  print -r -- "${library:h}"
}

build_test_broker() {
  local triple=$1
  "$cargo_root/bin/cargo" +1.94.0 build \
    --locked \
    --release \
    --manifest-path "$test_broker_manifest" \
    --target "$triple"
  local library="$CARGO_TARGET_DIR/$triple/release/libapppilotkit_transport_test_broker.a"
  [[ -s "$library" ]] || {
    print -u2 "missing test Broker static library for $triple"
    exit 3
  }
}

swift_link_flags() {
  local library_dir=$1
  print -r -- "-Xlinker" "-L$library_dir"
}

assert_release_rejected() {
  local subject=$1
  local expected_pattern=$2
  shift 2
  local output="$work_root/${subject//[^A-Za-z0-9._-]/_}.log"
  if "$@" >"$output" 2>&1; then
    print -u2 -- "$subject unexpectedly built as Release"
    exit 4
  fi
  if ! grep -Eq "$expected_pattern" "$output"; then
    print -u2 -- "$subject failed for a reason other than the expected Release boundary"
    cat "$output" >&2
    exit 4
  fi
}

write_external_release_probe() {
  local probe_dir=$1
  local requested_product=$2
  mkdir -p "$probe_dir/Sources/ReleaseLinkProbe"
  print -r -- "// swift-tools-version: 6.0

import PackageDescription

let package = Package(
  name: \"ReleaseLinkProbe\",
  platforms: [
    .macOS(.v13),
  ],
  dependencies: [
    .package(name: \"AppPilotKitTransportSmokeHost\", path: \"$package_dir\"),
  ],
  targets: [
    .executableTarget(
      name: \"ReleaseLinkProbe\",
      dependencies: [
        .product(
          name: \"$requested_product\",
          package: \"AppPilotKitTransportSmokeHost\"
        ),
      ]
    ),
  ]
)" >"$probe_dir/Package.swift"
  print -r -- "@_spi(AppPilotKitTargetTransportInternal) import AppPilotKitTargetTransportInternal

let _ = AppPilotKitTargetTransport.descriptorEnvironmentKey" >"$probe_dir/Sources/ReleaseLinkProbe/main.swift"
}

command=${1:-test}
case "$command" in
  test)
    library_dir=$(build_ffi aarch64-apple-darwin | tail -n 1)
    build_test_broker aarch64-apple-darwin
    swift test \
      --package-path "$package_dir" \
      --scratch-path "$work_root/swift-test" \
      --filter AppPilotKitTargetTransportInternalTests \
      --jobs 1 \
      -Xlinker "-L$library_dir"
    ;;
  release-negative|external-release-negative)
    for requested_product in AppPilotKitTargetTransportInternal TransportSmokeHost; do
      probe_dir="$work_root/external-release-link-probe-$requested_product"
      probe_scratch="$work_root/external-release-link-probe-build-$requested_product"
      write_external_release_probe "$probe_dir" "$requested_product"
      assert_release_rejected external-release-host "product '$requested_product'.*not found" \
        swift build \
        --package-path "$probe_dir" \
        --scratch-path "$probe_scratch" \
        --configuration release \
        --jobs 1 \
        -Xswiftc -DAPPPILOTKIT_INTERNAL
      if find "$probe_scratch" -type f -name ReleaseLinkProbe -print -quit | grep -q .; then
        print -u2 "external Release host emitted a link artifact"
        exit 4
      fi
    done
    ;;
  simulator)
    library_dir=$(build_ffi aarch64-apple-ios-sim | tail -n 1)
    build_test_broker aarch64-apple-ios-sim
    sdk=$(xcrun --sdk iphonesimulator --show-sdk-path)
    swift build \
      --package-path "$package_dir" \
      --scratch-path "$work_root/swift-simulator" \
      --configuration debug \
      --target AppPilotKitTargetTransportInternal \
      --triple arm64-apple-ios15.0-simulator \
      --sdk "$sdk" \
      --jobs 1 \
      -Xlinker "-L$library_dir"
    ;;
  smoke-host-simulator)
    library_dir=$(build_ffi aarch64-apple-ios-sim | tail -n 1)
    sdk=$(xcrun --sdk iphonesimulator --show-sdk-path)
    swift build \
      --package-path "$package_dir" \
      --scratch-path "$work_root/smoke-host-simulator" \
      --configuration debug \
      --target TransportSmokeHost \
      --triple arm64-apple-ios15.0-simulator \
      --sdk "$sdk" \
      --jobs 1 \
      -Xlinker "-L$library_dir"
    ;;
  device-staticlib)
    library_dir=$(build_ffi aarch64-apple-ios | tail -n 1)
    build_test_broker aarch64-apple-ios
    sdk=$(xcrun --sdk iphoneos --show-sdk-path)
    swift build \
      --package-path "$package_dir" \
      --scratch-path "$work_root/swift-device" \
      --configuration debug \
      --target AppPilotKitTargetTransportInternal \
      --triple arm64-apple-ios15.0 \
      --sdk "$sdk" \
      --jobs 1 \
      -Xlinker "-L$library_dir"
    ;;
  root-release-negative)
    root_scratch="$work_root/root-ios-release"
    swift build \
      --package-path "$repo_dir/ios" \
      --scratch-path "$root_scratch" \
      --configuration release \
      --jobs 1
    dump=$(swift package --package-path "$repo_dir/ios" dump-package)
    print -r -- "$dump" | grep -q '"name" : "AppPilotKit"'
    print -r -- "$dump" | grep -q '"name" : "AppPilotKitUIKit"'
    if print -r -- "$dump" | grep -q 'TargetTransport'; then
      print -u2 "root iOS package unexpectedly contains internal transport"
      exit 4
    fi
    if find "$root_scratch" -type f \( -name '*.o' -o -name '*.a' -o -name '*.swiftmodule' \) \
      -exec strings {} + | grep -E 'apppilotkit_tp_v1_|Noise_NK_|Noise_NNpsk0_|APPPILOTKIT_TRANSPORT_DESCRIPTOR'; then
      print -u2 "root iOS Release artifact contains internal transport surface"
      exit 4
    fi
    git -C "$repo_dir" diff --exit-code \
      09c846d86d0a18b0ccc6ca2e3fc6f00c305425b3 -- \
      ios/Package.swift ios/Sources/AppPilotKit
    ;;
  all)
    "$0" test
    "$0" external-release-negative
    "$0" simulator
    "$0" smoke-host-simulator
    "$0" device-staticlib
    "$0" root-release-negative
    ;;
  *)
    print -u2 "usage: $0 {test|release-negative|external-release-negative|simulator|smoke-host-simulator|device-staticlib|root-release-negative|all}"
    exit 2
    ;;
esac
