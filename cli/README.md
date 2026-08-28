# CLI

The desktop CLI is built in Rust on the foundation selected by ADR 0005. Its public executable is `apppilotkit`; internal dogfood distribution uses a signed and notarized GitHub Release plus a Homebrew tap Formula, with updates through `brew upgrade` and no self-updater.

## Production contract core

`crates/cli-contract` owns the Agent-facing discovery and rendering boundary from ADR 0006:

- deterministic human, JSON, and JSONL process output;
- root and command help, `capabilities`, `schema list`, `schema show`, and `doctor`;
- one declarative registry projected into both `clap` and the capability manifest;
- offline validation of the checked-in JSON Schema 2020-12 contracts under `contracts/v1`;
- structured usage errors, exit categories, disclosure, Artifacts, and Next Actions.

The crate is library-first and has no device, transport, credential, SDK, or Agent Skill dependency. `apppilotkit-cli-contract-fixture` is only a black-box test host; it is not the future public CLI.

The accepted Issue #16 implementation remains under `spikes/rust-foundation` as evidence. Production code does not depend on that package or its test syntax.

## Verify

From this directory:

```sh
cargo fmt --all -- --check
cargo clippy --jobs 1 --workspace --all-targets --all-features --locked -- -D warnings
cargo test --jobs 1 --workspace --all-targets --all-features --locked
```

The native macOS workflow repeats these checks for `aarch64-apple-darwin` and `x86_64-apple-darwin`, explicitly builds the production contract core, and runs the existing protocol suite.
