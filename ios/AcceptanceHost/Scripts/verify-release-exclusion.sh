#!/bin/zsh
set -euo pipefail

script_dir=${0:A:h}
host_dir=${script_dir:h}
ios_dir=${host_dir:h}
scratch_root=$(mktemp -d "${TMPDIR:-/tmp}/apppilotkit-acceptance-release.XXXXXX")
cleanup() {
  find "$scratch_root" -depth -delete 2>/dev/null || true
}
trap cleanup EXIT INT TERM

production_dump=$(swift package --package-path "$ios_dir" dump-package)
if print -r -- "$production_dump" | grep -q 'TargetTransport\|AcceptanceHost'; then
  print -u2 "production iOS package exposes an internal acceptance transport edge"
  exit 1
fi

swift build \
  --package-path "$ios_dir" \
  --scratch-path "$scratch_root/production" \
  --configuration release \
  --jobs 1

release_log="$scratch_root/acceptance-host-release.log"
if swift build \
  --package-path "$host_dir" \
  --scratch-path "$scratch_root/acceptance-host" \
  --configuration release \
  --target AcceptanceHost \
  --jobs 1 >"$release_log" 2>&1; then
  print -u2 "AcceptanceHost unexpectedly built as Release"
  exit 1
fi
if ! grep -q 'Debug/Internal-only' "$release_log"; then
  print -u2 "AcceptanceHost Release build failed for an unexpected reason"
  cat "$release_log" >&2
  exit 1
fi
