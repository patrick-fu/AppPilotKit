# Desktop CLI implementation stack research

Status: research recommendation, not an accepted architecture decision

Last reviewed: 2026-08-02

Decision issue: [#14](https://github.com/patrick-fu/AppPilotKit/issues/14)

## Recommendation

Use **Rust with `clap`** for the AppPilotKit MVP desktop CLI. Use `tokio` for process, socket, signal, timeout, and cancellation orchestration; `serde`/`serde_json` for typed JSON; `jsonschema` with network resolution disabled for the repository's JSON Schema 2020-12 contracts; and `schemars` only for CLI-owned schemas where generation from Rust types is useful.

Use **Go with Cobra as the runner-up**. It is the best reversal target because its standard library has unusually complete process, networking, signal, hashing, and filesystem support, while Cobra supplies the command tree and help. Its principal disadvantage here is a split validation/generation stack plus no existing Go toolchain or code in this repository.

This recommendation deliberately does **not** choose the executable name or settle Issue [#13](https://github.com/patrick-fu/AppPilotKit/issues/13)'s open output-selector spelling, JSON failure stream, exit-code allocation, manifest shape, or `next_actions` shape.

### Why Rust wins

The following are **inferences from the sourced facts and local probes**, not language guarantees:

1. Rust has the strongest combined fit for a long-lived local orchestration process: typed command parsing, explicit ownership of child processes and sockets, structured async cancellation, mature 2020-12 validation and schema generation, and a native executable with low measured startup and memory overhead.
2. Apple and Android integration does not require the CLI itself to be Swift or Kotlin. The supported seams are subprocesses (`xcrun devicectl`, `xcrun simctl`, and `adb`) plus local sockets/tunnels, all of which Rust and Go handle directly.
3. The CLI is already an isolated repository boundary under `cli/`; adding a Rust workspace there does not disturb the Swift iOS package or npm protocol-contract harness. Wire schemas under `protocol/` remain the source of truth and must not be regenerated from Rust types.
4. Rust costs more compile time and brings a larger transitive crate graph than Go. That cost is acceptable for the MVP only if the executable spike below proves the full stack, rather than the argument-parser-only lower bound measured here.

## Decision boundaries inherited from the repository

These are **facts** from [the product vision](../vision.md), [ADR 0001](../adr/0001-protocol-envelope-and-compatibility.md), and [ADR 0002](../adr/0002-ui-snapshot-and-inspection.md):

- The desktop CLI owns discovery, sessions, command behavior, output shaping, and artifacts; platform-native collection remains in the SDKs.
- The wire contract is a strict UTF-8 JSON-RPC 2.0 profile with no batches, notifications, or unknown fields for the negotiated minor version.
- JSON Schema 2020-12 files under `protocol/` are the canonical wire contract. Semantic fixture checks cover invariants that JSON Schema cannot express.
- Results must remain bounded and progressive; large screenshots and payloads become local artifacts.
- Simulator/emulator and physical-device support are required. Listeners remain loopback-only or behind authenticated device tunnels.
- Exact CLI output syntax and several machine-envelope choices are still unresolved in Issue #13.

## Method and evidence labels

- **Fact** means a behavior documented by the owning project/specification or observed by a reproducible local command listed below.
- **Inference** means this document's engineering judgment for AppPilotKit.
- Repository popularity and a recent release are maintenance signals, not proof of correctness or security.
- A clean package audit is point-in-time evidence only; it is not evidence that a dependency has no vulnerabilities.

The candidate set is the four stacks required by Issue #14. No candidate was removed. Go+Cobra and TypeScript+Commander were selected as the smallest credible maintained variants rather than heavier plugin frameworks.

## Common platform-integration facts

- Android's official ADB documentation defines `adb devices`, deterministic targeting with `-s SERIAL`, and `adb forward`; these are subprocess interfaces independent of implementation language. [Android Developers: ADB](https://developer.android.com/tools/adb)
- On the probed Xcode installation, `xcrun devicectl --help` says its versioned JSON file output is the only stable interface for scripts and programs; stdout is human-oriented. Xcode 27 release notes add supported JSON-to-stdout behavior, so the adapter must capability-probe the installed tool instead of assuming one form. [Xcode release notes](https://developer.apple.com/documentation/xcode-release-notes/xcode-27-release-notes)
- The open-source usbmuxd implementation exposes a macOS-compatible local socket interface. Its daemon is GPL-licensed, so AppPilotKit must not silently vendor it; direct protocol use or linking a client library requires a separate licensing and transport review. [usbmuxd project](https://github.com/libimobiledevice/usbmuxd)
- Therefore, **inference:** the MVP should put Apple tools, ADB, and any future usbmux adapter behind injected runner/tunnel interfaces. None of the four languages gets an exclusive supported transport path.

## Primary-source facts by candidate

### Rust + `clap`

- `clap` supplies typed derive parsing, subcommands, value enums, and generated command/argument help. [clap derive reference](https://docs.rs/clap/latest/clap/_derive/)
- Tokio's process API provides asynchronous `spawn`, `status`, and `output`, selectable stdio, Unix process groups, and `kill_on_drop`. Its documentation explicitly warns that dropping a child does not cancel it by default and recommends awaiting it for strict reaping guarantees. [Tokio `Command`](https://docs.rs/tokio/latest/tokio/process/struct.Command.html)
- Tokio provides async TCP/Unix sockets, timers, and signal streams in the same runtime. [Tokio networking](https://docs.rs/tokio/latest/tokio/net/) and [Tokio signals](https://docs.rs/tokio/latest/tokio/signal/)
- `serde` derives can reject unknown fields with `deny_unknown_fields`; `serde_json` supplies typed JSON parsing and serialization. Raw arbitrary-object parsing still needs an AppPilotKit duplicate-key guard before schema validation. [Serde attributes](https://serde.rs/container-attrs.html) and [`serde_json`](https://docs.rs/serde_json/latest/serde_json/)
- The `jsonschema` crate has an explicit Draft 2020-12 module, meta-schema validation, custom retrievers, and configurable reference resolution. Its default features include HTTP/file resolution and TLS, so AppPilotKit should disable defaults and register only embedded repository schemas. [`jsonschema` Draft 2020-12](https://docs.rs/jsonschema/latest/jsonschema/draft202012/) and [crate features](https://docs.rs/crate/jsonschema/latest/features)
- `schemars` currently emits JSON Schema 2020-12 by default. The documentation warns that the default may change, so CLI-owned generation must pin explicit settings and golden-test emitted schemas. [`schemars`](https://docs.rs/schemars/latest/schemars/)
- Rust's `aarch64-apple-darwin` and `x86_64-apple-darwin` targets are supported Apple targets; release construction still must run and be tested on macOS because Apple SDK/linking and signing are host concerns. [rustc Apple targets](https://doc.rust-lang.org/rustc/platform-support/apple-darwin.html)
- `std::fs::rename` exposes the same-filesystem rename primitive, and SHA-256 is available from the RustCrypto `sha2` crate. Atomic replacement durability still requires a sibling temporary file, flush/sync policy, and platform-specific tests. [`std::fs::rename`](https://doc.rust-lang.org/std/fs/fn.rename.html) and [`sha2`](https://docs.rs/sha2/latest/sha2/)
- `clap`, `jsonschema`, and `schemars` use permissive MIT and/or Apache-2.0 licenses according to their package metadata. The resolved full MVP graph and advisories remain **unknown** until the spike is locked and audited.

### Go + Cobra

- Cobra supplies nested subcommands, generated help, suggestions, shell completion, and documentation generation; it is Apache-2.0 licensed. [Cobra package documentation](https://pkg.go.dev/github.com/spf13/cobra)
- `os/exec.CommandContext` binds a process to a context. `Cmd.Cancel` can send a graceful signal or close a pipe/socket, and `WaitDelay` bounds a child that does not exit or inherited pipes that never close. [Go `os/exec`](https://pkg.go.dev/os/exec)
- The standard library supplies TCP/Unix sockets and context-aware signal handling. [`net`](https://pkg.go.dev/net) and [`signal.NotifyContext`](https://pkg.go.dev/os/signal#NotifyContext)
- `encoding/json.Decoder.DisallowUnknownFields` helps with typed envelopes, but `Unmarshal` processes duplicate object keys in order and later values replace or merge earlier values. AppPilotKit therefore still needs a raw duplicate-key guard. [Go `encoding/json`](https://pkg.go.dev/encoding/json)
- `santhosh-tekuri/jsonschema/v6` supports Draft 2020-12 validation and explicit draft/resource registration; `invopop/jsonschema` emits Draft 2020-12 schemas from Go types. This is a two-library source-of-drift risk that needs golden tests. [validator](https://pkg.go.dev/github.com/santhosh-tekuri/jsonschema/v6) and [generator](https://pkg.go.dev/github.com/invopop/jsonschema)
- Go supports `darwin/arm64` and `darwin/amd64` targets through `GOOS`/`GOARCH`, and its standard library includes SHA-256 and rename. [Go target documentation](https://go.dev/doc/install/source#environment), [`crypto/sha256`](https://pkg.go.dev/crypto/sha256), and [`os.Rename`](https://pkg.go.dev/os#Rename)
- **Unknown:** release binary size, startup, RSS, full dependency count, and schema compatibility on this host because `go` was not installed. No benchmark is claimed.

### Swift + Swift Argument Parser

- Swift Argument Parser is Apple's Apache-2.0 package for type-safe parsing, subcommands, generated help, validation, and async commands. [official repository](https://github.com/apple/swift-argument-parser)
- Foundation `Process` runs and monitors subprocesses, exposes independent stdin/stdout/stderr, and can interrupt or terminate a task and its subtasks. Its termination handler is not guaranteed to finish before `waitUntilExit()` returns, so a wrapper must define its own completion/cancellation ordering. [Apple `Process`](https://developer.apple.com/documentation/foundation/process) and [`terminationHandler`](https://developer.apple.com/documentation/foundation/process/terminationhandler)
- Swift concurrency is cooperatively cancelled; tasks must check cancellation or install cancellation handlers. [Swift concurrency](https://docs.swift.org/swift-book/LanguageGuide/Concurrency.html#ID646)
- Apple's Network framework is the strongest direct Apple-native socket option. Using it would make the host CLI macOS-specific; a cross-platform Swift implementation would instead add POSIX/SwiftNIO complexity. [Apple networking overview](https://developer.apple.com/documentation/technologyoverviews/networking-and-communication)
- `swift-json-schema` advertises validation, generation, deterministic ordered JSON, and Draft 2020-12, but it was created in 2024 and remains below 1.0. Its validator target depends on Swift Collections; its macro generator additionally brings SwiftSyntax. [official repository](https://github.com/ajevans99/swift-json-schema)
- **Inference:** Swift has the best path if the desktop CLI later must call Apple frameworks directly, but the MVP's documented seams are subprocesses and sockets. Its newer JSON Schema stack creates more contract risk than Rust, Go, or the repository's existing Ajv harness.

### TypeScript/Node + Commander

- Commander is a maintained MIT-licensed framework with strict unknown-option handling, subcommands, generated help, async parsing, and overridable output/exit behavior. [official repository](https://github.com/tj/commander.js)
- Node's `child_process.spawn` exposes separate stdio, exit and close events, signals, and `AbortSignal`. The docs warn that killing a parent does not necessarily kill descendants and that `error` and `exit` can both occur, so exactly-once completion needs an explicit state machine. [Node child processes](https://nodejs.org/api/child_process.html)
- Node supplies TCP/Unix sockets and process signal events. [`node:net`](https://nodejs.org/api/net.html) and [`node:process`](https://nodejs.org/api/process.html#signal-events)
- Ajv explicitly supports JSON Schema 2020-12 through its 2020-specific class. It validates parsed JavaScript values, so a duplicate-key guard must run before `JSON.parse`/Ajv. [Ajv 2020-12](https://ajv.js.org/json-schema.html#draft-2020-12-breaking)
- Node's single-executable application support remains marked active development; the current documentation says regular CI coverage on macOS is arm64 only and macOS output must be signed. [Node SEA](https://nodejs.org/api/single-executable-applications.html)
- **Inference:** this is the fastest repository onboarding path because the protocol harness already uses Node and Ajv, but runtime/distribution weight and process-tree semantics make it a weaker durable desktop tool than Rust or Go.

## Same-criteria assessment

This table is **inference**, based on the facts above. “Strong”, “adequate”, and “weak” are relative to this MVP, not general language ratings.

| Criterion | Rust + clap | Go + Cobra | Swift + Argument Parser | TypeScript/Node + Commander |
| --- | --- | --- | --- | --- |
| macOS/iOS tools, usbmux, physical tunnels | Strong subprocess/socket fit; Apple APIs require FFI | Strong subprocess/socket fit; Apple APIs require cgo | Strongest Apple-framework access; cross-platform path weaker | Strong subprocess/socket fit; native linking/distribution is awkward |
| ADB discovery/forwarding | Strong | Strong | Strong | Strong |
| Process, socket, timeout, signal, cancellation | Strong with Tokio, but process-group/reaping policy must be explicit | Strong standard-library primitives; simplest runner-up | Adequate; `Process` and cooperative cancellation need a careful bridge | Adequate; documented descendant and event-order caveats |
| Strict JSON and JSONL | Strong typed model; duplicate-key guard required | Adequate; duplicate-key guard required | Adequate; ordered parser available but young | Strong output ergonomics; duplicate-key guard required |
| JSON Schema 2020-12 validation/generation | Strong, mature separate crates; pin settings | Adequate, two libraries and golden tests | Weakest maturity; current credible stack is pre-1.0 | Strong validation via the already-used Ajv; generation policy still required |
| stdout/stderr and noninteractive mode | Strong if parser errors are captured rather than auto-exited | Strong with Cobra silence/output controls | Strong with explicit writers | Strong with Commander output overrides |
| Offline command/schema discovery | `clap::Command` can be walked; custom manifest required | Cobra command tree can be walked; custom manifest required | parser tool-info exists; custom manifest required | Commander command tree can be walked; custom manifest required |
| Startup, footprint, distribution | Strong native lower bound; full-stack unknown | Expected strong, but local result unknown | Strong native lower bound and Apple runtime integration | Weakest measured startup/RSS; SEA still active development |
| Artifacts, hashing, atomic writes | Strong; explicit sync/rename policy | Strong standard library | Strong Foundation/POSIX support | Strong APIs, but runtime remains required unless SEA is adopted |
| Transport and black-box testability | Strong traits + fixture child processes | Strong interfaces + fixture processes | Strong protocols, slower test/build loop | Strong dependency injection and existing Node tests |
| Maturity, license, vulnerability surface | Mature core packages, permissive licenses; full locked graph audit pending | Mature framework/std library, permissive licenses; full graph pending | Parser mature; schema stack young; licenses permissive | Mature framework/Ajv; runtime and npm graph expand surface |
| Repository/contributor fit | New toolchain, clean CLI boundary, strong long-term fit | New and currently absent toolchain | Existing language, but host CLI would couple Apple and cross-platform concerns | Existing toolchain/Ajv and fastest prototype fit |

## Reproducible local probes

Host: arm64 macOS 26.6. Installed tools were Swift 6.2.3, Rust 1.94.0, Node 25.6.1/npm 11.9.0, Xcode `devicectl` 506.6, and ADB 36.0.2. `go` was absent.

The checked-in [probe script](desktop-cli-stack-probe.sh) recreates the sources under a new system temporary directory. Each probe implements one generated-help command and one single-document JSON command. Run it from the repository root with `sh docs/research/desktop-cli-stack-probe.sh`. These are **framework-only lower bounds**, not production-stack benchmarks.

| Probe | Result |
| --- | --- |
| Swift Argument Parser 1.8.2 release build | 1,644,168–1,647,576-byte arm64 Mach-O; 20-run mean peak RSS 6,884,557–6,915,686 bytes; 200 warm invocations in 0.64–0.72 s |
| Rust clap 4.6.5 + serde/serde_json release build | 942,976-byte arm64 Mach-O; 28 resolved dependency packages; 20-run mean peak RSS 1,933,312 bytes; 200 warm invocations in 0.30–0.34 s |
| Node + Commander 15.0.0 + Ajv 8.20.0 | 2,968 KiB `node_modules`, 6 resolved npm packages; 20-run mean peak RSS 51,917,619–52,035,584 bytes; 200 warm invocations in 11.19–11.50 s |
| Go | Not run: toolchain unavailable |

The Node size excludes the Node runtime itself. `otool -L` showed that the Swift probe dynamically used macOS Foundation and Swift runtime libraries, while the Rust probe used only `libSystem`. All three executable probes generated command/subcommand help, wrote one newline-terminated JSON object to stdout, and passed `jq -e .`. The npm lockfile audit reported zero known vulnerabilities across the six resolved packages; that result is not comparable to Rust or Swift because their graphs were not audited in this probe.

The ranges combine the initial research run and a clean rerun of the checked-in script on the same host; they demonstrate normal build and scheduling variation. The script pins every direct package version used by the recorded run, creates lock files, and records the exact program sources and commands. Each probe has one `emit --value <string>` subcommand and emits `{"status":"<string>"}`. Binary size comes from `stat -f %z`, the Node dependency footprint from `du -sk node_modules`, and resolved package counts from `Cargo.lock` and `package-lock.json`, excluding each probe root.

For peak RSS, each already-built command received one unmeasured warm-up invocation, then 20 invocations under macOS `/usr/bin/time -lp`; the reported value is the arithmetic mean of its `maximum resident set size` field. The 200-invocation result used this exact loop shape for each command:

```sh
/usr/bin/time -p sh -c '
  i=0
  while [ "$i" -lt 200 ]; do
    PROBE_COMMAND emit --value ok >/dev/null
    i=$((i+1))
  done
'
```

`PROBE_COMMAND` was respectively the Swift release binary, the Rust release binary, and `node probe.js`. “Warm” means the program and dependencies had already been built and run and filesystem caches were not intentionally cleared. It is not a controlled cold-start benchmark. Reproduction should pin the documented package versions, use the same one-command behavior, and report host/toolchain differences rather than comparing unlike full-stack programs.

Platform probes, requiring no connected device, confirmed:

```text
xcrun devicectl --version   # 506.6
xcrun devicectl help        # versioned machine JSON goes to a file on this version
xcrun simctl help           # simulator command tree available
adb version                 # 36.0.2-14143358
adb help                    # serial targeting and tcp/local socket forwarding available
```

## Implementation shape if the spike passes

This is a **proposed internal shape**, not a frozen public CLI contract:

- `cli/` becomes a Cargo workspace with a thin executable and library-first modules for command metadata, orchestration, platform adapters, transport, protocol validation, output events, and artifacts.
- `clap` parses into typed invocation values without calling `process::exit` from library code. One top-level outcome renderer exclusively owns stdout, stderr, and exit status.
- `tokio` runs child processes and local sockets. Each child is placed in an owned process group where supported; cancellation follows a documented graceful-signal, bounded-wait, forced-kill, and reap sequence.
- All protocol schemas and CLI discovery schemas are embedded at build time. `jsonschema` has remote retrieval disabled. Discovery never needs authentication, a device, or a network.
- `protocol/` JSON Schema remains canonical. `schemars` may generate only CLI-owned metadata/result schemas, with checked-in golden compatibility tests.
- Artifacts stream to a sibling temporary file while SHA-256 is computed, then flush/sync and atomically rename. Existing destinations fail unless the eventually accepted contract explicitly authorizes replacement.

## Reversal conditions

Choose **Go + Cobra instead of Rust** if the identical spike passes in Go and any of these conditions holds:

1. Rust cannot validate every current schema/fixture fully offline without enabling HTTP/TLS resolver features, while Go's validator can with an embedded resource registry.
2. Rust cannot demonstrate deterministic descendant cancellation and reaping for `devicectl`, `simctl`, and ADB fixture process trees without unsafe or platform-fragile code, while Go can do so with a smaller adapter.
3. The team adopts Go as a supported repository toolchain and values build speed and a smaller conceptual dependency surface over Rust's stronger type/schema integration.
4. The full Rust spike materially exceeds Go in signed universal-binary size, cold startup, peak RSS, or locked dependency/advisory burden after both are measured with identical behavior.

Reconsider **Swift** if a supported Apple framework API, unavailable through stable subprocess/socket seams, becomes necessary for the MVP. Reconsider **TypeScript/Node** if distribution is explicitly npm/runtime-based, native single-file delivery is dropped, and iteration speed is more important than startup, memory, and process-tree control.

## Smallest executable spike before an ADR

Research is insufficient to approve Rust without one executable spike. Build a throwaway, device-free Rust binary; use a placeholder name and a spike-only mode selector so no Issue #13 syntax becomes precedent.

The spike must do only these things:

1. Embed every `protocol/v1` and `protocol/v1.1` schema, register all `$id` references offline, and run the repository's valid/invalid fixtures through `jsonschema`. Demonstrate that no validation path can perform network I/O.
2. Generate one CLI-owned result schema with explicitly pinned `schemars` settings and validate its output. Do not generate the wire schemas.
3. Run a fixture child that emits interleaved stdout/stderr, ignores the first signal, forks a descendant, times out, and exits by signal. Prove stream separation, one terminal outcome, bounded graceful cancellation, forced termination, and complete reaping.
4. Exercise a local TCP socket and Unix-domain socket with deadline/cancellation; invoke `devicectl --version`, `devicectl help`, `simctl help`, and `adb version` through injected runners without requiring a device.
5. Emit one JSON document and one JSONL stream selected by spike-only test plumbing; verify UTF-8, newline framing, duplicate-key rejection, no diagnostic contamination, exactly one terminal JSONL event, redirected stdin without prompts, and signal-consistent exit status.
6. Stream an artifact through a sibling temporary file, compute SHA-256, reject an existing destination, atomically rename, and remove a cancelled partial file.
7. Generate the offline command manifest from the same command model used by `clap`; black-box tests must prove that every public spike command/argument appears once and that discovery performs no environment, network, credential, or device lookup.
8. Build and test arm64 and x86_64 macOS artifacts, then record clean-build time, signed/thinned binary size, cold/warm startup, peak RSS, and the locked dependency/license/advisory report.

Run the same spike in Go only if Rust fails a gate or the full-stack measurements make reversal plausible. An ADR can accept Rust after all eight gates pass; otherwise the Go comparison, not more desk research, decides the stack.
