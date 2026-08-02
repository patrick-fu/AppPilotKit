# Agent-Friendly CLI Contract Research (2026)

Status: research and design guidance, not an accepted protocol specification  
Last reviewed: 2026-08-02

## Scope and method

This note identifies CLI behaviors that let a coding agent discover, invoke, observe, recover, and verify AppPilotKit operations without repository-specific training. It covers:

- human and machine-readable output;
- process status and structured errors;
- command and schema discovery;
- non-interactive execution, authentication, and TTY behavior;
- progress, logs, and artifact handling;
- recovery, retries, idempotency, and protocol negotiation.

The evidence base is limited to current primary sources: standards, official product documentation, and official source code. Durable standards are separated from vendor conventions. Recommendations are synthesis for AppPilotKit, not claims that a universal agent CLI standard already exists.

## Durable findings

### Standard streams remain the strongest interoperability boundary

POSIX assigns normal output to stdout and diagnostics to stderr. A machine-readable mode is therefore reliable only when stdout contains protocol data and nothing else. Progress bars, debug logs, update notices, authentication guidance, and warnings belong on stderr. OpenAI Codex and Gemini hooks implement this separation explicitly.

TTY detection is insufficient as an interaction policy. POSIX `isatty()` only establishes that a file descriptor refers to a terminal; it does not establish that a human is present. An explicit non-interactive mode must take precedence over TTY state.

### JSON and JSON Schema need explicit profiles

RFC 8259 requires interoperable JSON exchanged between systems to use UTF-8 and prohibits generators from adding a byte-order mark. AppPilotKit should additionally prohibit duplicate object member names because RFC 8259 only says names should be unique and receivers otherwise disagree.

JSON Lines is a widely used convention in which every line is a complete JSON value. It is not an IETF standard. RFC 7464 defines a different framing protocol, JSON Text Sequences, using an ASCII Record Separator before each JSON value. AppPilotKit should call newline-delimited output `JSONL` and must not claim RFC 7464 conformance unless it implements that framing.

JSON Schema 2020-12 provides durable vocabulary and dialect negotiation. A schema should identify its dialect with `$schema`. Unsupported required vocabularies must be rejected rather than silently ignored. The `format` keyword is annotation-only by default, so correctness-critical constraints cannot depend on every validator enforcing it. Object schemas must choose extension behavior explicitly: omitting `additionalProperties` allows undeclared properties.

### Exit codes and structured status solve different problems

The portable baseline is small: zero means success, while POSIX reserves established meanings such as 126 for a command that cannot execute and 127 for a command that cannot be found. Vendor-specific codes such as GitHub CLI's authentication code or Gemini CLI's turn-limit code are useful precedents, not portable standards.

An agent needs both:

1. a process exit status for shell orchestration; and
2. a stable structured terminal result for domain semantics.

The two must agree. A warning event in a stream does not necessarily mean that the process failed, so only the terminal result and exit status should determine the invocation outcome.

### Errors require stable machine identity and human recovery guidance

RFC 9457 applies to HTTP APIs, not CLIs, but its separation is useful: a stable `type` identifies the error class, while `detail` is human-facing and must not be parsed by software. Machine-actionable recovery data belongs in dedicated fields. For AppPilotKit this suggests a stable error code, retryability, affected target, optional retry delay, and explicit recovery commands in addition to a concise message.

### Retry safety must be declared

RFC 9110 defines idempotency in terms of the intended effect of repeated identical requests and restricts automatic retries of non-idempotent operations. This semantic model transfers to device-control commands even though AppPilotKit is not an HTTP API. Read operations can normally be retried; state-changing operations need a declared policy and a way to inspect whether an earlier attempt committed.

The proposed HTTP `Idempotency-Key` header is only an expired Internet-Draft as of this review. Its payload-bound unique-key design is useful prior art, but it is not a standard AppPilotKit can cite as normative authority.

## Vendor comparison

