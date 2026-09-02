# Apple Simulator adapter

This publish-disabled crate implements the private Apple Simulator side of the
Host `PlatformTargetAdapter` SPI. It accepts only an uppercase exact Simulator
UDID and exact app candidate, feature-probes `simctl`, rejects an
already-running or ambiguous candidate, launches it with one public descriptor
environment variable, verifies the exact PID, Darwin process start identity and
endpoint takeover, and returns loopback raw byte streams plus owned cleanup.

It does not parse the descriptor, claim Target-authenticated process generation
or listener epoch, implement Noise/CBOR/Protocol/session/runtime behavior, or
attach to an existing app process. The Host remains the exclusive owner of NK,
PBS, ACK, heartbeat, NNpsk0, framing, sessions, and runtime handoff.

## Artifact and installation ownership

The adapter opens the selected `.app` once through `O_DIRECTORY | O_NOFOLLOW`,
walks it with `openat`/`fstatat`, and copies accepted bytes into a private
owner-only snapshot. Symlinks, hard links, special files, ResourceForks,
unstable observations, invalid root bundle fields, unsafe executable names, and
all `ios-app-tree-v1` cap violations fail before install. The completed snapshot
is streamed once into an owner-only retained canonical spool while the same
chunks feed SHA-256, and must match `TargetSelection::artifact_digest`; the
source path is not used afterward. XML and binary `Info.plist` are parsed through
Darwin `CFPropertyList`, with no production dependency on the D0 reference
crate's `plist` pin.

An existing installed app is accepted only when its complete canonical tree has
the same digest and bundle facts. A different app is never replaced. An absent
app is installed only from the private snapshot, then rehashed after install,
after launch, and immediately before raw handoff. Cleanup uninstalls only an app
installed by this lease, and only after the exact installed path, digest, and
empty process inventory are re-proven. A borrowed matching app is never
uninstalled.

The exact `(UDID, app id)` reservation linearizes every prepare from this Broker
from `begin_launch` through cleanup, including absent/install and
inventory/launch windows. As fixed by the D0 threat model, a compromised
same-current-user process or CoreSimulator tool actively racing those windows is
an explicit residual; defending that actor would require external platform
exclusivity rather than a stronger `simctl` precheck.

`simctl list --json devices` is parsed as duplicate-rejecting JSON. Xcode 26.2
`simctl listapps` emits an OpenStep property list, so that output uses a separate
bounded, fully consuming grammar for dictionaries, arrays, data, strings, and
atoms. It rejects malformed containers and duplicate keys before absence can
trigger installation. Every app-affecting command includes the exact uppercase
UDID.

Focused Xcode 26.2 / iOS 26.3 evidence established that Simulator `pgrep` cannot
read the process list, repeat `simctl launch` returns the existing PID, and
killing a `simctl launch --console` proxy leaves the Target alive. The adapter
therefore uses exact-UDID `ps`, exact launch PID output, endpoint takeover, and
Target-process identity termination proof. Cleanup revalidates the exact
`proc_pidpath` plus microsecond process start time before TERM and before any
KILL escalation, so PID reuse fails closed. It does not use `pgrep`,
`--terminate-running-process`, or proxy exit as ownership evidence. A launch
failure releases the process-local reservation only after an exact-UDID process
probe proves that no candidate exists; uncertain failures remain tombstoned.
The process-local ledger retains each Target port from PendingLaunch through
cleanup and also reference-counts local source ports of every live raw stream,
so another prepare cannot select its own connection port. Each connector binds
and reserves its source port before connecting; there is no global launch or
connector critical section, and unrelated Targets independently honor their
caller's cancellation and absolute deadline.

The unit tests use injected artifact verifiers, fake tool processes, recorded
Xcode parser fixtures, private temporary bundles, and local loopback sockets.
They are contract evidence only and are not the packaged real Simulator journey
required to close issue #62.

This crate is a member of the CLI workspace and uses the shared `cli/Cargo.lock`.
