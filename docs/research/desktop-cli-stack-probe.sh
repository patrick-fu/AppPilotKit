#!/bin/sh

set -eu

for probe_tool in swift cargo rustc node npm jq awk git otool; do
  if ! command -v "$probe_tool" >/dev/null 2>&1; then
    printf 'missing required tool: %s\n' "$probe_tool" >&2
    exit 1
  fi
done

script_dir=$(CDPATH='' cd -- "$(dirname "$0")" && pwd)
repo_root=$(git -C "$script_dir" rev-parse --show-toplevel)
mkdir -p "$repo_root/artifacts"
probe_root=$(mktemp -d "$repo_root/artifacts/desktop-cli-stack.XXXXXX")
swift_root="$probe_root/swift"
rust_root="$probe_root/rust"
node_root="$probe_root/node"

mkdir -p "$swift_root/Sources/Probe" "$rust_root/src" "$node_root"

cat >"$swift_root/Package.swift" <<'EOF'
// swift-tools-version: 6.0

import PackageDescription

let package = Package(
  name: "Probe",
  platforms: [.macOS(.v13)],
  dependencies: [
    .package(url: "https://github.com/apple/swift-argument-parser", exact: "1.8.2")
  ],
  targets: [
    .executableTarget(
      name: "Probe",
      dependencies: [.product(name: "ArgumentParser", package: "swift-argument-parser")]
    )
  ]
)
EOF

cat >"$swift_root/Sources/Probe/main.swift" <<'EOF'
import ArgumentParser
import Foundation

@main
struct Probe: ParsableCommand {
  static let configuration = CommandConfiguration(
    abstract: "Framework-only CLI stack probe.",
    subcommands: [Emit.self]
  )
}

struct Emit: ParsableCommand {
  static let configuration = CommandConfiguration(abstract: "Emit one JSON document.")

  @Option(help: "A value to emit.")
  var value = "ok"

  func run() throws {
    let data = try JSONSerialization.data(withJSONObject: ["status": value], options: [.sortedKeys])
    FileHandle.standardOutput.write(data)
    FileHandle.standardOutput.write(Data([0x0a]))
  }
}
EOF

cat >"$rust_root/Cargo.toml" <<'EOF'
[package]
name = "probe"
version = "0.1.0"
edition = "2024"

[dependencies]
clap = { version = "=4.6.5", features = ["derive"] }
serde = { version = "=1.0.229", features = ["derive"] }
serde_json = "=1.0.151"
EOF

cat >"$rust_root/src/main.rs" <<'EOF'
use clap::{Parser, Subcommand};
use serde::Serialize;

#[derive(Parser)]
#[command(about = "Framework-only CLI stack probe.")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Emit one JSON document.
    Emit {
        /// A value to emit.
        #[arg(long, default_value = "ok")]
        value: String,
    },
}

#[derive(Serialize)]
struct Output {
    status: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Emit { value } => {
            println!("{}", serde_json::to_string(&Output { status: value })?)
        }
    }
    Ok(())
}
EOF

cat >"$node_root/package.json" <<'EOF'
{
  "name": "apppilotkit-cli-stack-probe",
  "version": "0.0.0",
  "private": true,
  "type": "module",
  "dependencies": {
    "ajv": "8.20.0",
    "commander": "15.0.0"
  }
}
EOF

cat >"$node_root/probe.js" <<'EOF'
#!/usr/bin/env node
import { Command } from "commander";

const program = new Command();
program.name("probe").description("Framework-only CLI stack probe.");
program
  .command("emit")
  .description("Emit one JSON document.")
  .option("--value <value>", "A value to emit.", "ok")
  .action(({ value }) => process.stdout.write(`${JSON.stringify({ status: value })}\n`));
await program.parseAsync();
EOF

