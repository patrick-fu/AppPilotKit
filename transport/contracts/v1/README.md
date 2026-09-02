# AppPilotKit private Target transport contract v1

This directory freezes the private Broker bootstrap and Target transport selected by ADR 0009. It is the single source of truth for future Host, Swift FFI, and Android JNI implementations. Protocol v1.2 JSON-RPC begins only after this transport authenticates and delivers one complete message; these files do not widen Protocol v1.2, CLI v1, or production SDK public interfaces.

## Required operator flow

Prepare reads exactly one UTF-8 JSON value from fd 0 and writes exactly one newline-terminated result:

```text
<prefix>/libexec/apppilotkit-target-prepare --request-fd=0 --output=json
```

The first installed-CLI verdict is:

```text
<prefix>/bin/apppilotkit catalog list --target=<opaque> --output=json --non-interactive
```

It contains no `--session`. A complete non-empty result projects the Target-issued session only through the existing safe show action:

```text
<prefix>/bin/apppilotkit catalog show --capability <exact-id> --declaration-revision <exact-revision> --session=<target-issued-id> --target=<same-ref> --output json --non-interactive
```

A truncated result projects `catalog.list.continue` with exact `--session=<id>`, `--target=<same-ref>`, `--cursor <opaque>`, `--output json`, and `--non-interactive`. A complete empty result projects existing `catalog.list` with exact `--session=<id>`, `--target=<same-ref>`, `--output json`, and `--non-interactive`. Only a session id returned by Target `session.open` can be reused, always with its bound target reference as a match-only selector; reuse does not redeem that already-consumed reference again. Every new Session uses a fresh prepare reference.

The #62 first real verdict remains only this installed `catalog list`. Evidence retains newline-terminated Machine Result bytes after replacing only the session and Target selector argv values with `<redacted>`; the two exact JSON pointers retain SHA-256 of their original non-secret values. The verifier parses those same retained bytes and validates the complete value against the repository's frozen CLI v1 `machine-result`, `artifact`, `disclosure`, `error`, and `next-action` schemas with external retrieval disabled. Before parsing or registration, `dependencies.lock.json` requires the exact sorted six-schema inventory and SHA-256 of every schema byte sequence, including `catalog`. Missing, extra, reordered, or changed pins and changed schema bytes fail closed. The verifier then validates `data` against `catalog.schema.json#/$defs/list`, requires `cli_version` to equal the installed `apppilotkit` identity version, derives the catalog, `smoke.ready` declaration, and redacted Next Action, and binds the selector digests, generation, platform, tool, and transport. Its stdout and Machine Result scan captures are byte-identical to retained stdout, its Next Actions capture is the canonical serialized `next_actions` value, and its empty stderr capture recomputes `stderr_sha256`. A list result has no Resource value, input presence, schema id, or schema digest. #62 does not add a command or require `catalog query`.

CLI v1's existing `--target` parser remains a broad opaque-string syntax check and is not a transport trust boundary. Without changing that parser, `BrokerCatalogRuntime.select()` validates exact `target_` plus 43-character unpadded-base64url grammar before IPC. The authenticated Broker then requires an exact live in-memory lease/ref or bound match-only session before platform effects. A raw UDID, serial, app id, endpoint, malformed token, unknown token, or stale token fails through the existing target/session selection or expiry family and never reaches a platform adapter.

## Normative encoding

