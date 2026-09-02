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

Build the Debug simulator package with its accepted Rust FFI dependency:

```text
Scripts/run-with-rust-ffi.sh smoke-host-simulator
```
