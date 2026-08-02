# Rust Desktop CLI Foundation Spike

Date: 2026-08-02

Status: **provisionally passes on arm64; final decision awaits the native x86_64 CI job**

This note records the Issue #16 spike. The package and binary are explicitly internal. Their command names, arguments, selectors, output shapes, and result schema are test plumbing and do not decide Issue #13.

## Decision

Rust currently passes gates 1 through 7 and the local arm64 portion of gate 8. No Go comparison ticket is justified by the evidence so far. The stack decision becomes final only after the checked-in matrix passes on both `aarch64-apple-darwin` and `x86_64-apple-darwin` and the independent Standards and Spec reviews have no P1/P2 findings.

The spike is compatible with ADR 0001 and ADR 0002: repository JSON Schema remains the wire source of truth, every current semantic check is preserved, schema resolution is offline, and native iOS/Android UI semantics are not unified or changed by this work.

## Gate results

1. **Offline protocol validation — pass.** Six Draft 2020-12 schemas are embedded with unique, exact `$id` values. A rejecting retriever is installed both while preparing the registry and while compiling validators. An unembedded `https://example.invalid/...` reference fails with the local sentinel error. Rust reproduces all 83 fixture expectations plus 21 negotiation, disclosure, pagination, and ASCII string-matching cases. The Node suite remains the independent oracle.
2. **Spike-owned schema generation — pass.** `schemars = 1.2.2` uses `SchemaSettings::draft2020_12().for_serialize()`. The generated `SpikeResult` schema is meta-validated, validates a serialized result, rejects an invalid outcome, and is structurally compared with a checked-in golden. It never writes `protocol/` schemas.
3. **Process control — pass within the POSIX boundary.** The Tokio runner starts an owned process group, drains stdout/stderr concurrently with independent limits, sends `SIGTERM`, waits a bounded grace period, sends `SIGKILL` when needed, always awaits the direct child, and verifies the managed PGID disappears. Tests cover output beyond pipe capacity, external cancellation, timeout, a descendant, forced termination, and signal exit. macOS cannot let a process `waitpid` an arbitrary grandchild; therefore “complete reaping” means the direct child is reaped and the managed PGID has no remaining members. A descendant that deliberately escapes the PGID is unsupported by both Rust and Go.
4. **Local transport and platform seams — pass.** TCP accepts only loopback `SocketAddr` values. TCP and Unix-domain round trips, pending reads, deadlines, and external cancellation are tested. `devicectl --version`, `devicectl help`, `simctl help`, and `adb version` use an injected runner. The real host smoke test passed without a connected device. No production listener or LAN exposure was added.
5. **Structured output — pass.** The library owns rendering and returns stdout, stderr, and exit code without calling `process::exit`. The executable writes them once. Tests cover strict newline-terminated JSON, UTF-8, JSONL with exactly one terminal event, parse diagnostics isolated on stderr, empty stdout on parse failure, redirected stdin without prompts, recursive duplicate-key rejection, and binary-level agreement between `succeeded`/`failed`/`cancelled` terminal events and spike-only exit statuses `0`/`1`/`130`. These selectors and exit mappings remain test plumbing rather than the public taxonomy tracked by Issue #13.
6. **Artifacts — pass.** Data streams to a sibling `NamedTempFile` while SHA-256 and byte count are accumulated. The file is flushed and `sync_all` is called before `persist_noclobber`; on macOS that publication uses the platform's exclusive atomic rename path. Existing and concurrent destinations are not replaced. Cancellation before publication drops the temporary file. Directory sync is attempted after publication and reported in the receipt instead of turning an already-published artifact into a retryable failure.
7. **Offline command discovery — pass.** Parsing and manifest generation use the same built `clap::Command`. Tests compare the complete public command/argument set, including generated help/version arguments, and reject omissions or duplicates. A black-box test runs with an empty environment, null stdin, poisoned platform-tool paths, and `sandbox-exec` network denial. The manifest branch constructs no runtime, runner, credential, or device service.
8. **Distribution and burden — partial.** Native arm64 build, ad-hoc signing, runtime metrics, locked dependency metadata, license expressions, and audit pass locally. The workflow uses the standard native `macos-15` arm64 and `macos-15-intel` x86_64 runners. The Intel result is pending.