- All JSON schemas use JSON Schema 2020-12. JSON producer output is UTF-8, contains no duplicate object member, and ends in one LF. Schemas use `additionalProperties: false` at every object boundary.
- Broker IPC and handshake payloads use RFC 8949 deterministic CBOR: definite lengths only; shortest integer and length encodings; no tags, floats, text normalization, indefinite items, or duplicate keys; maps are keyed by unsigned integers and serialized in ascending numeric order. CDDL map key numbers are wire commitments.
- A Broker IPC packet is `u32be(cbor_length) || deterministic_cbor`, with `1 <= cbor_length <= 67,109,120` and total packet bytes at most `67,109,124`. Open-session request/success packets are capped at 73,728 CBOR bytes; every other non-exchange packet is capped at 8,192. One UDS connection carries one request and one result; concurrency uses separate connections. There is no Broker-IPC chunking subprotocol.
- Every Noise handshake or transport ciphertext is `u16be(ciphertext_length) || ciphertext`, with `1 <= ciphertext_length <= 65,535`. Handshake payload plaintext is at most 8,192 bytes.
- Post-handshake plaintext records start with the 12-byte header in `wire/session.cddl`. Application request bytes are at most 16 MiB, response bytes at most 64 MiB, and pre-session `session.open` bytes at most 64 KiB.

### App artifact identity

`artifact_encoding` is mandatory and platform-fixed: iOS Simulator is `ios-app-tree-v1`; Android Emulator is `raw-file-v1`. Android hashes the exact selected regular-file bytes. iOS hashes the complete canonical byte stream below, never a directory path, archive metadata, per-file digest manifest, or concatenation of per-file digests. The app `artifact_bytes_base64url` evidence field and each iOS build scan capture contain the complete same canonical stream; `artifact_sha256` is SHA-256 of those exact decoded bytes.

`ios-app-tree-v1` is streamed as:

```text
"APPPILOTKIT-IOS-APP-TREE\0\x01"
|| entry_count:u32be
|| record[entry_count]

record = kind:u8
      || path_len:u32be
      || path:utf8[path_len]
      || executable_class:u8
      || (file_len:u64be || exact_file_bytes[file_len])  ; files only
```

The bundle root is implicit and is not an entry. `kind` is `1` for a directory and `2` for a regular file. `executable_class` is `1` for a regular file when any source execute bit is set and `0` otherwise; a directory always carries `0`. Records are in strictly ascending raw UTF-8 byte order by path, with no duplicate. Every non-root entry's direct parent path must exist as a directory record; consequently every ancestor exists and no file can be a path prefix. Paths are relative, non-empty UTF-8 with no NUL, leading/trailing slash, empty component, `.` component, or `..` component. A component is at most 255 UTF-8 bytes, the whole path at most 4,096 UTF-8 bytes, and depth at most 64 components including the entry's final file or directory name; the implicit root is not counted. No Unicode normalization or case folding occurs, so NFC/NFD and case-distinct paths remain distinct records.

The tree has at most 65,535 entries. A file has at most 512 MiB (`536,870,912` bytes), and the sum of all exact file payload bytes is at most 1 GiB (`1,073,741,824` bytes). Construction, hashing, and verification are streaming and must reject a declared cap violation before allocating or reading that payload. Symbolic links, hard links, sockets, devices, FIFOs, all other special files, and any ResourceFork are rejected. Ordinary extended attributes, ACLs, ownership, timestamps, BSD flags, directory modes, and non-execute file permission bits do not enter the stream. A private snapshot copies neither ordinary xattrs nor ACLs; it preserves only accepted directory/file topology, exact file bytes, and the one-bit file execute class. `Info.plist`, `_CodeSignature/**`, embedded signatures, and all other accepted bundle files are ordinary exact bytes.

Before hashing or launch, the root `Info.plist` must be a regular file and parse as a property-list dictionary whose `CFBundleIdentifier` exactly equals `app_id`, whose `CFBundlePackageType` is `APPL`, and whose non-empty string `CFBundleVersion` is at most 128 UTF-8 bytes. `CFBundleExecutable` must be one safe path component; the same root-relative entry must be a regular file with `executable_class = 1`. Invalid source types, unstable copy observations, invalid bundle fields, cap violations, or a changed snapshot fail before install.

