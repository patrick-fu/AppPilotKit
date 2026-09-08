#!/bin/zsh
set -euo pipefail

script_dir=${0:A:h}
host_dir=${script_dir:h}
ios_dir=${host_dir:h}
repo_dir=${ios_dir:h}
internal_runner="$ios_dir/InternalTargetTransport/Scripts/run-with-rust-ffi.sh"
work_root=$(mktemp -d "${TMPDIR:-/tmp}/apppilotkit-smoke-host.XXXXXX")

cleanup() {
  find "$work_root" -depth -delete 2>/dev/null || true
}
trap cleanup EXIT INT TERM

[[ -x "$internal_runner" ]] || {
  print -u2 "missing InternalTargetTransport Simulator wrapper"
  exit 2
}

validate_smoke_host_simulator_app() {
  local app_path=$1
  local canonical_app_path

  [[ "$app_path" == /* && "$app_path" == *.app && -d "$app_path" ]] || {
    print -u2 "app must be an absolute existing .app directory"
    exit 2
  }
  canonical_app_path=$(realpath "$app_path")
  "$internal_runner" validate-ios-simulator-app dev.apppilotkit.smoke "$canonical_app_path"
}

package_smoke_host_simulator() {
  local output_app=$1
  local output_parent=${output_app:h}
  local output_name=${output_app:t}
  local ffi_dir="$work_root/universal-simulator-ffi"
  local derived_data="$work_root/derived-data"
  local executable="$derived_data/Build/Products/Debug-iphonesimulator/TransportSmokeHost"

  [[ "$output_app" == /* && "$output_app" == *.app && ! -e "$output_app" && -d "$output_parent" ]] || {
    print -u2 "output app must be an absolute, nonexistent .app path with an existing parent"
    exit 2
  }
  output_parent=$(realpath "$output_parent")
  output_app="$output_parent/$output_name"

  "$internal_runner" build-universal-simulator-ffi "$ffi_dir" >/dev/null
  (
    cd "$host_dir"
    xcodebuild build \
      -scheme AppPilotKitTransportSmokeHost \
      -configuration Debug \
      -destination 'generic/platform=iOS Simulator' \
      -derivedDataPath "$derived_data" \
      OTHER_LDFLAGS="-L$ffi_dir" \
      CODE_SIGNING_ALLOWED=NO
  ) >&2

  [[ -x "$executable" ]] || {
    print -u2 "missing Debug Simulator Smoke Host executable"
    exit 3
  }
  mkdir "$output_app"
  cp "$executable" "$output_app/TransportSmokeHost"
  chmod 755 "$output_app/TransportSmokeHost"
  print -r -- '<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>TransportSmokeHost</string>
<key>CFBundleIdentifier</key><string>dev.apppilotkit.smoke</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleVersion</key><string>1</string>
</dict></plist>' >"$output_app/Info.plist"

  validate_smoke_host_simulator_app "$output_app"
  lipo -archs "$output_app/TransportSmokeHost" | grep -Eq '(^| )arm64( |$).*x86_64|(^| )x86_64( |$).*arm64'
  print -r -- "$output_app"
}

case ${1:-} in
  package-smoke-host-simulator)
    [[ $# -eq 2 ]] || {
      print -u2 "usage: $0 package-smoke-host-simulator /absolute/path/TransportSmokeHost.app"
      exit 2
    }
    package_smoke_host_simulator "$2"
    ;;
  install-smoke-host-simulator)
    [[ $# -eq 3 ]] || {
      print -u2 "usage: $0 install-smoke-host-simulator <simulator-udid> /absolute/existing/TransportSmokeHost.app"
      exit 2
    }
    canonical_app_path=$(realpath "$3")
    validate_smoke_host_simulator_app "$canonical_app_path"
    xcrun simctl install "$2" "$canonical_app_path"
    ;;
  *)
    print -u2 "usage: $0 {package-smoke-host-simulator|install-smoke-host-simulator}"
    exit 2
    ;;
esac
