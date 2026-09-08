#!/bin/zsh
set -euo pipefail

script_dir=${0:A:h}
host_dir=${script_dir:h}
ios_dir=${host_dir:h}
repo_dir=${ios_dir:h}
internal_runner="$ios_dir/InternalTargetTransport/Scripts/run-with-rust-ffi.sh"
ffi_manifest="$repo_dir/transport/crypto-core/ffi/Cargo.toml"
composition_manifest="$repo_dir/cli/crates/production-composition/Cargo.toml"
stage_install="$repo_dir/cli/crates/production-composition/scripts/stage-install.sh"
rustup_root=/Volumes/WD/Toolchains/AppPilotKit/rustup
cargo_root=/Volumes/WD/Toolchains/AppPilotKit/cargo
simulator_id=${1:-}

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

file_sha256() {
  [[ -s "$1" ]] || { print -u2 -- "provenance artifact is missing: $1"; return 1; }
  shasum -a 256 "$1" | awk '{print "sha256:" $1}'
}

write_provenance() {
  local output=$1
  local source_digest=$2
  local app_digest=$3
  local cli_digest=$4
  local broker_digest=$5
  local prepare_digest=$6
  local ffi_digest=$7
  local executable_digest=$8
  local ffi_cache_key=$9
  local cli_cache_key=${source_digest#sha256:}

  node - "$output" "$source_digest" "$app_digest" "$cli_digest" "$broker_digest" "$prepare_digest" "$ffi_digest" "$executable_digest" "$cli_cache_key" "$ffi_cache_key" <<'NODE'
const [output, source, app, cli, broker, prepare, ffi, executable, cliCacheKey, ffiCacheKey] = process.argv.slice(2);
require("node:fs").writeFileSync(output, `${JSON.stringify({
  schema_version: "1",
  source: {
    digest: source,
    inputs: [
      "acceptance/demo-foundation.contract.json",
      "cli",
      "ios/{Package.swift,InternalTargetTransport,AcceptanceHost}",
      "transport",
    ],
  },
  cache: {
    cli_cargo_target_key: cliCacheKey,
    ffi_cargo_target_key: ffiCacheKey,
    xcode_derived_data_key: cliCacheKey,
  },
  artifacts: {
    "apppilotkit": cli,
    "apppilotkit-broker": broker,
    "apppilotkit-target-prepare": prepare,
    "libapppilotkit_transport_ffi.a": ffi,
    "AcceptanceHost": executable,
    "AcceptanceHost.app": app,
  },
  build: { rust: "cargo +1.94.0", xcode: "xcodebuild" },
}, null, 2)}\n`);
NODE
}

provenance_matches_source() {
  node - "$1" "$2" <<'NODE'
const [path, expected] = process.argv.slice(2);
const value = JSON.parse(require("node:fs").readFileSync(path, "utf8"));
process.exit(value?.source?.digest === expected ? 0 : 1);
NODE
}

require_unchanged_source() {
  [[ "$1" == "$2" ]] || {
    print -u2 "source changed while artifacts were being built"
    return 1
  }
}

run_provenance_self_test() {
  local fixture
  fixture=$(mktemp -d "${TMPDIR:-/tmp}/apppilotkit-ios-provenance-red.XXXXXX")
  mkdir "$fixture/source"
  print -r -- 'current' >"$fixture/source/build-marker"
  local current_digest=$(tree_sha256 "$fixture/source")
  write_provenance "$fixture/provenance.json" "$current_digest" \
    'sha256:artifact' 'sha256:cli' 'sha256:broker' 'sha256:prepare' 'sha256:ffi' 'sha256:executable' 'ffi-cache-key'
  print -r -- 'stale' >"$fixture/source/build-marker"
  local stale_digest=$(tree_sha256 "$fixture/source")
  [[ "$current_digest" != "$stale_digest" ]] || {
    print -u2 "provenance test marker did not change the source digest"
    return 4
  }
  if require_unchanged_source "$current_digest" "$stale_digest" 2>/dev/null; then
    print -u2 "stale source digest was accepted after the build marker changed"
    find "$fixture" -depth -delete 2>/dev/null || true
    return 4
  fi
  if provenance_matches_source "$fixture/provenance.json" "$stale_digest"; then
    print -u2 "stale artifact provenance was accepted"
    find "$fixture" -depth -delete 2>/dev/null || true
    return 4
  fi
  find "$fixture" -depth -delete 2>/dev/null || true
}

if [[ "$simulator_id" == '--provenance-self-test' ]]; then
  run_provenance_self_test
  exit 0
fi

source_digest=$(tree_sha256 \
  "$repo_dir/acceptance/demo-foundation.contract.json" \
  "$repo_dir/cli" \
  "$repo_dir/ios/Package.swift" \
  "$repo_dir/ios/InternalTargetTransport" \
  "$repo_dir/ios/AcceptanceHost" \
  "$repo_dir/transport")
work_root=$(mktemp -d "${TMPDIR:-/tmp}/apppilotkit-acceptance-ios-journey.XXXXXX")
work_root=$(realpath "$work_root")
cache_key=${source_digest#sha256:}
prefix="$work_root/installed-prefix-$cache_key"
app_path="$work_root/AcceptanceHost.app"
config_path="$work_root/foundation-run.json"
evidence_path="$work_root/foundation-evidence.json"
provenance_path="$work_root/foundation-provenance.json"

[[ -x "$internal_runner" && -x "$stage_install" && -f "$ffi_manifest" && -f "$composition_manifest" ]] || {
  print -u2 "missing acceptance journey dependency"
  exit 2
}
[[ -d "$rustup_root" && -d "$cargo_root" ]] || {
  print -u2 "missing isolated AppPilotKit Rust toolchain"
  exit 2
}

if [[ -z "$simulator_id" ]]; then
  simulator_id=$(xcrun simctl list devices available | sed -nE 's/.*\(([0-9A-F-]{36})\) \(Booted\).*/\1/p' | head -n 1)
fi
if [[ -z "$simulator_id" ]]; then
  simulator_id=$(xcrun simctl list devices available | sed -nE 's/.*\(([0-9A-F-]{36})\) \(Shutdown\).*/\1/p' | head -n 1)
  [[ -n "$simulator_id" ]] || { print -u2 "no available iOS Simulator"; exit 2; }
  xcrun simctl boot "$simulator_id"
fi
xcrun simctl bootstatus "$simulator_id" -b

export RUSTUP_HOME="$rustup_root"
export CARGO_HOME="$cargo_root"
export CARGO_TARGET_DIR="$work_root/cargo-target-$cache_key"

"$cargo_root/bin/cargo" +1.94.0 build \
  --locked \
  --release \
  --manifest-path "$composition_manifest"
"$stage_install" "$CARGO_TARGET_DIR/release" "$prefix"

ffi_dir="$work_root/universal-simulator-ffi-$cache_key"
"$internal_runner" build-universal-simulator-ffi "$ffi_dir" >/dev/null
ffi_cache_digest=$("$internal_runner" staticlib-source-digest)
derived_data="$work_root/derived-data-$cache_key"
(
  cd "$host_dir"
  xcodebuild build \
    -scheme AppPilotKitAcceptanceHost \
    -configuration Debug \
    -destination 'generic/platform=iOS Simulator' \
    -derivedDataPath "$derived_data" \
    CODE_SIGNING_ALLOWED=NO \
    PRODUCT_BUNDLE_IDENTIFIER=dev.apppilotkit.acceptancehost.ios \
    "OTHER_LDFLAGS=-L$ffi_dir"
)
executable="$derived_data/Build/Products/Debug-iphonesimulator/AcceptanceHost"
[[ -x "$executable" ]] || { print -u2 "missing Debug Simulator Acceptance Host executable"; exit 3; }
mkdir "$app_path"
cp "$executable" "$app_path/AcceptanceHost"
chmod 755 "$app_path/AcceptanceHost"
print -r -- '<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>AcceptanceHost</string>
<key>CFBundleIdentifier</key><string>dev.apppilotkit.acceptancehost.ios</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleVersion</key><string>1</string>
</dict></plist>' >"$app_path/Info.plist"
app_tree_digest=$("$internal_runner" validate-ios-simulator-app dev.apppilotkit.acceptancehost.ios "$app_path")
app_path=$(realpath "$app_path")
cli_digest=$(file_sha256 "$prefix/bin/apppilotkit")
broker_digest=$(file_sha256 "$prefix/libexec/apppilotkit-broker")
prepare_digest=$(file_sha256 "$prefix/libexec/apppilotkit-target-prepare")
ffi_digest=$(file_sha256 "$ffi_dir/libapppilotkit_transport_ffi.a")
executable_digest=$(file_sha256 "$app_path/AcceptanceHost")
completed_source_digest=$(tree_sha256 \
  "$repo_dir/acceptance/demo-foundation.contract.json" \
  "$repo_dir/cli" \
  "$repo_dir/ios/Package.swift" \
  "$repo_dir/ios/InternalTargetTransport" \
  "$repo_dir/ios/AcceptanceHost" \
  "$repo_dir/transport")
require_unchanged_source "$source_digest" "$completed_source_digest"
write_provenance "$provenance_path" "$source_digest" "$app_tree_digest" \
  "$cli_digest" "$broker_digest" "$prepare_digest" "$ffi_digest" "$executable_digest" \
  "${ffi_cache_digest#sha256:}"
final_source_digest=$(tree_sha256 \
  "$repo_dir/acceptance/demo-foundation.contract.json" \
  "$repo_dir/cli" \
  "$repo_dir/ios/Package.swift" \
  "$repo_dir/ios/InternalTargetTransport" \
  "$repo_dir/ios/AcceptanceHost" \
  "$repo_dir/transport")
require_unchanged_source "$source_digest" "$final_source_digest"
provenance_matches_source "$provenance_path" "$source_digest" || {
  print -u2 "current source provenance was not recorded"
  exit 3
}

# The adapter intentionally refuses to take over an installed bundle with the
# same identity. Start this independent journey from a clean Simulator state
# so the first prepare owns its launch.
xcrun simctl uninstall "$simulator_id" dev.apppilotkit.acceptancehost.ios >/dev/null 2>&1 || true

node - "$config_path" "$prefix" "$repo_dir/acceptance/demo-foundation.contract.json" "$simulator_id" "$app_path" <<'NODE'
const [configPath, prefix, contract, udid, appArtifact] = process.argv.slice(2);
const config = {
  prefix,
  platform: "ios",
  contract,
  prepare_request: {
    schema_version: "1.0",
    platform: "ios-simulator",
    device_selector: udid,
    app_id: "dev.apppilotkit.acceptancehost.ios",
    app_artifact: appArtifact,
    artifact_encoding: "ios-app-tree-v1",
  },
  restart: {
    argv: [require("node:path").join(prefix, "libexec", "apppilotkit-target-prepare"), "--release-fd=0", "--output=json"],
  },
};
require("node:fs").writeFileSync(configPath, `${JSON.stringify(config, null, 2)}\n`);
NODE

node "$repo_dir/acceptance/harness/installed-cli-harness.mjs" \
  --config "$config_path" \
  --evidence "$evidence_path"

print -r -- "$evidence_path"