A Ready Target Reference has one bijective representation: the CBOR `target-reference-token` is exactly 32 bytes, and its JSON/argv form is ASCII `target_ || BASE64URL_NO_PAD(token)`. Decoding JSON must yield exactly 32 bytes and re-encoding those bytes must be byte-identical to the original 43 characters. The Broker stores the token only in memory and binds it to one lease. `SHA-256(UTF8(full initial JSON/argv reference))` exists only in the initial launch descriptor/NK bootstrap. Later fresh references for the same eligible lease are Broker locators and are not Target handshake inputs. A lease id is 16 random bytes and a target nonce is 32 random bytes. A Process Generation is sampled uniformly from `1..=9007199254740991`; this JSON-safe positive-integer range is lossless in Swift `UInt64`, Kotlin `Long`, Rust `u64`, and standard JSON number consumers. A new Listener Epoch starts at `1`, increments before each eligibility invalidation, and at `9007199254740991` forces terminal process replacement instead of wrapping. Random values come from the OS CSPRNG through the shared Rust core. Ready References and Target-issued session ids are non-secret but appear only in exact prepare result/argv/Next Action surfaces and are never persisted by the product.

Every JSON string whose CDDL peer has a byte cap is checked by the reference semantic validator after JSON Schema validation. ASCII-only identifiers use ASCII schema patterns. Unicode absolute paths remain permitted, but `UTF8(path).len` must be within the CDDL byte limit. Every unpadded base64url field rejects padding, length class `len % 4 == 1`, non-alphabet bytes, non-zero unused terminal bits, and any value whose decode then re-encode is not byte-identical. Fixed 16-byte and 32-byte fields additionally require exactly 22 and 43 characters with the legal terminal character class. JSON diagnostic to canonical CBOR and canonical CBOR back to JSON are executable verifier checks.

Prepare validates the platform-fixed encoding, hashes those exact artifact bytes, and forms `(platform, device_selector, app_id, artifact_encoding, digest)`. With no lease it launches and performs NK once. With an eligible same-Broker lease matching key, build, generation, epoch, endpoint/cleanup ledger, 900-second absolute/120-second idle bounds, and authenticated heartbeat, it only mints a new child reference. Any conflict fails closed without replacement or relaunch. `issued_at_unix_ms` is sampled by the Broker after bootstrap acknowledgement or successful owned-lease eligibility check; `expires_at_unix_ms = issued_at_unix_ms + 30000`. The child state is `Minted -> Redeeming -> Consumed | Expired`; `now >= expires` rejects. Target-only redemption consumes once. Match-only use is permitted only with the already-bound session; a new session requires a fresh reference. A stale/mismatched reference never selects a different lease.

The global cap is derived from the largest legal exchange success, not from a typical vector: `67,108,864` response bytes + `256` deterministic-CBOR bytes of maximum legal map/key/length/session-id/generation/epoch/digest overhead = `67,109,120` CBOR bytes; the `u32be` prefix makes `67,109,124` total packet bytes. The maximum legal exchange request is `16,777,216 + 264 = 16,777,480` CBOR bytes (`16,777,484` including the prefix). Open-session request/success packets are `73,728` CBOR bytes maximum (`73,732` including prefix) so they can carry one bounded 64 KiB opaque Protocol message plus routing metadata. Prepare/close requests, other non-exchange successes, and failures remain capped at `8,192`. Checked-in boundary goldens fix all six literals and cap+1 rejection; per-operation caps are checked after the global prefix guard and before allocation.

## Broker startup and private IPC

The Broker derives the Darwin per-user temporary directory with `confstr(_CS_DARWIN_USER_TEMP_DIR)`, not `TMPDIR`. It opens each path component with no symlink following, verifies ownership by the current euid, and creates `apppilotkit/broker-v1` with mode `0700`. The only persistent names are `broker.lock` (regular file, owner euid, mode `0600`) and `control.sock` (Unix stream socket, mode `0600`); neither stores a credential, lease, endpoint, reference, or Target state.

