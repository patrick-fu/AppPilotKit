# ADR 0007: Production session topology and ownership

- Status: Accepted
- Date: 2026-08-28
- Issue: [#31](https://github.com/patrick-fu/AppPilotKit/issues/31)

## Context

The accepted protocol, provider, image, action, transport, bootstrap, and CLI
decisions need one durable production dependency boundary. Without it, the
Host CLI could absorb Target-native policy, the SDKs could acquire Host tool
and Artifact responsibilities, or disposable bootstrap probes could become a
public compatibility contract accidentally.

Issue #24 selected an on-demand, current-user Host-local Session Broker and
narrow private platform adapters. Issue #38 rejected secrets in argv and Apple
launch environments, accepted encrypted in-memory Apple bootstrap plus
ADB-forwarded in-memory Android provisioning, and retained one waived physical
Apple canary as a residual risk. Issue #28 fixed the installed no-Skill CLI
workflow without fixing production module interfaces.

## Decision

### Ownership seams

| Concern | Owner | Boundary |
| --- | --- | --- |
| Wire schemas, compatibility rules, semantic fixtures, and JSON-RPC envelope meaning | `protocol/` | The checked-in contract is authoritative. It owns no platform collection, transport, Artifact, or CLI behavior. |
| Request parsing, negotiated dispatch, session registry, and request limits | Each SDK protocol runtime | It applies the selected protocol contract only after transport proof. It does not discover devices or persist Artifacts. |
| Request orchestration, response validation, correlation, and cursor forwarding | CLI protocol client | It consumes the selected protocol contract without reinterpreting provider fields, retry safety, or Target policy. |
| Candidate discovery, explicit selection, and Host tool feature probing | CLI Host adapters | Discovery yields candidates, never an implicit foreground Target. Platform-tool output is adapter input, not product schema. |
| Bootstrap orchestration, private IPC, Target Leases, cleanup ledger/orchestration, and Host subprocess lifetime | Host-local Session Broker | The Broker owns one exact Target process and Listener Epoch per lease. It is current-user, on-demand, and memory-only. |
| `simctl`, `devicectl`, direct usbmux, ADB forwarding, and exact cleanup primitives | Narrow CLI Host transport adapters | These remain private compatibility seams that yield one Target-bound candidate byte stream and execute only the Broker-requested cleanup for their own resources. |
| Pre-protocol proof, message framing, byte limits, EOF, and deadlines | Paired CLI/SDK transport layer | It converts a selected stream into complete authenticated messages without interpreting JSON-RPC methods or provider payloads. Exact framing and transcript are specification-owned. |
| Listener eligibility, binding an authenticated channel to one process and epoch, lifecycle invalidation, and Release exclusion | Each mobile SDK integration boundary | Debug/Internal integrations accept only the selected channel. Release artifacts expose no active diagnostic surface. |
| Native capture, classification, redaction, provider-native fields, and App Surface truth | Platform SDK providers | Generic runtimes and the CLI cannot reconstruct native semantics or bypass provider-owned redaction. |
| Snapshot retention, opaque references, projection, query, cursors, and bounds | SDK snapshot runtime | It retains only detached, redacted Target Ephemeral Data. Providers expose neither live objects nor provider-local identities. |
| Authoritative App-Surface image acquisition and Screenshot Masks | Platform SDK image provider | Only SDK-masked bytes may leave the Target. Host display captures are discard-only diagnostics, never Image Evidence. |
| Artifact publication, hashing, conflicts, retention, cleanup, crops, and annotations | CLI Artifact subsystem | It persists already-redacted bytes in Host-local Artifact Workspaces and never writes Target data or credentials to a device. |
| Effective Action Policy, backend choice, Single-Writer enforcement, dispatch, stability, evidence, and ambiguity | SDK Target Action Coordinator | The CLI cannot bypass policy, invoke platform controls directly, or choose a hidden fallback backend. |
| Commands, Machine Results, exit status, Next Actions, and rendering | CLI command and rendering boundary | ADR 0006 remains authoritative. Renderers receive typed redacted outcomes and never inspect credentials or live native state. |

The dependency direction is Host orchestration to private platform adapter to
authenticated stream to SDK protocol runtime to provider-owned behavior. The
shared protocol describes messages and compatibility, not platform mechanisms.
The CLI owns Host persistence; SDKs own Target truth and Target mutation.

### Lifecycle and credential invariants

1. Discovery returns only candidates. The Broker creates a Target Lease for one
   selected Target process and Listener Epoch only after establishing its
   platform stream and completing pre-protocol proof. An Agent can address that
   Target only after opening a Protocol Session.
2. The Broker owns the Target Lease and its exact forward, tunnel, descriptor,
   and subprocess cleanup ledger. It retains only opaque session and snapshot
   references plus routing state; snapshot bytes remain in SDK Target memory.
   A CLI invocation is a short-lived Broker client and never owns or persists
   bootstrap material.
3. Apple launch metadata carries only Bootstrap Public Material. A one-time
   public key protects encrypted in-memory delivery of the Process Bootstrap
   Secret. `SIMCTL_CHILD_*`, `DEVICECTL_CHILD_*`, argv, preferences, and disk
   are prohibited secret channels.
4. Android Activity extras carry only non-secret lease and socket material.
   The Process Bootstrap Secret is provisioned once over the exact selected
   ADB-forwarded stream. A token in Activity extras or argv is prohibited.
5. Foreground ineligibility closes listeners, connections, challenges, and
   Protocol Sessions and advances the Listener Epoch. Foreground return needs
   fresh proof and `session.open`.
6. Process restart changes process generation. Broker crash, lease expiry,
   endpoint loss, stale epoch, detach, or authorization loss fails closed and
   requires explicit rediscovery or rebootstrap; prior credentials and sessions
   are never restored from disk.
7. Protocol Sessions and their key material are independent per Agent. Reads
   may be concurrent across sessions and Targets; mutations remain serialized
   per Single-Writer Target, never globally.
8. SDKs retain snapshots, redacted UI content, masked image bytes, and action
   evidence only as Target Ephemeral Data. The CLI alone publishes Host-local
   Sensitive Artifacts and reports their retention and cleanup state.

### Forbidden crossings

- The CLI, Broker, and transport adapters do not traverse native UI trees,
  classify or redact provider payloads, invoke platform controls directly, or
  invent screenshot or action fallbacks.
- SDKs do not discover Hosts or devices, invoke Xcode or ADB tools, create Host
  Artifacts, persist Target data, or infer CLI command and rendering policy.
- `protocol/` does not own transport adapters, key storage, UI providers,
  Artifact files, or a normalized cross-platform native tree.
- Device pairing, ADB authorization, serials, UDIDs, ports, forwards, usbmux
  descriptors, Target Leases, and session IDs are not Session Credentials.
- Product code never writes bootstrap or session secrets to argv, environment
  variables, preferences, Keychain, disk, product logs or diagnostic bundles,
  Machine Results, Next Actions, or Artifacts.
- The product has no LAN listener, mDNS or port scan, fixed port, persistent
  device service, `iproxy` fallback, `adb reverse`, or global current Target.
- Host display screenshots without SDK masking and window correlation are not
  Image Evidence and cannot be persisted or used for crops or annotations.
- A mutation that may have crossed backend dispatch is
  `action.outcomeUnknown`; no component queues, replays, or relabels it safe.

### Specification-owned details

Later specifications may choose exact framing, handshake transcript,
cryptographic algorithms, deadlines, limits, TTLs, private IPC layout, OS peer
credential checks, adapter interfaces, module and package names, and public
command spelling. They also own screenshot and action protocol fields,
transforms, backend identifiers, stability algorithms, lifecycle compatibility
details, and release-verification implementation.

Those choices must preserve this ownership and dependency direction. Any new
public protocol or CLI behavior remains versioned under ADR 0001 and ADR 0006;
a missing capability fails closed instead of triggering an optimistic fallback.

## Consequences

- Provider-native truth, redaction, image masking, and mutation policy stay on
  the Target, where their semantics are known.
- Device discovery, tool compatibility, tunnels, subprocesses, and Artifact
  persistence stay on the Host, where their lifecycle can be cleaned exactly.
- Disposable #38 probe framing and class names are evidence, not production
  interfaces or protocol commitments.
- The waived wired-iPhone launch-environment canary remains a documented risk,
  but no production design may add an environment-secret fallback.
- The #38 probes do not prove zeroization from managed memory, swap,
  hibernation, or OS crash diagnostics, and their diagnostic collection was
  incomplete. Production must minimize secret lifetime and copies; any leak in
  a product-controlled observable surface remains a failure.

## Rejected alternatives

- **Put discovery and transport in each SDK:** makes Targets invoke Host tools
  and duplicates platform orchestration inside opted-in Apps.
- **Put native collection, masking, or action dispatch in the CLI:** loses
  provider truth and crosses the pre-serialization disclosure boundary.
- **Make the Broker a persistent server or credential store:** expands attack
  surface and contradicts the accepted current-user, memory-only lifecycle.
- **Promote bootstrap probe framing directly:** turns disposable evidence into
  an unreviewed compatibility and security commitment.
