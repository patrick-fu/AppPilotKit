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

**Host-local Session Broker**:
An on-demand, current-user-only Host component that coordinates selected Targets and keeps its routing and bootstrap state only in memory.
_Avoid_: Server, daemon, remote service

**Target Lease**:
A bounded-lifetime Broker record for one selected Target process and Listener Epoch, including the Host resources that must be cleaned for that relationship.
_Avoid_: Device identity, Protocol Session, credential

**Bootstrap Public Material**:
One-time non-secret data that binds a selected Target process to its bootstrap exchange and may safely cross platform launch metadata.
_Avoid_: Bootstrap token, Session Credential

**Process Bootstrap Secret**:
A process-generation-scoped Session Credential used only for pre-protocol bootstrap and delivered through the selected protected memory stream. It may authorize independently keyed Protocol Sessions but is never reused as a Protocol Session key or action credential.
_Avoid_: Environment token, saved credential

**Listener Epoch**:
The current eligible incarnation of a Target listener. Losing foreground eligibility invalidates the epoch and its interactive sessions without implying a new process generation.
_Avoid_: Process generation, Protocol Session

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

**Acceptance Host**:
A repository-owned iOS or Android Opted-in App that exposes deterministic Demo Scenarios through public AppPilotKit surfaces for MVP acceptance.
_Avoid_: Sample app, test harness

**Demo Scenario Contract**:
The versioned cross-platform catalog of Demo Scenario identities, logical states, fixed fixtures, provider obligations, and observable evidence. Its revision is independent of protocol and CLI contract versions.
_Avoid_: Demo API, test plan

**Demo Scenario**:
A Target-local, independently resettable logical state machine with a shared cross-platform identity and declared native differences. Its result is judged from public evidence rather than private App state.
_Avoid_: Screen, test case

**Scenario Seed**:
The deterministic initial state restored for one Demo Scenario by cold launch or reset without affecting another Target.
_Avoid_: Fixture setup, persisted app data

**Acceptance Journey**:
A deterministic sequence over one or more Demo Scenarios and Targets whose outcome is established only by public SDK, Protocol, and CLI evidence.
_Avoid_: Scripted workflow, end-to-end test

**Fixture Canary**:
A stable synthetic value assigned to one disclosure category so its permitted presence or forbidden leakage can be verified across results and Artifacts.
_Avoid_: Test credential, real secret

**Image Evidence**:
An original App Surface screenshot or a separately generated crop or annotation derived on the Host. Every original and derived image is a distinct sensitive Artifact; annotation never mutates the original.
_Avoid_: Inline screenshot payload, modified original

**Disclosure Policy**:
The App-owned upper bound on data that providers may disclose. An Agent request may narrow this policy but can never expand it, and required redaction happens before data enters a snapshot store or Artifact.
_Avoid_: Agent permission, output filter

**Structural Data**:
UI class or type, geometry, visibility, interactivity, and traits that an authenticated Protocol Session may receive by default.
_Avoid_: User content, native payload

**Developer Metadata**:
Developer-authored identifiers such as accessibility identifiers and test tags. A provider discloses them only when the Opted-in App enables that category.
_Avoid_: Structural Data, User Content

**User Content**:
Visible text, accessibility labels, values and hints, and input content. It is denied by default and requires explicit App policy plus provider-owned redaction before storage.
_Avoid_: Developer Metadata, Secret Content

**Secret Content**:
Passwords, secure text, tokens, keys, payment information, and equivalent secrets that are never serialized under any Disclosure Policy.
_Avoid_: Redacted User Content, sensitive Artifact

**Fail-Closed Disclosure**:
A provider outcome in which any unclassified field, incomplete redaction, or invalid output fails the entire capture before snapshot or Artifact creation. Partial and best-effort disclosure are prohibited.
_Avoid_: Best-effort redaction, output scrubbing

**Target Ephemeral Data**:
Snapshots, UI content, Image Evidence, and action evidence retained only in Target memory. It is never written to device storage and is destroyed when its session or process scope expires.
_Avoid_: Device cache, persisted snapshot

