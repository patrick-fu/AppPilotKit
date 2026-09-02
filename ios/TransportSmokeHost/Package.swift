// swift-tools-version: 6.0

import PackageDescription

// This package is deliberately outside the production AppPilotKit package.
// Its transport target is intentionally not a product: only this internal
// Debug evidence host and its tests can depend on it.
let package = Package(
  name: "AppPilotKitTransportSmokeHost",
  platforms: [
    .iOS(.v15),
    .macOS(.v13),
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
    .executableTarget(
      name: "TransportSmokeHost",
      dependencies: [
        "AppPilotKitTargetTransportInternal",
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
    .testTarget(
      name: "TransportSmokeHostTests",
      dependencies: ["TransportSmokeHost"],
      swiftSettings: [
        .define("APPPILOTKIT_INTERNAL", .when(configuration: .debug)),
      ]
    ),
  ]
)
