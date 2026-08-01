# iOS

This directory contains the Swift package for the AppPilotKit iOS debug SDK.
Its current production-shaped module is the provider SPI and bounded immutable
snapshot runtime behind the protocol v1.1 UI inspection contract.

The package targets iOS 15 and later. It currently includes no UIKit/SwiftUI
collector, transport, listener, screenshot, action, CLI, or app integration.
See [ADR 0003](../docs/adr/0003-ios-provider-spi-and-snapshot-store.md)
for the runtime seam and retention decisions.

Run its tests from the repository root:

```sh
swift test --package-path ios
```

Later vertical slices must retain iOS 15 through iOS 26 compatibility and
exercise Simulator and physical-device paths.
