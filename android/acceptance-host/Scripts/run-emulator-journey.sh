#!/bin/zsh
set -euo pipefail

script_dir=${0:A:h}
host_dir=${script_dir:h}
android_dir=${host_dir:h}
repo_dir=${android_dir:h}
app_id=dev.apppilotkit.acceptancehost
rustup_root=/Volumes/WD/Toolchains/AppPilotKit/rustup
cargo_root=/Volumes/WD/Toolchains/AppPilotKit/cargo
rust_toolchain=1.94.0

[[ -d "$rustup_root" && -d "$cargo_root" ]] || {
  print -u2 "Rust toolchain checkpoint: missing isolated AppPilotKit Rust toolchain"
  exit 2
}
rustup_bin="$cargo_root/bin/rustup"
cargo="$cargo_root/bin/cargo"
rustc="$cargo_root/bin/rustc"
[[ -x "$rustup_bin" && -x "$cargo" && -x "$rustc" ]] || {
  print -u2 "Rust toolchain checkpoint: rustup/cargo/rustc shims are unavailable"
  exit 2
}
export RUSTUP_HOME="$rustup_root"
export CARGO_HOME="$cargo_root"
export RUSTUP_TOOLCHAIN="$rust_toolchain"
export PATH="$cargo_root/bin:$PATH"
export CARGO="$cargo"
export RUSTC="$rustc"

for rust_target in aarch64-linux-android x86_64-linux-android; do
  if ! "$rustup_bin" target list --installed --toolchain "$rust_toolchain" | grep -Fx "$rust_target" >/dev/null; then
    print -u2 "Rust toolchain checkpoint: target $rust_target is not installed for $rust_toolchain"
    exit 2
  fi
done

if (( $# > 1 )); then
  print -u2 "usage: $0 [emulator-serial]"
  exit 2
fi

if [[ -n ${APPPILOTKIT_ANDROID_ADB:-} ]]; then
  adb=$APPPILOTKIT_ANDROID_ADB
else
  adb=$(command -v adb || true)
fi
[[ -n "$adb" ]] || { print -u2 "Android Emulator dependency checkpoint: adb is unavailable"; exit 2; }
adb=${adb:A}
[[ -x "$adb" ]] || { print -u2 "Android Emulator dependency checkpoint: adb is not executable: $adb"; exit 2; }

emulator_lines=$("$adb" devices | awk '$1 ~ /^emulator-[0-9]+$/ && $2 == "device" { print $1 }')
emulators=()
[[ -n "$emulator_lines" ]] && emulators=("${(@f)emulator_lines}")
if (( $# == 1 )); then
  serial=$1
  [[ $serial =~ '^emulator-[0-9]+$' ]] || {
    print -u2 "Android Emulator dependency checkpoint: serial is not an emulator: $serial"
    exit 2
  }
  (( ${emulators[(Ie)$serial]} )) || {
    print -u2 "Android Emulator dependency checkpoint: requested emulator is not online: $serial"
    exit 2
  }
elif (( ${#emulators[@]} == 1 )); then
  serial=$emulators[1]
elif (( ${#emulators[@]} == 0 )); then
  print -u2 "Android Emulator dependency checkpoint: adb devices lists no online emulator"
  exit 2
else
  print -u2 "Android Emulator dependency checkpoint: select one online emulator: ${emulators[*]}"
  exit 2
fi

java_home=$(/usr/libexec/java_home -v 17)
[[ -n "$java_home" ]] || { print -u2 "Android Emulator dependency checkpoint: JDK 17 is unavailable"; exit 2; }

work_root=$(mktemp -d "${TMPDIR:-/tmp}/apppilotkit-acceptance-android.XXXXXX")
work_root=${work_root:A}
print "Android acceptance evidence directory retained at: $work_root"

# Keep the diagnostic channel opt-in.  The installed helper only sees this
# sidecar when the caller explicitly requests a retained sidecar.
if [[ ${APPPILOTKIT_INTERNAL_PREPARE_FAILURE_DIAGNOSTICS:-0} == 1 ]]; then
  diagnostic_sidecar="$work_root/prepare-failure.ndjson"
  export APPPILOTKIT_INTERNAL_PREPARE_FAILURE_DIAGNOSTICS=1
  export APPPILOTKIT_INTERNAL_PREPARE_FAILURE_SIDECAR="$diagnostic_sidecar"
  production_features=(--features internal-diagnostics)
  print "Internal prepare-failure sidecar retained at: $diagnostic_sidecar"
else
  production_features=()
fi

export APPPILOTKIT_ANDROID_ADB="$adb"
export JAVA_HOME="$java_home"
export CARGO_TARGET_DIR="$work_root/cargo-target"
export CARGO_BUILD_JOBS=1
# Gradle and the installed CLI build share the same explicit Rust toolchain.

(
  cd "$android_dir"
  ./gradlew --no-daemon --max-workers=1 :acceptance-host:assembleDebug
)

apk="$android_dir/acceptance-host/build/outputs/apk/debug/acceptance-host-debug.apk"
[[ -s "$apk" ]] || { print -u2 "missing Debug acceptance APK: $apk"; exit 3; }
apk=${apk:A}

(
  cd "$repo_dir/cli"
  "$cargo" build \
    --jobs 1 \
    --locked \
    --release \
    --package apppilotkit-production-composition \
    --bins \
    "${production_features[@]}"
)

prefix="$work_root/installed"
"$repo_dir/cli/crates/production-composition/scripts/stage-install.sh" \
  "$CARGO_TARGET_DIR/release" \
  "$prefix"
prepare_program="$prefix/libexec/apppilotkit-target-prepare"
[[ -x "$prepare_program" ]] || { print -u2 "missing installed target prepare: $prepare_program"; exit 3; }

config="$work_root/catalog-run.json"
cat >"$config" <<EOF
{
  "prefix": "$prefix",
  "platform": "android",
  "contract": "$repo_dir/acceptance/demo-catalog.contract.json",
  "prepare_request": {
    "schema_version": "1.0",
    "platform": "android-emulator",
    "device_selector": "$serial",
    "app_id": "$app_id",
    "app_artifact": "$apk",
    "artifact_encoding": "raw-file-v1"
  },
  "restart": {
    "argv": ["$prepare_program", "--release-fd=0", "--output=json"]
  }
}
EOF

node "$repo_dir/acceptance/harness/installed-cli-harness.mjs" \
  --config "$config" \
  --evidence "$work_root/catalog-evidence.json"

print "Android acceptance journey passed."
print "Configuration: $config"
print "Evidence: $work_root/catalog-evidence.json"
