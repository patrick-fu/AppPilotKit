# AppPilotKit

AppPilotKit lets coding Agents inspect and operate opted-in mobile apps through embedded debug SDKs and a self-guiding desktop CLI while preserving platform-native diagnostic truth.

## Language

**Host**:
The macOS machine where coding Agents and the AppPilotKit desktop CLI run. The MVP supports both Apple Silicon and Intel Hosts; Linux and Windows Hosts are deferred.
_Avoid_: Target device, client

**Opted-in App**:
An App that explicitly integrates AppPilotKit in a Debug/Internal configuration. It does not need a source checkout on the Host, and a Production App can never become a Target.
_Avoid_: Test app, instrumented production app

**Target**:
A single running opted-in Debug/Internal iOS or Android app process that AppPilotKit can address independently on a Simulator, Emulator, or physical device.
_Avoid_: Device, app, endpoint

**Foreground Target**:
A Target whose App Surface is active and eligible for guaranteed inspection and actions. Backgrounding invalidates its interactive Protocol Sessions until it is reactivated and reconnected.
_Avoid_: Connected process, background automation target

**App Surface**:
UI owned by a Target. The MVP guarantees inspection, stable references, screenshots, and supported actions only within this boundary. The system keyboard may participate as an input mechanism.
_Avoid_: Screen, device UI

**System Surface**:
OS-owned or cross-App UI such as permission dialogs, Settings, Notification Center, share panels, and other Apps. It may appear in screenshot evidence but has no MVP guarantee for hierarchy inspection, stable references, or reliable actions.
_Avoid_: App Surface, external screen

**Semantic Projection**:
A provider-owned representation of public SwiftUI or Compose semantics that supports the MVP inspection and reference-driven action contract without claiming access to a framework-private implementation tree.
_Avoid_: Raw framework tree, hosting view hierarchy

**MVP UI Provider Set**:
UIKit View hierarchy, SwiftUI Semantic Projection, Android View hierarchy, and Compose Semantic Projection. Each provider remains a distinct source with its own native fields rather than being collapsed into one lossy tree.
_Avoid_: Unified UI tree, screenshot-only framework support

**Android MVP Range**:
Android 8.0 (API 26) through Android 17 (API 37), with no declared maximum SDK version. Newer Android releases remain best effort until compatibility is verified, and the Opted-in App owns its target SDK level.
_Avoid_: maxSdkVersion, Host-controlled target SDK

**MVP Inspection Surface**:
The complete ADR 0002 inspection contract: compact and explicitly full hierarchies, stable-reference and indexed queries, bounded ancestor and sibling context, provider-native fields, explicit limits and truncation, and cursor pagination.
_Avoid_: Hierarchy dump, compact-only inspection

**Image Evidence**:
An original App Surface screenshot or a separately generated crop or annotation derived on the Host. Every original and derived image is a distinct sensitive Artifact; annotation never mutates the original.
_Avoid_: Inline screenshot payload, modified original

**Action Intent**:
An Agent request such as tap, long-press, swipe, scroll, or type whose available execution backends and fidelity are disclosed separately.
_Avoid_: Synthetic touch, backend command

**Text Entry Intent**:
The type Action Intent. Semantic set or insert text is required for the MVP; real IME or keyboard input is an optional backend. Capability discovery exposes each available backend and its fidelity so the Agent can choose.
_Avoid_: Key injection, guaranteed IME typing

**Optional Back Capability**:
App-scoped Android back behavior exposed only when the Opted-in App supplies a compatible adapter or Semantic Action. Back is not a cross-platform core Action Intent, and global ADB key injection is not an acceptable fallback.
_Avoid_: System back command, adb keyevent fallback

**Action Evidence**:
The bounded before-and-after snapshots and Image Evidence associated with an Action Intent, including whether UI stability was reached and whether execution is known, cancelled, or ambiguous.
_Avoid_: Action result, retry signal

**Semantic Action**:
An App-registered domain operation with explicit policy metadata. It is never discovered through reflection, invoked implicitly, or allowed to bypass authorization and safety rules.
_Avoid_: Hidden command, reflected method

**Protocol Session**:
An authenticated, process-generation-scoped relationship between an Agent client and one Target.
_Avoid_: Target, connection

**Concurrent Target Sessions**:
Multiple Agents operating independent Targets at the same time, without a global CLI, device, or SDK singleton that serializes unrelated work.
_Avoid_: Multi-device mode, parallel commands

**Single-Writer Target**:
A Target that permits concurrent read-only Protocol Sessions but no more than one in-flight mutation. A competing mutation fails with an explicit conflict and is never queued or retried implicitly.
_Avoid_: Global mutation lock, automatic mutation queue

**Machine Discovery**:
The offline, authentication-free description of the installed CLI's commands, arguments, result schemas, errors, side effects, and recovery paths.
_Avoid_: Command dump, dynamic help

**Machine Result**:
A versioned structured terminal description of one CLI invocation, including its status, data or error, disclosure, artifacts, and recovery guidance.
_Avoid_: JSON output, response blob

**Next Action**:
A structured recommendation containing an exact argv array and the safety information an Agent needs to decide whether to invoke it.
_Avoid_: Suggested command, shell snippet

**Side-Effect Class**:
The scope of state an invocation may change, independent of whether repeating it is safe.
_Avoid_: Idempotency, retryability

**Retry Safety**:
The conditions under which an equivalent invocation may be repeated after its observed outcome, especially when execution may have occurred without acknowledgement.
_Avoid_: Side effect, retryable boolean

**Artifact**:
A file-backed, potentially sensitive output represented to an Agent by its absolute path and integrity, media, size, and sensitivity metadata.
_Avoid_: Attachment, payload file
