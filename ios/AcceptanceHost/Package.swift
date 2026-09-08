// swift-tools-version: 6.0

import PackageDescription

// This Debug/Internal-only application is an acceptance fixture. It is kept
// outside the production AppPilotKit package and explicitly composes the
// private transport package only for its Debug configuration.
let package = Package(
  name: "AppPilotKitAcceptanceHost",
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
      name: "AcceptanceHost",
      dependencies: [
        .product(name: "AppPilotKit", package: "AppPilotKit"),
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
      name: "AcceptanceHostTests",
      dependencies: ["AcceptanceHost"],
      swiftSettings: [
        .define("APPPILOTKIT_INTERNAL", .when(configuration: .debug)),
      ]
    ),
  ]
)
