#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: stage-install.sh <built-bin-dir> <prefix>" >&2
  exit 2
fi

built_bin_dir=$1
prefix=$2

for name in apppilotkit apppilotkit-broker apppilotkit-target-prepare; do
  if [ ! -f "$built_bin_dir/$name" ]; then
    echo "missing built executable: $name" >&2
    exit 1
  fi
done

mkdir -p "$prefix/bin" "$prefix/libexec"
install -m 755 "$built_bin_dir/apppilotkit" "$prefix/bin/apppilotkit"
install -m 755 "$built_bin_dir/apppilotkit-broker" "$prefix/libexec/apppilotkit-broker"
install -m 755 "$built_bin_dir/apppilotkit-target-prepare" "$prefix/libexec/apppilotkit-target-prepare"

"$prefix/bin/apppilotkit" capabilities --output json >/dev/null
"$prefix/libexec/apppilotkit-broker" >/dev/null 2>&1 && exit 1 || test "$?" -eq 2
"$prefix/libexec/apppilotkit-target-prepare" >/dev/null 2>&1 && exit 1 || test "$?" -eq 2
