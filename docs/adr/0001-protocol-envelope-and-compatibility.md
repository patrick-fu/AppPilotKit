# ADR 0001: Protocol envelope and compatibility

- Status: Accepted
- Date: 2026-08-01
- Issue: [#2](https://github.com/patrick-fu/AppPilotKit/issues/2)

## Context

The iOS SDK, Android SDK, and desktop CLI need one transport-independent seam
for requests, responses, session identity, errors, and bounded disclosure.
Platform inspection and action payloads will vary, so the core protocol must
not introduce a shared lossy UI model.

Transport authentication is established before protocol messages are
accepted. It is intentionally separate from the protocol session: the
transport proves access to the opted-in app, while the protocol session pins a
negotiated version, capabilities, limits, and process generation.

## Decision

### Envelope

AppPilotKit v1 is a strict JSON-RPC 2.0 profile encoded as UTF-8 JSON.

- Each message is one JSON object. Batch requests are unsupported.
- Every request has a non-empty string `id`; notifications are unsupported.
- Method names are dot-separated and namespaced, such as `session.open`.
- Success and error are mutually exclusive response shapes.
- Fields not declared by the selected protocol-minor schema are rejected.
- The envelope is independent of HTTP, TCP, usbmux, and ADB forwarding.

This is the shared module interface. Transport adapters only deliver complete
authenticated messages; platform modules own method-specific payloads behind
that seam.

### Session establishment

`session.open` is the first protocol request after transport authentication.
The client supplies one protocol major, a supported minor range, its identity,
and any required capabilities. The server selects the highest mutually
supported minor and returns:

- a session ID and process generation;
- the selected protocol version;
- available capabilities;
- hard request, response, and page limits.

All later requests carry the returned session context in the envelope. A
listener restart or app-process restart invalidates the prior context. Session
IDs are correlation identifiers, not authentication secrets.

### Compatibility

- Protocol majors must match exactly.
- A server selects a minor inside the client's advertised inclusive range.
- Adding optional behavior within a major requires a new minor and, when the
  caller must opt in, a named capability.
- The server emits only fields valid for the selected minor.
- Removing or renaming fields, changing existing semantics, or changing an
  error's meaning requires a new major.
- A missing required capability fails `session.open`; callers never
  optimistically invoke unavailable behavior.

The current `protocol/v1` schema path is the immutable v1.0 contract. A future
minor gets an explicit sibling path such as `protocol/v1.1`; negotiation selects
the matching immutable schema set rather than widening an older minor in place.

### Errors

JSON-RPC codes remain available for generic tooling. Callers branch on the
stable `error.data.kind`, never on the human-readable message.

| Code | Kind | Meaning |
| ---: | --- | --- |
| `-32700` | `parseError` | The bytes are not a valid JSON message. |
| `-32600` | `invalidRequest` | The envelope is invalid. |
| `-32601` | `methodNotFound` | The selected protocol does not expose the method. |
| `-32602` | `invalidParams` | Method parameters are invalid. |
| `-32603` | `internalError` | The provider failed unexpectedly. |
| `-32001` | `incompatibleProtocol` | No compatible protocol version exists. |
| `-32002` | `sessionExpired` | Session ID or generation is no longer valid. |
| `-32003` | `capabilityUnavailable` | A required capability is unavailable. |
| `-32004` | `resourceExhausted` | A safe bounded result cannot be produced. |
| `-32005` | `timeout` | The operation exceeded its deadline. |
| `-32006` | `cursorExpired` | The provider snapshot behind a cursor is gone. |

`retryable: true` means issuing an equivalent new request is safe. It must be
`false` when a non-idempotent operation may already have executed, even if its
result is unknown. Details are structured, optional, and must already be
redacted before serialization.

### Bounded disclosure

Potentially large methods accept optional `maxItems` and `maxBytes` hints. The
server applies values no larger than the limits negotiated by `session.open`.
Their responses include disclosure metadata with the effective limits and
returned item count.

Byte limits count the complete UTF-8 JSON message, including its envelope and
disclosure metadata. Item counting is defined by each method-specific schema.

A truncated response always declares one or more reasons and an opaque
`nextCursor`. A complete response contains neither. Cursors are bound to the
session, method, canonical original parameters, and provider snapshot; callers
must not inspect or edit them. A modified/malformed cursor request fails with
`invalidParams`; an unavailable provider snapshot fails with `cursorExpired`.

Output that cannot be safely truncated fails with `resourceExhausted` instead
of silently returning an ambiguous partial result.

### Contract source

JSON Schema 2020-12 files under `protocol/v1/schema` are the machine-readable
wire contract, including error code/kind correspondence. Fixtures and semantic
checks cover cross-message invariants JSON Schema cannot express, including
ordered minor ranges, response correlation, capabilities, and applied limits.

## Consequences

- SDKs can implement one small envelope/session module and keep platform-native
  providers behind it.
- CLI callers get deterministic IDs, errors, negotiation, and pagination
  without learning transport details.
- Minor-version behavior is explicit; strict schemas do not accidentally turn
  unknown fields into compatibility promises.
- UI snapshots, screenshots, and actions remain separate future contracts.

## Rejected alternatives

- **Custom RPC envelope:** duplicates established request, response, ID, and
  error semantics without useful leverage.
- **Protocol Buffers as the first wire format:** adds Swift/Kotlin/CLI codegen
  and makes agent-facing JSON inspection less direct before performance proves
  it necessary.
- **Session identity only in transport headers:** couples the public protocol
  to HTTP-like adapters and obscures replay/staleness behavior.
- **One normalized cross-platform UI schema now:** collapses UIKit, SwiftUI,
  Android View, Compose, and accessibility semantics before their providers are
  understood.
