// swift-tools-version: 6.0

import PackageDescription

let package = Package(
  name: "AppPilotKit",
  platforms: [
    .iOS(.v15),
    .macOS(.v13),
  ],
  products: [
    .library(name: "AppPilotKit", targets: ["AppPilotKit"])
  ],
  targets: [
    .target(name: "AppPilotKit"),
    .testTarget(name: "AppPilotKitTests", dependencies: ["AppPilotKit"]),
  ]
)
