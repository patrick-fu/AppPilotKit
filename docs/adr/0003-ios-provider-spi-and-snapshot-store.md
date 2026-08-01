# ADR 0003: iOS provider SPI and snapshot store

- Status: Accepted
- Date: 2026-08-02
- Issue: [#6](https://github.com/patrick-fu/AppPilotKit/issues/6)

## Context

Protocol v1.1 defines immutable UI snapshots, provider-native source identity,
snapshot-scoped node references, provider-owned redaction, and bounded snapshot
lifetime. The first iOS implementation needs to enforce those rules without
exposing registry or storage mechanics to later JSON-RPC, inspection, or
platform-collector modules.

Provider capture must run on the main actor because future UIKit, SwiftUI, and
accessibility adapters read main-thread-owned state. Storage, reference
assignment, and eviction do not need to run on the main actor.

## Decision

### Runtime seam

The Swift package exposes one deep `UISnapshotRuntime` actor. Its caller
interface has three operations:

- capture a provider selection in a session/process scope;
- resolve a retained snapshot in that scope;
- invalidate all retained snapshots in a scope.

Providers are registered only when the runtime is constructed. The registry,
graph validator, reference allocator, canonical encoder, and bounded store are
implementation details behind the runtime interface.

### Provider ownership and redaction

`UISnapshotProvider` has an immutable descriptor and one `@MainActor` capture
operation. The runtime invokes requested adapters in registration order,
regardless of request order.

An adapter must convert raw platform state into a detached,
value-semantic `RedactedProviderCapture` before returning. Provider-native
fields use JSON-safe value types, including exact signed and unsigned 64-bit
integers, and the store has no raw-capture type or unredacted bypass. The
adapter owns classification and redaction because only
it understands the native fields it emits; the runtime validates structure
and JSON safety but does not attempt heuristic redaction.

The retained record contains the complete redacted provider capture. Compact
selection and disclosure pagination are later projections over that immutable
record, not alternate stored snapshots.

### Identity and validation

Snapshot identity is resolved only together with its protocol session ID and
process generation. Successful commits receive a runtime-monotonic generation
and an opaque snapshot ID. Invalidating a scope does not reset the generation.

Provider-local node IDs are scoped to their source and are used only while
validating and assigning references. They are neither stored nor exposed.
The runtime assigns opaque node references centrally, so equal local IDs from
different providers or sources cannot collide. Provider-supplied source IDs
remain snapshot-local protocol identities and must be unique across the
logical snapshot.

Before reference assignment or storage, the runtime validates provider/source
agreement, iOS point coordinates, source metadata, JSON-safe payloads, root
cardinality, unique local identities, parent/depth adjacency, native
depth-first order, sibling indices, and child counts.

### Atomic capture and cancellation

Runtime capture requests are serialized in actor acceptance order even while a
provider awaits the main actor. Each request follows one pipeline:

1. validate the scope and provider selection;
2. capture every selected provider in registration order;
3. validate the complete logical capture;
4. assign opaque snapshot and node references;
5. measure the stored record;
6. commit once.

Provider failure, validation failure, cancellation before commit, or an
oversized record leaves the store unchanged. A provider result that arrives
after cancellation is discarded. Only a successful commit advances the
snapshot generation.

### Retention and eviction

The store is bounded by both snapshot count and total stored bytes. FIFO order
is the successful snapshot generation order; resolving a snapshot does not
refresh its position.

`storedBytes` is the UTF-8 byte length of compact JSON with sorted object keys
for the complete redacted record after reference assignment. The measured
record contains `scope`, `identity`, `sources`, and `nodes`; it excludes the
`storedBytes` bookkeeping field itself and any JSON-RPC envelope. A new record
larger than the total byte capacity fails with `resourceExhausted` before any
existing snapshot is evicted. Otherwise the store evicts oldest generations
until both bounds are satisfied.

Evicted, invalidated, unknown, or scope-mismatched identities all resolve as
`ui.snapshotExpired`. Invalid caller input maps to `invalidParams`; provider
failures and structurally invalid provider captures map to `internalError`
without exposing provider-local node identities. Capacity failures map to
`resourceExhausted`. This module adds no wire error kind.

## Consequences

- Later RPC and inspection modules learn one small runtime interface instead
  of coordinating providers, validation, identities, and retention themselves.
- UIKit, SwiftUI, accessibility, and future provider adapters retain their
  native schemas and redaction policies.
- Deterministic capture order, canonical byte accounting, and FIFO eviction
  are testable entirely through the caller interface.
- The retained snapshot can support later compact selection, inspection, and
  pagination without recapturing mutable platform state.
- This decision adds no transport, listener, screenshot, action, query,
  traversal, cursor, platform collector, CLI, or app integration.

## Rejected alternatives

- **Expose separate registry and store interfaces:** creates shallow modules
  and makes callers reproduce ordering, atomicity, and lifetime rules.
- **Redact inside the runtime:** requires generic code to understand every
  provider-native schema and risks storing raw sensitive state first.
- **Store only the compact response projection:** loses nodes required for
  later inspection and makes one logical snapshot change across requests.
- **Use LRU eviction:** makes read traffic alter lifetime and complicates
  deterministic cursor and test behavior.
- **Expose provider-local node IDs:** leaks adapter implementation details and
  permits collisions across overlapping native source trees.
