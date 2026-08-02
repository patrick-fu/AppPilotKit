# Product vision

## Goal

Let an AI coding agent see, understand, and operate a running mobile app without depending on a human to relay screenshots, hierarchy dumps, or touch actions.

## Product shape

AppPilotKit has four cooperating parts:

1. An iOS debug SDK embedded in an opted-in app target.
2. An Android debug SDK embedded in an opted-in app build variant.
3. A desktop CLI that discovers devices and running AppPilotKit endpoints.
4. A versioned, platform-neutral protocol shared by both sides.

The SDK exposes runtime truth that external automation cannot reliably see. Platform tools such as `devicectl`, USB tunnels, ADB, and UI automation APIs may be used as transport or fallback primitives, but they do not define the public product model.

## Initial capabilities

### Inspect

- Capture the current app screenshot.
- Enumerate the complete native UI hierarchy when explicitly requested.
- Return an interactive or visible-node summary by default.
- Query nodes by stable reference, identifier, text, class/type, traits, or geometry.
- Return a bounded subtree with optional ancestors and sibling context.
- Produce a cropped or annotated screenshot for selected nodes.

### Act

- Tap, long-press, swipe, scroll, and type through platform-appropriate primitives.
- Act on stable references from the latest UI snapshot.
- Invoke app-registered semantic actions for operations that raw gestures cannot express safely.
- Wait for UI stability and return before/after evidence.

### Agent-facing disclosure

- Default to compact summaries rather than full trees.
- Support `root`, `depth`, `maxNodes`, filters, and cursors for bounded traversal.
- Separate cheap node fields from expensive details.
- Save full screenshots and large payloads as local artifacts; return paths and metadata.
- Make truncation explicit and provide the next query needed to reveal more.

## Platform model

The protocol must preserve each source tree's identity, for example:

- UIKit view hierarchy;
- SwiftUI/accessibility projection where raw views are insufficient;
- Android View hierarchy;
- Compose semantics tree;
- accessibility hierarchy;
- pixel/screenshot evidence.

Cross-platform commands may normalize common fields, but raw platform details remain available on demand.

## Security and release isolation

- Integration is opt-in and limited to dedicated Debug/Internal configurations.
- Production artifacts must use a no-op or absent implementation, verified by build tests.
- Sessions require an ephemeral authentication token and protocol handshake.
- Network listeners are loopback-only; physical-device access goes through trusted platform tunnels.
- Sensitive fields are redacted before serialization.
- Destructive semantic actions require explicit policy metadata and must not be invoked implicitly.

## Delivery sequence

1. Define the protocol envelope, sessions, errors, and output limits.
2. Build an iOS vertical slice: connect, screenshot, compact hierarchy, query, tap.
3. Build the equivalent Android vertical slice.
4. Add subtree pagination, annotation, stability waits, and semantic actions.
5. Harden physical-device transports, release isolation, compatibility, and performance.

## Not decided yet

- CLI executable name.
- Package coordinates and public license.
- MCP and Agent Skill adapters; SDK and CLI come first.
- Optional compatibility adapters for Lookin, Appium, or other ecosystems.
