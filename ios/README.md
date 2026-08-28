# iOS

This directory contains the Swift package for the AppPilotKit iOS debug SDK.
It has two library products:

- `AppPilotKit` provides the Foundation-only provider SPI and bounded immutable
  snapshot runtime behind the protocol v1.1 UI inspection contract.
- `AppPilotKitUIKit` provides the `@MainActor` UIKit view snapshot adapter and
  depends on `AppPilotKit`.

The package targets iOS 15 and later. See
[ADR 0003](../docs/adr/0003-ios-provider-spi-and-snapshot-store.md) for
the runtime seam and retention decisions and
[ADR 0004](../docs/adr/0004-uikit-view-snapshot-provider.md) for UIKit
source, geometry, schema, and disclosure semantics.

Run its tests from the repository root:

```sh
swift test --jobs 1 --package-path ios
```

UIKit adapter tests run against real UIKit objects in Simulator. From `ios/`:

```sh
xcodebuild test -jobs 1 -parallel-testing-enabled NO \
  -scheme AppPilotKit-Package \
  -destination 'platform=iOS Simulator,id=<available-iphone-udid>' \
  -derivedDataPath .build/xcode-derived \
  CODE_SIGNING_ALLOWED=NO
```

The UIKit provider intentionally omits user-controlled text, SwiftUI semantic
internals, and accessibility-tree coverage. The package still includes no
transport, listener, screenshot, action, CLI, or app integration. Later
vertical slices must retain iOS 15 through iOS 26 compatibility and exercise
Simulator and physical-device paths.
