#!/bin/zsh
set -euo pipefail

script_dir=${0:A:h}
host_dir=${script_dir:h}
ios_dir=${host_dir:h}
repo_dir=${ios_dir:h}
scratch_root=$(mktemp -d "${TMPDIR:-/tmp}/apppilotkit-smoke-release.XXXXXX")
cleanup() {
  find "$scratch_root" -depth -delete 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# The production package must not gain a product, target, or dependency edge
# merely because the Internal-only evidence app exists beside it.
production_dump=$(swift package --package-path "$ios_dir" dump-package)
if print -r -- "$production_dump" | grep -q 'TargetTransport\|TransportSmokeHost'; then
  print -u2 "production iOS package exposes an internal smoke transport edge"
  exit 1
fi

swift build \
  --package-path "$ios_dir" \
  --scratch-path "$scratch_root/production" \
  --configuration release \
  --jobs 1

# A Release build of the Smoke Host itself is forbidden at compile time. This
# protects against a future project/configuration accidentally turning the
# evidence source into a production Target.
if swift build \
  --package-path "$host_dir" \
  --scratch-path "$scratch_root/smoke-release" \
  --configuration release \
  --target TransportSmokeHost \
  --jobs 1 >/dev/null 2>&1; then
  print -u2 "TransportSmokeHost unexpectedly built as Release"
  exit 1
fi