| Capability | OpenAI Codex | Claude Code | Gemini CLI | GitHub CLI | AppPilotKit implication |
| --- | --- | --- | --- | --- | --- |
| Non-interactive entry | `codex exec` | `claude -p`; `--bare` removes ambient integrations | Automatic for non-TTY or `-p` | Prompt suppression through environment | Provide an explicit `--non-interactive`; never rely only on TTY inference |
| Machine output | `--json` emits JSONL events | `json` and `stream-json` | single JSON object or stream JSON | command-specific `--json` | Support one final JSON document and a separately named JSONL event mode |
| Stream framing | One JSON event per stdout line | Newline-delimited events ending in `result` | `init`, work events, and final `result` | Mostly single-result JSON | Define one terminal event and preserve stdout purity |
| Structured result schema | `--output-schema`; optional final-message file | `--json-schema`; invalid output is an explicit error | Stable documented envelopes, no equivalent general final-schema flag found | Selectable JSON fields | Publish AppPilotKit-owned schemas and reject invalid terminal results |
| Progress and logs | Progress on stderr; final answer on stdout | Structured stream plus diagnostic stderr behavior | Hook stdout is JSON and stderr is logs | Debug and update notices use stderr | Never mix logs, spinners, or notices into machine stdout |
| Discovery | Documented CLI reference; app-server schema generation | Typo suggestions, but official help is explicitly incomplete | Documented headless formats | Omitting `--json` fields discovers available fields | Make `--help` authoritative and add a machine-readable manifest/schema command |
| Authentication | Saved login or invocation-scoped API key; CI guidance | Explicit key/helper in bare mode | Cached login or environment credentials | Token environment variables avoid prompts | Preflight auth and fail immediately in non-interactive mode with remediation |
| Recovery | Stable thread ID and `resume` | Session resume/fork; retry events | Terminal result and categorized exit codes | Command-specific retries/status | Return a stable operation ID and expose `status`, `resume`, and `cancel` where meaningful |
| Artifacts | `-o` writes final output; ephemeral sessions available | Result includes session and metadata | Result includes response and stats | `--output`, explicit `--clobber`, `--skip-existing` | Return artifact metadata; never overwrite silently |
| Protocol negotiation | Required initialize handshake; capabilities; version-matched schema bundles | Flag- and SDK-version-specific contracts | Versioned documented event envelope | Per-command field discovery | Negotiate protocol version and capabilities before device control |

No vendor behavior in this table should be copied as a numerical exit-code standard or wire-compatible schema. The useful convergence is behavioral: deterministic non-interactive execution, clean stream separation, discoverable structured output, explicit terminal status, and recoverable operation identity.

## Emerging 2026 proposals

Two new projects make the agent-facing direction unusually explicit:

