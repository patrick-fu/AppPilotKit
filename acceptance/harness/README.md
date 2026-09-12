# Installed CLI Journey7 harness

This harness is the shared public-evidence runner for `demo.catalog` (Journey7).
It drives the installed `apppilotkit` CLI against a platform Acceptance Host;
the harness is not itself a Target fixture. The contract is
[`acceptance/demo-catalog.contract.json`](../demo-catalog.contract.json).

Journey7 requires exactly three catalog capabilities at declaration revision 1:
the read-only Resource `acceptance.catalog.state`, the ordinary Action
`acceptance.catalog.increment`, and the destructive Action
`acceptance.catalog.reset`. The resource and actions expose dynamic availability
without changing catalog membership: the resource follows the
`available → unavailable → available` transition around the first two ordinary
increments, while the destructive action requires a process-bound authorization.
This fixture denies destructive grants, so the public Journey proves the
fail-closed path without minting or retaining a grant. After a process restart
the catalog generation changes, the `catalog-v1` seed is restored, ordinary
history is empty, and old Target/session identifiers are rejected.

The run also exercises fail-closed negative fixtures. Schema-mismatched,
undeclared, and oversized inputs must fail before side effects; destructive
invocation without authorization must not mutate state; unclassified or
over-limit disclosure must return the contract's denial/error kind. The fixed
secret canary must never appear in argv, environment, logs, Machine Results,
their public Artifacts, or retained evidence. The app/apk used as the prepare
input is a fixture, not a public result Artifact.

Run it with a staged installed prefix:

```sh
node acceptance/harness/installed-cli-harness.mjs \
  --config /absolute/path/to/catalog-run.json \
  --evidence /absolute/path/to/catalog-evidence.json
```

The platform scripts provide `catalog-run.json` with the exact six-key
production prepare request and the private release callback
`<prefix>/libexec/apppilotkit-target-prepare --release-fd=0 --output=json`.
The callback receives the old opaque Target only on stdin. Before each child is
started, the runner persists a minimal redacted record and preflights the
artifact. Evidence keeps exit status, byte counts/digests, safe result kinds,
catalog identity/generation, capability membership, and availability/restart
observations; it does not retain raw stdout/stderr/stdin, executable or artifact
paths, descriptors, tokens, or app content. The harness follows and validates
returned `catalog.*` Next Actions and only derives later schema/query/invoke
arguments from a returned Target-bound action; it never substitutes an
unrelated capability or arbitrary Target/session.

This README describes the Journey7 contract and evidence boundary; it does not
claim that every native/platform matrix has passed. The older
`demo.foundation` contract and its tests remain supported as historical
compatibility assets, but are not the Journey7 scenario.