The Broker holds a nonblocking exclusive `fcntl(F_SETLK)` write lock on `broker.lock` for its lifetime. Client and Broker both verify peer euid with `getpeereid`. A client that cannot connect attempts one constant spawn with argv exactly `["<prefix>/libexec/apppilotkit-broker","--serve"]`, empty stdin, inherited current-user identity, and no target, locator, endpoint, or secret in argv/environment. Spawn plus connect/peer verification has one 1,000 ms budget. If the lock is owned but the socket is unusable, or the peer/version differs, the client fails closed and never kills or replaces the owner. Only a process holding the exclusive lock can unlink a stale socket before bind. Broker exit closes the listener, unlinks only its inode-verified `control.sock`, releases the lock, and retains no restorable state.

## Cryptographic pin

The only permitted algorithms are Noise revision 34 `Noise_NK_25519_ChaChaPoly_SHA256` for process bootstrap and `Noise_NNpsk0_25519_ChaChaPoly_SHA256` for each Protocol Session. `dependencies.lock.json` pins the revision-34 source content and `snow 0.10.0`. Production implementations use the same Rust core. They do not implement a KDF, HMAC, AEAD, DH, rekey, or nonce schedule outside `snow`.

For a session, the Target is Noise initiator and the Broker is responder. The exact deterministic-CBOR prologue binds private version/roles, lease, generation, epoch, hard limits, and NK hash; it contains neither Ready Reference digest nor agent binding. After split, Target sends initiator Finished and Broker sends responder Finished. Transport then accepts one complete Broker-to-Target opaque application plaintext up to 64 KiB and hands it to a fresh per-connection Protocol Runtime. Transport does not decode UTF-8, JSON-RPC, envelope, method, parameters, or id. `BrokerCatalogRuntime` guarantees it constructed `session.open`; the Target Runtime alone enforces that it is first. Record kind `2` is connection-state specific: NK accepts only bootstrap acknowledgement and NNpsk0 only Finished.

Each direction stops before either `2^32` transport records or `2^40` plaintext bytes. There is no in-session rekey or rehandshake. Reaching either bound sends `recordLimit` close when a nonce remains; otherwise the connection closes without another ciphertext. A new explicit session is required.

## Deadlines and TTLs

| Operation | Exact limit |
| --- | ---: |
| Broker spawn plus peer check | 1,000 ms |
| Android Emulator install, Activity start, forward, and raw-stream connect | 20,000 ms |
| iOS Simulator launch phase | 10,000 ms |
| NK bootstrap after the raw stream connects | 10,000 ms |
| Prepare total (Android Emulator / iOS Simulator) | 30,000 ms / 20,000 ms |
| Ready Target Reference redemption | 30,000 ms |
| Session stream connect | 1,000 ms |
| NNpsk0 plus Finished | 1,000 ms |
| `session.open` response | 2,000 ms |
| Connect through opened session total | 4,000 ms |
| Incomplete outer header or body | 2,000 ms |
| Protocol Session idle TTL | 30,000 ms |
| Target Lease idle TTL | 120,000 ms |
| Target Lease absolute TTL | 900,000 ms |
| Encrypted lease heartbeat interval | 30,000 ms |
| Missing-heartbeat terminal threshold | 4 intervals / 120,000 ms |
| Broker idle exit after no lease/client | 5,000 ms |
| Close and cleanup total | 2,000 ms |

Active requests also obey the CLI command deadline. An ordinary command or transport deadline is `timeout`; Broker EOF/crash and the encrypted-heartbeat threshold are exclusively `brokerLost` and use the handoff matrix below. Deadline, EOF, authentication, generation/epoch, and TTL failures are terminal for that connection or lease and are never retried automatically.

## Close reasons and error mapping

