# Production discovery, transport, and session topology

Status: research recommendation, not an accepted implementation or protocol decision
Decision issue: [#24](https://github.com/patrick-fu/AppPilotKit/issues/24)
Evidence reused: [#1](https://github.com/patrick-fu/AppPilotKit/issues/1), including its accepted iOS Simulator, Android Emulator, Release-isolation, and wired-iPhone probe records

## Recommendation

Use one public product model—**discover a device/app candidate, explicitly
bootstrap its current process when needed, open an authenticated byte stream,
then call `session.open`**—with four narrow platform adapters. Only the
authenticated process is a Target; a device or installed-app discovery result
is not. A private Host-local Session Broker owns selection, subprocesses,
tunnel lifetime, memory-only bootstrap credentials, byte-stream deadlines, and
typed transport outcomes. The SDK owns a Debug/Internal-only listener, pre-protocol
proof, lifecycle invalidation, and the existing protocol/session contract.

| Target kind | Discovery adapter | Bootstrap / endpoint | Host-to-Target stream | Support classification |
| --- | --- | --- | --- | --- |
| iOS Simulator | `simctl list --json` | fresh random TCP port and bootstrap secret through `SIMCTL_CHILD_*`; bind `127.0.0.1` | direct host loopback TCP | Xcode CLI mechanism; parse only feature-probed JSON |
| wired iPhone | `devicectl` machine JSON | fresh process, port, and bootstrap secret through `DEVICECTL_CHILD_*`; bind device loopback | direct stream after usbmux `Connect` | launch injection is supported CLI; usbmux plist is private compatibility seam |
| Android Emulator | `adb devices -l` | Debug/Internal activity receives a fresh non-secret local-abstract name; Broker provisions its secret in memory through the exact forward | `adb forward tcp:0 localabstract:<name>` then host loopback TCP | public ADB forwarding; `localabstract` is AOSP-service/probe compatibility seam |
| wired Android | `adb devices -l` | same as Emulator | same ADB forward path | public ADB forwarding; `localabstract` is AOSP-service/probe compatibility seam |

This is a recommendation, not a claim that the four tools have one shared API.
It deliberately does **not** add a LAN path, a fixed port, a persistent service,
or a production listener. It preserves the product boundary in
[the vision](../vision.md): transport helpers do not become the public model;
the authenticated, process-generation-scoped Protocol Session is still defined
by [ADR 0001](../adr/0001-protocol-envelope-and-compatibility.md) and
[`CONTEXT.md`](../../CONTEXT.md).

## Evidence labels and boundary

- **Documented** is a behavior stated by an owning Apple/Android public API,
  official tool help, or AOSP/ADB source.
- **Observed** is a completed AppPilotKit probe. It proves the named host/device
  run, not a platform compatibility promise.
- **Inference / recommendation** is the topology proposed here.

Apple identifies `simctl` and `devicectl` as Xcode command-line tools and
directs callers to their installed help; their available flags and output vary
with Xcode. [Apple: Xcode command-line tool reference](https://developer.apple.com/documentation/xcode/xcode-command-line-tool-reference)
On the investigated `devicectl 506.6`, the help explicitly says that a
user-supplied `--json-output` file is its **only supported interface for
scripts/programs**. Therefore no adapter may parse its human stdout/stderr;
it must request and validate that file, feature-probe the installed help, and
translate failures into AppPilotKit errors. This is an official-tool boundary,
not a guarantee for arbitrary printed fields.

ADB is the supported Android bridge over USB or TCP; its official man page
defines forwarding commands and `tcp:0`, while the AOSP service protocol
documents the `localabstract` endpoint variant. [AOSP: ADB overview](https://android.googlesource.com/platform/packages/modules/adb/) [AOSP: `adb.1.md`](https://android.googlesource.com/platform/packages/modules/adb/%2Bshow/refs/heads/main/docs/user/adb.1.md) [AOSP: ADB services](https://android.googlesource.com/platform/packages/modules/adb/+/HEAD/docs/dev/services.md)
`adb devices -l`, `track-devices`, `am` shell text, and tool diagnostics are
useful discovery/operation interfaces but are not an AppPilotKit schema. Parse
only the minimal documented token fields required for selection, retain raw
diagnostics only as redacted local diagnostics, and capability-probe the
installed Platform Tools version. The local `adb 36.0.2` help lists
`localabstract`; the web-rendered man page can differ, so feature-probe that
endpoint rather than treating a particular help/output rendering as immutable.
Direct smart-socket `host:track-devices` is an AOSP protocol with length-framed
change lists, not a stable Android app API. [AOSP: ADB services](https://android.googlesource.com/platform/packages/modules/adb/+/HEAD/docs/dev/services.md)

No Apple public source found in this investigation specifies the macOS
`/var/run/usbmuxd` plist wire protocol. The completed physical-device probe
therefore remains **Observed/private**: it selected the hardware UDID's `USB`
record, connected to the injected port, completed mutual proof and a JSON-RPC
ping, and rejected a wrong token. [#1 physical-device verification](https://github.com/patrick-fu/AppPilotKit/issues/1#issuecomment-5152677832)
Keep that code behind one small macOS adapter with contract tests; never
describe it as an Apple SDK API or leak usbmux fields into the product model.

## Public model and adapter contract

### Candidate and Target handles

**Recommendation:** discovery returns a redacted `DeviceCandidate` and an
`AppCandidate`; neither is a Target. Explicit selection yields an internal
`BootstrapSelection`:

```text
BootstrapSelection = {
  platform: ios | android,
  form_factor: simulator | emulator | physical,
  host_scoped_locator: opaque,
  app_identity: bundle_id | package_name,
  availability: discovered | bootstrappable | unavailable
}
```

Only after the Broker completes stream authentication and `session.open`
returns a process generation may it issue an opaque `TargetHandle`. The locator
is adapter-private: a Simulator UDID, a CoreDevice selection value, or an ADB
serial/transport selection is useful to the adapter but is not a session ID,
authentication material, TargetHandle, or durable cross-host identity. A device
can contain several app processes and an app can be relaunched; they must never
share a Target or a session. This follows the repository definition of a Target
and its required concurrent isolation in [`CONTEXT.md`](../../CONTEXT.md).

The selected app identity plus the adapter's current locator must be recorded
in the CLI invocation state. It must be required on every forwarding or launch
subprocess (`--device` / `-s`), rather than relying on a tool's implicit
“current” device. The Android CLI documents `-s SERIAL` as the deterministic
target selector. [Android Developers: ADB](https://developer.android.com/tools/adb)

**Discovery closure:** `simctl`, `devicectl`, and ADB create only
`DeviceCandidate` records. An `AppCandidate` is either an app enumerated by the
official tool or an exact user-supplied bundle/package identity; installed-app
metadata never proves it is opted in or running. Only the Broker's current,
authenticated in-memory lease after `session.open` is a Target. Existing
processes without a verifiable current credential must explicitly relaunch or
activate; the Broker must not blind-attach. Successful handshake, not tool
metadata, confirms opt-in. Discovery never uses LAN, mDNS, or port scanning.

### Host-local Session Broker and internal seams

The Rust CLI foundation already requires injected platform processes and
tunnels, bounded cancellation, and library-first typed outcomes. [ADR 0005](../adr/0005-rust-desktop-cli-foundation.md)
**Recommendation:** public CLI invocations are short-lived Broker clients. The
Broker is an on-demand, bounded-lifetime Host-only process reached through a private Unix-domain socket in a
per-user directory (directory `0700`, socket `0600`), has no TCP/LAN listener,
and retains bootstrap credentials, Target leases, transport descriptors, and
session state only in memory. Its implementation must authenticate the local
peer using OS ownership/peer-credential facilities where available and reject
foreign-user connections. It emits no credential in logs, Machine Results, or
Artifacts. This is needed because a process-per-invocation CLI otherwise cannot
keep an in-memory bootstrap secret while later commands reuse an authenticated
Target, snapshot/session context, or independent concurrent Agent sessions.

An opaque TargetHandle/session reference is not a secret and can be passed by a
CLI client; the Broker maps it to the in-memory target lease, rejects a changed
process generation, and opens independent protocol sessions/keys per Agent.
One Broker coordinates Target leases concurrently and must never introduce a
global command, device, or Target lock; the Target remains authoritative for
its Single-Writer Target rule.
The Broker is an internal host component, not a persistent remote product
service. Every Target lease has a bounded heartbeat/idle TTL. Broker crash,
lease expiry, or transport disappearance closes the stream and makes the SDK
close its listener and destroy bootstrap/session state; a replacement Broker
never restores old credentials and requires explicit re-bootstrap/relaunch.
The exact timeout and crash-detection contract are deferred to the session
module decision; this topology only requires bounded expiry and fail-closed
rebootstrap.

Use these additional internal seams; none is public protocol surface:

```text
DiscoveryAdapter  -> list DeviceCandidate/AppCandidate / select BootstrapSelection
BootstrapAdapter  -> fresh process credentials + endpoint descriptor
StreamAdapter     -> one owned, authenticated-duplex candidate stream
LifecycleObserver -> foreground/listener-epoch invalidation
```

`StreamAdapter` returns bytes only. It does not return a device, construct a
Protocol Session, or interpret UI payloads. Its implementation must bind each
stream to exactly one `TargetHandle`, endpoint epoch, deadline, and cleanup
owner, so a stale forward or USB handle cannot be accidentally reused for a
different Target.

## Discovery and bootstrap by platform

### iOS Simulator

**Documented tool mechanism:** installed `simctl` help supports `list --json`
and says that setting a caller environment variable with a `SIMCTL_CHILD_`
prefix makes it available to the launched app. The local 2026-08-05 help is the
authoritative detail for the installed Xcode; Apple documents `simctl` as an
Xcode tool rather than publishing a stable JSON schema. [Apple: Xcode command-line tool reference](https://developer.apple.com/documentation/xcode/xcode-command-line-tool-reference)

**Recommendation:** use JSON only after validating its runtime/device
availability and select one Simulator explicitly. For a new bootstrap, generate
a random high loopback port and 256-bit bootstrap secret immediately before
`simctl launch`; pass them only as `SIMCTL_CHILD_APPPILOT_*`; let the Debug /
Internal SDK bind `127.0.0.1:<port>`. Do not use an argument, preferences,
artifact, or persistent file for the secret. An app can read launch environment
from `ProcessInfo.environment`. [Apple: `ProcessInfo.environment`](https://developer.apple.com/documentation/foundation/processinfo/environment)

**Observed:** the Issue #1 Simulator gate twice established a mutually
authenticated loopback session, closed the listener and invalidated its session
on background, reopened on foreground, then rejected the pre-restart session
after process termination/relaunch. [#1](https://github.com/patrick-fu/AppPilotKit/issues/1)

### Wired iPhone

**Documented tool mechanism:** `devicectl device process launch` accepts an
explicit device selector and supports either `DEVICECTL_CHILD_*` or a JSON
`--environment-variables` dictionary; the flag overrides the prefixed caller
environment. It can also terminate an existing process, but its help says that
is not supported on all platforms. Its machine result must be written through
`--json-output`, never parsed from stdout. Apple supplies `devicectl` as the
supported host-management tool. [Apple: Xcode command-line tool reference](https://developer.apple.com/documentation/xcode/xcode-command-line-tool-reference) [Apple: Xcode 16 release notes](https://developer.apple.com/documentation/xcode-release-notes/xcode-16-release-notes)

**Recommendation:** require an explicit “relaunch/bootstrap” operation when a
fresh process secret is needed; it is an app/device side effect, not discovery.
Supply a fresh port and bootstrap secret with `DEVICECTL_CHILD_APPPILOT_*`,
then make the SDK bind only device loopback. If an existing process cannot be
proved replaced, fail `target.bootstrapRequiresFreshProcess`; do not attach to
an endpoint with unknown credentials.

**Observed/private transport seam:** after bootstrap, open a direct usbmux
stream selected for the desired wired device and injected port; close that file
descriptor at end of the attempt. Do not start `iproxy`, bind a host TCP port,
or silently fall back to Wi-Fi/LAN. Issue #1 verified this exact direct stream
on one iPhone but also records that the plist protocol is not an Apple public
API. [#1](https://github.com/patrick-fu/AppPilotKit/issues/1)

### Android Emulator and wired Android

**Documented transport mechanism:** ADB forwards a host port to a device
endpoint, and `tcp:0` requests an available host port. [Android Developers: ADB forwarding](https://developer.android.com/tools/adb#set-up-port-forwarding) [AOSP: `adb.1.md`](https://android.googlesource.com/platform/packages/modules/adb/%2Bshow/refs/heads/main/docs/user/adb.1.md)
`localabstract:<name>` is documented by the AOSP service protocol, passed the
Issue #1 Platform Tools probe, and appears in the local `adb 36.0.2` help, but
is omitted from the currently published user man page. It is therefore an
implementation CLI/AOSP seam to feature-probe, not a permanent product ABI.
[AOSP: ADB services](https://android.googlesource.com/platform/packages/modules/adb/+/HEAD/docs/dev/services.md) [#1](https://github.com/patrick-fu/AppPilotKit/issues/1)
Android's `LocalServerSocket` creates an inbound UNIX-domain socket in the
Linux abstract namespace. [Android API: `LocalServerSocket`](https://developer.android.com/reference/android/net/LocalServerSocket)

**Recommendation:** use the same Debug/Internal SDK adapter for Emulator and
wired hardware: create an unguessable per-process `localabstract` name and pass
only that non-secret lease/name to a bootstrap Activity, then create exactly
one `adb -s <serial> forward tcp:0 localabstract:<name>`. The Broker connects
only to the returned host loopback port and sends a fresh bootstrap secret only
in memory over that exact ADB-authenticated stream. The SDK accepts one
provision message, then starts the normal challenge proof. The app never opens
an INET or LAN listener. `LocalSocket.getPeerCredentials()` is useful
defense-in-depth telemetry, but its UID is not Host/Agent identity: one adbd
can carry many authorized Hosts and UID values vary by build state. Thus the
ADB-authorized Host, random endpoint, and one-provision race are an explicit
unverified trust boundary—not Session authentication—and must be probed before
implementation. [Android API: `LocalSocket.getPeerCredentials`](https://developer.android.com/reference/android/net/LocalSocket#getPeerCredentials()) [Android API: `Credentials`](https://developer.android.com/reference/android/net/Credentials)

The Issue #1 Android probe used a Debug-only `LocalServerSocket`, ADB dynamic
forwarding, activity extras for the name **and token**, wrong-token rejection,
and a Release APK marker check on `emulator-5556`. That token path is
**Observed feasibility only** and rejected for production: `am start --es` is
documented for string extras but gives no secret-redaction guarantee and places
the value in argv/command transport. [Android Developers: Activity Manager](https://developer.android.com/tools/adb) [#1](https://github.com/patrick-fu/AppPilotKit/issues/1)

## Stream framing, authentication, and `session.open`

### Framing ownership

**Recommendation:** after a stream is established, the SDK/CLI transport pair
owns a small common framing layer: a bounded, unsigned 32-bit big-endian byte
length followed by exactly one UTF-8 JSON object. The layer rejects zero,
oversize, partial, and trailing frames before dispatch; its maximum is no
larger than a fixed conservative hard cap before `session.open`; only after
negotiation does it tighten separately to the request/response limits returned
by `session.open`. HTTP from the Issue #1 throwaway probes is evidence only and
is not the production product contract.

The transport adapter owns reads/writes, framing, byte limits, EOF, and
deadlines. The protocol module owns JSON-RPC validation, request IDs, and
methods. This exactly keeps ADR 0001's rule that the envelope is independent
of HTTP, TCP, usbmux, and ADB forwarding. [ADR 0001](../adr/0001-protocol-envelope-and-compatibility.md)

### Pre-protocol mutual authentication

**Recommendation:** a bootstrap secret is a random in-memory process secret,
not a protocol session ID and never a user action credential. Before accepting
any framed JSON-RPC request, require a challenge/response proof with independent
fresh client and server nonces, a transcript/domain binding containing the
selected Target and listener epoch, constant-time verification, one-use
challenges, and a bounded handshake deadline. Derive a distinct per-connection
authentication key from the bootstrap secret; discard handshake state on EOF,
listener close, process death, or backgrounding. Never echo a secret in a
failure, diagnostic, process output, or Artifact.

The working Issue #1 probes established an HMAC-style mutual proof and
wrong-token rejection on Simulator, Android Emulator, and direct usbmux. This
supports feasibility, not the exact future transcript or a cryptographic
standard. [#1](https://github.com/patrick-fu/AppPilotKit/issues/1)

Only after that proof may the client send `session.open`. ADR 0001 requires it
to be the first protocol request, negotiates version/capabilities/limits, and
returns a session ID plus process generation; session IDs are correlation
identifiers, not secrets. [ADR 0001](../adr/0001-protocol-envelope-and-compatibility.md)

### Session and Agent isolation

For every accepted `session.open`, the SDK records `{target process generation,
listener epoch, session id, Agent identity}`. A bootstrap secret can establish
many sessions for its one process, but each session receives independent
session key material and must not be reused across Agents or Targets. Permit
concurrent read-only sessions; enforce the existing Single-Writer Target rule
for mutations, returning an explicit conflict rather than queueing or replaying
one. [`CONTEXT.md`](../../CONTEXT.md)

The CLI must never use a global “current device”, global token, shared forward,
or a process-wide mutation lock. A forward/socket/usbmux descriptor belongs to
one Broker Target lease; a CLI invocation is only its short-lived client
request. This is required for the repository's Concurrent Target Sessions
definition, not an optional optimization. [`CONTEXT.md`](../../CONTEXT.md)

## Lifecycle, cleanup, recovery, and errors

### Foreground, background, and process death

The product requirement is stricter than reachability: a Foreground Target is
the only Target eligible for guaranteed inspection/actions, and backgrounding
invalidates its interactive Protocol Sessions. [`CONTEXT.md`](../../CONTEXT.md)

**Recommendation:** close the listener, all connections, challenges, and
session state on loss of foreground eligibility; increase the **listener epoch**
before reopening, but retain the same process generation. Do not keep an
authenticated background service. On a foreground return, a new pre-protocol
proof and `session.open` are required. Only a process restart changes process
generation, receives a new bootstrap secret, and makes every old session
terminally stale.

This policy maps naturally to platform lifecycle ownership, but each adapter
must be conservative about multi-window:

- UIKit exposes scene/app lifecycle changes; Apple says a dismissed UI moves its
  scene to background and eventually suspended, and foreground/background apps
  have different execution allowances. [Apple: managing your app life cycle](https://developer.apple.com/documentation/uikit/managing-your-app-s-life-cycle)
- Android's `onPause` is not reliably invisible in multi-window; `onStop` is
  invoked when the activity is no longer visible. The product must explicitly
  choose its "active App Surface" predicate and invalidate on every transition
  that no longer satisfies it; never infer it from a TCP connection. [Android: activity lifecycle](https://developer.android.com/guide/components/activities/activity-lifecycle)
- The Android SDK should observe its own `Application.ActivityLifecycleCallbacks`
  or `ProcessLifecycleOwner`, not `dumpsys`. The latter delays final pause/stop
  and does not cover other processes, so the chosen predicate and multi-process
  scope must be in the contract. [Android API: `ActivityLifecycleCallbacks`](https://developer.android.com/reference/android/app/Application.ActivityLifecycleCallbacks) [Android API: `ProcessLifecycleOwner`](https://developer.android.com/reference/androidx/lifecycle/ProcessLifecycleOwner)
- Android may kill a background process; the official lifecycle guidance says
  process kill destroys its components and does not guarantee `onDestroy`.
  Treat EOF/forward failure after that as process loss, not a retryable session.
  [Android: process lifecycle](https://developer.android.com/guide/components/activities/process-lifecycle)

The iOS Simulator probe already observed listener close/reopen and old-session
rejection on background/foreground, and old-session rejection after process
restart. [#1](https://github.com/patrick-fu/AppPilotKit/issues/1)
The equivalent Android lifecycle and physical Android device matrix is still
unverified.

### Cleanup and recovery ownership

The Broker Target lease, not each CLI invocation, owns a cleanup ledger before
every transport side effect:

1. close the client stream and cancel pending reads;
2. remove only the exact ADB forward it created (`adb forward --remove`);
3. close a direct-usbmux descriptor (no helper process or host port exists);
4. delete private temporary tool-result files and zero in-memory bootstrap
   material; and
5. return one typed, redacted cleanup outcome to the CLI renderer.

Do not terminate, force-stop, uninstall, reboot, or remove another invocation's
forward during ordinary cleanup. The Android pre-bootstrap exception is a
confirmed Activity start followed by failed listener/forward/connect setup:
the adapter uses an independent bounded cleanup deadline to force-stop only
the exact selected package. Explicit relaunch/reset commands own all other
side effects and must describe them in the CLI contract. A stale forward,
missing endpoint, EOF, pairing/authorization change, or USB detach triggers
rediscovery and a new explicit bootstrap—not transparent mutation replay.

### Stable machine-error mapping

Existing protocol error kinds and CLI exit categories are commitments: callers
branch on `error.kind`, not text; transport/authentication failures use exit 3,
changed-target/session preconditions use exit 4, ambiguous mutations use exit
5, and incompatibility uses exit 6. [ADR 0001](../adr/0001-protocol-envelope-and-compatibility.md) [ADR 0006](../adr/0006-agent-facing-cli-contract.md)

**Recommendation for new CLI-owned kinds** (names require the future command
schema before becoming public):

| Condition | Proposed kind | Exit category | Safe recovery |
| --- | --- | ---: | --- |
| no selected/online candidate | `target.notFound` / `target.selectionRequired` | 4 | rediscover or select one Target |
| tool unavailable, pairing/ADB authorization/forward/usbmux stream unavailable | `transport.unavailable` | 3 | repair host/device transport, then rediscover |
| endpoint exists but fresh process credentials are required | `target.bootstrapRequiresFreshProcess` | 4 | explicit relaunch/bootstrap |
| proof absent, malformed, expired, or wrong | `transport.authenticationRequired` / `transport.authenticationFailed` | 3 | discard stream; fresh proof/bootstrap only |
| backgrounded, listener epoch changed, process died, or old generation | existing `sessionExpired` | 4 | wait for foreground/relaunch; authenticate and `session.open` again |
| incompatible frame/protocol/capability | existing `parseError`, `incompatibleProtocol`, or `capabilityUnavailable` | 6 | choose compatible client/Target; never downgrade silently |
| concurrent mutation | `target.mutationConflict` | 4 | wait for/read current Target state; do not queue/replay |
| mutation connection loss after dispatch | existing `action.outcomeUnknown` | 5 | read-only inspection only; no automatic retry |

Error details may contain only safe structural facts (selected platform/form
factor, opaque target reference, lifecycle state); they must not expose serial
numbers unless needed for an explicitly selected local diagnostic, port,
socket name, challenge, token, UI text, or request payload.

## Release isolation

Release artifacts must contain no active listener, launch-credential parser,
Debug bootstrap Activity/manifest entry, local-abstract server, token symbols,
or production fallback path. The architecture is opt-in Debug/Internal only,
as required by [the vision](../vision.md). The Issue #1 probe observed an iOS
Release binary without listener/credential/server markers and an Android
Release APK without debug Activity/server markers; this is useful regression
evidence, not a complete release-security proof. [#1](https://github.com/patrick-fu/AppPilotKit/issues/1)

The production acceptance suite should build each release variant, inspect its
binary/APK and manifest for the forbidden capability markers, and prove that a
Release app rejects/does not expose the bootstrap surface. No release build may
open a diagnostic TCP, abstract socket, usbmux, or ADB endpoint.

## Rejected alternatives

| Rejected design | Reason |
| --- | --- |
| LAN listener, mDNS discovery, or a fixed device/host port | Violates loopback/authenticated-tunnel boundary; expands exposure and creates collisions. |
| Treat device pairing, ADB authorization, a UDID, or usbmux handle as AppPilotKit authentication | These identify/authorize a transport relationship, not the opted-in app process or a Protocol Session. ADR 0001 requires pre-protocol authentication. |
| Product-wide HTTP endpoint or HTTP as the protocol framing contract | The Issue #1 HTTP server was a throwaway feasibility probe. It would couple the envelope to one transport and duplicate framing/error concerns. |
| `iproxy` or an always-running forwarder for iPhone | Adds an unneeded process and host TCP listener; the verified direct path has neither. It remains only a private adapter option if direct usbmux changes. |
| Android `adb reverse` | Inverts ownership and is unnecessary when the Target owns its local-abstract endpoint and the Host owns one forward. |
| One device-level session/token/forward shared by all Agents or apps | Breaks Target/process generation isolation and can cause cross-app or cross-Agent actions. |
| Persist a bootstrap secret, or place a **Session Credential** in preferences, Keychain, disk, logs, Machine Results, or Artifacts | Contradicts the repository's ephemeral credential/data constraints and makes a diagnostic tool a long-lived secret store. The old Android extra-token probe is explicitly rejected above. |
| Ship a generic production background service to survive lifecycle changes | Conflicts with the Foreground Target guarantee and release exclusion. |

## Residual risks and minimum future probes

| Risk | Why documentation cannot settle it | Smallest necessary probe |
| --- | --- | --- |
| direct usbmux compatibility | Apple does not publish its plist protocol as a public API; one host/iPhone proves only one seam instance | Matrix: oldest supported iOS 15/Xcode, mid-range, current; wired detach/reattach, wrong port, stale handle, lock/pairing change, and reconnect. Keep adapter contract tests independent of a real device. |
| Apple launch-secret secrecy | `SIMCTL_CHILD_` / `DEVICECTL_CHILD_` avoid argv/disk but Apple help does not promise that child environment values never enter verbose/log/JSON/crash diagnostics | Inject a unique canary and scan tool stdout/stderr, machine JSON, process listings, and controlled logs on Simulator and wired iPhone. If it leaks, replace the launch channel with a Broker bootstrap channel. |
| Android Broker bootstrap/peer credentials | `am` extras are not a secret channel; the proposed in-memory provisioning path and expected `LocalSocket` peer UIDs are untested | Emulator and wired API 26–37 matrix: only non-secret extras, exact forward, peer UID capture, one-time provisioning, wrong-proof rejection, and proof that no secret reaches argv/log/result/artifact. Reject the path if an unexpected local peer can provision. |
| Android `localabstract` compatibility and forward cleanup | Current local CLI help supports it, but published ADB man-page output differs and no public source promises forward cleanup after device/process loss | Feature-probe each supported Platform Tools version; kill app/detach USB/kill ADB server and prove Broker removes only its exact mapping and detects stale state. |
| Android lifecycle/process generation | Official lifecycle callbacks describe states, not AppPilotKit's active-surface policy or server behavior | Emulator plus wired device: pause/multi-window/stop, rotate, force-stop, OS kill, reconnect; prove all old sessions fail and only foreground can reopen. |
| iPhone lifecycle after direct usbmux | #1 proved launch/proof, but not a physical lifecycle matrix | Wired iPhone: background/foreground, process kill/relaunch, locked/unlocked, USB detach/reattach; prove close, session invalidation, and no leaked endpoint. |
| tool-version drift | Xcode and Platform Tools change command availability/output | CI fixture adapters from captured stable machine JSON/help and ADB grammar; fail closed on unknown tool versions/fields rather than guessing. |
| framing/authentication design | Feasibility HMAC does not specify a durable, reviewed transcript or cancellation behavior | Cross-language golden vectors plus negative cases: replay, reflection, nonce reuse, partial/oversize frame, concurrent sessions, EOF at every handshake point. |

These probes are intentionally narrower than Issue #1: they do not re-prove
loopback, usbmux, or ADB-forward feasibility. They settle only compatibility,
lifecycle, and credential-boundary facts that documentation cannot answer.

## Decision consequences

The credential-safe bootstrap and Broker-continuity uncertainty is now the
blocking prototype [#38](https://github.com/patrick-fu/AppPilotKit/issues/38).
It must resolve the Apple launch-secret and Android stream-provisioning rows
above before the installed no-Skill workflow or production ownership decision
can close.

Issue #24 supplies topology evidence only. The next module decision (Issue
[#31](https://github.com/patrick-fu/AppPilotKit/issues/31)) must select the
Broker/module ownership, adapter traits, TargetHandle/cleanup state machine,
framing/authentication vectors, TTLs, and new CLI error schema. It should not
promote probe code, use undocumented tool output as a product schema, or add a
listener before release-exclusion and lifecycle tests are specified. A separate
ADR is warranted before making framing or an authentication transcript a durable
compatibility commitment.
