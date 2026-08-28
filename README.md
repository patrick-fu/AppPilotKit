# AppPilotKit

AppPilotKit is an agent-native inspection and control layer for iOS and Android apps.

It will combine an app-embedded debug SDK with a desktop CLI so coding agents can:

- inspect UI trees and screenshots;
- query elements and request bounded subtrees;
- tap, swipe, type, and invoke explicitly registered app actions;
- receive stable, compact JSON designed for progressive disclosure;
- work with simulators, emulators, and physical devices.

The project is in early development. Package coordinates and the final CLI command name remain under active development.

## Repository layout

- `ios/` — iOS SDK and sample app.
- `android/` — Android SDK and sample app.
- `cli/` — desktop client and device transports.
- `protocol/` — platform-neutral protocol and schemas.
- `docs/` — product constraints, architecture decisions, and agent configuration.

Read [docs/vision.md](docs/vision.md) before proposing implementation work.

## License

AppPilotKit is available under the [MIT License](LICENSE).