| Condition | Close reason | Existing CLI/Protocol mapping |
| --- | --- | --- |
| clean explicit close | `normal` | success |
| Noise decrypt, Finished, or peer proof failure | `authenticationFailed` | `transport.authenticationRequired`, exit 3 |
| handshake AEAD failure caused by wrong target/lease/generation/epoch/prologue/PSK, tamper, replay, or wrong Noise role | `authenticationFailed` | `transport.authenticationRequired`, exit 3 |
| authenticated Finished/session-open binding differs from stored target/lease/generation/epoch | `bindingMismatch` | exact mapping below, exit 4 |
| expired reference/session/lease or prior terminal state | `stale` | `sessionExpired`, exit 4 |
| any exact command or transport deadline reached, excluding Broker loss | `timeout` | `timeout`; post-dispatch mutation becomes `action.outcomeUnknown` |
| CLI-owned semantic request or complete-response size exceeds its cap | `oversize` | locally emitted `resourceExhausted` |
| authenticated transport frame/record declaration or accumulation exceeds its cap | `oversize` | `sessionExpired`; post-dispatch mutation uses the ambiguity rule |
| truncated header/body, trailing bytes, invalid deterministic CBOR | `malformed` | `sessionExpired`, exit 4 |
| nonzero reserved, non-START total length, duplicate/reordered/gapped/overlapping/interleaved record, or half-duplex violation | `sequenceViolation` | `sessionExpired`, exit 4 |
| per-direction record or plaintext-byte cap reached | `recordLimit` | `sessionExpired`, explicit new session required |
| authenticated peer cleanly closes or EOFs early | `peerClosed` | pre-dispatch `sessionExpired`; post-dispatch uses ambiguity rule |
| Broker EOF/crash or heartbeat threshold | `brokerLost` | use the exact handoff matrix below |
| Target foreground/Debug/Internal eligibility loss | `eligibilityLost` | `sessionExpired`, exit 4 |
| adapter-owned cleanup exceeds 2 seconds or leaves a resource | `cleanupFailed` | failed/stale `internalError`; resource never reused |
| invariant or non-peer implementation failure | `internalError` | `internalError`, exit 1 |

`authenticationFailed`, `bindingMismatch`, `malformed`, and `sequenceViolation` send no detailed peer diagnostic. A close record is sent only when authentication still holds and an unused nonce remains. Otherwise the transport closes immediately. Safe error JSON contains only the enumerated kind, stock redacted message, retryable flag, stage, two-value handoff state, and close reason.

Authenticated `malformed` and `sequenceViolation` failures in transport framing, record reassembly, and close-record decoding use the existing `sessionExpired` public error and exit 4, matching authenticated transport `oversize` and `recordLimit`. Transport and Broker never synthesize peer JSON-RPC `parseError` or `invalidRequest`: application bytes are opaque until Transport Handoff, and only the existing Protocol Runtime may classify a decoded JSON-RPC request. The frame vectors are pre-handoff parser controls; the existing post-handoff mutation ambiguity matrix still takes precedence when a complete mutating request may have executed.

The mapping is closed: private Broker IPC never contains `session.selectionRequired`, `resourceExhausted`, or `action.outcomeUnknown`; a parser-accepted but malformed or unknown Ready Target Reference is `target.selectionRequired`; a stale/expired valid reference, unknown/expired Target-issued session, expired lease, authenticated session-open binding mismatch, or transport-size termination is `sessionExpired`; target/session mismatch is `target.selectionRequired`; authentication failure is `transport.authenticationRequired`; invariant/cleanup/non-peer failure is `internalError`. CLI-owned semantic request and complete-response limits continue to emit the existing local `resourceExhausted` result without crossing Broker IPC. `BrokerCatalogRuntime` stamps the exact trusted CLI-created method (`semantic.invoke = app_mutation`; list/show/schema/query = read_only), maps private handoff state to existing `CatalogExchangeError` phase, and the existing command handler alone performs the public `action.outcomeUnknown` mapping.

