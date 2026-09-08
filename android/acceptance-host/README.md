# Android Acceptance Host — Journey7

The Debug APK is the repository-owned Android fixture for the installed-CLI
`demo.catalog` Journey7 Emulator journey. Its catalog contains exactly three
declaration-revision-1 capabilities: the read-only Resource
`acceptance.catalog.state`, ordinary Action `acceptance.catalog.increment`, and
destructive Action `acceptance.catalog.reset`.

Availability is dynamic while catalog membership stays fixed. The resource is
available initially, unavailable after the first ordinary increment, and
available again after the second. Destructive reset requires an authorization
bound to the current Target process; this fixture denies destructive grants.
Restarting the process changes catalog generation, restores seed `catalog-v1`,
clears ordinary history, and invalidates old Target/session identifiers.
Schema-mismatch, undeclared, oversized, unauthorized, unclassified,
and disclosure-limit fixtures must fail closed before side effects; the fixed
secret canary must not enter public evidence.

The Journey script pins the repository's isolated Rust 1.94.0 toolchain
(`RUSTUP_HOME`, `CARGO_HOME`, `RUSTUP_TOOLCHAIN`, `PATH`, `CARGO`, and `RUSTC`)
and checks both `aarch64-linux-android` and `x86_64-linux-android` targets
before Gradle starts. Build the Debug fixture and verify Release exclusion
with:

```shell
JAVA_HOME=$(/usr/libexec/java_home -v 17) ./gradlew --no-daemon --max-workers=1 \
  :acceptance-host:verifyAcceptanceHostArtifacts
```

Run the installed-CLI Emulator Journey7 path with exactly one online Emulator
(or pass its serial explicitly):

```shell
Scripts/run-emulator-journey.sh [emulator-serial]
```

The script builds the Debug APK, stages one temporary installed CLI/Broker
prefix, writes the exact Android prepare request, invokes
`acceptance/demo-catalog.contract.json` through the shared harness, and retains
the generated config/evidence in the printed temporary directory. Evidence is
public and redacted; the private release Machine Result is validated separately.
Rust-dependent builds honor `CARGO`, `RUSTUP_HOME`, `CARGO_HOME`, and `RUSTC`.

Release contains no exported Acceptance Host activity or transport edge. The
older `demo.foundation` fixture/tests remain supported as historical
compatibility assets, but are not the Journey7 scenario. This README does not
claim that every Android/device matrix has passed.
