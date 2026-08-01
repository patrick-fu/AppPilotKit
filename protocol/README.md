# Protocol

This directory owns the transport-independent wire contracts shared by the CLI
and mobile SDKs. It does not normalize platform UI models.

## Version 1.0 core

- `v1/schema/envelope.schema.json` defines the strict JSON-RPC profile,
  session context, and structured errors.
- `v1/schema/session.schema.json` defines version/capability negotiation.
- `v1/schema/disclosure.schema.json` defines output limits, truncation, and
  opaque continuation cursors.
- `v1/fixtures` contains valid and invalid contract examples.
- `v1/negotiation-cases.json` covers cross-message version, request-ID, and
  capability invariants.
- `v1/disclosure-cases.json` covers request/session limit clamping.

## Version 1.1 UI inspection

- `v1.1/schema/ui.schema.json` defines `ui.snapshot`, `ui.inspect`,
  snapshot-scoped references, searchable node indexes, and provider-native
  details.
- `v1.1/schema/envelope.schema.json` adds namespaced UI reference errors.
- `v1.1/schema/session.schema.json` pins minor 1 capability negotiation.
- `v1.1/fixtures` covers compact and full trees, multiple providers, selectors,
  traversal, graph integrity, byte limits, and stale references.
- `v1.1/pagination-cases.json` verifies opaque-cursor continuation and final
  page progress across messages.
- `v1.1/string-matching-cases.json` fixes cross-platform string predicate
  semantics, including mixed Unicode candidates.

See [ADR 0002](../docs/adr/0002-ui-snapshot-and-inspection.md) for selection,
reference lifetime, traversal, and platform-preservation rules.

The Node package is only a contract-test harness; it does not select the final
CLI implementation language.

Install dependencies and run every schema and semantic fixture:

```sh
npm --prefix protocol ci
npm --prefix protocol test
```

See [ADR 0001](../docs/adr/0001-protocol-envelope-and-compatibility.md)
for invariants and compatibility rules.