| Connection observation | Handoff | Read-only | `semantic.invoke` |
| --- | --- | --- | --- |
| no application ciphertext emitted | `NotHandedOff` | `sessionExpired`, exit 4 | same |
| partial request; END provably not emitted | `NotHandedOff` | `sessionExpired`, exit 4 | same |
| full request emitted; no complete response | `HandoffPossibleOrConfirmed` | `sessionExpired`, exit 4 | `action.outcomeUnknown`, exit 5, no replay |
| complete authenticated response reassembled | success | validate response | validate response |
| response produced/lost, partial, or EOF before END | `HandoffPossibleOrConfirmed` | `sessionExpired`, exit 4 | `action.outcomeUnknown`, exit 5, no replay |

There is no runtime-ack record. Conservative post-handoff classification may mark a not-yet-executed invoke unknown; that is the required fail-closed outcome.

## Lifecycle and dispatch

Broker state is `Absent -> Starting -> Ready`. A lease is `Absent -> Preparing -> BootstrapHandshaking -> Eligible -> Terminal|Stale -> Closing -> Closed`. Each child ref is `Minted -> Redeeming -> Consumed | Expired`; each child session is `Handshaking -> Opening -> Open <-> Exchanging|Idle -> Closing -> Closed|Stale`. Every new session has fresh NNpsk0 ephemerals, session id, limits, idle state, and runtime instance. Session A close/idle/auth failure affects only A; concurrent read on B remains live. Lease, process, epoch, or Broker-heartbeat loss atomically stales every child ref/session. No event is silently reordered, queued after terminal state, or reported successful after close/stale wins.

The Transport Handoff Boundary occurs after complete authenticated opaque application bytes pass framing, sequence, binding, lifecycle, and raw byte caps, immediately before `runtime.handle(bytes)`. JSON-RPC checks occur only inside that runtime. The separate Mutation Dispatch Boundary is the existing coordinator's policy/authorization/evidence/liveness/Single-Writer handler handoff and is never observed by transport.

The Target constructs its Protocol `CatalogIdentity` with the exact transport Process Generation, so the `session.open` `context.generation` is the same value. `BrokerCatalogRuntime` constructs the exact JSON-RPC request before IPC. Broker transports it and the Target response as opaque bytes with independent digests; it never extracts a JSON-RPC id, method, session id, capability, or limit. The adapter validates the returned bytes and copies the Target context into `OpenedProtocolSession`. A response/transport generation mismatch is `bindingMismatch` and maps to existing `sessionExpired` before any later application request crosses Transport Handoff. This preserves the existing public runtime and Protocol shapes rather than introducing a second generation domain.

Target composition creates one existing `SemanticProtocolRuntime`/`ProtocolRuntime` per authenticated connection, sharing the immutable Catalog and one `TargetActionCoordinator`. A single-session failure destroys only that runtime/keys and never calls all-session invalidation. Foreground/eligibility loss first advances Listener Epoch, then rejects frames, closes listener/connections, invalidates all live per-session runtimes, and destroys keys. Process restart creates fresh generation, PBS, lease, and initial reference. Broker heartbeat requires exact generation/epoch. Cleanup order is: mark lease stale; classify I/O by handoff; close every child session; close listener/connections and invalidate all live runtimes; clean lease-owned resources; destroy PBS/private keys; release lease; idle-exit only with no client/lease.

Retained evidence proves session independence with closed, complete records rather than trusting the `fresh_session_ids` flag. Each primary or concurrent record carries a session id digest and handshake hash plus request, response, and runtime facts. Every fact repeats the same session digest; request and response have independent SHA-256 identities; the runtime has an independent instance digest and repeats both exchange digests. The two concurrent records must differ in session id, handshake, request, response, and runtime instance, and the primary record must equal one complete concurrent record.

## Platform launch descriptors

