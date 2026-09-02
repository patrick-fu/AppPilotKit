#!/bin/sh
set -eu

ffi_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
scratch=$(mktemp -d "${TMPDIR:-/tmp}/apppilotkit-ffi-smoke.XXXXXX")
trap 'rm -rf "$scratch"' EXIT HUP INT TERM

export CARGO_TARGET_DIR="$scratch/production-target"
cargo build --manifest-path "$ffi_dir/Cargo.toml" --release --locked --offline

clang -std=c11 -Wall -Wextra -Werror \
    -I"$ffi_dir/include" \
    "$ffi_dir/tests/c_smoke.c" \
    "$CARGO_TARGET_DIR/release/libapppilotkit_transport_ffi.a" \
    -framework Security -framework CoreFoundation \
    -o "$scratch/c_smoke"
"$scratch/c_smoke"

javac -d "$scratch/classes" \
    "$ffi_dir/tests/java/dev/apppilotkit/transport/NativeTransport.java"
CARGO_TARGET_DIR="$scratch/jni-target" \
    RUSTFLAGS="--cfg apppilotkit_jni_smoke" \
    cargo build --manifest-path "$ffi_dir/Cargo.toml" --release --locked --offline
java -cp "$scratch/classes" dev.apppilotkit.transport.NativeTransport \
    "$scratch/jni-target/release/libapppilotkit_transport_ffi.dylib" \
    "$ffi_dir/../../contracts/v1/vectors/bootstrap-android-descriptor.json"
