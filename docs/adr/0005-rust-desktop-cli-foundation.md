# ADR 0005: Rust desktop CLI foundation

- Status: Accepted
- Date: 2026-08-02
- Issues: [#14](https://github.com/patrick-fu/AppPilotKit/issues/14), [#16](https://github.com/patrick-fu/AppPilotKit/issues/16)

## Context

The desktop CLI must ship as a native macOS tool, run Apple and Android
platform processes, manage local sockets and bounded cancellation, validate the
repository's Draft 2020-12 protocol schemas fully offline, expose a self-guiding
command model, and publish sensitive local artifacts safely. Swift, Rust, Go,
and TypeScript/Node were evaluated against the same MVP requirements.

Research preferred Rust with Go as the runner-up but required an executable
reversal test before accepting the production language. The Issue #16 spike
then exercised offline schema and semantic-fixture parity, strict JSON and
JSONL, process-group cancellation and direct-child reaping, local TCP and Unix
sockets, injected Apple/ADB runners, atomic no-clobber artifacts, command-model
reflection, advisory/license burden, and native arm64/x86_64 distribution.

All eight gates passed on both native macOS architectures. The final locked
graph contained 142 packages and reported no Rust advisory or license blocker;
independent Standards and Spec reviews had no remaining P1/P2 findings. No
reversal condition justified the identical Go comparison.

## Decision

The production desktop CLI uses Rust in the existing `cli/` Cargo workspace.
It follows a library-first shape: command handlers and orchestration return
typed outcomes, one top-level module owns stream rendering and exit status, and
the executable remains a thin writer. The public executable name is a separate
decision.

The foundation uses:

- `clap` for typed parsing, authoritative help, and reflection from one command
  model;
- Tokio for child processes, local sockets, deadlines, signals, and explicit
  cancellation;
- Serde for typed JSON and a recursive duplicate-key guard before typed or
  schema parsing;
- `jsonschema` with default remote resolvers disabled and only embedded schema
  resources registered;
- checked-in Cargo locks, exact direct dependency pins for sensitive contract
  tooling, and native arm64/x86_64 CI.

Checked-in repository JSON Schema remains the source of truth for both protocol
and Rust-owned CLI contracts. Rust types may be generated from those schemas
with pinned settings, but Rust tooling never authors, regenerates, or replaces
the checked-in schemas.

Platform processes and tunnels sit behind injected internal adapters. Owned
child processes use process groups where supported; cancellation follows a
bounded graceful signal, forced termination, direct-child wait, output drain,
and managed-process-group-empty check. This does not claim POSIX can wait an
arbitrary grandchild or prevent a descendant from deliberately escaping its
process group.

Artifacts stream to a sibling temporary file while hashing, sync before
exclusive no-clobber publication, and report directory-sync durability
separately. Production listeners, device transport composition, packaging,
notarization, and a universal binary remain outside this foundation decision.

## Consequences

- The CLI gains one native, cross-platform host implementation for iOS and
  Android workflows without coupling SDK code to the host language.
- The repository carries a Rust toolchain, Cargo workspace, locked dependency
  graph, native dual-architecture CI, and ongoing advisory/license maintenance.
- Schema, process, socket, rendering, and artifact behavior proven by the spike
  must be reimplemented behind production module interfaces; the spike package
  and its test-only command syntax are not promoted wholesale.
- Swift remains available inside the iOS SDK, Kotlin inside the Android SDK,
  and Apple/ADB tools remain external adapters rather than product models.

## Rejected alternatives

- **Go with Cobra:** the runner-up had no demonstrated advantage after Rust
  passed every reversal gate; adding an identical implementation would delay
  the MVP without resolving remaining risk.
- **Swift with Argument Parser:** strong for Apple frameworks but weaker for a
  shared iOS/Android host CLI and a less mature Draft 2020-12 schema stack.
- **TypeScript/Node with Commander:** fastest prototype path, but materially
  heavier startup/runtime distribution and weaker descendant-process control
  for the durable native tool.
- **Promote the Rust spike directly:** its package name, command syntax, result
  selectors, and interfaces are disposable evidence rather than a production
  contract.
