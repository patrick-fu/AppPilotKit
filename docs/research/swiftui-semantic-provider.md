# SwiftUI semantic provider architecture

- Status: research recommendation, not an implementation or protocol decision
- Date: 2026-08-08
- Issue: [#34](https://github.com/patrick-fu/AppPilotKit/issues/34)
- Scope: first-class SwiftUI Semantic Projection for iOS 15 through iOS 26

## Conclusion

Build the SwiftUI provider as an app-owned semantic projection, registered by
the opted-in Target and captured on the main actor. The app declares the
projection at SwiftUI view boundaries; the provider receives detached records
containing only declared identity, text, identifiers, traits, geometry, and
reference-driven actions. It must not inspect SwiftUI's framework-private view
graph, use reflection, infer semantics from `Mirror`, or depend on private
UIKit/Accessibility selectors.

The public SwiftUI API is excellent for *declaring* accessibility semantics,
but Apple does not document an in-process API that enumerates SwiftUI's
semantic tree, returns stable node handles, or invokes an arbitrary SwiftUI
view action by such a handle. The UIKit provider therefore remains separate;
an accessibility projection is an additional overlapping source, not a
replacement for either source.

## Evidence from Apple documentation

The following are documented public mechanisms. They describe what an app may
declare or what UIKit accessibility containers may expose; they do not promise
an AppPilotKit-readable SwiftUI tree.

| Concern | Public mechanism | Evidence and boundary |
| --- | --- | --- |
| Semantic ownership | SwiftUI accessibility modifiers; `accessibilityElement(children:)`, `accessibilityChildren`, and `accessibilityRepresentation` | SwiftUI creates/combines/contains/hides accessibility elements and can provide synthetic representations. The App declares semantics through these modifiers; framework realization and traversal are internal, not a public enumeration/handle API. `accessibilityChildren` and `accessibilityRepresentation` require iOS 16+. [Accessibility fundamentals](https://developer.apple.com/documentation/swiftui/accessibility-fundamentals), [accessibilityElement(children:)](https://developer.apple.com/documentation/swiftui/view/accessibilityelement%28children%3A%29), [accessibilityRepresentation](https://developer.apple.com/documentation/swiftui/view/accessibilityrepresentation%28representation%3A%29) |
| Identity | `accessibilityIdentifier(_:)` | A developer-supplied, non-user-visible testing identifier. It is suitable as a query field only when the app disclosure policy allows it. SwiftUI `id(_:)` identifies view state/reconciliation, not a public inspectable node handle. [accessibilityIdentifier](https://developer.apple.com/documentation/swiftui/view/accessibilityidentifier%28_%3A%29), [id(_:)](https://developer.apple.com/documentation/swiftui/view/id%28_%3A%29) |
| Text | `accessibilityLabel`, `accessibilityValue`, `accessibilityHint`, built-in control defaults | Apple documents inferred labels/values for common controls and explicit modifiers. These are user content and must be redacted or policy-enabled before storage. [Accessibility modifiers](https://developer.apple.com/documentation/swiftui/view-accessibility), [accessible descriptions](https://developer.apple.com/documentation/swiftui/accessible-descriptions) |
| Traits | `AccessibilityTraits`, `accessibilityAddTraits`, `accessibilityRemoveTraits` | Public provider-native traits include button, image, header, link, selected, toggle, search field, adjustable-related behavior, and others. Preserve names; do not invent a cross-platform role. [AccessibilityTraits](https://developer.apple.com/documentation/swiftui/accessibilitytraits) |
| Geometry | SwiftUI layout/geometry APIs; accessibility content shape; UIKit `UIAccessibilityElement.accessibilityFrame` | SwiftUI lets the app observe layout through its own view code and can alter the accessibility shape. UIKit accessibility elements expose screen-space frames. There is no documented API to ask SwiftUI for every realized semantic element's frame from outside the declaration. [ContentShapeKinds.accessibility](https://developer.apple.com/documentation/swiftui/contentshapekinds/accessibility), [UIAccessibilityElement](https://developer.apple.com/documentation/uikit/uiaccessibilityelement) |
| Actions | `accessibilityAction`, `accessibilityActions`, adjustable/scroll actions, and AppIntent overloads | These register actions for assistive technologies. The action closure/intent is app-owned; a returned `Bool`-like acknowledgement is not a business-outcome guarantee. [Accessible controls](https://developer.apple.com/documentation/swiftui/accessible-controls), [accessibilityAction(named:_:)](https://developer.apple.com/documentation/swiftui/view/accessibilityaction%28named%3A_%3A%29) |
| UIKit container bridge | `UIAccessibilityContainer`, `UIAccessibilityElement`, `UIView` accessibility properties | UIKit permits an app-owned container to expose elements, labels, traits, and frames. It does not make SwiftUI's private realization contract public. [UIAccessibilityContainer](https://developer.apple.com/documentation/uikit/uiaccessibilitycontainer), [UIAccessibility protocol](https://developer.apple.com/documentation/uikit/uiaccessibility-protocol) |
| Windows | `Window`, `WindowGroup`, scene IDs, `openWindow` | SwiftUI documents singleton and grouped windows and programmatic opening. Window identity belongs to the Scene declaration; discovery of eligible app windows remains an app integration concern. [Window](https://developer.apple.com/documentation/swiftui/window), [WindowGroup](https://developer.apple.com/documentation/swiftui/windowgroup) |
| Lifecycle | `scenePhase` (`active`, `inactive`, `background`) | A view reads its containing scene phase from the SwiftUI environment; an `App` observes aggregate phase across scenes. Capture/actions must require an active Foreground Target and invalidate interactive sessions when it backgrounds, per project context. [ScenePhase](https://developer.apple.com/documentation/swiftui/scenephase) |

## Decisions

### Ownership and capture seam

The app owns a registry of semantic projection roots, ordered per app policy.
Each root is associated with one app-owned SwiftUI scene/window and captures
on `@MainActor`. The registry is not a second global hierarchy: each root is
one source with one root node, native schema `swiftui.semantics@1`, and
coordinate space in iOS points plus display scale. The provider converts every
record to detached `RedactedProviderCapture` before returning to the
Foundation-only runtime, matching ADR 0003.

The projection is explicit: a view wrapper/modifier supplies a developer key,
optional label/value text, identifiers, traits, geometry anchor, visibility /
interactivity, and named action descriptors. It may use SwiftUI's
`accessibilityRepresentation` and `accessibilityChildren` to align what the
user sees with what assistive technologies see, but the provider's own
registry is the source of stable AppPilotKit node identity. No provider code
walks `some View`, `body`, `ModifiedContent`, or hosting internals.

### Identity, queries, and references

The app key is a matching hint inside one source, not a cross-snapshot
reference. The runtime assigns opaque snapshot-scoped references exactly as
ADR 0002/0003 require. Queries expose only policy-permitted identifier, text,
provider trait strings, visibility, interactivity, and frame. `id(_:)` is not
used as an external identity promise; changing view state can reset it by
design. Duplicate keys fail capture or are disambiguated only by the app's
explicit parent/index path before runtime reference assignment.

### Actions and text

Only app-registered named actions and declared accessibility actions are
advertised. The action adapter executes on the main actor, binds to the exact
snapshot reference, and reports dispatch/acknowledgement plus bounded
before/after evidence. It never claims domain completion. Text entry is an
explicit set/insert semantic adapter with selection and keyboard effects
disclosed; secure text is never captured. This follows the action-backend
recommendation and avoids pretending that VoiceOver actions are generic touch
injection.

### Windows and lifecycle

The app supplies current scene/window roots rather than the provider guessing
at system windows. A `WindowGroup` may have multiple scene instances; a
singleton `Window` has one declared identity. The provider captures all
eligible app roots supplied for the active Target, excludes keyboard,
permission, Settings, and other System Surface windows, and fails closed when
no eligible root exists. `scenePhase == .active` is required for guaranteed
inspection/actions; background or scene destruction invalidates in-flight
interactive use and leaves retained immutable snapshots subject to normal
scope rules.

### Redaction and overlap

Structural fields (type/schema, geometry, visibility, interactivity, traits,
adjacency) are default disclosure. Identifiers require the explicit
Developer Metadata policy. Labels, values, hints, custom content, and text
input are User Content; secure fields and secrets are always omitted. Redaction
occurs in the app-owned provider before runtime storage; unknown fields fail
closed. A SwiftUI projection may overlap the UIKit hosting view and the
separate accessibility source. ADR 0002 requires preserving source identity
and forbids deduplication, so overlapping nodes are expected and must not be
merged into a lossy tree.

## Compatibility (iOS 15–26)

The baseline uses only SwiftUI View accessibility modifiers, `AccessibilityTraits`,
scene/window APIs, main-actor capture, and Foundation/Swift concurrency already
available to the package. Availability annotations must be checked in the
deployment SDK; where a newer convenience overload is unavailable, the app
uses the older closure-based action or label form. The provider capability
must be negotiated as a named protocol capability (ADR 0001), and a missing
capability fails rather than silently falling back to UIKit or screenshots.

`accessibilityChildren` and `accessibilityRepresentation` are iOS 16+.
An iOS 15 projection root must rely only on its own declared record and
iOS-15-available modifiers; it cannot register a root whose semantics depend
on either iOS 16 mechanism.

This architecture is resilient to framework changes because it depends on an
app-owned record contract, not SwiftUI's undocumented implementation tree. New
public modifiers may add optional native fields in a new provider schema/minor;
they cannot change existing field meaning. OS behavior differences discovered
by the probe below remain provider coverage metadata, never an implicit claim
of complete semantic coverage.

## Smallest throwaway probe

Use one acceptance-host SwiftUI screen on the oldest supported SDK/runtime and
the newest available runtime: two `WindowGroup` scenes, one singleton window,
`Text`, `Button`, `Toggle`, `Slider`, `TextField`, secure field, custom
`accessibilityChildren`, `accessibilityRepresentation`, duplicate and changing
keys, custom traits, and named actions. Register the proposed app-owned
projection and separately inspect the hosting `UIView`'s documented
accessibility-container surface. Record only detached structural data,
redacted text/identifier outcomes, screen-space frames, action dispatch result,
scene phase, and source overlap. The probe must answer whether each declared
record remains live through state updates, scene recreation, and backgrounding;
it must not use reflection, private symbols, or a production dependency.

The probe cannot make undocumented SwiftUI enumeration safe. If a behavior is
visible only through a hosting view's private descendants or changes across OS
versions, mark that source partial and keep the app-owned projection as the
MVP provider.

## Rejected approaches

- Walking SwiftUI `body`/`some View` values with reflection: private
  implementation, unstable identity, and forbidden by the issue boundary.
- Treating the UIKit hosting hierarchy as SwiftUI semantics: it is useful for
  UIKit structural diagnosis but does not preserve SwiftUI semantic ownership.
- Treating VoiceOver/accessibility APIs as a general in-process tree query:
  Apple documents declaration and assistive-technology containers, not a
  stable SwiftUI inspection/action handle API.
- Reusing `id(_:)` as a persistent reference: SwiftUI explicitly resets state
  when the proxy changes, and ADR 0002 makes references snapshot-scoped.
- Falling back to screenshots, XCTest, UI automation, or private selectors:
  these cross the App Surface/provider boundary and do not satisfy a stable
  semantic projection.

## Sources and checks

Primary sources are the Apple documentation links in the evidence table and
the accepted repository decisions [ADR 0001](../adr/0001-protocol-envelope-and-compatibility.md),
[ADR 0002](../adr/0002-ui-snapshot-and-inspection.md),
[ADR 0003](../adr/0003-ios-provider-spi-and-snapshot-store.md),
[ADR 0004](../adr/0004-uikit-view-snapshot-provider.md), and
[ADR 0006](../adr/0006-agent-facing-cli-contract.md), plus
[CONTEXT.md](../../CONTEXT.md), [vision](../vision.md), and
[iOS README](../../ios/README.md). No production SwiftUI or protocol code is
recommended or added. The only required repository check is `git diff --check`.
