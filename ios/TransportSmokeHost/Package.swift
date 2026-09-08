// swift-tools-version: 6.0

import PackageDescription

// This package is deliberately outside the production AppPilotKit package.
// It composes the separately packaged internal transport only in Debug.
let package = Package(
  name: "AppPilotKitTransportSmokeHost",
  platforms: [
    .iOS(.v15),
    .macOS(.v13),
  ],
  dependencies: [
    .package(name: "AppPilotKit", path: ".."),
    .package(name: "AppPilotKitInternalTargetTransport", path: "../InternalTargetTransport"),
  ],
  targets: [
    .executableTarget(
      name: "TransportSmokeHost",
      dependencies: [
        .product(
          name: "AppPilotKitTargetTransportInternal",
          package: "AppPilotKitInternalTargetTransport"
        ),
      ],
      swiftSettings: [
        .define("APPPILOTKIT_INTERNAL", .when(configuration: .debug)),
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
