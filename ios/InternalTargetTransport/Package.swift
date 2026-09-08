// swift-tools-version: 6.0

import PackageDescription

// This package is Debug/Internal-only. It is intentionally separate from the
// production AppPilotKit package so production products cannot acquire a
// transport, FFI, or listener dependency edge.
let package = Package(
  name: "AppPilotKitInternalTargetTransport",
  platforms: [
    .iOS(.v15),
    .macOS(.v13),
  ],
  products: [
    .library(
      name: "AppPilotKitTargetTransportInternal",
      targets: ["AppPilotKitTargetTransportInternal"]
    ),
  ],
  dependencies: [
    .package(name: "AppPilotKit", path: ".."),
  ],
  targets: [
    .systemLibrary(
      name: "CAppPilotKitTargetTransport",
      path: "Sources/CAppPilotKitTargetTransport"
    ),
    .systemLibrary(
      name: "CAppPilotKitTargetTransportTestBroker",
      path: "Tests/CAppPilotKitTargetTransportTestBroker"
    ),
    .target(
      name: "AppPilotKitTargetTransportInternal",
      dependencies: [
        .product(name: "AppPilotKit", package: "AppPilotKit"),
        "CAppPilotKitTargetTransport",
      ],
      swiftSettings: [
        .define("APPPILOTKIT_INTERNAL", .when(configuration: .debug)),
      ]
    ),
    .testTarget(
      name: "AppPilotKitTargetTransportInternalTests",
      dependencies: [
        "AppPilotKitTargetTransportInternal",
        "CAppPilotKitTargetTransport",
        "CAppPilotKitTargetTransportTestBroker",
      ]
    ),
  ]
)