iOS Simulator #62 uses explicit install and launch on the exact selected UDID; it does not claim attach to an already-running Target. Prepare builds a Broker-owned private `.app` snapshot using the rules above, derives and retains the exact `ios-app-tree-v1` bytes, installs that snapshot rather than the caller's mutable path, and launches its exact `CFBundleIdentifier`. The adapter must request termination of an older instance, record the PID returned for this launch, and bind lease ownership and cleanup only to that exact PID/process generation; an absent, ambiguous, changed, or already-running PID fails closed and is never adopted. The Broker selects an unused dynamic port in `49152..65535`, closes its probe socket immediately before exact-UDID launch, and passes `127.0.0.1:<port>` in `SIMCTL_CHILD_APPPILOTKIT_TRANSPORT_DESCRIPTOR`. The value is unpadded base64url of the deterministic-CBOR non-secret descriptor: version, platform, lease id, target nonce, app artifact SHA-256, one-time Broker static public key, exact loopback endpoint, expiry, and Ready Target Reference digest. The Debug/Internal Target binds only that loopback endpoint and uses the supplied digest in the NK prologue and M1; Broker recomputes it from the full in-memory reference and requires equality. Snapshot/install/PID ownership is a D0 contract obligation; this checkpoint does not claim that libproc attribution, the adapter, or a real Simulator journey is implemented. A same-current-user process that obtains the public descriptor and wins the endpoint race is an explicit residual, not a defended actor. No secret enters the environment.

Android Emulator commands always use `adb -s <serial>`. One Debug-only Activity extra carries the unpadded-base64url deterministic-CBOR descriptor; that descriptor itself contains the random localabstract name and Ready Target Reference digest. There is no duplicate endpoint extra. Target opens `LocalServerSocket`; Broker creates `adb forward tcp:0 localabstract:<exact-name>` and connects only to the returned loopback port. `LocalSocket.getPeerCredentials()` must match the expected shell/adbd UID or the platform is unsupported; this check is defense-in-depth, not Host identity. There is no `adb reverse`, INET Target listener, fixed port, secret extra, or secret argv.

The defended threat boundary covers other OS users at Broker IPC, unrelated Android app processes, wrong Noise roles, modification, replay, cross-target/lease/generation/epoch use, endpoint mix-up, stale state, product-controlled secret surfaces, LAN exposure, discovery, fixed global ports, and mDNS. It excludes root/kernel/hypervisor, compromised same-current-user processes, any same-current-user process that obtains the public descriptor and wins its endpoint race, debugger/ptrace, swap/hibernation/crash dumps, compromised CoreSimulator/ADB/adbd/usbmux/platform tooling, another authorized ADB Host racing bootstrap, and replacement of non-secret launch metadata. Expanding the defended set requires an external pairing/trust decision and blocks this contract rather than weakening it.

## Release and security negative proof

Every implementation checkpoint records all of these results:

- `git diff --exit-code 09c846d86d0a18b0ccc6ca2e3fc6f00c305425b3 -- protocol/v1.2 cli/contracts/v1 cli/crates/cli-contract/src ios/Sources/AppPilotKit android/protocol-runtime/src/main android/semantic-registry/src/main` is empty.
- The iOS root `Package.swift` products and public symbol graph equal base. A Release-negative app with no internal transport link has no transport build edge; `nm`, `otool`, `strings`, merged resources, and runtime socket scan contain no listener, bootstrap, Noise, or FFI surface.
- Android Release merged manifest, resources, DEX/class listing, `aapt`, and `dexdump` contain no bootstrap Activity, transport class, `LocalServerSocket`, localabstract name, JNI load, or listener; runtime exposes no abstract socket. Production JVM module APIs equal base.
- Rust `cli-contract` registry/API and Protocol v1.2 hashes equal base. The reference crate is absent from the production workspace dependency graph.
- Wrong proof, tamper, replay, cross-target/lease/generation/epoch, wrong role/UID, partial/oversize/trailing/EOF/deadline, record ordering, nonce/byte limit, Broker loss/TTL, foreground loss, and cleanup failure each fail closed before Transport Handoff unless a vector's raw emitted/reassembled bytes prove otherwise.
- Outside the checked-in vector/reference corpus, the fixed synthetic TEST-ONLY canary and a distinct execution-unique runtime canary have zero occurrences across actual product argv, environment, Activity extras, stdout, stderr, product logs, diagnostics, Machine Results, Next Actions, Artifacts, the Smoke Host build artifact, each Production build artifact, and each Release build artifact. Every surface records scanner name/version, the exact scan command or canonical operation, captured artifact identity/path/SHA-256, byte count, both match counts, and completeness. Each build capture also records an explicit App id, build, configuration, platform-fixed `artifact_encoding`, and artifact digest. The iOS captures are complete `ios-app-tree-v1` streams, not per-file manifests. The Smoke Host capture is byte-identical to `app.artifact_bytes_base64url` and matches the evidence App id/build/encoding/digest. Production and Release captures bind to their own explicitly identified artifacts, must differ from the Smoke Host artifact, and may legitimately equal each other. Missing, unreadable, zero-byte, malformed, decoy, exchanged, or Smoke-rebranded captures fail. The dev/spec-only reference binary is not a product or acceptance artifact and is deleted after verification.
- The old ignored Android probe and any fake adapter, fixture socket, private direct handler, `@testable` state, or canned trace are absent from the real evidence chain.

