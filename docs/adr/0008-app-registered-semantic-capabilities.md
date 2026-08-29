# ADR 0008: App-registered semantic capabilities

- Status: Accepted
- Date: 2026-08-29
- Issue: [#46](https://github.com/patrick-fu/AppPilotKit/issues/46)

## Context

Raw UI inspection and Action Intents cannot safely express every useful
application operation. The existing Semantic Action provides an App-registered
mutation seam, but Agents also need bounded, App-defined read-only domain
observations. This is not an arbitrary App-state dump. The generic SDK and CLI
cannot understand application experimentation, gradual rollout, remote
configuration, or other business semantics.

The integration seam is hard to reverse. It determines where business
dependencies, typing, schema evolution, safety classification, and runtime
availability live; making them generic-SDK concerns would permanently spread
application knowledge across both SDKs and the CLI.

## Decision

### App-owned catalog seam

An Opted-in App explicitly registers its Semantic Capabilities in its
Debug/Internal composition root, where it constructs the needed business
dependencies. The resulting flat Semantic Catalog is frozen before the Target
begins serving requests. There is no required dependency-injection container,
reflection, service lookup, or runtime registration path.

A Semantic Resource is an App-registered read-only domain observation.
Semantic Action retains its existing meaning as an App-registered domain
operation with explicit policy metadata. Catalog membership is immutable for
one process generation; a registered capability's value and availability are
evaluated per invocation. Losing a Listener Epoch invalidates Protocol Sessions
but does not replace the Catalog; only a process generation change does.

Business handlers remain native, strongly typed App code and declare explicit,
versioned JSON Schemas at the App/generic-runtime seam. An App-owned Semantic
Capability adapter translates those handlers to detached JSON. It owns
classification, redaction, and encoding before the generic SDK protocol
runtime can hold the payload; Secret Content is never serialized, and an
unclassified field or incomplete redaction fails the whole response. The
generic runtime does not decode or interpret business semantics, nor does it
model business rollout, experiments, or remote configuration.

### Negotiated access and independent policies

Consistent with ADR 0001, a compatible protocol minor will define a negotiated
semantic-protocol family through which CLI and Agent clients can dynamically
list capabilities, show their declarations, obtain schemas, query Semantic
Resources, and invoke Semantic Actions. This protocol family is distinct from
both Semantic Catalog membership and a session's negotiated Protocol
Capabilities. An App schema revision does not automatically require a protocol
minor revision; a protocol minor changes only when the generic wire behavior
changes. An unnegotiated or unavailable operation fails closed under ADR 0001.

The SDK protocol runtime owns Catalog exposure, protocol negotiation, request
and response limits, and concurrent read-only Semantic Resource queries. The
Target Action Coordinator exclusively owns Semantic Action dispatch, Effective
Action Policy, Single-Writer enforcement, evidence, and ambiguity handling.
The CLI is a generic protocol client: it renders and forwards declared data
without interpreting business fields or policies.

Three decisions stay distinct:

- **Discovery** decides whether a registered capability and its safe
  declaration can be named to this session.
- **Disclosure** decides whether a Resource schema, value, or Action-related
  data may leave the Target, subject to App policy and redaction.
- **Effective Action Policy** decides whether a requested Action mutation may
  proceed, including authorization and safety handling.

Discovery grants neither value disclosure nor Action invocation. Disclosure
does not authorize a mutation. The effective policy for an Action remains the
single resolved policy described in the existing Action model.

Schemas, categories, redaction, request and response size bounds, and every
applicable policy are independently fail-closed. Missing, conflicting,
unclassified, invalid, or oversized data is not disclosed or dispatched. The
generic runtime must not substitute a permissive default, inspect typed
business input in errors, or infer a policy from a capability name.

Semantic Actions continue through the Target Action Coordinator and therefore
retain Single-Writer enforcement, evidence, and `action.outcomeUnknown`
semantics. This decision adds no automatic business validation, rollback, or
retry. Upper-layer Adapters use the same negotiated capability contract and
cannot bypass registration, disclosure, or Action-policy paths.

Offline Machine Discovery remains the installed CLI contract from ADR 0006;
it never enumerates a Target Catalog. App schemas are retrieved on demand only
through an opened Protocol Session. An Agent without a Skill uses the installed
generic commands to establish that session and traverse the negotiated
semantic-protocol family.

## Consequences

Issue #46's MVP includes self-registered Semantic Resources and Semantic
Actions, the frozen per-generation Catalog, explicit schema evolution, and
dynamic negotiated discovery, declaration/schema retrieval,
resource queries, and Action invocation. It does not add generic-SDK support for
business experimentation, gray rollout, cloud control, dynamic registration,
automatic business validation, rollback, retry, or arbitrary App-state dumps.

The App owns business dependency construction, domain typing, schema meaning,
capability declarations, and pre-runtime disclosure. The generic SDK and
Protocol gain one deep seam that can serve both platforms and clients without
interpreting native handler types. Catalog immutability makes sessions and
compatibility deterministic; dynamic values and availability avoid
re-registration for ordinary App state changes.

Issue #46 cannot close without a blocking Demo Scenario and Acceptance Journey
that exercise list, show, schema retrieval, Resource query, and Action
invocation; distinguish immutable membership from per-invocation availability;
prove disclosure with a Fixture Canary; and prove that a process restart yields
a new Catalog.

## Rejected alternatives

- **Reflection, a DI container, or dynamic registration:** makes capability
  membership, policy, and typing depend on runtime discovery rather than an
  explicit App-owned composition root.
- **Generic-SDK-owned AB, rollout, or cloud-control models:** encodes
  application business semantics into a cross-platform diagnostic runtime that
  cannot judge them correctly.
- **A single permission decision for discovery, disclosure, and invocation:**
  lets seeing a declaration accidentally grant data access or mutation rights.
- **Unversioned JSON maps or generic-runtime-decoded business handlers:**
  removes native type checking and turns business-schema evolution into an
  implicit generic-runtime compatibility promise.
- **Adapter-specific execution paths:** duplicate negotiation and create ways
  to bypass the Catalog's disclosure and Action-policy rules.
- **Automatic business rollback, validation, or retry:** assumes domain
  semantics the generic runtime cannot prove, and conflicts with ambiguous
  Action outcome handling.
