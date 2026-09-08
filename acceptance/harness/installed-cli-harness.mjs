#!/usr/bin/env node

import { createHash } from "node:crypto";
import { closeSync, lstatSync, openSync, readFileSync, readdirSync, renameSync, unlinkSync, writeFileSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { spawnSync } from "node:child_process";
import { dirname, isAbsolute, join, resolve } from "node:path";
import { isDeepStrictEqual } from "node:util";

const REDACTED = "<redacted>";
const capabilityId = "acceptance.foundation.state";
const preparePlatformByHostPlatform = Object.freeze({
  ios: "ios-simulator",
  android: "android-emulator",
});
const prepareEncodingByHostPlatform = Object.freeze({
  ios: "ios-app-tree-v1",
  android: "raw-file-v1",
});
const safeMachineResultKinds = new Set(["succeeded", "sessionExpired"]);

function fail(message) {
  throw new Error(`installed-cli acceptance harness: ${message}`);
}

function sha256(value) {
  return `sha256:${createHash("sha256").update(value).digest("hex")}`;
}

function jsonType(value) {
  if (value === null) return "null";
  if (Array.isArray(value)) return "array";
  return typeof value;
}

function canonicalJson(value) {
  if (value === null || typeof value === "boolean" || typeof value === "number" || typeof value === "string") {
    return JSON.stringify(value);
  }
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  if (value && typeof value === "object") {
    return `{${Object.keys(value).sort().map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`).join(",")}}`;
  }
  fail("stdin JSON contains an unsupported value");
}

function canonicalStdinEvidence(input) {
  if (input.length === 0) return { canonical_sha256: sha256(""), json_type: "empty" };
  try {
    const parsed = JSON.parse(input);
    return { canonical_sha256: sha256(canonicalJson(parsed)), json_type: jsonType(parsed) };
  } catch {
    return { canonical_sha256: sha256(input), json_type: "invalid" };
  }
}

function argvShape(argv) {
  return argv.map((value, index) => {
    if (!value.startsWith("--")) {
      if (index > 0 && argv[index - 1]?.startsWith("--") && !argv[index - 1].includes("=")) return "<redacted>";
      return index < 2 && ["catalog", "list", "show", "schema", "query"].includes(value) ? value : "<redacted>";
    }
    const equals = value.indexOf("=");
    return equals === -1 ? value : `${value.slice(0, equals)}=<redacted>`;
  });
}

function controlledTreeDigest(path) {
  const digest = createHash("sha256");
  let containsSymlink = false;
  function visit(currentPath, relativePath) {
    const stat = lstatSync(currentPath);
    if (stat.isSymbolicLink()) {
      containsSymlink = true;
      return;
    }
    if (stat.isDirectory()) {
      digest.update(`directory\\0${relativePath}\\0`);
      for (const name of readdirSync(currentPath).sort()) visit(join(currentPath, name), `${relativePath}/${name}`);
      return;
    }
    if (stat.isFile()) {
      digest.update(`file\\0${relativePath}\\0`);
      digest.update(createHash("sha256").update(readFileSync(currentPath)).digest("hex"));
      return;
    }
    digest.update(`other\\0${relativePath}\\0`);
  }
  visit(path, ".");
  return { digest: containsSymlink ? null : `sha256:${digest.digest("hex")}`, containsSymlink };
}

function inspectArtifact(path) {
  try {
    const stat = lstatSync(path);
    const type = stat.isSymbolicLink() ? "symlink" : stat.isDirectory() ? "directory" : stat.isFile() ? "file" : "other";
    if (stat.isSymbolicLink()) return { type, exists: true, symlink: true, controlled_tree_sha256: null };
    const tree = controlledTreeDigest(path);
    return { type, exists: true, symlink: tree.containsSymlink, controlled_tree_sha256: tree.digest };
  } catch (error) {
    if (error?.code === "ENOENT") return { type: "missing", exists: false, symlink: false, controlled_tree_sha256: null };
    return { type: "unreadable", exists: false, symlink: false, controlled_tree_sha256: null };
  }
}

function prepareRequestEvidence(request, artifact) {
  return {
    key_names: Object.keys(request).sort(),
    value_types: Object.fromEntries(Object.keys(request).sort().map((key) => [key, jsonType(request[key])])),
    artifact,
  };
}

function requireValidArtifact(artifact, platform) {
  const expectedType = platform === "ios" ? "directory" : "file";
  if (!artifact.exists || artifact.symlink || artifact.type !== expectedType || artifact.controlled_tree_sha256 === null) {
    fail(`prepare app artifact must be an existing non-symlink ${expectedType} with a controlled tree`);
  }
}

class EvidenceWriter {
  constructor(path, evidence) {
    this.path = path;
    this.evidence = evidence;
    this.writeCount = 0;
  }

  persist() {
    const temporaryPath = `${this.path}.apppilotkit-${process.pid}-${this.writeCount += 1}.tmp`;
    try {
      writeFileSync(temporaryPath, `${JSON.stringify(this.evidence, null, 2)}\n`, "utf8");
      renameSync(temporaryPath, this.path);
    } catch (error) {
      try { unlinkSync(temporaryPath); } catch {}
      fail(`cannot atomically write evidence: ${error.message}`);
    }
  }

  start(commandId, argv, input, prepareRequest) {
    const record = {
      command_id: commandId,
      argv_shape: argvShape(argv),
      stdin: canonicalStdinEvidence(input),
      ...(prepareRequest ? { prepare_request: prepareRequest } : {}),
    };
    this.evidence.runs.push(record);
    this.persist();
    return record;
  }

  complete(record, execution, raw, singleMachineResult) {
    Object.assign(record, {
      exit_code: execution.status,
      stdout: { byte_count: Buffer.byteLength(execution.stdout ?? ""), sha256: sha256(execution.stdout ?? "") },
      stderr: { byte_count: Buffer.byteLength(execution.stderr ?? ""), sha256: sha256(execution.stderr ?? "") },
      single_machine_result: singleMachineResult,
      ...(safeMachineResultKind(raw) ? { safe_kind: safeMachineResultKind(raw) } : {}),
    });
    this.persist();
  }
}

function parseArgs(argv) {
  let config;
  let evidence;
  for (let index = 0; index < argv.length; index += 1) {
    if (argv[index] === "--config") config = argv[++index];
    else if (argv[index] === "--evidence") evidence = argv[++index];
    else fail(`unknown argument ${argv[index]}`);
  }
  if (!config || !evidence) fail("--config and --evidence are required");
  return { config: resolve(config), evidence: resolve(evidence) };
}

async function readJson(path, label) {
  try {
    return JSON.parse(await readFile(path, "utf8"));
  } catch (error) {
    fail(`cannot read ${label}: ${error.message}`);
  }
}

function requireObject(value, label) {
  if (!value || Array.isArray(value) || typeof value !== "object") fail(`${label} must be an object`);
  return value;
}

function requireString(value, label) {
  if (typeof value !== "string" || value.length === 0) fail(`${label} must be a non-empty string`);
  return value;
}

function validateContract(contract, platform) {
  const scenario = requireObject(contract.scenario, "contract.scenario");
  if (scenario.id !== "demo.foundation") fail("contract scenario must be demo.foundation");
  if (scenario.catalog_membership !== "not_a_semantic_capability") fail("scenario must not be a catalog capability");
  const seed = requireObject(scenario.seed, "contract.scenario.seed");
  if (seed.id !== "foundation-v1") fail("scenario seed must be foundation-v1");
  const resource = requireObject(scenario.resource, "contract.scenario.resource");
  if (resource.id !== capabilityId || resource.kind !== "resource" || resource.declaration_revision !== 1) {
    fail("contract resource must declare acceptance.foundation.state revision 1");
  }
  if (resource.input_schema !== null) fail("foundation resource must not accept input");
  const schema = requireObject(resource.value_schema, "contract.scenario.resource.value_schema");
  requireString(schema.id, "contract value schema id");
  requireString(schema.digest, "contract value schema digest");
  if (!Number.isSafeInteger(schema.revision) || schema.revision < 1) fail("contract value schema revision is invalid");
  const publicValue = requireObject(scenario.public_value, "contract.scenario.public_value");
  if (publicValue.scenario !== scenario.id || publicValue.seed !== seed.id) fail("contract public value must match scenario and seed");
  const native = requireObject(scenario.native_differences, "contract.scenario.native_differences");
  if (native.ios?.platform !== "ios" || native.android?.platform !== "android" || native[platform]?.platform !== platform) {
    fail("contract native platform differences are incomplete");
  }
  requireObject(resource.document, "contract value schema document");
  return { scenario, seed, resource, schema, publicValue };
}

function redactText(value) {
  return String(value)
    .replace(/(--(?:target|session|authorization-grant))=(?:[^\s"]+)/gi, "$1=" + REDACTED)
    .replace(/\b(?:ready[_-]?target|target_|session_)[^\s"',}\]]*/gi, REDACTED)
    .replace(/\b[^\s"',}\]]*(?:credential|secret|token|password)[^\s"',}\]]*/gi, REDACTED);
}

function parseSingleJson(stdout, label) {
  try {
    return JSON.parse(stdout);
  } catch {
    fail(`${label} did not write one JSON result to stdout`);
  }
}

function safeMachineResultKind(raw) {
  if (!raw || Array.isArray(raw) || typeof raw !== "object") return undefined;
  if (raw.status === "succeeded") return "succeeded";
  const kind = raw.error?.kind;
  return raw.status === "failed" && safeMachineResultKinds.has(kind) ? kind : undefined;
}

let internalDiagnosticFd;

function openInternalDiagnosticSidecar() {
  if (process.env.APPPILOTKIT_INTERNAL_PREPARE_FAILURE_DIAGNOSTICS !== "1") return;
  const sidecar = process.env.APPPILOTKIT_INTERNAL_PREPARE_FAILURE_SIDECAR;
  if (!sidecar || !isAbsolute(sidecar)) fail("internal diagnostic sidecar must be an absolute path");
  try {
    internalDiagnosticFd = openSync(sidecar, "a", 0o600);
  } catch (error) {
    fail(`cannot open internal diagnostic sidecar: ${error.message}`);
  }
  process.env.APPPILOTKIT_INTERNAL_PREPARE_FAILURE_FD = String(internalDiagnosticFd);
  process.once("exit", () => closeSync(internalDiagnosticFd));
}

function inheritedDiagnosticStdio() {
  const rawFd = process.env.APPPILOTKIT_INTERNAL_PREPARE_FAILURE_FD;
  if (!rawFd || !/^\d+$/.test(rawFd)) return undefined;
  const fd = Number(rawFd);
  if (!Number.isSafeInteger(fd) || fd < 3 || fd > 63) return undefined;
  const stdio = ["pipe", "pipe", "pipe"];
  while (stdio.length <= fd) stdio.push("ignore");
  // Preserve the explicitly inherited descriptor at the same child number;
  // no path or payload is introduced into the helper environment.
  stdio[fd] = fd;
  return stdio;
}

function execute(role, executable, argv, input, evidenceWriter, {
  parseMachineResult = true,
  prepareRequest,
  preflight,
} = {}) {
  const record = evidenceWriter.start(role, argv, input, prepareRequest);
  if (preflight) preflight();
  const diagnosticsEnabled = process.env.APPPILOTKIT_INTERNAL_PREPARE_FAILURE_DIAGNOSTICS === "1";
  const childEnv = { ...process.env };
  if (!diagnosticsEnabled) {
    delete childEnv.APPPILOTKIT_INTERNAL_PREPARE_FAILURE_FD;
    delete childEnv.APPPILOTKIT_INTERNAL_PREPARE_FAILURE_DIAGNOSTICS;
    delete childEnv.APPPILOTKIT_INTERNAL_PREPARE_FAILURE_SIDECAR;
  }
  const diagnosticStdio = diagnosticsEnabled ? inheritedDiagnosticStdio() : undefined;
  const child = spawnSync(executable, argv, {
    input,
    encoding: "utf8",
    env: childEnv,
    ...(diagnosticStdio ? { stdio: diagnosticStdio } : {}),
  });
  const stdout = child.stdout ?? "";
  const stderr = child.stderr ?? "";
  let raw;
  let parseFailed = false;
  if (parseMachineResult && stdout.trim()) {
    try {
      raw = JSON.parse(stdout);
    } catch {
      parseFailed = true;
    }
  }
  const singleMachineResult = parseMachineResult && !parseFailed && raw !== undefined && !Array.isArray(raw) && typeof raw === "object" && raw !== null;
  evidenceWriter.complete(record, { status: child.status, stdout, stderr }, raw, singleMachineResult);
  if (child.error) fail(`${role} could not start: ${child.error.message}`);
  if (parseFailed) parseSingleJson(stdout, role);
  return {
    record,
    raw,
    stdout,
    stderr,
    exitCode: child.status,
    signal: child.signal,
  };
}

function requireMachineSucceeded(execution, label) {
  const { record, raw } = execution;
  if (execution.exitCode !== 0) fail(`${label} exited ${execution.exitCode ?? execution.signal ?? "without status"}: ${execution.stderr || "no stderr"}`);
  const result = requireObject(raw, `${label} machine result`);
  if (result.status !== "succeeded") fail(`${label} did not report succeeded`);
  if (!Array.isArray(result.artifacts) || !Array.isArray(result.next_actions)) fail(`${label} omitted Artifacts or Next Actions`);
  return result;
}

function requireSucceeded(execution, label) {
  if (execution.exitCode !== 0) fail(`${label} exited ${execution.exitCode ?? execution.signal ?? "without status"}: ${execution.stderr || "no stderr"}`);
  const result = requireObject(execution.raw, `${label} result`);
  if (result.status !== "succeeded") fail(`${label} did not report succeeded`);
  return result;
}

function requireReleaseSucceeded(execution, target, label) {
  const result = requireSucceeded(execution, label);
  if (result.schema_version !== "1.0") fail(`${label} returned an invalid schema version`);
  if (JSON.stringify(result).includes(target)) fail(`${label} echoed the opaque Target reference`);
  return result;
}

function actionArgument(argv, name) {
  const values = [];
  for (let index = 0; index < argv.length; index += 1) {
    const value = argv[index];
    if (typeof value !== "string") continue;
    if (value === name) values.push(argv[++index]);
    else if (value.startsWith(`${name}=`)) values.push(value.slice(name.length + 1));
  }
  if (values.length !== 1 || typeof values[0] !== "string" || values[0].length === 0) {
    fail(`catalog.show Next Action must include one non-empty ${name} argument`);
  }
  return values[0];
}

function foundationShowContinuation(result, target, capabilities) {
  const action = result.next_actions[0];
  if (!action || action.id !== "catalog.show" || !Array.isArray(action.argv) || action.argv[1] !== "catalog" || action.argv[2] !== "show") {
    fail("first catalog list Next Action must be catalog.show for the foundation resource");
  }
  if (action.side_effect !== "read_only" || action.retry_safety !== "safe") fail("catalog.show Next Action must be safe and read-only");
  const selectedCapability = actionArgument(action.argv, "--capability");
  const selectedRevision = Number(actionArgument(action.argv, "--declaration-revision"));
  const session = actionArgument(action.argv, "--session");
  const selectedTarget = actionArgument(action.argv, "--target");
  if (selectedTarget !== target) fail("catalog.show Next Action changed the selected Target");
  if (selectedCapability !== capabilityId || selectedRevision !== 1 || !capabilities.some((item) =>
    item?.id === selectedCapability && item.kind === "resource" && item.declaration_revision === selectedRevision,
  )) {
    fail("catalog.show Next Action is not bound to the listed foundation capability and revision");
  }
  if (actionArgument(action.argv, "--output") !== "json") fail("catalog.show Next Action must request bounded JSON output");
  return { session, argv: action.argv };
}

function requireExact(value, expected, label) {
  if (!isDeepStrictEqual(value, expected)) fail(`${label} does not match the demo scenario contract`);
}

function commonCatalogArgs(target, session) {
  return [`--session=${session}`, `--target=${target}`, "--output=json", "--non-interactive"];
}

function runScenario(phase, cli, target, contract, evidenceWriter, platform) {
  const list = execute(`${phase}.catalog.list`, cli, ["catalog", "list", `--target=${target}`, "--output=json", "--non-interactive"], "", evidenceWriter);
  const listResult = requireMachineSucceeded(list, `${phase} catalog list`);
  const listData = requireObject(listResult.data, `${phase} catalog list data`);
  const catalog = requireObject(listData.catalog, `${phase} catalog`);
  if (!Number.isSafeInteger(catalog.generation) || catalog.generation < 1) fail(`${phase} catalog generation is invalid`);
  const capabilities = listData.capabilities;
  if (!Array.isArray(capabilities) || !capabilities.some((item) => item?.id === capabilityId && item.kind === "resource" && item.declaration_revision === 1)) {
    fail(`${phase} catalog does not expose the foundation resource`);
  }
  if (requireObject(listResult.disclosure, `${phase} catalog list disclosure`).truncated !== false) {
    fail(`${phase} catalog list must be complete before the foundation catalog.show Next Action is used`);
  }
  const continuation = foundationShowContinuation(listResult, target, capabilities);
  const session = continuation.session;
  const shared = commonCatalogArgs(target, session);

  const show = execute(`${phase}.catalog.show`, cli, continuation.argv.slice(1), "", evidenceWriter);
  const showData = requireObject(requireMachineSucceeded(show, `${phase} catalog show`).data, `${phase} catalog show data`);
  if (showData.id !== capabilityId || showData.kind !== "resource" || showData.declaration_revision !== 1 || showData.input_schema !== undefined) {
    fail(`${phase} catalog show has an unexpected foundation resource declaration`);
  }
  requireExact(showData.value_schema, contract.schema, `${phase} value schema`);

  const schema = execute(`${phase}.catalog.schema`, cli, ["catalog", "schema", "--capability", capabilityId, "--declaration-revision", "1", "--schema-id", contract.schema.id, "--schema-revision", String(contract.schema.revision), "--schema-digest", contract.schema.digest, ...shared], "", evidenceWriter);
  const schemaData = requireObject(requireMachineSucceeded(schema, `${phase} catalog schema`).data, `${phase} catalog schema data`);
  requireExact(schemaData.schema, contract.schema, `${phase} returned schema handle`);
  requireExact(schemaData.document, contract.resource.document, `${phase} returned schema document`);

  const query = execute(`${phase}.catalog.query`, cli, ["catalog", "query", "--capability", capabilityId, "--declaration-revision", "1", "--value-schema-id", contract.schema.id, "--value-schema-revision", String(contract.schema.revision), "--value-schema-digest", contract.schema.digest, ...shared], "", evidenceWriter);
  const queryData = requireObject(requireMachineSucceeded(query, `${phase} catalog query`).data, `${phase} catalog query data`);
  requireExact(queryData.value_schema, contract.schema, `${phase} queried value schema`);
  requireExact(queryData.value, { ...contract.publicValue, platform }, `${phase} queried public value`);

  return { target_sha256: sha256(target), session_sha256: sha256(session), generation: catalog.generation, session, continuation: continuation.argv };
}

function prepare(phase, prepareProgram, request, artifact, platform, evidenceWriter) {
  const execution = execute(
    `${phase}.prepare`,
    prepareProgram,
    ["--request-fd=0", "--output=json"],
    canonicalJson(request),
    evidenceWriter,
    { prepareRequest: prepareRequestEvidence(request, artifact), preflight: () => requireValidArtifact(artifact, platform) },
  );
  const result = requireSucceeded(execution, `${phase} target prepare`);
  const ready = requireObject(result.ready_target, `${phase} ready target`);
  const target = requireString(ready.target, `${phase} ready target reference`);
  return target;
}

async function main() {
  const { config: configPath, evidence: evidencePath } = parseArgs(process.argv.slice(2));
  openInternalDiagnosticSidecar();
  const config = requireObject(await readJson(configPath, "configuration"), "configuration");
  const platform = config.platform;
  if (platform !== "ios" && platform !== "android") fail("configuration platform must be ios or android");
  const prefix = requireString(config.prefix, "configuration prefix");
  const configuredContract = requireString(config.contract, "configuration contract");
  const contractPath = isAbsolute(configuredContract) ? configuredContract : resolve(dirname(configPath), configuredContract);
  const contract = validateContract(await readJson(contractPath, "demo scenario contract"), platform);
  const request = requireObject(config.prepare_request, "configuration prepare_request");
  const expectedPrepareRequestKeys = ["app_artifact", "app_id", "artifact_encoding", "device_selector", "platform", "schema_version"];
  if (!isDeepStrictEqual(Object.keys(request).sort(), expectedPrepareRequestKeys)) fail("prepare request must contain exactly six keys");
  if (request.schema_version !== "1.0") fail("prepare request schema_version must be 1.0");
  const preparePlatform = preparePlatformByHostPlatform[platform];
  if (request.platform !== preparePlatform) {
    fail(`prepare request platform must be ${preparePlatform} for configuration platform ${platform}`);
  }
  for (const name of ["device_selector", "app_id", "app_artifact", "artifact_encoding"]) requireString(request[name], `prepare request ${name}`);
  if (!isAbsolute(request.app_artifact)) fail("prepare request app_artifact must be absolute");
  if (request.artifact_encoding !== prepareEncodingByHostPlatform[platform]) {
    fail(`prepare request artifact_encoding must be ${prepareEncodingByHostPlatform[platform]} for configuration platform ${platform}`);
  }
  const restart = requireObject(config.restart, "configuration restart");
  if (!Array.isArray(restart.argv) || restart.argv.length === 0 || !restart.argv.every((value) => typeof value === "string" && value.length > 0)) fail("restart argv is required");

  const cli = join(prefix, "bin", "apppilotkit");
  const prepareProgram = join(prefix, "libexec", "apppilotkit-target-prepare");
  if (!isDeepStrictEqual(restart.argv, [prepareProgram, "--release-fd=0", "--output=json"])) {
    fail("restart argv must invoke the installed target-prepare release mode");
  }
  const artifact = inspectArtifact(request.app_artifact);
  const evidence = {
    schema_version: "1",
    scenario: { id: contract.scenario.id, contract_revision: "1", seed: contract.seed.id },
    platform,
    runs: [],
  };
  const evidenceWriter = new EvidenceWriter(evidencePath, evidence);
  const oldTarget = prepare("initial", prepareProgram, request, artifact, platform, evidenceWriter);
  const initial = runScenario("initial", cli, oldTarget, contract, evidenceWriter, platform);

  const releaseRequest = canonicalJson({ schema_version: "1.0", target: oldTarget });
  const restartExecution = execute("restart.callback", restart.argv[0], restart.argv.slice(1), releaseRequest, evidenceWriter);
  requireReleaseSucceeded(restartExecution, oldTarget, "platform restart callback");
  const oldSession = execute("restart.old_session", cli, initial.continuation.slice(1), "", evidenceWriter);
  if (oldSession.exitCode === 0 || oldSession.raw?.status !== "failed" || oldSession.raw?.error?.kind !== "sessionExpired") {
    fail("old session remained usable after platform restart");
  }

  const newTarget = prepare("restart", prepareProgram, request, artifact, platform, evidenceWriter);
  const restartScenario = runScenario("restart", cli, newTarget, contract, evidenceWriter, platform);
  if (initial.target_sha256 === restartScenario.target_sha256) fail("restart did not mint a fresh Target reference");
  if (initial.session_sha256 === restartScenario.session_sha256) fail("restart did not mint a fresh Session");
  if (initial.generation === restartScenario.generation) fail("restart did not change process generation");

  evidence.initial = { target_sha256: initial.target_sha256, session_sha256: initial.session_sha256, generation: initial.generation };
  evidence.restart = {
    callback_command_id: restartExecution.record.command_id,
    old_session_command_id: oldSession.record.command_id,
    target_sha256: restartScenario.target_sha256,
    session_sha256: restartScenario.session_sha256,
    generation: restartScenario.generation,
  };
  evidenceWriter.persist();
}

main().catch((error) => {
  process.stderr.write(`${redactText(error.message)}\n`);
  process.exitCode = 1;
});