- [The CLI Spec 0.2](https://clispec.dev/) proposes six principles: structured output, schema introspection, stdout/stderr separation, non-interactive operation, idempotency, and bounded output. Its most useful idea is an offline, unauthenticated `schema` command that describes commands, arguments, outputs, errors, mutation markers, and pagination.
- [OpenCLI 0.1](https://opencli.org/) proposes an OpenAPI-inspired JSON or YAML description of a command tree for documentation, client generation, change detection, completion generation, and automation.

Both are useful design inputs, not established interoperability standards. OpenCLI explicitly calls itself a proposal, while The CLI Spec is a new independently published specification with an early conformance tool. AppPilotKit should borrow the discovery requirements and test them locally, but should not claim conformance or freeze either external schema before adoption and tooling maturity are demonstrated.

The CLI Spec's idempotency principle also needs narrowing for device control. Desired-state commands can converge safely, but `tap`, `swipe`, and `type` may already have executed when a connection fails. Those actions must remain explicitly non-idempotent unless a backend can prove deduplication.

## Recommendation maturity caveat

The following are AppPilotKit proposals synthesized from the evidence. They are not established standards and should remain experimental until validated by contract tests and black-box agent evaluation:

- a CLI capability manifest;
- a versioned JSONL operation-event envelope;
- AppPilotKit-specific exit-code classes;
- a payload-bound idempotency key for state-changing commands;
- normalized artifact descriptors;
- machine-readable recovery actions embedded in errors.

Before stabilizing any of these, publish an ADR and freeze exact behavior only at a major protocol boundary. Field additions should remain backward-compatible within a major version. Required semantics must be negotiated as capabilities rather than inferred from the presence of unknown fields.

## Recommended AppPilotKit contract

### Invocation and discovery

Every command should accept these common controls where applicable:

```text
--output human|json|jsonl
--non-interactive
--timeout <duration>
--protocol-version <major.minor>
--output-file <path>
--clobber
--color auto|never|always
```

The final executable name is still open; `<cli>` is only a placeholder below. Discovery should work without authentication, configuration, a network connection, a connected device, or repository knowledge:

```text
<cli> --help
<cli> help <command>
<cli> capabilities --output json
<cli> schema list --output json
<cli> schema show <schema-id> --output json
<cli> doctor --output json --non-interactive
```

`--help` must be authoritative for public commands and flags. `capabilities` should expose CLI version, supported protocol versions, output modes, schema identifiers, commands, optional features, and authentication mechanisms. `schema show` should emit the exact schema shipped with the installed binary.

Help must explain both syntax and safe workflows. Root help should point to discovery, authentication, target selection, inspection, action, artifact, and troubleshooting topics. Command help should include a read-only example, machine-output example, side-effect classification, output fields, documented errors, and the next narrower help command. This lets an agent progress by following the CLI instead of relying on a separate skill.

### Output modes

- `human`: concise terminal-oriented output; progress and color are allowed only when appropriate.
- `json`: exactly one UTF-8 JSON document on stdout, followed by a newline.
- `jsonl`: exactly one UTF-8 JSON event per stdout line, ending in exactly one terminal event.

Machine modes must disable pagers, colors, spinners, prompts, and update notices on stdout. Diagnostics remain on stderr. JSON objects must not contain duplicate keys or non-finite numbers.

Human output should honor the de facto [`NO_COLOR`](https://no-color.org/) convention. An explicit `--color` value takes precedence so behavior is discoverable and testable.

A JSONL stream should begin with an initialization event:

```json
{"type":"run.started","schema_version":"1.0","protocol_version":"1.1","run_id":"run_…","command":"inspect","capabilities":["artifacts.v1"]}
```

Intermediate event types may be command-specific, but every stream must end with exactly one of:

```json
{"type":"run.completed","run_id":"run_…","status":"succeeded","result":{},"artifacts":[]}
{"type":"run.failed","run_id":"run_…","status":"failed","error":{}}
{"type":"run.cancelled","run_id":"run_…","status":"cancelled","error":{}}
```

An event named `warning` or `error` during execution is not itself terminal unless its type is `run.failed`. Broken JSONL, a missing terminal event, or disagreement between terminal status and process exit status is a protocol failure.

A single-document success should use the same concepts:

```json
{
  "schema_version": "1.0",
  "cli_version": "0.1.0",
  "status": "succeeded",
  "protocol": {"version": "1.1", "capabilities": []},
  "data": {},
  "disclosure": {"truncated": false},
  "artifacts": [],
  "next_actions": []
}
```

`next_actions` is optional guidance derived from the current result and negotiated capabilities. Each entry should contain a stable action identifier, an argv array, whether it mutates app state, required preconditions, and a short reason. It must never contain secrets or bypass a confirmation policy. Both successful and failed commands may return next actions; agents must not have to parse prose to recover or continue.

### Exit status

Keep the public set small and document command-specific additions:

| Code | Meaning |
| --- | --- |
| `0` | Completed successfully |
| `1` | General operation failure |
| `2` | Invalid invocation or input |
| `3` | Authentication or authorization required |
| `4` | Target device or app unavailable |
| `5` | Timeout or interrupted operation |
| `6` | Protocol or schema incompatibility |
| `7` | Conflict, including artifact collision or duplicate unsafe request |

Do not allocate 126 or 127. Preserve signal-derived shell behavior when the process is terminated by a signal. A cancelled domain operation should use the documented cancellation status only when AppPilotKit itself handled and reported the cancellation.

### Error envelope

```json
{
  "code": "target.app_not_running",
  "type": "urn:apppilotkit:error:target.app-not-running",
  "message": "The opted-in app is not running on the selected device.",
  "retryable": true,
  "target": {"device_id": "…", "app_id": "…"},
  "retry_after_ms": 1000,
  "recovery": [
    {"action": "launch", "argv": ["<cli>", "app", "launch", "--device", "…", "--app", "…"]},
    {"action": "status", "argv": ["<cli>", "target", "status", "--device", "…", "--app", "…", "--output", "json"]}
  ]
}
```

Agents may branch on `code`, `retryable`, and typed fields. They must not parse `message`. The CLI mapping should preserve the accepted protocol's stable `error.data.kind` rather than introduce a competing classification. If a URI-like `type` is retained, it must remain stable and documented.

### Authentication and interaction

`--non-interactive` must guarantee that stdin is never used for an implicit prompt. Missing credentials, consent, target selection, or destructive confirmation must fail immediately with a structured error and recovery command. Explicit stdin payloads must remain possible through a dedicated flag or `-` path, so data input is distinguishable from prompting.

Authentication precedence and credential sources must be discoverable. Secret values must never appear in stdout events, stderr logs, artifacts, shell examples, or resumed-operation metadata.

### Artifacts

Large screenshots, logs, UI trees, and traces should be file-backed rather than embedded into agent context. Each artifact descriptor should contain:

```json
{
  "id": "artifact_…",
  "kind": "screenshot",
  "path": "/absolute/path/screenshot.png",
  "media_type": "image/png",
  "size_bytes": 12345,
  "digest": {"algorithm": "sha-256", "value": "…"},
  "sensitive": true
}
```

Writing to an existing path must fail by default. `--clobber` authorizes replacement; `--skip-existing` is appropriate only for batch operations and must report skipped artifacts. Partial files should use a temporary sibling and atomic rename where the platform permits it.

### Recovery and idempotency

Long-running operations should return a stable `run_id` as early as possible. `status <run-id>` must distinguish queued, running, succeeded, failed, cancelled, unknown, and expired. `resume` should continue only operations designed for continuation; it must not silently replay an unsafe action.

Each command schema should declare one of:

- `read_only`: safe to retry;
- `idempotent`: repeated identical invocation has the same intended effect;
- `deduplicated`: safe only when the same idempotency key and canonical payload are reused;
- `non_idempotent`: never automatically retry after an ambiguous failure.

If AppPilotKit introduces `--idempotency-key`, the key must be bound to the canonical request. Reusing it with different input must return a conflict. Reusing it with identical input should return the original operation identity and outcome rather than execute again.

Physical interactions require stricter handling. A timeout or disconnect after `tap`, `swipe`, or `type` must report `retryable: false` unless the selected backend can prove that replay is safe. The result should include any available before/after evidence and an argv-based status or inspection action so the agent can observe state before deciding what to do next.

### Protocol and schema negotiation

Before control operations, the CLI and SDK should exchange major/minor protocol versions and capabilities. An incompatible major version fails before side effects. Unknown optional response fields are ignored; unknown required capabilities are rejected explicitly. The negotiated versions and capabilities must appear in `run.started` and in single-document JSON metadata.

Schemas should be versioned independently from command implementation, identify JSON Schema 2020-12 with `$schema`, and use stable `$id` values. The repository should generate and contract-test the schemas shipped by each CLI build.

## No-skill black-box evaluation

The acceptance test should use an agent with no AppPilotKit skill, prompt examples, repository documentation, source access, or prior conversation. Give it only the installed binary, an opted-in test app, and a goal such as: “Find the foreground app, capture its UI summary and screenshot, tap the Settings button, verify the screen changed, and return artifact paths.”

Run at least these cases:

1. happy path with one connected device;
2. multiple devices requiring deterministic selection;
3. app not running;
4. expired or missing authentication;
5. incompatible SDK protocol version;
6. timeout after the device may have applied a state change;
7. interrupted process followed by status or resume;
8. existing artifact path;
9. JSONL consumer that rejects malformed lines and missing terminal events;
10. invocation with stdin redirected and no human available.

The evaluator should observe only commands, stdout, stderr, exit status, and declared artifact files. Pass criteria:

- the agent discovers commands and schemas using built-in help/discovery;
- it performs no speculative destructive command merely to learn syntax;
- machine stdout parses without filtering;
- every invocation has an unambiguous terminal outcome;
- failures provide a correct next command without prose parsing;
- retry behavior respects the command's declared idempotency class;
- artifacts are located and verified without embedding large payloads in context;
- no secret or sensitive app content leaks into unrelated diagnostics.

Record command count, failed-invocation count, help round trips, malformed-output count, unnecessary retries, task completion, elapsed time, and consumed output bytes. Compare results across CLI versions to detect discoverability regressions even when protocol contract tests still pass.

## Open decisions

- Whether `json` and `jsonl` are selected by `--output`, separate flags, or both with one canonical form.
- Whether command and capability discovery uses a bespoke manifest or generated JSON Schema plus metadata.
- Exact stable exit-code allocation and whether authentication deserves a distinct code.
- Whether a JSON-mode failure writes its terminal error envelope to stdout or stderr; whichever is chosen must be singular, documented, and black-box tested.
- Which operations warrant durable `run_id` persistence and their retention period.
- Whether idempotency keys are client-supplied, generated by the CLI, or both.
- Canonicalization rules used to bind an idempotency key to a request.
- Artifact storage root, lifecycle, redaction defaults, and cleanup ownership.
- Whether artifact digests are mandatory for every file or only remotely transferred files.
- How minor-version capability negotiation maps to SDK versions on iOS 15–26 and supported Android versions.
- Which event fields are extensible and which schemas use `additionalProperties: false`.
- Whether recovery commands are executable argv arrays, display strings, or both.
- The exact `next_actions` schema and which commands are required to emit it.

## Primary sources

### Standards and specifications

- [RFC 8259: The JavaScript Object Notation (JSON) Data Interchange Format](https://www.rfc-editor.org/rfc/rfc8259.html)
- [JSON Lines](https://jsonlines.org/)
- [RFC 7464: JavaScript Object Notation (JSON) Text Sequences](https://www.rfc-editor.org/rfc/rfc7464.html)
- [JSON Schema Draft 2020-12 Core](https://json-schema.org/draft/2020-12/json-schema-core.html)
- [JSON Schema Draft 2020-12 Validation](https://json-schema.org/draft/2020-12/json-schema-validation.html)
- [RFC 9457: Problem Details for HTTP APIs](https://www.rfc-editor.org/rfc/rfc9457.html)
- [RFC 9110: HTTP Semantics](https://www.rfc-editor.org/rfc/rfc9110.html)
- [Idempotency-Key HTTP Header Field, Internet-Draft 07](https://datatracker.ietf.org/doc/html/draft-ietf-httpapi-idempotency-key-header-07)
- [POSIX.1-2024 Shell Command Language](https://pubs.opengroup.org/onlinepubs/9799919799/utilities/V3_chap02.html)
- [POSIX `isatty()`](https://pubs.opengroup.org/onlinepubs/009695299/functions/isatty.html)
- [POSIX Standard I/O Streams](https://pubs.opengroup.org/onlinepubs/9699919799/functions/stdin.html)
- [Command Line Interface Guidelines](https://clig.dev/)
- [`NO_COLOR` convention](https://no-color.org/)

### Emerging proposals

- [The CLI Spec 0.2](https://clispec.dev/)
- [OpenCLI 0.1](https://opencli.org/)

### Official implementations and documentation

- [OpenAI Codex non-interactive mode](https://developers.openai.com/codex/noninteractive)
- [OpenAI Codex CLI reference](https://developers.openai.com/codex/cli/reference)
- [OpenAI Codex `exec` implementation](https://github.com/openai/codex/blob/main/codex-rs/exec/src/lib.rs)
- [OpenAI Codex `exec` event definitions](https://github.com/openai/codex/blob/main/codex-rs/exec/src/exec_events.rs)
- [OpenAI Codex app-server protocol](https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md)
- [Claude Code CLI reference](https://code.claude.com/docs/en/cli-usage)
- [Claude Code headless mode](https://code.claude.com/docs/en/headless)
- [Claude Agent SDK structured outputs](https://code.claude.com/docs/en/agent-sdk/structured-outputs)
- [Gemini CLI headless mode](https://geminicli.com/docs/cli/headless/)
- [Gemini CLI authentication](https://geminicli.com/docs/get-started/authentication/)
- [Gemini CLI hooks reference](https://geminicli.com/docs/hooks/reference/)
- [GitHub CLI formatting](https://cli.github.com/manual/gh_help_formatting)
- [GitHub CLI exit codes](https://cli.github.com/manual/gh_help_exit-codes)
- [GitHub CLI environment variables](https://cli.github.com/manual/gh_help_environment)
- [GitHub CLI `repo read-file`](https://cli.github.com/manual/gh_repo_read-file)
- [GitHub CLI `release download`](https://cli.github.com/manual/gh_release_download)
