# Installed CLI foundation harness

This harness is the shared public-evidence runner for `demo.foundation`.
It is not a Target fixture and does not emulate a real journey. Platform
Acceptance Hosts set `restart.argv` to the same installed
`apppilotkit-target-prepare` binary used for the initial Prepare. The harness
passes the old opaque Target only through canonical JSON on stdin to its
private `--release-fd=0 --output=json` mode before the second Prepare.

```sh
node acceptance/harness/installed-cli-harness.mjs \
  --config /absolute/path/to/foundation-run.json \
  --evidence /absolute/path/to/foundation-evidence.json
```

`foundation-run.json` supplies an installed prefix, the abstract host platform
used by the public scenario contract, the exact non-secret production prepare
request, and the installed private release command. Before every child process
is started, the runner atomically persists a minimal, redacted record. For
prepare this includes the canonical stdin digest, six request key names and JSON
types, and the artifact's type, existence, symlink status, and controlled-tree
digest. The release callback's stdin is exactly the canonical two-field object
`{ "schema_version": "1.0", "target": "<opaque-ref>" }`; evidence stores only
its JSON type and digest. If that write or artifact preflight fails, the child
process is not started. The request platform is
`ios-simulator` for an `ios` host and `android-emulator` for an `android` host:

```json
{
  "prefix": "/absolute/installed/prefix",
  "platform": "ios",
  "contract": "/absolute/path/to/acceptance/demo-foundation.contract.json",
  "prepare_request": {
    "schema_version": "1.0",
    "platform": "ios-simulator",
    "device_selector": "exact-platform-selector",
    "app_id": "exact.acceptance.host.id",
    "app_artifact": "/absolute/path/to/AcceptanceHost.app",
    "artifact_encoding": "ios-app-tree-v1"
  },
  "restart": {
    "argv": [
      "/absolute/installed/prefix/libexec/apppilotkit-target-prepare",
      "--release-fd=0",
      "--output=json"
    ]
  }
}
```

`prepare_request` must contain exactly those six keys: it uses a production
prepare platform and an absolute Target artifact path. The runner invokes only
`<prefix>/libexec/apppilotkit-target-prepare` in request/release modes and
`<prefix>/bin/apppilotkit`. The release callback must exit successfully and
return a `status: "succeeded"` Machine Result without echoing the opaque Target;
failure stops the journey before the old-session probe or the second Prepare.
Evidence stores no raw stdout, stderr, stdin, executable path, artifact path,
descriptor, token, or app content; it only adds exit status, output byte counts
and digests, a single-Machine-Result indicator, and a safe result kind after
each child exits. It executes the first returned `catalog.show` Next Action
for the listed foundation resource rather than synthesizing that command. A run passes only
when the restarted Target and Session differ, the process generation changes,
and the old explicit public Target/Session fails with `sessionExpired`.
