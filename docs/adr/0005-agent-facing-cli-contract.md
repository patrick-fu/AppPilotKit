# ADR 0005: Agent-facing CLI contract

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
- `side_effect` and `retry_safety` as separate classifications;
- exactly one of command `data` or a structured `error`;
- `disclosure`, `artifacts`, and ordered `next_actions` arrays.

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
- JSONL emits every structured event on stdout and exactly one terminal event:
  `run.succeeded`, `run.failed`, or `run.cancelled`. A late failure never moves
  the terminal event to stderr.
- Help and version output use stdout and exit zero. Invalid invocation syntax
  uses stderr and the usage exit status.
- Machine modes and `--non-interactive` never prompt or consume implicit input.

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
| `6` | Protocol, session, or capability incompatibility. |
| `7` | Local artifact or output-path conflict. |
| `130` | Caller cancellation corresponding to an interrupt. |

### Self-guiding discovery

Root and command help name the safe starting workflow, side-effect class,
result modes, errors, bounded-output behavior, and recovery commands. Three
public commands are available without network, authentication, device lookup,
or prompting:

- `capabilities --output json` returns the complete installed manifest,
  including versions, command paths, canonical arguments and aliases, result
  fields, error kinds, side-effect classes, and retry-safety values;
- `schema list --output json` returns every installed machine-result schema
  identifier, while `schema show <schema-id> --output json` returns the exact
  embedded schema;
- `doctor --output json --non-interactive` checks local prerequisites and
  explicitly marks checks that require a device or credential as skipped.

Parsing, help, and the capabilities manifest are derived from the same command
model. Black-box tests compare the complete public command and argument records
so an omission, duplicate, alias drift, or hidden lookup fails verification.

### Recovery and safety

Each Next Action contains `id`, an exact `argv` string array, `side_effect`,
`retry_safety`, ordered `preconditions`, and a human `reason`. The first argv
entry is the executable token reported by the installed CLI; entries are never
combined into a shell string and never contain secrets. Next Actions are
bounded and deterministically ordered.

Side-effect classes describe what can change: `read_only`, `local_write`,
`app_mutation`, or `device_mutation`. Retry safety separately describes the
conditions for repetition: `safe`, `requires_idempotency_key`,
`requires_artifact_conflict_policy`, or `unsafe_after_ambiguous_result`.

After an ambiguous `tap`, `swipe`, or `type` result, the Machine Result uses
`action.outcomeUnknown`, exit status `5`, `retryable: false`, and
`unsafe_after_ambiguous_result`. Its Next Actions may inspect current state or
query operation status but must not replay the mutation.

Potentially large results use ADR 0001 disclosure semantics. Truncation exposes
an opaque cursor and a safe continuation Next Action. Artifacts are no-clobber
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
