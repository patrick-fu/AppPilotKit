# AppPilotKit agent guide

## Project intent

AppPilotKit gives coding agents a compact, deterministic way to inspect and control opted-in iOS and Android apps through an embedded debug SDK and a desktop CLI.

Before changing architecture or public interfaces, read `docs/vision.md` and relevant ADRs under `docs/adr/`.

## Repository boundaries

- `ios/` owns the iOS SDK and Apple-platform transport adapters.
- `android/` owns the Android SDK and Android transport adapters.
- `cli/` owns discovery, sessions, commands, output shaping, and host-side artifacts.
- `protocol/` owns shared message contracts and versioning.
- Keep platform-native collection and action code behind the shared protocol; do not force UIKit, SwiftUI, Android View, Compose, or accessibility trees into one lossy internal shape.

## Non-negotiable constraints

- Support simulators/emulators and physical devices.
- Keep the iOS design compatible with iOS 15 through iOS 26.
- Ship no active server or privileged inspection path in production builds.
- Bind listeners to loopback or an authenticated device tunnel; never expose an unauthenticated LAN listener.
- Treat screenshots, UI text, logs, tokens, and app-provided state as sensitive.
- Prefer compact summaries, stable element references, bounded subtrees, cursors, and file-backed screenshots over dumping full state into Agent context.
- Do not introduce a required dependency on Lookin, Appium, WebDriverAgent, or another automation framework without an ADR.
- Keep protocol changes backward-compatible within a major version and cover them with contract tests.

## Change discipline

- Implement the smallest vertical slice that can be exercised end to end.
- Add tests for protocol behavior, filtering limits, authorization boundaries, and release-build exclusion.
- Record durable architecture choices under `docs/adr/`; avoid speculative abstractions.
- Use English for code, identifiers, documentation, and commit messages.

## Agent skills

### Issue tracker

Issues and PRDs are tracked in GitHub Issues. See `docs/agents/issue-tracker.md`.

### Triage labels

This repo uses the default triage label vocabulary. See `docs/agents/triage-labels.md`.

### Domain docs

This repository uses a single-context domain-doc layout. See `docs/agents/domain.md`.
