# Internal dogfood MVP acceptance gates

- Status: Accepted
- Date: 2026-08-28
- Issue: [#32](https://github.com/patrick-fu/AppPilotKit/issues/32)

These gates decide whether AppPilotKit's internally dogfoodable MVP is
complete. They do not report an acceptance run, authorize implementation, or
turn research and prototype evidence into a compatibility claim.

Every blocking result records the Host model and architecture, macOS, Xcode,
Android Platform Tools, device model, device OS build, Acceptance Host build,
protocol and CLI versions, and Demo Scenario Contract revision. A skipped,
flaky, manually asserted, or privately self-reported result fails the gate.

All build, compile, and test commands run with one worker/job. The explicit
runtime concurrency Journey exercises product isolation only; it does not
permit concurrent build, compile, or test jobs.

## Compatibility matrix

The supported MVP Host is an Apple Silicon Mac. The supported Host OS range is
macOS 15.6 through macOS 26. Native `x86_64-apple-darwin` CLI build, contract,
signing, and offline discovery checks remain non-blocking portability evidence;
Intel is not an internally dogfoodable MVP Host.

The Target ranges remain iOS 15 through iOS 26 and Android API 26 through API
37. “Current” means the latest generally available patch inside the accepted
range at the acceptance run. A newer Android release remains best effort until
verified.

| Host lane | Required Target rows | Required coverage |
| --- | --- | --- |
| Apple Silicon, macOS 15.6 latest patch, latest compatible Xcode 26 | iOS 15 iPhone Simulator; wired iPhone on the newest iOS 15–26 patch supported by that Xcode; Android API 26 phone Emulator; current wired Android phone | Journeys 1–3 and 6 on every row; Journey 4 on both physical rows; offline CLI and Release checks |
| Apple Silicon, current macOS 26 and current stable Xcode 26 | iOS 26 iPhone Simulator; iOS 26 iPad Simulator in landscape; current wired iPhone; Android API 37 phone Emulator; Android API 37 tablet Emulator in landscape; current wired Android phone | Journeys 1–3 and 6 on every row; Journey 4 on physical, phone Simulator, and phone Emulator rows; Journey 5 once; performance, security, Release, and no-Skill gates |

Minimum and current endpoints are the mandatory OS representatives. An
intermediate release becomes an additional blocking row when an implementation
uses version-specific behavior or a regression is found there. iPhone, iPad,
Android phone and tablet, portrait and landscape are covered by the rows above;
the matrix does not require an obsolete physical device for every OS version.

Every iOS row covers UIKit View and SwiftUI Semantic Projection. Every Android
row covers Android View and Compose Semantic Projection. Provider availability
is negotiated explicitly. A missing provider returns
`capabilityUnavailable`; it never falls back silently to a hosting view,
screenshot, private API, XCTest/UI automation, ADB control, or System Surface
path.

## Required public Journeys

The repository-owned Acceptance Hosts and the resettable scenarios from #21
are the only acceptance fixtures. Verdicts use public SDK, Protocol, CLI,
Machine Result, and Artifact evidence.

1. **Inspect and evidence.** Reset `demo.inspection`; capture compact and full
   inspection; force both item and byte cursor continuation; cover identifier,
   type/class, trait, visibility, interactivity, geometry, source, `withinRef`,
   ancestor, descendant, sibling, and text queries; then produce an original
   image, reference crop, geometry crop, and separate annotation. The 40-row
   fixture, source identity, truncation, cursor behavior, stable identifiers,
   and wrong-snapshot reference rejection must be observable.
2. **Provider and action coverage.** Complete `demo.mixed-providers` from
   `counter=0` to `counter=11` without source deduplication, then complete tap,
   long-press, swipe, scroll, and semantic set/insert text in `demo.actions`.
   Every action exposes backend and fidelity, snapshot binding, Effective
   Action Policy, dispatch knowledge, stability, and bounded before/after
   evidence. Real IME input and Android Back remain optional capabilities.
3. **Disclosure, Artifact, and safety.** Complete every structural, Developer
   Metadata, User Content, Secret Content, policy-tightening, denied-expansion,
   fail-closed-redaction, Screenshot Mask, and protected-content branch in
   `demo.disclosure`. The two Secret canaries from #21 occur zero times in
   protocol bytes, stdout, stderr, JSON, JSONL, product diagnostics, Machine
   Results, product-controlled process arguments/environment, and Artifacts.
   Failed classification or redaction creates no partial snapshot or Artifact.
4. **Guarded mutation and lifecycle.** On every physical row and the current
   phone Simulator/Emulator rows, prove stale-reference rejection, pre-dispatch
   cancellation, known non-execution, never-stable behavior, post-dispatch
   acknowledgement loss, no replay after `action.outcomeUnknown`, immediate
   Single-Writer conflict without queueing, background invalidation, foreground
   reconnect, process-generation change after restart, and concurrent reads.
5. **Concurrent Targets.** On one Apple Silicon Host, release the five
   `demo.concurrent-targets` mutations (`ios-1`, `ios-2`, `ios-3`, `android-1`,
   `android-2`) from their ready barrier. At least one physical iOS Target and
   one physical Android Target participate. Every counter reaches `1` without
   crossing Target identity, session key, snapshot, state, result, or Artifact
   Workspace. The fixed current-Host composition is: wired iPhone, iOS 26
   iPhone Simulator, iOS 26 iPad Simulator, wired Android phone, and Android
   API 37 phone Emulator.
6. **Protocol and CLI compatibility.** An incompatible major, unavailable
   required capability, malformed/modified cursor, expired session/snapshot,
   and unknown required command/schema capability fail before side effects
   with the established error kind and exit category. ADR 0001 and ADR 0006
   compatibility and strict-schema rules remain authoritative.

## Performance and bounded resources

Measure the standard Acceptance Host fixture after five warm-up runs and 30
measured runs per operation. Both p95 and maximum must satisfy the table. A
correct bounded failure does not count as a happy-path latency pass.

| Operation | p95 | Maximum |
| --- | ---: | ---: |
| Discover and select one known Target | 3 s | 5 s |
| Authenticate and complete `session.open` | 2 s | 4 s |
| Compact snapshot or targeted query | 1.5 s | 3 s |
| First full-inspection page or continuation | 2 s | 4 s |
| Persist one original masked screenshot Artifact | 3 s | 6 s |
| Derive one eligible crop or annotation | 1 s | 2 s |
| Complete one non-destructive action on stable UI | 3 s | 6 s |
| Clean up after pre-dispatch cancellation | 2 s | 4 s |

The standard fixture also satisfies all of these limits:

- Compact Machine Result data is at most `64 KiB`; one full inspection page is
  at most `1 MiB`; no successful protocol response exceeds negotiated
  `maxBytes`.
- One session retains at most eight snapshots and `16 MiB` of detached snapshot
  data. Eviction is explicit; old references and cursors fail rather than
  retargeting.
- One standard original Image Evidence Artifact is at most `20 MiB`. Machine
  Results contain its descriptor, never image bytes.
- After 100 capture/query/action-evidence cycles and 60 seconds idle, one Target
  process is at most `64 MiB` above its pre-run resident-memory baseline and
  grows by at most `8 MiB` between the final two 20-cycle windows.
- With one connected idle Target, the Host Broker is at most `32 MiB` above its
  pre-Target resident-memory baseline and grows by at most `4 MiB` between the
  final two 20-command windows.
- Artifact publication is atomic and no-clobber. Cancellation before
  publication leaves no final or partial Artifact. A conflict returns
  `artifact.alreadyExists` with exit `7`; replacement requires one exact
  explicitly authorized destination.
- A test-clock run proves default 24-hour retention and explicit cleanup.
  Cleanup failure remains failed and reports the exact remaining sensitive
  path.

The existing snapshot-store and protocol limits are inputs, not end-to-end
performance evidence. Rust foundation spike measurements do not pass these
gates.

## Converted image, action, and provider probes

The unverified mechanism probes named by #29, #30, and #34 are blocking public
acceptance gates, not additional pre-development prototypes.

- On iOS 15 and iOS 26, secure-text and declared-mask fixtures yield a fully
  opaque mask region and zero Secret-canary disclosure. Point-to-pixel crops
  match expected dimensions and markers within one physical pixel. When a
  mapping cannot be established, crops and annotations are explicitly
  unavailable and create no derived Artifact.
- The wide-gamut fixture renders fixed Display-P3 patches plus sRGB controls.
  MVP Image Evidence is converted to sRGB before leaving the Target; its
  descriptor declares sRGB, and the encoded file contains a recognized sRGB
  ICC profile or PNG sRGB chunk. Color-managed decoding must match the expected
  conversion with CIEDE2000 delta E at most `2.0` for every patch. The MVP does
  not claim wide-gamut preservation.
- On Android Emulator and physical device, `SurfaceView` PixelCopy contains the
  declared marker; `FLAG_SECURE` capture is explicitly unavailable with no
  Artifact; diagnostic `screencap` bytes are discarded; portrait/landscape
  source-to-bitmap markers map within one physical pixel or derived images are
  unavailable.
- iOS Simulator and wired iPhone plus Android Emulator and wired phone prove
  semantic controls and text. Every core Action Intent has an explicit allowed
  Acceptance Host backend. An unavailable public raw iOS gesture backend is not
  advertised; the declared App/semantic path still completes the core fixture.
- Android Host/global automation is never an automatic fallback. WebView has no
  arbitrary DOM or JavaScript capability; undeclared behavior is explicitly
  unavailable.
- Ambiguity, concurrency, and stability/evidence fixtures prove
  `action.outcomeUnknown`, exit `5`, `retryable: false`, no replay Next Action,
  explicit writer conflict, and separate dispatch, stability, and after-evidence
  facts.
- SwiftUI on the minimum and current iOS rows proves state replacement,
  detach/lifecycle behavior, declared named actions, redaction, geometry, and
  retained overlap with UIKit without using a private implementation tree. A
  two-window fixture proves multiple App-supplied semantic roots, active App
  Surface selection, window activation/deactivation, and stale-reference
  invalidation without merging or deduplicating roots.
- Android minimum and current rows prove App-supplied multi-Display roots,
  lifecycle invalidation, View/Compose source separation, redaction, geometry,
  and orientation changes without Host-side tree reconstruction.

A negative mechanism result passes only when the public capability is absent,
the product fails closed, and no weaker fallback is advertised. Inference,
prototype output, or a private test flag cannot waive a gate.

## Security and Release exclusion

For every iOS and Android Release build artifact:

- Static artifact/manifest inspection and runtime launch prove there is no
  active AppPilotKit Target bootstrap surface, listener, credential parser,
  debug Activity or manifest entry, local socket, diagnostic endpoint, or
  production fallback. The App is undiscoverable as a Target and cannot open a
  Protocol Session.
- Debug/Internal builds expose only the authenticated loopback or trusted-tunnel
  path. LAN listeners, mDNS, generic background services, persisted credentials,
  and argv/environment secret fallbacks are forbidden.
- Wrong, expired, replayed, cross-Target, or stale proof plus Broker crash/TTL,
  detach, pairing/authorization loss, and stale forwards fail closed and require
  explicit rediscovery or rebootstrap. Mutations are never silently replayed.
- Current-user Artifact directories resist symlink traversal and contain no
  Secret Content. Errors, product diagnostics, Next Actions, and descriptors
  expose only Safe Error Context.
- The #38 wired-iPhone `DEVICECTL_CHILD_*` exact-secret canary remains waived
  because environment-secret bootstrap is prohibited, not because it was
  proven safe. Production retains encrypted in-memory Apple bootstrap and has
  no environment-secret fallback.

## No-Skill black-box evaluator

Run three independent fresh evaluations. Each evaluator receives only the
installed `apppilotkit` binary, available opted-in App/device, and one goal
card. It has no AppPilotKit Skill, MCP adapter, source checkout, repository
documentation, prompt example, saved credential, or prior conversation.

Each evaluator completes the ten cases in
`docs/research/agent-friendly-cli-2026.md`: happy path; deterministic
multi-device selection; App absent; unavailable/expired authentication;
incompatible protocol; ambiguous post-dispatch timeout; interrupt and recovery;
existing Artifact destination; malformed/missing-terminal JSONL rejection; and
redirected stdin without a human.

Passing requires 30/30 completed cases and all of the following:

- zero malformed machine-output lines, missing terminal results, Secret
  disclosures, speculative destructive actions, or unsafe mutation replays;
- JSON/JSONL stdout parses without filtering, stderr is not needed to reconstruct
  a Machine Result, and exit status agrees with terminal status;
- installed help, `capabilities`, `schema list/show`, and non-interactive
  `doctor` are sufficient for discovery;
- every failure offers correct safe `next_actions[].argv` recovery without
  prose parsing;
- the happy path completes within 12 CLI invocations, four discovery/help
  round trips, one invalid invocation, and ten minutes; and
- Artifact path, SHA-256, size, media type, and sensitivity are verified without
  putting image payloads in evaluator context.

Record command count, failed invocations, help rounds, elapsed time, output
bytes, malformed output, unsafe retries, and completion for every run. Future
CLI versions cannot regress a zero-tolerance criterion or happy-path limit.

## Deferrals and residual risk

The following do not block the MVP after every gate above passes: Intel,
Linux, and Windows Hosts; System Surface and cross-App automation; a standalone
accessibility-tree provider; WebView DOM, React Native, and Flutter providers;
logs, network, database, filesystem, and arbitrary App-state inspection; public
packaging and licensing commitments; guaranteed real IME input; Android global
Back; and wide-gamut preservation beyond accurately labelled output.

Accepted residual risks are the waived physical `DEVICECTL_CHILD_*` canary;
managed-memory, swap, hibernation, and OS crash-diagnostic zeroization not being
proven; no Host screenshot fallback on physical iOS; compositor/hardware image
behavior outside the matrix; and newer Android releases remaining best effort.
None permits weaker disclosure, Release exclusion, authentication, or ambiguous
mutation behavior.

## Go/no-go

The MVP is acceptable only when every blocking matrix row, Journey, budget,
converted probe, security/Release check, and no-Skill evaluation passes with
retained public evidence. A failure remains no-go until corrected, re-scoped by
a new recorded decision, or replaced by an equally measurable fail-closed gate.
