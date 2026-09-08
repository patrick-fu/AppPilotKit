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

tree_sha256() {
  (
    for input in "$@"; do
      if [[ -d "$input" ]]; then
        find "$input" \
          \( -path '*/.build' -o -path '*/target' \) -prune -o \
          -type f -print
      elif [[ -f "$input" ]]; then
        print -r -- "$input"
      else
        print -u2 -- "provenance input is missing: $input"
        return 1
      fi
    done | LC_ALL=C sort | while IFS= read -r file; do
      print -rn -- "$file\0"
      shasum -a 256 "$file"
    done
  ) | shasum -a 256 | awk '{print "sha256:" $1}'
}

# The target directory name is a content key, not a source-path cache. A
# changed FFI or test-Broker input can therefore never reuse a prior static
# library.
staticlib_source_digest=$(tree_sha256 "$repo_dir/transport" "$package_dir/Tests/RustBroker")
export CARGO_TARGET_DIR="$work_root/cargo-target-${staticlib_source_digest#sha256:}"

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

# Evidence hosts own their application composition. This package provides only
# the shared Debug/Internal FFI and the same artifact validation boundary that
# target preparation applies before a Simulator install.
build_universal_simulator_ffi() {
  local output_dir=$1
  local arm64_dir
  local x86_64_dir

  [[ "$output_dir" == /* && ! -e "$output_dir" && -d "${output_dir:h}" ]] || {
    print -u2 "output directory must be an absolute nonexistent path with an existing parent"
    exit 2
  }
  arm64_dir=$(build_ffi aarch64-apple-ios-sim | tail -n 1)
  x86_64_dir=$(build_ffi x86_64-apple-ios | tail -n 1)
  mkdir "$output_dir"
  lipo -create \
    "$arm64_dir/libapppilotkit_transport_ffi.a" \
    "$x86_64_dir/libapppilotkit_transport_ffi.a" \
    -output "$output_dir/libapppilotkit_transport_ffi.a"
  print -r -- "$output_dir"
}

validate_ios_simulator_app() {
  local bundle_identifier=$1
  local app_path=$2
  local canonical_app_path
  local validator_dir="$work_root/prepare-artifact-validator"

  [[ "$bundle_identifier" == [A-Za-z0-9]* && "$app_path" == /* && "$app_path" == *.app && -d "$app_path" ]] || {
    print -u2 "expected a bundle identifier and an absolute existing .app directory"
    exit 2
  }
  canonical_app_path=$(realpath "$app_path")
  mkdir -p "$validator_dir/src"
  print -r -- "[package]
name = \"apppilotkit-ios-artifact-validator\"
version = \"0.1.0\"
edition = \"2024\"
publish = false

[dependencies]
apppilotkit-apple-simulator-adapter = { path = \"$repo_dir/cli/crates/apple-simulator-adapter\" }
apppilotkit-host-runtime = { path = \"$repo_dir/cli/crates/host-runtime\" }" >"$validator_dir/Cargo.toml"
  print -r -- 'use apppilotkit_apple_simulator_adapter::inspect_ios_app_tree_digest;
use apppilotkit_host_runtime::adapter::{AbsoluteDeadline, Cancellation};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let mut arguments = std::env::args_os();
    let _ = arguments.next();
    let bundle_identifier = arguments.next().expect("bundle identifier");
    let app_path = arguments.next().expect("app path");
    if arguments.next().is_some() {
        std::process::exit(2);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("unix time")
        .as_millis() as u64;
    let deadline = AbsoluteDeadline::new(now.saturating_add(30_000))
        .unwrap_or_else(|_| std::process::exit(1));
    let digest = inspect_ios_app_tree_digest(
        Path::new(&app_path),
        bundle_identifier.to_str().expect("UTF-8 bundle identifier"),
        &Cancellation::new(),
        deadline,
    )
    .unwrap_or_else(|error| {
        eprintln!("prepare artifact scanner rejected app: {:?}", error.primary_kind());
        std::process::exit(1);
    });
    print!("sha256:");
    for byte in digest { print!("{byte:02x}"); }
    println!();
}' >"$validator_dir/src/main.rs"
  "$cargo_root/bin/cargo" +1.94.0 run --quiet --manifest-path "$validator_dir/Cargo.toml" -- "$bundle_identifier" "$canonical_app_path"
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
  release-negative)
    assert_release_rejected internal-transport-release 'Debug/Internal-only' \
      swift build \
      --package-path "$package_dir" \
      --scratch-path "$work_root/internal-release" \
      --configuration release \
      --target AppPilotKitTargetTransportInternal \
      --jobs 1
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
  build-universal-simulator-ffi)
    [[ $# -eq 2 ]] || {
      print -u2 "usage: $0 build-universal-simulator-ffi /absolute/nonexistent/output-directory"
      exit 2
    }
    build_universal_simulator_ffi "$2"
    ;;
  validate-ios-simulator-app)
    [[ $# -eq 3 ]] || {
      print -u2 "usage: $0 validate-ios-simulator-app <bundle-identifier> /absolute/existing/App.app"
      exit 2
    }
    validate_ios_simulator_app "$2" "$3"
    ;;
  staticlib-source-digest)
    print -r -- "$staticlib_source_digest"
    ;;
  provenance-self-test)
    fixture_dir="$work_root/provenance-red"
    mkdir -p "$fixture_dir/source"
    print -r -- 'before' >"$fixture_dir/source/build-marker"
    before=$(tree_sha256 "$fixture_dir/source")
    print -r -- 'after' >"$fixture_dir/source/build-marker"
    after=$(tree_sha256 "$fixture_dir/source")
    [[ "$before" != "$after" ]] || {
      print -u2 "provenance source marker did not invalidate the cache key"
      exit 4
    }
    [[ "$CARGO_TARGET_DIR" == *"${staticlib_source_digest#sha256:}" ]] || {
      print -u2 "Rust static-library target is not keyed by current source"
      exit 4
    }
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
  all)
    "$0" test
    "$0" release-negative
    "$0" simulator
    "$0" device-staticlib
    ;;
  *)
    print -u2 "usage: $0 {test|release-negative|simulator|build-universal-simulator-ffi|validate-ios-simulator-app|staticlib-source-digest|provenance-self-test|device-staticlib|all}"
    exit 2
    ;;
esac