## Local arm64 measurements

Host:

- macOS 26.6 (25G72), arm64
- Xcode 26.2 (17C52), `devicectl` 506.6
- Homebrew Rust/Cargo 1.94.0, LLVM 21.1.8
- ADB 36.0.2-14143358
- `MACOSX_DEPLOYMENT_TARGET=11.0`

Method and results:

| Measurement | Result | Method |
| --- | ---: | --- |
| Clean release build | 38.73 s real | Fresh temporary `CARGO_TARGET_DIR`; registry/source cache already populated |
| Thin architecture | arm64 | `lipo -archs` |
| Unsigned thin binary | 998,992 bytes | `stat -f %z` |
| Ad-hoc signed thin binary | 1,011,328 bytes | `codesign --sign - --timestamp=none` then `stat` |
| Signed SHA-256 | `6aec0a924437a9e50d9677bf7ba8645d3c88669f5605379789f2b9aaa3145587` | `shasum -a 256` |
| Fresh-copy first invocation | 0.14 s real | First `manifest` execution of a fresh copy of the signed artifact on this host; not a controlled cold-cache benchmark |
| 200 warm invocations | 0.43 s real | One unmeasured warm-up, then a shell loop |
| Mean peak RSS | 1,993,114 bytes | 20 runs under `/usr/bin/time -lp` after warm-up |

CI reports the two architectures separately because the runner CPU and memory shapes differ. The values must not be compared as a language benchmark.

## Dependencies, licenses, and advisories

Every direct version is exact and `Cargo.lock` is committed. Direct dependencies and reasons:

| Dependency | Reason |
| --- | --- |
| `clap 4.6.5` | Typed parsing and reflection from one command model |
| `tokio 1.53.1` | Process, signal, local socket, deadline, and asynchronous I/O probes |
| `tokio-util 0.7.19` | Explicit cancellation tokens shared across process, socket, and artifact probes |
| `serde 1.0.229`, `serde_json 1.0.151` | Strict JSON parsing and typed/structured output |
| `jsonschema 0.49.3` | Draft 2020-12 validation with default HTTP/file/TLS resolver features disabled |
| `schemars 1.2.2` | One CLI-owned result schema with pinned settings |
| `rustix 1.1.4` | Safe process-group signal and liveness operations |
| `tempfile 3.27.0` | Secure sibling temporary artifacts and cleanup |
| `sha2 0.11.0` | Streaming SHA-256 receipts |

The lock contains 142 packages across all target-specific dependency branches. Package metadata reports only permissive/Unicode license expressions: MIT, Apache-2.0, BSD-2-Clause, Zlib, Unlicense, MIT-0, Unicode-3.0, and compatible combinations. The only package without a license field is this private `publish = false` spike package; public licensing is out of scope.

`cargo-audit 0.22.2` against RustSec advisory database commit `308808d74a1462ec8b09c1e76938471c53b55dcc` reported 142 dependencies, 1,178 advisories in the database, zero vulnerabilities, and zero warnings.

## Reproduction

```sh
cargo fmt --manifest-path cli/Cargo.toml --all -- --check
cargo clippy --manifest-path cli/Cargo.toml --workspace \
  --all-targets --all-features --locked -- -D warnings
cargo test --manifest-path cli/Cargo.toml --workspace \
  --all-targets --all-features --locked

APPPILOTKIT_XCRUN=/usr/bin/xcrun \
APPPILOTKIT_ADB=/opt/homebrew/bin/adb \
cargo test --manifest-path cli/Cargo.toml \
  --test platform_tools installed_platform_tools_run_without_a_connected_device \
  -- --ignored

npm --prefix protocol ci
npm --prefix protocol test
npm --prefix protocol audit --audit-level=high
```

The exact native matrix and measurement commands are executable in `.github/workflows/rust-foundation-spike.yml`.

## Remaining decision gate

Do not mark Rust final or close Issue #16 until both native CI jobs pass and the result table is updated with the x86_64 artifact facts. If the Intel job fails because of Rust code, distribution, licenses, or advisories, capture the failure and open the identical Go comparison ticket. Runner availability or account quota is infrastructure evidence, not a Rust reversal by itself.
