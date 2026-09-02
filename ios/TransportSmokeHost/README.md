# iOS transport smoke host

This is the #62 Debug/Internal-only iOS evidence application. It owns the
non-public `AppPilotKitTargetTransportInternal` target and, after a
Broker-owned descriptor launch, registers only the read-only `smoke.ready` Resource (revision `1`,
schema `schema_smoke_ready_v1`, value `{"ready":true}`).

It is deliberately not a product or dependency of `ios/Package.swift`. The
transport target has no SwiftPM product edge, so a normal external consumer
cannot link it through the package dependency graph. This product-edge property
is packaging scope, not adversarial isolation: an actor controlling SmokeHost
sources, manifests, compiler/linker flags, or final signing inputs can change
what is built or shipped. There is no Release target:
`Scripts/verify-release-exclusion.sh` builds the production package and proves
that a Release Smoke Host compilation fails.

Build a Debug/Internal-only universal Simulator `.app` with its accepted Rust FFI dependency:

```text
Scripts/run-with-rust-ffi.sh package-smoke-host-simulator /absolute/path/TransportSmokeHost.app
```

The output has bundle identifier `dev.apppilotkit.smoke`, package type `APPL`,
and a universal arm64/x86_64 executable. It is a real Simulator app bundle, not
a Swift executable probe. Install it on an exact Simulator UDID with:

```text
Scripts/run-with-rust-ffi.sh install-smoke-host-simulator <simulator-udid> /absolute/path/TransportSmokeHost.app
```

The package command rejects an existing output path and prints its canonical
artifact path. The install command canonicalizes the existing path, then runs
the same `ios-app-tree-v1` artifact scanner used by prepare before calling
`simctl`. The installed `apppilotkit` prepare path must use the exact canonical
path printed by the package command and app id with `artifact_encoding` set to
`ios-app-tree-v1`.
