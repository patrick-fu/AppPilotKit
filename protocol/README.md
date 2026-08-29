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

## Version 1.2 semantic capabilities

- `v1.2/schema/semantic.schema.json` defines the negotiated
  `semantic.list`, `semantic.show`, `semantic.schema`, `semantic.query`, and
  `semantic.invoke` methods. The only new session capability is
  `semantic.catalog`; it is not a member of an App's Semantic Catalog and is
  never emitted by v1.0 or v1.1 negotiation.
- Catalog membership is immutable for a process generation. `semantic.list`
  is the only paginated semantic method. Its opaque cursor is bound to the
  session, process generation, method, original parameters, and catalog.
  Catalog items contain only `id`, `kind`, and `declarationRevision`.
- `semantic.show` returns only a static declaration. Resource declarations
  identify an optional input schema and required value schema. Action
  declarations identify their input schema plus explicit authorization and
  retry-safety policy. Neither declaration contains values or business output.
- `semantic.schema` retrieves one atomic App JSON Schema by stable
  `{id, revision, digest}` handle. The document identifies JSON Schema
  2020-12 with `$schema` and a URI `$id`; it is never a Resource value.
  An oversized schema fails with `resourceExhausted`, never pagination or
  truncation.
- `semantic.query` is read-only and returns one opaque value, its value-schema
  handle, and disclosed UTF-8 value bytes. Optional input and input-schema
  handles are paired. `semantic.invoke` routes every mutation through the
  Target Action Coordinator and confirms only handler completion; it never
  returns business output, rolls back, or retries automatically.
- Calls echo `declarationRevision` and schema handles to reject stale
  declarations and schemas. App schema revisions may change while protocol
  minor 1.2 remains unchanged.
- `v1.2/schema/envelope.schema.json` adds the closed Safe Error Context for
  both new and inherited errors. Error messages are stock code/kind strings;
  typed input, values, and authorization grants cannot appear in either a
  message or `details`. It defines
  one-to-one errors: `semantic.capabilityNotFound` (`-32020`),
  `semantic.schemaMismatch` (`-32021`), `semantic.unavailable` (`-32022`),
  `semantic.disclosureDenied` (`-32023`), `action.policyDenied` (`-32024`),
  `action.conflict` (`-32025`), and `action.outcomeUnknown` (`-32026`). An
  ambiguous action outcome is always `retryable: false`.
- `v1.2/fixtures`, `negotiation-cases.json`, and `semantic-cases.json` cover
  strict objects, negotiation isolation, bounded catalog pagination, schema
  evolution, safe errors, size limits, and action dispatch safety. The trace
  files referenced by `semantic-cases.json` are contract traces for harness
  assertions, not runtime acceptance evidence or an SDK execution format.

See [ADR 0008](../docs/adr/0008-app-registered-semantic-capabilities.md) for
the App-owned catalog, disclosure, and action-policy ownership boundaries.

The Node package is only a contract-test harness; it does not select the final
CLI implementation language.

Install dependencies and run every schema and semantic fixture:

```sh
npm --prefix protocol ci
npm --prefix protocol test
```

See [ADR 0001](../docs/adr/0001-protocol-envelope-and-compatibility.md)
for invariants and compatibility rules.
