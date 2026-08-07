# Android runtime and View provider (Issue #27)

## Decision

Build the Android provider as an opt-in, in-process `Android View` provider in
the Debug/Internal app variant. It captures only the opted-in foreground
Target's attached application windows, synchronously on Android's main thread,
and immediately converts them to a detached, redacted value graph. The core
runtime then validates, assigns snapshot-scoped opaque references, stores an
immutable record, and exposes the existing v1.1 compact/full selection and
pagination contract. Compose semantics is a separate provider; it is not
flattened into this tree.

This is the smallest architecture that preserves Android-native truth while
matching [ADR 0002](../adr/0002-ui-snapshot-and-inspection.md), [ADR
0003](../adr/0003-ios-provider-spi-and-snapshot-store.md), and the product
boundary in [CONTEXT.md](../../CONTEXT.md). It is a design decision, not a
Kotlin or wire-protocol implementation.

## Evidence and implications

### Build isolation

**Documented.** Android Studio/AGP creates `debug` and `release` build types;
the default debug type is `debuggable true`, and source sets are merged by
variant. A custom `internal` type/flavor can therefore carry the SDK and
registration only in opted-in artifacts. [Configure build variants](https://developer.android.com/build/build-variants),
[Gradle build overview](https://developer.android.com/build/gradle-build-overview),
[Debug your app](https://developer.android.com/studio/debug).

**Decision (inference from the documented variant mechanism).** Put the
provider, endpoint registration, and any debug-only dependencies behind an
explicit Debug/Internal source-set boundary. Release contains an absent or
no-op integration and no listener. Build verification must prove both that
Debug/Internal exposes the capability and that release bytecode/manifest has
no active provider path. `debuggable` alone is not a security policy.

### API-level scope

**Decision.** The Android MVP baseline is API 26 through API 37. The provider
uses only framework View/Window operations available throughout that range.
Multi-window visibility exists in the baseline, but multi-resume behavior is
Android 10/API 29 and later: on earlier releases, a visible but non-resumed
Activity is not an eligible Foreground Target for actions. Android 14/API 34
app-process accessibility querying is an optional future action enhancement,
not provider baseline behavior. [multi-window
guide](https://developer.android.com/develop/ui/views/layout/support-multi-window-mode)

### Main-thread capture

**Documented.** Android requires every `View` method to be called on the UI
thread; `View` also exposes the attached window/root relationship. [View API
reference](https://developer.android.com/reference/android/view/View.html).
The main `Looper` is the process UI event loop. [Looper API
reference](https://developer.android.com/reference/android/os/Looper.html).

**Decision.** Provider discovery, window enumeration, field reads, geometry,
and traversal run as one bounded main-thread capture transaction. Serialization,
validation, reference assignment, byte measurement, and retention run after
the provider has returned detached data. Never retain a `View`, `Activity`,
`Window`, `Context`, `Drawable`, or `CharSequence` object in a snapshot.

### Activity, window, and source selection

**Documented.** An Activity is visible from `onStart` to `onStop` and in the
foreground/interacting state from `onResume` to `onPause`; it can be destroyed
and recreated. [Activity API reference](https://developer.android.com/reference/android/app/Activity.html),
[activity lifecycle guide](https://developer.android.com/guide/components/activities/activity-lifecycle).
`WindowManager` instances are bound to a `Display`, and an app can have
multiple visible activity stacks/windows, including multi-window and
multi-resume. [WindowManager API](https://developer.android.com/reference/android/view/WindowManager.html),
[multi-window guide](https://developer.android.com/develop/ui/views/layout/support-multi-window-mode),
[tasks and back stack](https://developer.android.com/guide/components/activities/tasks-and-back-stack).

**Decision.** The app integration supplies an ordered, current set of
app-owned `Window` roots (normally each resumed/visible Activity's decor view,
plus explicitly registered app windows). Each root is a separate provider
source with one depth-zero node. Exclude system-owned windows, other apps,
IME/permission surfaces, and unattached roots. A capture with no eligible
root fails as unavailable rather than inventing a synthetic root. Source order
must be deterministic (registration/discovery order captured by the app
adapter); no cross-snapshot source identity is promised.

Each supplied root also records its owning Display identity. The provider
captures every eligible root the App explicitly supplies, including roots on a
secondary Display, but it performs no system-wide Display/window enumeration.
That keeps external-display ownership with the opted-in App rather than
guessing at other windows.

### Traversal, native order, and coordinates

**Documented.** `ViewGroup` exposes indexed children (`getChildAt`), and its
native drawing order is normally child index order but may be overridden by
`getChildDrawingOrder`. [ViewGroup API reference](https://developer.android.com/reference/android/view/ViewGroup.html).
View geometry is expressed in integer pixels; APIs such as
`getLocationOnScreen`/`getGlobalVisibleRect` report screen/global placement.
[View geometry API](https://developer.android.com/reference/android/view/View.html).

**Decision (with one explicit inference).** Traverse each root iteratively in
depth-first pre-order, preserving `ViewGroup` child indices as `childIndex`;
do not silently replace native adjacency with visual z-order. Record the
provider's native class/type, parent, depth, child count, and source-space
rectangle. Use screen-space physical pixels (scale 1), consistent with ADR
0002; bounds are read only while attached. Because custom drawing order and
transform/scroll behavior can make “native order” differ from pixels, report
the native order and do not claim pixel occlusion. **Inference:** an adapter
may expose drawing-order metadata later, but it must not reorder v1.1 nodes.
`SurfaceView` and `TextureView` nodes remain ordinary View-tree nodes with
their structural geometry; their independently rendered surface pixels are not
View-provider data and remain a screenshot-capability concern.

### Fields, disclosure, and redaction

**Documented.** `View` exposes structural state (visibility, enabled state,
clickability, bounds, class/type, IDs, attachment) and user-facing fields such
as text/content description; these are distinct API properties. [View API
reference](https://developer.android.com/reference/android/view/View.html).

**Decision.** Structural disclosure is default: class/type, geometry,
visibility, enabled/interactivity signals, child adjacency, and provider-native
numeric/boolean flags. Developer identifiers and user content require an
explicit App Disclosure Policy. Text, content descriptions, input values,
and arbitrary reflection/Kotlin fields/debug descriptions are omitted or
redacted unless individually allowed. Android `View.getTag()` returns arbitrary
Object data, unlike iOS's integer `UIView.tag`; it is Developer Metadata, is
absent from Structural Data by default, and may only cross the boundary through
an explicitly typed App policy projection. Secure/password fields are always
Secret Content. Classification and redaction happen in the Android provider
before detached data enters the core store; there is no unredacted bypass.
Unknown or non-finite native values fail closed.

### Detachment, retention, eviction, cancellation

**Repository decision.** ADR 0003 requires a complete redacted provider
capture, immutable snapshot storage, atomic commit, snapshot-scoped opaque
references, FIFO eviction bounded by count and bytes, and no store change on
provider failure, validation failure, cancellation before commit, or capacity
failure. ADR 0002 defines `ui.snapshotExpired`, cursor binding, and bounded
disclosure. These rules apply unchanged to Android.

**Android-specific inference.** Android has no framework API that freezes a
`View` tree for later inspection. Therefore the provider must copy every
needed scalar/list/object into value data during the main-thread transaction;
resolution and pagination never revisit live views. Cancellation checks occur
before and after capture and before commit; a late result is discarded. A
configuration change, Activity destruction, window detachment, process death,
or policy tightening invalidates affected snapshots/cursors.

### Lifecycle and failure boundary

**Documented.** Views can be attached/detached from a window; Activity
visibility and foreground state change independently, and `onDestroy` may be
caused by finish or configuration change. [Activity lifecycle](https://developer.android.com/guide/components/activities/activity-lifecycle),
[View API](https://developer.android.com/reference/android/view/View.html).

**Decision.** An action-eligible Foreground Target means a resumed and
interactive Activity (`onResume` through `onPause`), not merely an
`onStart`-to-`onStop` visible Activity. Capture only when that Target is
foreground and the root remains attached for the whole transaction. Re-check
attachment/root identity before commit; if it changes, fail the capture and
leave the store untouched. The provider does not observe or serialize system
UI. Backgrounding invalidates interactive sessions per CONTEXT; a fresh
foreground capture is required.

### Emulator and physical device

**Documented.** The same Android framework `Activity`/`Window`/`View` APIs are
used by apps on emulator and hardware; emulator/device configuration affects
display, density, cutouts, GPU, and API level. [Run apps on the emulator](https://developer.android.com/studio/run/emulator),
[support different pixel densities](https://developer.android.com/training/multiscreen/screendensities).

**Inference and boundary.** In-process hierarchy semantics should therefore be
identical across Emulator and physical devices for a fixed API/configuration,
but geometry, timing, custom rendering, and SurfaceView/TextureView pixels can
differ. The MVP provider guarantees native View data, not complete pixels or
system surfaces; screenshot capture remains a separate capability.

### Difference from iOS

Android's provider roots are app-supplied `Window`/decor hierarchies and use
physical-pixel geometry; Android permits multiple displays/windows and
multi-resume. iOS ADR 0004 discovers `UIWindowScene` windows, preserves scene
and window ordering, and reports screen-space points plus display scale.
Both providers share detached redacted captures, provider-native fields,
main-thread ownership, opaque snapshot references, atomic commit, and bounded
FIFO retention. Neither platform promises cross-snapshot node identity or
pixel occlusion analysis.

## Smallest unresolved probes

Only these throwaway probes remain before implementation:

1. A deterministic app fixture with two Activities/windows, one custom
   `ViewGroup` drawing order override, scrolling/translation, and a detached
   child: record root/source ordering, child indices, attachment transitions,
   and screen rectangles on one Emulator and one physical device.
2. A redaction fixture containing ordinary text, `contentDescription`,
   `EditText`/password input, resource IDs, and app tags: verify structural
   default output, policy-gated identifiers/content, and fail-closed behavior.
3. A lifecycle race fixture that destroys/recreates an Activity or detaches a
   root during capture: verify cancellation/invalidity leaves the retained
   store unchanged and returns a fresh-capture recovery.

No probe should decide protocol shape, add a production dependency, or use
AccessibilityService/ADB as the provider.
