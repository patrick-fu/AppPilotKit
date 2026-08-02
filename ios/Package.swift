// swift-tools-version: 6.0

import PackageDescription

let package = Package(
  name: "AppPilotKit",
  platforms: [
    .iOS(.v15),
    .macOS(.v13),
  ],
  products: [
    .library(name: "AppPilotKit", targets: ["AppPilotKit"]),
    .library(name: "AppPilotKitUIKit", targets: ["AppPilotKitUIKit"]),
  ],
  targets: [
    .target(name: "AppPilotKit"),
    .target(name: "AppPilotKitUIKit", dependencies: ["AppPilotKit"]),
    .testTarget(name: "AppPilotKitTests", dependencies: ["AppPilotKit"]),
    .testTarget(
      name: "AppPilotKitUIKitTests",
      dependencies: ["AppPilotKit", "AppPilotKitUIKit"]
    ),
  ]
)
