# Rust Foundation Spike

This package is internal, non-production test plumbing for Issue #16. Its binary name, commands, arguments, output selectors, and schemas do not define the public CLI contract tracked by Issue #13.

## Verification

From the repository root:

```sh
cargo fmt --manifest-path cli/Cargo.toml --all -- --check
cargo clippy --manifest-path cli/Cargo.toml --workspace --all-targets --all-features --locked -- -D warnings
cargo test --manifest-path cli/Cargo.toml --workspace --all-targets --all-features --locked
npm --prefix protocol ci
npm --prefix protocol test
```

The platform smoke test invokes only version/help commands and needs no connected device:

```sh
APPPILOTKIT_XCRUN=/usr/bin/xcrun \
APPPILOTKIT_ADB=/opt/homebrew/bin/adb \
cargo test --manifest-path cli/Cargo.toml \
  --test platform_tools installed_platform_tools_run_without_a_connected_device \
  -- --ignored
```

The offline manifest can be exercised with an empty environment and denied network access on macOS:

```sh
cargo build --manifest-path cli/Cargo.toml \
  --package apppilotkit-rust-foundation-spike

/usr/bin/sandbox-exec \
  -p '(version 1)(allow default)(deny network*)' \
  /usr/bin/env -i \
  cli/target/debug/apppilotkit-rust-foundation-spike manifest
```

The native arm64/x86_64 build, test, signing, metrics, license, and advisory matrix is defined in `.github/workflows/rust-foundation-spike.yml`.
