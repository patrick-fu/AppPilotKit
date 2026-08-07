# Cross-platform action backend options

- Status: research recommendation, not an accepted implementation or protocol decision
- Date: 2026-08-08
- Issue: [#26 — Research iOS and Android action backend options](https://github.com/patrick-fu/AppPilotKit/issues/26)
- Scope: MVP App Surface actions on Simulator, Emulator, and physical devices; no production action, protocol, or automation implementation

## Recommendation

Make actions Target-owned and backend-explicit. An Action Intent binds one
snapshot generation, one source/node or App-registered Semantic Action, one
selected backend, and one Effective Action Policy. The Target Action
Coordinator validates those inputs, captures bounded evidence, performs at most
one mutation, and reports the actual backend and acknowledgement model.

Required preference order:

1. App-registered Semantic Action.
2. Provider-native semantic control or accessibility action.
3. Semantic text set or insert.
4. Explicitly selected, policy-permitted raw gesture.

There is no silent fallback. A backend acknowledgement reports dispatch or
backend completion, not a business outcome. A timeout, disconnect, or lost
acknowledgement after dispatch is action.outcomeUnknown, never a retry.

### Decision boundary

This document recommends backend ownership, selection, fidelity disclosure,
evidence, and smallest probes. It does not define a protocol minor, action
schemas, production SDK code, CLI commands, gesture injection, WebView bridge,
or third-party dependency.

## Backend matrix

| Platform | Backend family | MVP position | Boundary |
| --- | --- | --- | --- |
| Both | App-registered Semantic Action | required | App-defined operation with explicit policy and completion |
| iOS | UIKit control or App adapter semantic action | required when exposed by provider | invokes a selected control or adapter, not fabricated touch |
| iOS | UIKeyInput or UITextInput text semantics | required candidate | set/insert semantics; real keyboard optional |
| iOS | raw synthetic touch | probe-only | no public embedded-SDK injection seam is selected |
| Android | View semantic or accessibility actions | required when exposed by provider | click, long-click, scroll, or text semantics |
| Android | App-owned text adapter | required candidate | replacement or insert semantics; IME optional |
| Android | raw MotionEvent or host automation | probe-only or excluded | never silently targets the current device or System Surface |
| Both | WebView bridge | separate optional family | App-registered and origin/frame-bounded |

## iOS evidence

UIKit exposes semantic control dispatch. UIControl.sendActions(for:) sends
registered target-actions, while UIApplication.sendAction reports whether a
receiver handled a selector. These are useful only behind an App-selected
adapter: neither proves navigation, network, or domain completion. [Apple:
UIControl sendActions](https://developer.apple.com/documentation/uikit/uicontrol/sendactions%28for%3A%29);
[Apple: UIApplication sendAction](https://developer.apple.com/documentation/uikit/uiapplication/sendaction%28_%3Ato%3Afrom%3Afor%3A%29)

For text, UIKeyInput.insertText and UITextInput expose native text semantics
for an App-selected first responder. Results disclose whether they are set or
insert, selection behavior, and keyboard effect; they do not claim keystroke
fidelity because text changed. [Apple: UIKeyInput](https://developer.apple.com/documentation/uikit/uikeyinput);
[Apple: insertText](https://developer.apple.com/documentation/uikit/uikeyinput/inserttext%28_%3A%29);
[Apple: UITextInput](https://developer.apple.com/documentation/uikit/uitextinput)

XCTest offers a separate UI-testing runner, for example XCUIElement.tap. It is
not an embedded SDK primitive and remains an optional research/tooling
reference. No public in-process UIKit API is selected here as a faithful
general UITouch synthesizer. AppUse may inform the probe but cannot become a
dependency. [Apple: XCUIElement tap](https://developer.apple.com/documentation/xctest/xcuielement/tap())

## Android evidence

An opted-in App can use View-owned semantic operations such as performClick,
performLongClick, and performAccessibilityAction. These are App-local
dispatches, not a promise to reproduce every physical pointer gesture.
ACTION_SET_TEXT is a semantic replacement action; an embedded SDK must use an
in-process View/App adapter rather than impersonating an AccessibilityService.
[Android: View](https://developer.android.com/reference/android/view/View);
[Android: AccessibilityNodeInfo](https://developer.android.com/reference/android/view/accessibility/AccessibilityNodeInfo)

AccessibilityNodeInfo.performAction is traditionally service-only. Android 14
adds setQueryFromAppProcessEnabled for a Debug/testing tool to query an
attached View hierarchy from the App process, making it an optional API 34+
semantic candidate, never the API 26 MVP baseline. Its action list and return
value still describe node-level handling rather than an App-level outcome.
[Android: setQueryFromAppProcessEnabled](https://developer.android.com/reference/android/view/accessibility/AccessibilityNodeInfo#setQueryFromAppProcessEnabled(android.view.View,%20boolean))

InputConnection.commitText is an IME-to-App API with focus and composition
state. It is optional rather than the MVP's only text path; the required path
is App-semantic set/insert with disclosed selection semantics. [Android:
InputConnection](https://developer.android.com/reference/android/view/inputmethod/InputConnection)

AccessibilityService.dispatchGesture, UI Automator, Instrumentation, and ADB
can reach beyond the opted-in App Surface or require a test/service context.
They may be explicit diagnostics, never automatic MVP fallbacks; they cannot
bypass Target serialization, foreground checks, policy, or the System Surface
boundary. [Android: AccessibilityService](https://developer.android.com/reference/android/accessibilityservice/AccessibilityService);
[Android: UI Automator](https://developer.android.com/training/testing/other-components/ui-automator)

In particular, dispatchGesture is public on API 24+ but requires an
AccessibilityService with the user-enabled canPerformGestures capability. Its
completion callback means injection completed or was cancelled, not that the
Target consumed the gesture or reached a business outcome. It is a candidate
for a deliberately enabled optional backend/probe, not a baseline.

## Capability discovery and selection

The installed CLI capabilities manifest describes installed commands, not a
Target's live action backends. Target-specific selection therefore needs a
future protocol minor and named capability, as ADR 0001 requires: absent
required capability fails negotiation instead of inviting an optimistic call.
[ADR 0001](../adr/0001-protocol-envelope-and-compatibility.md)

The later action contract must report a bounded backend record for the exact
Target and snapshot: stable ID; platform/provider/source scope; intents and
fidelity; snapshot/node binding and raw coordinate space; effective policy and
authorization requirement; acknowledgement/cancellation model; text semantics,
keyboard effect, and secret_echo false; Web origin/frame scope; stability,
retry safety, and limitations.

The Agent explicitly selects a disclosed backend. The Target rejects stale
references, unavailable backends, policy conflicts, background state, and a
second in-flight mutation before dispatch. A raw coordinate backend requires an
explicit choice and source-specific coordinate policy.

## Keyboard and WebView boundaries

The system keyboard may participate in App Surface input but stays a System
Surface with no MVP hierarchy/action guarantee. Set and insert must state their
replacement/cursor behavior instead of simulating keys. Secret text may be
ephemeral action input only; it cannot enter snapshots, errors, audit data,
diagnostics, or Image Evidence. [CONTEXT.md](../../CONTEXT.md)

WKWebView.evaluateJavaScript and Android WebView.evaluateJavascript are
asynchronous script facilities: a callback proves script completion/error, not
a DOM or business outcome. WebView is App Surface but its DOM/CSS coordinates
are distinct from native provider coordinates. Keep it as an optional separate
action.web family: only an App-registered, origin/frame-bounded bridge may use
opaque DOM references. Arbitrary selectors and JavaScript remain unavailable by
default. [Apple: WKWebView](https://developer.apple.com/documentation/webkit/wkwebview);
[Android: WebView](https://developer.android.com/reference/android/webkit/WebView)

## Evidence, stability, and ambiguity

Action Evidence is bounded before/after snapshots and, when policy permits,
masked Image Evidence. Minimum evidence is the validated pre-dispatch snapshot,
chosen backend and acknowledgement model, stability result, and post-dispatch
snapshot. If after-evidence fails after a known dispatch, report known execution
with incomplete evidence; do not rewrite it as non-execution. [CONTEXT.md](../../CONTEXT.md);
[ADR 0002](../adr/0002-ui-snapshot-and-inspection.md)

Neither UIKit layout/transactions nor Android frame callbacks promise global
App quiescence. Define stability as a bounded, provider-declared observation:
foreground state plus at least two consecutive UI frames with unchanged
redacted structural digest and no declared pending transition. A deadline yields
timed_out, not automatic action failure or retry permission. [Apple:
CATransaction](https://developer.apple.com/documentation/quartzcore/catransaction);
[Android: Choreographer](https://developer.android.com/reference/android/view/Choreographer)

Cancellation is safe only when the coordinator proves did_not_execute before
backend handoff. Once handed off, a missing acknowledgement, disconnect, or
timeout is ambiguous. A callback or boolean alone is not Retry Proof. [ADR
0006](../adr/0006-agent-facing-cli-contract.md); [CONTEXT.md](../../CONTEXT.md)

## Concurrency and isolation

The Action Coordinator lives in every Target, not behind a global Host/CLI
lock. It holds the Target Single-Writer span across reference/policy validation,
before-evidence, dispatch, stability, and after-evidence. Read-only sessions
remain concurrent; a competing mutation fails explicitly and is never queued or
replayed. Independent Targets must not share a current-device selector, gesture
singleton, ADB selector, or session key. [CONTEXT.md](../../CONTEXT.md)

## Smallest throwaway probes

1. iOS semantic controls and text: on Simulator and a physical iPhone, compare
   App adapter, UIControl dispatch, and UIKeyInput set/insert; record only
   acknowledgement, selection result, foreground state, and redacted digest.
2. iOS raw-gesture boundary: use a toy control with tap, long-press, swipe, and
   scroll recognizers; determine whether a candidate needs a test runner/private
   seam or loses native semantics. Do not retain it as an MVP dependency.
3. Android semantic actions and text: on Emulator and physical device, exercise
   click, long-click, scroll, and in-process set/insert against a toy View,
   including focus and keyboard-visible cases.
4. Android host/global exclusion: demonstrate that ADB/UI Automator/Accessibility
   Service can address non-App or System Surface state, then verify the Target
   never advertises them as automatic fallbacks.
5. WebView: distinguish native coordinate tap, accessibility exposure, and
   registered origin-bounded DOM action; arbitrary JavaScript stays undisclosed.
6. Ambiguity and concurrency: drop the Host response after handoff and issue a
   second mutation. Verify outcomeUnknown, no replay Next Action, explicit
   Single-Writer conflict, and concurrent action on another Target.
7. Stability/evidence: trigger controlled animation and after-capture failure.
   Verify bounded reached/timed_out reporting and known dispatch preservation.

These are throwaway acceptance-host probes for #30, not authorization to
implement production action backends.

## Residual risks

- iOS has no selected public embedded raw-touch backend. The MVP needs enough
  semantic/App-adapter coverage; the raw-gesture probe decides whether a narrow
  optional seam is warranted.
- Android semantic actions can report handled without proving App-level outcome,
  so every mutation backend retains ambiguity handling.
- WebView DOM semantics, real IME behavior, and global test automation stay
  optional capability families, not cross-platform guarantees.
- Exact action protocol fields and schema versioning remain intentionally
  deferred to the ownership-boundary ticket after the probes report.
