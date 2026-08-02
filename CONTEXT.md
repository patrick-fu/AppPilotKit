# AppPilotKit

AppPilotKit lets coding Agents inspect and operate opted-in mobile apps through embedded debug SDKs and a self-guiding desktop CLI while preserving platform-native diagnostic truth.

## Language

**Machine Discovery**:
The offline, authentication-free description of the installed CLI's commands, arguments, result schemas, errors, side effects, and recovery paths.
_Avoid_: Command dump, dynamic help

**Machine Result**:
A versioned structured terminal description of one CLI invocation, including its status, data or error, disclosure, artifacts, and recovery guidance.
_Avoid_: JSON output, response blob

**Next Action**:
A structured recommendation containing an exact argv array and the safety information an Agent needs to decide whether to invoke it.
_Avoid_: Suggested command, shell snippet

**Side-Effect Class**:
The scope of state an invocation may change, independent of whether repeating it is safe.
_Avoid_: Idempotency, retryability

**Retry Safety**:
The conditions under which an equivalent invocation may be repeated after its observed outcome, especially when execution may have occurred without acknowledgement.
_Avoid_: Side effect, retryable boolean

**Artifact**:
A file-backed, potentially sensitive output represented to an Agent by its absolute path and integrity, media, size, and sensitivity metadata.
_Avoid_: Attachment, payload file