## Evidence boundary

`generate` only writes deterministic positive goldens, raw hostile inputs, and manifests. `verify` never calls the generator or trusts vector-supplied validator/stage/classification labels. A pinned suite/case table selects one validator for each exact id. Every negative is a positive control plus one cause mutation over raw/canonical fields. The verifier independently consumes raw CBOR, outer frames, record sequences, lifecycle events, Noise ciphertext, retained capture bytes, and literal outcomes; it decrypts/reassembles both positive crypto suites and verifies transcript hashes. Broker-loss counts and END state are derived against the positive session request/response ciphertext, including complete-response success controls. Exact-lease reuse binds its observed bootstrap inventory to the positive NK transcript and rejects any second-prepare transcript. Close records decode deterministic CBOR and validate the complete close-reason/Handoff enums. Synthetic TEST-ONLY canaries and deterministic private keys exist only in checked-in vector/reference material. Product-controlled surfaces require zero matches; hostile canary-hit controls require a real recomputed match count greater than zero and reject dishonest declared counts.

Local contract evidence cannot close the real journey. Completion requires new tracked, rerunnable iOS and Android Smoke Host evidence matching `schema/transport-evidence.schema.json`. The ignored historical Android probe is observation-only and cannot be generated, cited, or promoted as real acceptance.

## Reference-tool use

`reference/` is an independent `publish = false`, dev/spec-only crate and is not a member of the repository workspace. Its `plist = 1.8.0` pin is reference-only and lets the independent artifact verifier validate both XML and binary `Info.plist`; no production dependency is introduced. Its reverse inventory excludes only the root `manifest.json` self-reference; any other file at any depth is normative source and must be pinned. `verify --evidence <path>` reads one retained bundle from raw bytes, rejects duplicate JSON object members recursively before schema parsing, validates the transport evidence schema, and applies the same semantic evidence validator used by the contract suite. Keep Cargo output outside the contract tree and run:

```text
CARGO_TARGET_DIR="$(mktemp -d /tmp/apppilotkit-transport-reference.XXXXXX)" cargo run --locked --manifest-path transport/contracts/v1/reference/Cargo.toml -- verify
CARGO_TARGET_DIR="$(mktemp -d /tmp/apppilotkit-transport-reference.XXXXXX)" cargo run --locked --manifest-path transport/contracts/v1/reference/Cargo.toml -- verify --evidence /absolute/path/to/retained-evidence.json
CARGO_TARGET_DIR="$(mktemp -d /tmp/apppilotkit-transport-reference.XXXXXX)" cargo test --locked --manifest-path transport/contracts/v1/reference/Cargo.toml
```

`generate` deterministically rewrites only the vector and manifest files it owns and does not invoke `verify`. `verify` always runs the independent fixture/vector paths and supports `verify --fixture <checked-in-fixture>` for focused hostile tests. Reviewers must verify a clean second generation. Production crates must not depend on this reference crate.
