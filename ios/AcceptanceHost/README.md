# iOS Acceptance Host — Journey7

`AcceptanceHost` is the Debug/Internal iOS fixture for the installed-CLI
`demo.catalog` Journey7. Its Simulator identity is
`dev.apppilotkit.acceptancehost.ios`. The Host registers exactly three
declaration-revision-1 capabilities: the read-only Resource
`acceptance.catalog.state`, ordinary Action `acceptance.catalog.increment`, and
destructive Action `acceptance.catalog.reset`.

The catalog keeps those IDs stable while availability changes at runtime. The
resource is initially available, becomes unavailable after the first increment,
and becomes available after the second. The destructive action requires a
process-bound authorization; this fixture denies destructive grants. A new
process gets a new catalog generation, restores seed `catalog-v1`, clears
ordinary action history, and rejects old Target/session identifiers. Negative fixtures must fail closed (schema mismatch, undeclared or
oversized input, unauthorized destructive invocation, unclassified disclosure)
without a side effect or secret-canary disclosure.

Run the native composition checks with:

```text
Scripts/run-simulator-tests.sh [simulator-udid]
```

Run the installed-CLI Simulator Journey7 path with one staged CLI/Broker
prefix:

```text
Scripts/run-installed-cli-journey.sh [simulator-udid]
```

The script builds a Debug/Internal universal Simulator `.app`, uses
`acceptance/demo-catalog.contract.json`, and invokes the shared harness. It
retains only redacted public evidence plus generated configuration in a new
system temporary directory; the printed path is an evidence artifact, not a
private verdict. Release exclusion remains checked separately:

```text
Scripts/verify-release-exclusion.sh
```

The Acceptance Host is not a product or dependency of `ios/Package.swift`; a
Release compilation intentionally excludes the private transport edge. The
older `demo.foundation` fixture/tests remain available for compatibility with
historical assets. These commands document the contract and evidence shape and
do not, by themselves, assert that the full native matrix has passed.