printf 'probe_root=%s\n' "$probe_root"
printf 'swift=%s\n' "$(swift --version 2>&1 | head -n 1)"
printf 'rustc=%s\n' "$(rustc --version)"
printf 'node=%s npm=%s\n' "$(node --version)" "$(npm --version)"

swift build --package-path "$swift_root" --configuration release
cargo generate-lockfile --manifest-path "$rust_root/Cargo.toml"
cargo build --manifest-path "$rust_root/Cargo.toml" --release --locked
(
  cd "$node_root"
  npm install --package-lock-only --ignore-scripts --no-audit --no-fund
  npm ci --ignore-scripts --no-audit --no-fund
  npm audit --audit-level=high
)

swift_probe="$swift_root/.build/release/Probe"
rust_probe="$rust_root/target/release/probe"

swift_help=$("$swift_probe" --help)
rust_help=$("$rust_probe" --help)
node_help=$(node "$node_root/probe.js" --help)
printf '%s\n' "$swift_help" | grep -q emit
printf '%s\n' "$rust_help" | grep -q emit
printf '%s\n' "$node_help" | grep -q emit

swift_json=$("$swift_probe" emit --value ok)
rust_json=$("$rust_probe" emit --value ok)
node_json=$(node "$node_root/probe.js" emit --value ok)
printf '%s\n' "$swift_json" | jq -e '.status == "ok"' >/dev/null
printf '%s\n' "$rust_json" | jq -e '.status == "ok"' >/dev/null
printf '%s\n' "$node_json" | jq -e '.status == "ok"' >/dev/null

printf 'swift_binary_bytes=%s\n' "$(stat -f %z "$swift_probe")"
printf 'rust_binary_bytes=%s\n' "$(stat -f %z "$rust_probe")"
printf 'rust_resolved_dependencies=%s\n' "$(awk '
  /^\[\[package\]\]$/ { count++ }
  END { print count - 1 }
' "$rust_root/Cargo.lock")"
printf 'node_modules_kib=%s\n' "$(du -sk "$node_root/node_modules" | awk '{print $1}')"
printf 'node_resolved_packages=%s\n' "$(jq '.packages | length - 1' "$node_root/package-lock.json")"
printf 'swift_linkage\n'
otool -L "$swift_probe"
printf 'rust_linkage\n'
otool -L "$rust_probe"

bench_rss() {
  probe_label="$1"
  shift
  "$@" >/dev/null
  measurement_file="$probe_root/$probe_label-rss.txt"
  : >"$measurement_file"

  probe_i=0
  while [ "$probe_i" -lt 20 ]; do
    /usr/bin/time -lp "$@" >/dev/null 2>>"$measurement_file"
    probe_i=$((probe_i + 1))
  done

  awk -v label="$probe_label" '
      $1 == "real" {
        time_sum += $2
        time_count++
      }
      /maximum resident set size/ {
        rss_sum += $1
        rss_count++
      }
      END {
        printf "%s runs=%d mean_real_s=%.4f mean_max_rss_bytes=%.0f\n",
          label,
          time_count,
          time_sum / time_count,
          rss_sum / rss_count
      }
    ' "$measurement_file"
}

bench_200() {
  probe_label="$1"
  shift
  printf '%s_200_invocations\n' "$probe_label"
  # The inner shell receives the command as positional arguments.
  # shellcheck disable=SC2016
  /usr/bin/time -p sh -eu -c '
    probe_i=0
    while [ "$probe_i" -lt 200 ]; do
      "$@" >/dev/null
      probe_i=$((probe_i + 1))
    done
  ' sh "$@"
}

bench_rss swift "$swift_probe" emit --value ok
bench_rss rust "$rust_probe" emit --value ok
bench_rss node node "$node_root/probe.js" emit --value ok

bench_200 swift "$swift_probe" emit --value ok
bench_200 rust "$rust_probe" emit --value ok
bench_200 node node "$node_root/probe.js" emit --value ok

printf 'probe output retained at %s\n' "$probe_root"