**Session Credential**:
An ephemeral in-memory authentication secret scoped either to one Target process bootstrap or one Protocol Session. Process Bootstrap Secrets and Protocol Session keys are distinct and never interchangeable.
_Avoid_: API key, saved token

**Session Key Isolation**:
Each Protocol Session uses an independent key that is never reused across Agents, Targets, or concurrent sessions. A Process Bootstrap Secret may authorize session establishment but is never reused as the session key or an action credential.
_Avoid_: Shared session token, bootstrap action token

**Diagnostic Metadata**:
Non-sensitive runtime facts that exclude UI content, credential material, Artifact contents, and provider-native payloads. An explicit diagnostic bundle is still a Sensitive Artifact and can never include Secret Content.
_Avoid_: Debug dump, verbose payload log

**Safe Error Context**:
Error, diagnostic, and Next Action metadata limited to error kinds, field names, opaque references, and safe structural facts. It never echoes User Content, Secret Content, credentials, authorization grants, or typed input.
_Avoid_: Debug payload, echoed request

**Sensitive Artifact**:
A Host-local Artifact that is never embedded in machine output or exported, uploaded, synchronized, copied to the clipboard, or shared with a third party implicitly.
_Avoid_: Attachment, inline evidence

**Artifact Workspace**:
A private per-invocation Host directory whose Artifacts are not enumerated or reused by another invocation unless an absolute path is shared explicitly.
_Avoid_: Shared output folder, device artifact directory

**Artifact Retention**:
The period in which a Machine Result consumer can read an Artifact before cleanup. The MVP default is 24 hours, with an explicit policy able to shorten or extend it.
_Avoid_: Session lifetime, permanent evidence

**Artifact Conflict Policy**:
An explicit choice to fail, create a unique destination, or atomically replace one exact destination. Implicit overwrite and retained partial files are prohibited.
_Avoid_: Filename suffixing, best-effort overwrite

**Cleanup Failure**:
An explicit failure to remove a Sensitive Artifact or partial file. It reports the exact remaining absolute path and sensitivity so manual cleanup is possible; it is never silently downgraded to success.
_Avoid_: Cleanup warning, ignored temporary file

**Screenshot Mask**:
An App-declared Secret Content region that must be obscured before Image Evidence is persisted. No unmasked original Artifact is retained.
_Avoid_: Screenshot annotation, post-storage redaction

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

**Destructive Authorization**:
A one-time, short-lived grant from a user or preconfigured policy that is bound to an exact Target, action, parameters, and snapshot generation. An Agent cannot mint it, and a denied destructive action never falls back to an ordinary mutation.
_Avoid_: Confirmation flag, global approval

**Effective Action Policy**:
The single resolved safety policy for a mutation, derived from a Semantic Action declaration, node or action metadata, or an App-declared provider default. Missing, conflicting, and unclassified coordinate policies fail closed or require Destructive Authorization.
_Avoid_: Gesture safety guess, Agent-selected risk

**Ambiguous Action Outcome**:
A mutation result where execution may have occurred but acknowledgement is unavailable. It is reported as `action.outcomeUnknown`, is unsafe to repeat, and can recommend only read-only recovery.
_Avoid_: Timeout, retryable failure

**Retry Proof**:
Evidence that an equivalent mutation may be repeated: cancellation before dispatch, backend proof of `did_not_execute`, or a backend-guaranteed exact idempotency key. Timeout, disconnect, and missing acknowledgement are not Retry Proof.
_Avoid_: Retryable error, successful recovery

**Action Audit Record**:
The Machine Result for one action invocation. No global persistent action history exists by default; optional diagnostics are Sensitive Artifacts, and only a non-secret authorization grant identifier or digest may be recorded.
_Avoid_: Action log, authorization archive

**Disclosure Policy Revision**:
A change to an App Disclosure Policy. Tightening invalidates affected snapshots and cursors immediately; loosening applies only to new captures. Existing Sensitive Artifacts remain until cleanup.
_Avoid_: Snapshot update, retroactive Artifact revocation

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
