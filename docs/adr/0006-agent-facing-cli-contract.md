# ADR 0006: Agent-facing CLI contract

- Status: Accepted
- Date: 2026-08-02
- Issue: [#13](https://github.com/patrick-fu/AppPilotKit/issues/13)

## Context

AppPilotKit must remain usable by a coding Agent that has only the installed
desktop CLI: no Skill, MCP adapter, repository checkout, device credential, or
prior product knowledge. Human-oriented help alone is insufficient, while
ad-hoc JSON flags and prose errors would force each Agent integration to infer
command behavior, safety, and recovery differently.

The CLI also has two output channels and three result modes. A late JSONL
failure cannot move to stderr without splitting one structured event stream
across file descriptors. Non-idempotent mobile actions add a separate hazard:
timeout or disconnect can leave execution unknown, so a generic retry hint can
repeat a gesture or text entry that already happened.

## Decision

### One versioned machine contract

Every public command is described by one versioned CLI machine contract that
is independent of the device protocol version. Machine Results use snake-case
JSON fields and contain:

- `schema_version`, `cli_version`, and terminal `status`;
- `command` as the canonical command-path string array;
- scalar primary `side_effect` and independent `retry_safety` classifications;
- exactly one of command `data` or a structured `error`;
- `disclosure`, `artifacts`, and ordered `next_actions` arrays.

The initial CLI machine-contract `schema_version` is the string `"1.0"`;
`cli_version` is a SemVer string. The two versions advance independently.

Terminal status is `succeeded`, `failed`, or `cancelled`. Errors expose a stable
`kind`, human `message`, `retryable`, and redacted structured `details`. Agents
branch on `error.kind`, never on the message or exit status alone. Protocol
error kinds remain unchanged when they cross the CLI seam; CLI-owned errors
use namespaced kinds such as `transport.authenticationRequired`,
`target.selectionRequired`, `action.outcomeUnknown`, and
`artifact.alreadyExists`.

The checked-in JSON Schema 2020-12 files for this contract are the source of
truth. Rust types may be tested against those schemas but do not regenerate or
replace them.

### Output and channel rules

Public commands accept `--output human|json|jsonl`. The deterministic default
is `human`; machine callers select `json` or `jsonl` explicitly.

- Human success and help use stdout; human diagnostics and failures use
  stderr.
- JSON emits exactly one newline-terminated Machine Result on stdout for both
  success and failure. Stderr contains diagnostics only.
- For a valid invocation, JSONL starts with `run.started`, may contain bounded
  command-specific events, and ends with exactly one terminal event:
  `run.succeeded`, `run.failed`, or `run.cancelled`. Every event carries
  `schema_version`, `cli_version`, and one invocation-scoped `run_id`; started
  events also carry the canonical command path and its safety classifications,
  while terminal events carry the complete Machine Result fields. A late
  failure never moves the terminal event to stderr.
- Help and version output use stdout and exit zero. If raw argv contains one
  unambiguous valid `--output json` or `--output jsonl`, invalid invocation
  syntax produces a structured `cli.invalidInvocation` failure on stdout with
  exit `2`; JSONL may contain only that terminal event because command execution
  never started. Human, missing, invalid, duplicate, or conflicting output-mode
  selection instead produces a parser diagnostic on stderr with empty stdout
  and exit `2`.
- Machine modes and `--non-interactive` never prompt or consume implicit input.

The empty command array is the canonical root path. A structured invalid
invocation reports the longest recognized canonical command prefix, falling
back to the root, with `side_effect: read_only` and `retry_safety: safe` because
no handler ran. Unknown user tokens never appear in `command`.

The terminal event or JSON status must agree with the process exit status.
Exit statuses are stable categories; detailed behavior remains in
`error.kind`:

| Status | Category |
| ---: | --- |
| `0` | Success. |
| `1` | Internal or otherwise unclassified operational failure. |
| `2` | Invalid invocation or usage. |
| `3` | Transport or authentication unavailable. |
| `4` | Target selection, app state, snapshot, or another precondition requires a changed request. |
| `5` | A mutating operation may have executed but its outcome is unknown. |
| `6` | Protocol contract, peer-message, or capability incompatibility. |
| `7` | Local artifact or output-path conflict. |
| `130` | A handled interrupt after bounded cleanup and a rendered `cancelled` result. |

Current error kinds map to those categories as follows:

- local invocation validation uses `cli.invalidInvocation` and exit `2`;
- transport/authentication kinds use exit `3`;
- target and app-state kinds plus protocol `sessionExpired`, `cursorExpired`,
  `ui.snapshotExpired`, and `ui.referenceNotFound` use exit `4` because
  recovery requires a changed target, request, session, cursor, or snapshot;
- `action.outcomeUnknown` alone uses exit `5`;
- peer `parseError`, `invalidRequest`, `methodNotFound`, `invalidParams`,
  `incompatibleProtocol`, and `capabilityUnavailable` use exit `6`; local
  argument validation never masquerades as a peer `invalidParams` response;
- artifact/output conflicts, including `artifact.alreadyExists`, use exit `7`;
- `internalError`, `resourceExhausted`, and a `timeout` whose execution outcome
  is known use exit `1`. A mutating timeout that may have executed is converted
  to `action.outcomeUnknown` instead.

New error kinds choose a category by these semantics; they do not allocate a
new process status merely to gain a unique number.

An unhandled operating-system signal does not render a Machine Result or call
`exit(130)`; the shell observes signal-derived termination. The CLI uses exit
`130`, terminal status `cancelled`, and `error.kind: run.cancelled` only when it
intercepts an interrupt, performs bounded cleanup, successfully writes the
terminal result, and can establish that the operation did not execute or was
definitively cancelled. The result is retryable only when its declared
`retry_safety` is `safe`. If a mutation may already have executed, ambiguity
takes precedence over cancellation: the CLI returns failed
`action.outcomeUnknown`, exit `5`, and
`unsafe_after_ambiguous_result` instead.

### Self-guiding discovery

Root and command help name the safe starting workflow, side-effect class,
result modes, errors, bounded-output behavior, and recovery commands. Three
public commands are available without network, authentication, device lookup,
or prompting:

- `capabilities --output json` returns the complete installed manifest,
  including the injected `executable` token, versions, command paths, canonical
  arguments and aliases, result fields, error kinds, side-effect classes, and
  retry-safety values;
- `schema list --output json` returns every installed machine-result schema
  identifier, while `schema show <schema-id> --output json` returns the exact
  embedded schema as `data.schema` inside a Machine Result together with
  `data.schema_id`. “Exact” means structural JSON equality with the embedded
  resource, not byte-for-byte output outside the result envelope;
- `doctor --output json --non-interactive` checks local prerequisites and
  explicitly marks checks that require a device or credential as skipped.

A single declarative command-contract registry owns parser metadata plus the
domain annotations that `clap::Command` cannot represent: result schema IDs,
error kinds, primary side effect, retry safety, and result fields. It builds the
`clap::Command`, help, and capabilities manifest; none is maintained as a
separate list. Black-box tests compare the complete public records and the
registry-to-parser projection so an omission, duplicate, alias drift, annotation
drift, or hidden lookup fails verification.

### Recovery and safety

Each Next Action contains `id`, an exact `argv` string array, `side_effect`,
`retry_safety`, ordered `preconditions`, and a human `reason`. The first argv
entry is the executable token reported by the installed CLI; entries are never
combined into a shell string and never contain secrets. Next Actions are
bounded and deterministically ordered.

Side-effect classes describe what can change: `read_only`, `local_write`,
`app_mutation`, or `device_mutation`. The field is one primary class with risk
precedence `device_mutation`, `app_mutation`, `local_write`, then `read_only`.
An app mutation that also writes evidence is therefore `app_mutation`; its
Artifact descriptors disclose the additional local writes. Retry safety
separately describes the conditions for repetition: `safe`,
`requires_idempotency_key`,
`requires_artifact_conflict_policy`, or `unsafe_after_ambiguous_result`.
`requires_idempotency_key` means a retry is permitted only when the installed
command contract exposes a key mechanism and the exact same request and key can
be reused. Until then, the CLI emits no retry Next Action for that class; key
generation, binding, and retention remain decisions of the future command that
introduces the mechanism.

After an ambiguous `tap`, `swipe`, or `type` result, the Machine Result uses
`action.outcomeUnknown`, exit status `5`, `retryable: false`, and
`unsafe_after_ambiguous_result`. Its Next Actions may inspect current state or
query operation status but must not replay the mutation.

Potentially large results use a snake-case projection of ADR 0001 disclosure:
`truncated`, `returned_items`, optional `applied_limits`, and—only when
truncated—a non-empty `reasons` array and opaque `next_cursor`. Protocol-backed
commands preserve the negotiated values; CLI-local bounded commands apply the
same invariants. When an installed command accepts that cursor, truncation
exposes a safe continuation Next Action for that command; contracts without an
installed cursor consumer must not invent future argv. Artifacts are no-clobber
by default and disclose absolute path, media type, byte size, digest, and
sensitivity. Replacing an existing artifact requires explicit authorization.

## Consequences

- An Agent can learn the complete installed workflow and recover without a
  separate Skill or parsing human prose.
- Structured success and failure remain one valid stdout stream, including
  late JSONL failures; stderr stays safe for diagnostics.
- Command implementations return one domain outcome to a deep rendering and
  discovery module instead of reproducing framing, exit, safety, and guidance
  rules in every caller.
- CLI schemas and exit categories become compatibility commitments and require
  explicit versioning when changed incompatibly.
- This decision does not choose the public executable name, transport,
  packaging, device discovery implementation, SDK integration, screenshots,
  or action backends.

## Rejected alternatives

- **Structured failures on stderr:** splits JSONL after a late failure and
  forces Agents to merge two machine streams.
- **Environment-dependent output defaults:** makes identical argv produce a
  different contract based on TTY detection.
- **One exit status per error kind:** exhausts a weak process-level channel and
  encourages callers to ignore the richer stable kind.
- **Shell-string recovery commands:** require quoting and shell interpretation
  and can accidentally expose or execute unsafe text.
- **One combined side-effect/retry field:** cannot represent a read-only app
  operation that writes a local artifact or a mutation with an unknown result.
- **Separate hand-maintained help and manifest models:** allows the Agent-facing
  contract to drift from the actual parser.
