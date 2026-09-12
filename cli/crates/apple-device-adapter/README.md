# Apple physical-device adapter

This publish-disabled crate implements the private wired-iPhone side of the
Host `PlatformTargetAdapter` SPI. It accepts only an exact hardware UDID in the
dashed usbmux `SerialNumber` form and requires a USB `ListDevices` record for
that serial.

After the USB gate and a host `ios-app-tree-v1` digest check, it refuses launch
when `device info processes` already shows an executable ending in
`/{AppBundleName}.app/{CFBundleExecutable}` from the Host `.app`. Bare
executable basenames and process-list bundle IDs are not occupancy identity;
the live `runningProcesses` items have only `executable` and
`processIdentifier`. `device info apps --bundle-id` then decides install:
absent (`result.apps = []`) installs this Host `.app` and marks
`installed_by_lease`; present is `Rejected` because the on-device tree cannot
be hashed. The adapter does not overwrite a pre-existing same-bundle app.

If `install app` is invoked and then fails, cancels, or times out, the adapter
re-queries `info apps --bundle-id` and uninstalls a leftover or returns
`CleanupFailed`. It does not release only the reservation after a mutation.

Launch uses one public `DEVICECTL_CHILD_APPPILOTKIT_TRANSPORT_DESCRIPTOR`
environment variable, never `--terminate-existing`. The JSON launch PID must
uniquely match a fresh on-device process list by the same `.app/exec` suffix.
Cleanup re-proves that PID's `executable` still uniquely matches that suffix
before terminate; a reused PID with a different executable is `CleanupFailed`
and is not killed. An already-exited PID is success. It uninstalls only
`installed_by_lease`.

usbmux `Connect` refreshes `DeviceID` from `ListDevices` first. Launch-time
Connect `Number` of 3 (`RESULT_CONNREFUSED`) may be retried until the existing
deadline as an unaccepted candidate; session reconnect fails closed on the
first refused later connect. USB miss, malformed plist, duplicate serial, and
any other Number fail closed. That refused-only launch retry is unverified
against a live device.

It rejects a CoreDevice UUID used as the selector. A USB miss or Network-only
record is `Unavailable`, not `Rejected`. `devicectl info` may acquire a
CoreDevice tunnel as a side effect; the adapter does not use that tunnel or
DDI as a data path. It never places a token, PBS, or HMAC in env/argv, never
attaches by executable, and never opens `iproxy`, LAN, or a Python relay.

Unit tests inject FakeRunner and FakeUsbMux; they are not a live iPhone
journey.

This crate is a member of the CLI workspace and uses the shared `cli/Cargo.lock`.
