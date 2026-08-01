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

The Node package is only a contract-test harness; it does not select the final
CLI implementation language.

Install dependencies and run every schema and semantic fixture:

```sh
npm --prefix protocol ci
npm --prefix protocol test
```

See [ADR 0001](../docs/adr/0001-protocol-envelope-and-compatibility.md)
for invariants and compatibility rules.
