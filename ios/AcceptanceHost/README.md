# iOS Acceptance Host

`AcceptanceHost` is the dedicated Debug/Internal iOS application for the
`demo.foundation` scenario. Its Simulator identity is
`dev.apppilotkit.acceptancehost.ios`; it exposes exactly one public read-only
Resource, `acceptance.foundation.state`, with the fixed cold-process seed
`foundation-v1` and platform value `ios`.

The Host composes the existing private Target transport solely through its
SPI. It is not a product or dependency of `ios/Package.swift`; a Release
compilation intentionally fails.

Run the deterministic native composition tests with the accepted Rust FFI:

```text
Scripts/run-simulator-tests.sh [simulator-udid]
```

Run the production-edge and Release-negative proof with:

```text
Scripts/verify-release-exclusion.sh
```

Run the installed-CLI Simulator journey with one staged CLI/Broker prefix:

```text
Scripts/run-installed-cli-journey.sh [simulator-udid]
```

The script builds a Debug/Internal universal Simulator `.app`, gives the
shared installed-CLI harness an exact six-key `ios-simulator` prepare request,
and retains the redacted public-run evidence in a newly created system temporary
directory. Its restart callback invokes the staged
`<prefix>/libexec/apppilotkit-target-prepare --release-fd=0 --output=json` with
the old opaque Target on stdin; the path printed on success is the evidence
file, not a private verdict.
