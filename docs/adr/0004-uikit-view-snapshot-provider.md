# ADR 0004: UIKit view snapshot provider

- Status: Accepted
- Date: 2026-08-02
- Issue: [#8](https://github.com/patrick-fu/AppPilotKit/issues/8)

## Context

ADR 0003 defines a provider seam that accepts detached redacted captures but
does not read platform state. The first real iOS adapter must capture UIKit
view trees without making the Foundation-only runtime depend on UIKit, erasing
native hierarchy semantics, or retaining live view objects.

UIKit can expose several windows across several scenes. It also exposes many
user-controlled strings and arbitrary subclass state that must not cross the
redaction seam by default.

## Decision

### Module and interface

`AppPilotKitUIKit` is a separate Swift Package library target that depends on
the core `AppPilotKit` target. Its deep module is one `@MainActor`
`UIKitSnapshotProvider` adapter. Callers choose one disclosure mode and may
inject a main-actor window supplier for deterministic embedding or tests; all
window discovery, traversal, projection, and redaction remain implementation
details.

The adapter uses provider name `uikit.views`, native representation, and schema
`uikit.view@1`. Capture reads UIKit state synchronously on the main actor and
returns only detached `RedactedProviderCapture` values.

### Windows and sources

Production discovery uses `UIApplication.shared.connectedScenes`, keeps only
`UIWindowScene` instances, sorts scenes by session persistent identifier, and
then preserves each scene's `windows` array order. Repeated `UIWindow` objects
are removed by identity while preserving their first occurrence.

Each retained window becomes one complete source named
`uikit.window.<offset>`, with that `UIWindow` as its only root. Hidden windows
remain part of the native hierarchy. No cross-snapshot source stability is
claimed.

The adapter traverses `UIView.subviews` iteratively in native depth-first
pre-order. Snapshot-local provider IDs are sequential assembly keys only. They
may repeat in different sources and are replaced by the core runtime's opaque
references before storage.

### Geometry and index fields

Every source uses screen-space iOS points and the source window's screen scale.
Each view's bounds are converted through its window to screen coordinates. A
non-finite or negative-size result fails the provider capture instead of
returning a record that the runtime would reject.

The index contains the dynamic Objective-C class name, screen frame, effective
visibility, effective interactivity, and—only in identifiers mode—a bounded
accessibility identifier.

Effective visibility is true only when all of these hold:

- the node is the source window or is attached to it;
- its bounds are non-empty;
- no ancestor through the source window is hidden;
- cumulative alpha through the source window is greater than `0.01`;
- its converted frame intersects the source window bounds.

The adapter does not attempt pixel occlusion or clipping-region analysis.
Effective interactivity additionally requires `isUserInteractionEnabled` and
either an enabled `UIControl` or an enabled gesture recognizer. A source
`UIWindow` root is never marked interactive because UIKit/test hosts may attach
private window gestures and the root is already included structurally.

### Disclosure and native schema

Structural disclosure is the default. It omits `accessibilityIdentifier` and
all user-controlled text. Identifiers disclosure copies a non-empty
`accessibilityIdentifier` only when it is at most 512 Unicode scalars. It does
not enable any other string field.

Neither mode reads accessibility label/value/hint, visible or attributed text,
text-input contents, button titles, arbitrary KVC values, reflection output,
debug descriptions, pointers, or memory addresses.

Schema `uikit.view@1` contains only these structural native fields:

- every view: `alpha`, `hidden`, `opaque`, `clipsToBounds`,
  `userInteractionEnabled`, and `tag`;
- window roots: `keyWindow` and `windowLevel` in addition to the view fields.

The provider returns `noWindows` when discovery yields no source and
`invalidGeometry` when a view cannot produce protocol-safe geometry. The core
runtime maps either provider failure to `internalError` and commits no partial
snapshot.

## Consequences

- UIKit hierarchy collection can evolve without importing UIKit into the core
  runtime or exposing traversal mechanics to callers.
- A fixed UIKit tree produces deterministic window/source order, native DFS
  order, adjacency, index fields, and native payloads.
- Default capture is useful for structural diagnosis without serializing
  user-visible text.
- UIKit view coverage does not claim SwiftUI semantic internals, an
  accessibility tree, pixel visibility, or app UI outside discovered windows.
- This decision adds no transport, listener, RPC/session layer, screenshot,
  query engine, cursor, action, sample host app, CLI, or Agent Skill.

## Rejected alternatives

- **Put UIKit collection in the core target:** removes macOS host testability
  and couples storage policy to one platform adapter.
- **One recursive source containing all windows:** violates the single-root
  source contract and invents a non-UIKit synthetic root.
- **Serialize arbitrary view properties:** leaks user data, private UIKit
  implementation state, and unstable descriptions.
- **Capture visible text by default:** crosses the redaction seam without an
  application-owned policy for sensitive content.
- **Use pointer-derived node IDs as stored references:** leaks process details
  and bypasses the runtime's snapshot-scoped reference ownership.
