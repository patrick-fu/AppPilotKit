# Android Acceptance Host

The Debug APK is the repository-owned Android fixture for the shared
`demo.foundation` Emulator journey. Its only semantic capability is the
read-only `acceptance.foundation.state` Resource at declaration revision `1`.
Each cold process creates the fixed Android value
`{"scenario":"demo.foundation","seed":"foundation-v1","platform":"android"}`.

Build the Debug fixture and prove the separate Release artifact has no
transport, bootstrap, or native marker:

```shell
JAVA_HOME=$(/usr/libexec/java_home -v 17) ./gradlew --no-daemon --max-workers=1 \
  :acceptance-host:verifyAcceptanceHostArtifacts
```

The Emulator journey launches the Debug-only exported
`dev.apppilotkit.acceptancehost.AppPilotKitBootstrapActivity` with the private
transport descriptor extra. Release contains no such Activity or transport edge.

Run the installed-CLI Emulator journey with exactly one online Android Emulator
(or pass its serial explicitly when more than one is online):

```shell
Scripts/run-emulator-journey.sh [emulator-serial]
```

Rust-dependent Android builds use `CARGO` when it is set, otherwise the
`cargo` found on `PATH`. `RUSTUP_HOME`, `CARGO_HOME`, and `RUSTC` are passed
through to both the installed CLI build and Gradle's Rust FFI tasks, so an
isolated rustup installation can be selected without changing a user's default
toolchain:

```shell
CARGO=/path/to/cargo \
RUSTUP_HOME=/path/to/rustup \
CARGO_HOME=/path/to/cargo-home \
RUSTC=/path/to/rustc \
Scripts/run-emulator-journey.sh [emulator-serial]
```

The script builds the Debug APK, stages one temporary installed CLI/Broker
prefix, writes the six-field Android prepare request with the exact APK and
serial, then invokes the shared harness. It retains its generated configuration
and evidence in the printed temporary directory. The restart callback is the
staged `<prefix>/libexec/apppilotkit-target-prepare --release-fd=0
--output=json`, which receives the old opaque Target only on stdin; its output
is validated as the private release Machine Result, never as public journey
evidence.
