# Android Emulator adapter

This publish-disabled crate implements the existing Host raw platform seam for
one exact Android Emulator selection. It does not own Broker IPC, CBOR launch
descriptor construction, Noise, framing, Session state, heartbeat, or Protocol
dispatch.

The opted-in Debug/Internal app contract is private and fixed: the selected
package exposes `.AppPilotKitBootstrapActivity`, and that Activity accepts one
string extra named `dev.apppilotkit.transport.DESCRIPTOR`. The value is the
unpadded-base64url form of the canonical, non-secret descriptor supplied by the
Host runtime. No endpoint or secret is carried in another extra.

The adapter lifecycle is:

```text
Reserved(endpoint only)
  -> Validated -> Installed -> Started -> ForwardOwned -> Connected
  -> Cleaned

Any failure before ForwardOwned -> Terminal
Any failure after ForwardOwned  -> Cleaned -> Terminal
                               or CleanupFailed
```

`begin_launch` performs no ADB operation. Before any ADB operation, `launch`
requires the selected serial to match exactly `emulator-<digits>`; physical
device or other selector shapes are rejected. It then uses only
`adb -s <exact-emulator-serial>`, installs the exact selected artifact, starts a new
selected app process once, creates exactly one
`forward tcp:0 localabstract:<exact-name>`, and connects only to the allocated
loopback port. The cleanup receipt removes only that exact mapping. Missing,
partial, malformed, oversized, non-UTF-8, or ambiguous tool output fails
closed. Local contract tests are not real-device acceptance evidence.

Before invoking ADB, the adapter copies the caller-selected artifact through a
bounded SHA-256 check into a mode-`0400` adapter-owned snapshot. ADB receives
only that verified snapshot path. The snapshot is deleted immediately after
the install attempt; a deletion failure is terminal `CleanupFailed`.

The snapshot must be a real APK ZIP with a bounded, CRC-valid binary
`AndroidManifest.xml`. The manifest package must equal the selected app id and
must contain exactly one exported, enabled
`<package>.AppPilotKitBootstrapActivity`. Identity attributes use typed
`Res_value` data, require consistent raw values, and bind Android
namespace/name strings to their framework resource-map IDs. Plain XML or marker
text is not an APK identity proof. The frozen Android 36 host-tool golden builds
minimal APKs with `aapt2` and checks the same manifest independently with
`aapt2 dump xmltree`.

Successful launch output must contain exactly one supported launch state:
`LaunchState: COLD` or `LaunchState: UNKNOWN (<digits>)`. The latter is an
observed valid Android 16 result. Missing, duplicate, malformed, `HOT`, `WARM`,
or other states fail closed. Once forward creation may have had a side effect,
rollback uses a fresh two-second cleanup deadline and a new
cancellation token. Successful rollback preserves the original failure kind;
failed or foreign cleanup becomes `CleanupFailed` without deleting another
mapping.
