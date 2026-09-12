# Android physical-device adapter

This publish-disabled crate implements the private wired-Android side of the
Host `PlatformTargetAdapter` SPI. It accepts only an exact USB/hardware serial
and rejects `emulator-*` selectors, empty selectors, CoreDevice UUIDs, and iOS
UDIDs.

After a host APK snapshot whose digest equals the PrepareKey, `pm path <package>`
decides install: empty/absent installs this Host APK without `-r` and marks
`installed_by_lease`; one or more `package:/...` lines is `Rejected` because the
on-device package cannot be hashed. `pm list packages <filter>` is substring
matching and is not used. The adapter does not overwrite a pre-existing
same-package app.

Launch uses one public `dev.apppilotkit.transport.DESCRIPTOR` extra, never a
secret, PBS, or token. After `am start`, `pidof` must return exactly one PID
that was absent before that start. Cleanup re-proves that PID still uniquely
belongs to the selected package before `force-stop`; a reused or colliding PID
is `CleanupFailed` and is not killed. An already-exited PID is success. It
uninstalls only `installed_by_lease` after the owned process is gone. Package
or process query failure is `CleanupFailed`.

Forwarding is `adb forward tcp:0 localabstract:<adapter-owned-name>` with the
same two-second rollback budget as the emulator adapter. Unit tests inject a
fake `CommandRunner`; they are not a live `adb` journey.
