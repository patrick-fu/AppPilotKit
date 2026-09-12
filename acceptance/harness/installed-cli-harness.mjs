#!/usr/bin/env node

import { createHash } from "node:crypto";
import { closeSync, lstatSync, openSync, readFileSync, readdirSync, renameSync, unlinkSync, writeFileSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { spawnSync } from "node:child_process";
import { dirname, isAbsolute, join, resolve } from "node:path";
import { isDeepStrictEqual } from "node:util";

const REDACTED = "<redacted>";
const capabilityId = "acceptance.foundation.state";
const allowedPreparePlatformsByHostPlatform = Object.freeze({
  ios: Object.freeze(["ios-simulator", "ios-device"]),
  android: Object.freeze(["android-emulator", "android-device"]),
});
const prepareEncodingByHostPlatform = Object.freeze({
  ios: "ios-app-tree-v1",
  android: "raw-file-v1",
});
const safeMachineResultKinds = new Set(["succeeded", "sessionExpired"]);
let activeCanary;

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

function validateFoundationContract(contract, platform) {
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

function firstObject(...values) {
  return values.find((value) => value && !Array.isArray(value) && typeof value === "object");
}

function validateCatalogContract(contract, platform) {
  const scenario = requireObject(contract.scenario, "contract.scenario");
  if (scenario.catalog_membership && scenario.catalog_membership !== "not_a_semantic_capability" && scenario.catalog_membership !== "semantic_capability") {
    fail("catalog scenario catalog_membership is invalid");
  }
  const seed = requireObject(scenario.seed, "contract.scenario.seed");
  requireString(seed.id, "catalog scenario seed id");
  const resources = Array.isArray(scenario.capabilities?.resources) ? scenario.capabilities.resources : (Array.isArray(scenario.resources) ? scenario.resources : []);
  const actions = Array.isArray(scenario.capabilities?.actions) ? scenario.capabilities.actions : (Array.isArray(scenario.actions) ? scenario.actions : []);
  const declared = [...resources, ...actions];
  const resource = requireObject(firstObject(scenario.resource, resources.find((item) => item?.kind === "resource") ?? resources[0], declared.find((item) => item?.kind === "resource")), "contract.scenario.resource");
  const ordinaryAction = firstObject(scenario.ordinary_action, scenario.ordinary, actions.find((item) => item?.id?.includes("increment")), actions.find((item) => item?.authorization !== "destructive_authorization" && item?.authorization !== "destructiveAuthorization"), declared.find((item) => item?.kind === "action" && item.authorization !== "destructive_authorization" && item.authorization !== "destructiveAuthorization"), scenario.action);
  const destructiveAction = firstObject(scenario.destructive_action, scenario.destructive, actions.find((item) => item?.id?.includes("reset")), actions.find((item) => item?.authorization === "destructive_authorization" || item?.authorization === "destructiveAuthorization"), declared.find((item) => item?.kind === "action" && (item.authorization === "destructive_authorization" || item.authorization === "destructiveAuthorization")));
  if (!ordinaryAction || !destructiveAction) fail("catalog scenario must declare ordinary and destructive actions");
  for (const [label, declaration, expectedKind] of [
    ["resource", resource, "resource"],
    ["ordinary action", ordinaryAction, "action"],
    ["destructive action", destructiveAction, "action"],
  ]) {
    requireString(declaration.id, `${label} id`);
    if (declaration.kind && declaration.kind !== expectedKind) fail(`${label} kind is invalid`);
    if (!Number.isSafeInteger(declaration.declaration_revision) || declaration.declaration_revision < 1) {
      fail(`${label} declaration revision is invalid`);
    }
  }
  const resourceSchema = requireObject(resource.value_schema, "catalog resource value schema");
  requireString(resourceSchema.id, "catalog resource value schema id");
  requireString(resourceSchema.digest, "catalog resource value schema digest");
  if (!Number.isSafeInteger(resourceSchema.revision) || resourceSchema.revision < 1) fail("catalog resource value schema revision is invalid");
  const canary = scenario.observations?.secret?.fixed_canary ?? scenario.secret_canary ?? scenario.canary ?? contract.secret_canary;
  if (canary !== undefined) requireString(canary, "catalog secret canary");
  return {
    scenario,
    seed,
    resource,
    ordinaryAction,
    destructiveAction,
    canary,
    platform,
    publicValue: scenario.public_value ?? scenario.resource_public_value ?? {},
  };
}

function validateContract(contract, platform) {
  const id = requireObject(contract.scenario, "contract.scenario").id;
  if (id === "demo.foundation") return validateFoundationContract(contract, platform);
  if (id === "demo.catalog") return validateCatalogContract(contract, platform);
  fail(`unsupported contract scenario ${id}`);
}

function redactText(value, canary = activeCanary) {
  const text = String(value);
  const escapedCanary = canary ? canary.replace(/[.*+?^${}()|[\]\\]/g, "\\$&") : undefined;
  return text
    .replace(escapedCanary ? new RegExp(escapedCanary, "g") : /$^/, REDACTED)
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
  if (activeCanary) {
    ensureNoCanary(argv, activeCanary, `${role} argv`);
    ensureNoCanary(input, activeCanary, `${role} stdin`);
  }
  const record = evidenceWriter.start(role, argv, input, prepareRequest);
  if (preflight) preflight();
  const diagnosticsEnabled = process.env.APPPILOTKIT_INTERNAL_PREPARE_FAILURE_DIAGNOSTICS === "1";
  const childEnv = { ...process.env };
  if (activeCanary) ensureNoCanary(childEnv, activeCanary, `${role} environment`);
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
  if (activeCanary) {
    ensureNoCanary(stdout, activeCanary, `${role} stdout`);
    ensureNoCanary(stderr, activeCanary, `${role} stderr`);
  }
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

function commandArgsFromNextAction(action, cli) {
  if (!action || !Array.isArray(action.argv) || action.argv.length < 2) fail("Next Action must contain an executable and argv");
  const argv = action.argv[0] === cli || action.argv[0] === "apppilotkit" || String(action.argv[0]).endsWith("/bin/apppilotkit") ? action.argv.slice(1) : action.argv;
  if (!argv.length) fail("Next Action argv is empty");
  return argv;
}

function actionForCapability(result, capabilityId, label, declaration) {
  const action = result.next_actions.find((candidate) => {
    if (!Array.isArray(candidate?.argv)) return false;
    const argv = candidate.argv;
    const capIndex = argv.findIndex((value) => value === "--capability");
    return candidate.id === "catalog.show" &&
      capIndex >= 0 && argv[capIndex + 1] === capabilityId;
  });
  if (!action) fail(`${label} catalog.show Next Action was not returned for the listed capability`);
  if (action.side_effect !== "read_only" || action.retry_safety !== "safe") {
    fail(`${label} catalog.show Next Action is not safe and read-only`);
  }
  const args = commandArgsFromNextAction(action, "apppilotkit");
  if (args[0] !== "catalog" || args[1] !== "show" || actionArgument(action.argv, "--capability") !== capabilityId) {
    fail(`${label} catalog.show Next Action is not bound to the listed capability`);
  }
  if (Number(actionArgument(action.argv, "--declaration-revision")) !== declaration.declaration_revision) {
    fail(`${label} catalog.show Next Action has the wrong declaration revision`);
  }
  return action;
}

function capabilityKey(item) {
  return `${item?.id ?? ""}|${item?.kind ?? ""}|${item?.declaration_revision ?? ""}`;
}

function catalogMembership(capabilities) {
  if (!Array.isArray(capabilities)) fail("catalog capabilities must be an array");
  return capabilities.map((item) => capabilityKey(item)).sort();
}

function ensureNoCanary(value, canary, label) {
  if (canary && JSON.stringify(value ?? "").includes(canary)) fail(`${label} disclosed the secret canary`);
}

function requireCatalogDeclaration(showResult, declaration, label) {
  const data = requireObject(showResult.data, `${label} data`);
  if (data.id !== declaration.id || (data.kind !== "action" && data.kind !== "resource") || data.declaration_revision !== declaration.declaration_revision) {
    fail(`${label} declaration does not match the contract`);
  }
  if (declaration.kind === "resource") {
    if (data.kind !== "resource" || !isDeepStrictEqual(data.value_schema, declaration.value_schema)) fail(`${label} resource schema does not match the contract`);
  } else if (data.kind !== "action" || !data.input_schema || !data.policy) {
    fail(`${label} action declaration is incomplete`);
  } else {
    if (!isDeepStrictEqual(data.input_schema, declaration.input_schema)) fail(`${label} action input schema does not match the contract`);
    if (declaration.policy && !isDeepStrictEqual(data.policy, declaration.policy)) fail(`${label} action policy does not match the contract`);
  }
  return data;
}

function requireExpectedFailure(execution, kind, label, canary) {
  if (execution.exitCode === 0 || execution.raw?.status !== "failed" || execution.raw?.error?.kind !== kind) {
    fail(`${label} must fail with ${kind}`);
  }
  ensureNoCanary(execution.stdout, canary, label);
  ensureNoCanary(execution.stderr, canary, `${label} stderr`);
  ensureNoCanary(execution.raw, canary, `${label} Machine Result`);
  return execution.raw;
}

function derivedAction(result, id, capabilityId, declaration, mode, fallback) {
  const source = result.next_actions.find((candidate) => Array.isArray(candidate?.argv)) ?? fallback;
  if (!source) return undefined;
  const sourceArgs = commandArgsFromNextAction(source, "apppilotkit");
  const base = [];
  for (let index = 0; index < sourceArgs.length; index += 1) {
    if (sourceArgs[index] === "--session" || sourceArgs[index] === "--target") base.push(sourceArgs[index], sourceArgs[++index]);
    else if (sourceArgs[index]?.startsWith("--session=") || sourceArgs[index]?.startsWith("--target=")) base.push(sourceArgs[index]);
  }
  const sessionCount = base.filter((value) => value === "--session" || value.startsWith("--session=")).length;
  const targetCount = base.filter((value) => value === "--target" || value.startsWith("--target=")).length;
  if (sessionCount !== 1 || targetCount !== 1) fail(`${id} derived Next Action lost the Target-issued session binding`);
  const action = { id, argv: ["apppilotkit", "catalog", mode, "--capability", capabilityId, "--declaration-revision", String(declaration.declaration_revision), ...base, "--output", "json", "--non-interactive"], side_effect: mode === "invoke" ? "app_mutation" : "read_only", retry_safety: mode === "invoke" ? "requires_idempotency_key" : "safe", preconditions: [], reason: `Inspect ${capabilityId}` };
  const schema = mode === "schema" ? declaration.value_schema ?? declaration.input_schema : declaration.value_schema ?? declaration.input_schema;
  if (mode === "schema" && schema) action.argv.push("--schema-id", schema.id, "--schema-revision", String(schema.revision), "--schema-digest", schema.digest);
  if (mode === "query" && schema) action.argv.push("--value-schema-id", schema.id, "--value-schema-revision", String(schema.revision), "--value-schema-digest", schema.digest);
  if (mode === "invoke" && schema) {
    action.argv.push("--input-schema-id", schema.id, "--input-schema-revision", String(schema.revision), "--input-schema-digest", schema.digest);
    action.argv.push("--input", JSON.stringify(capabilityId.includes("increment") ? { amount: 1 } : { confirm: "reset-catalog-v1" }));
  }
  return action;
}

function schemaActionFrom(result, capabilityId, label, declaration, fallback) {
  const action = result.next_actions.find((candidate) => {
    if (candidate?.id !== "catalog.schema" || !Array.isArray(candidate.argv)) return false;
    const index = candidate.argv.findIndex((value) => value === "--capability");
    return index >= 0 && candidate.argv[index + 1] === capabilityId;
  });
  return action ?? derivedAction(result, "catalog.schema", capabilityId, declaration, "schema", fallback) ?? fail(`${label} schema Next Action was not returned`);
}

function queryActionFrom(result, capabilityId, label, declaration, fallback) {
  const action = result.next_actions.find((candidate) => {
    if (candidate?.id !== "catalog.query" || !Array.isArray(candidate.argv)) return false;
    const index = candidate.argv.findIndex((value) => value === "--capability");
    return index >= 0 && candidate.argv[index + 1] === capabilityId;
  });
  return action ?? derivedAction(result, "catalog.query", capabilityId, declaration, "query", fallback) ?? fail(`${label} query Next Action was not returned`);
}

function invokeActionFrom(result, capabilityId, label, declaration, fallback) {
  const action = result.next_actions.find((candidate) => {
    if (candidate?.id !== "catalog.invoke" || !Array.isArray(candidate.argv)) return false;
    const index = candidate.argv.findIndex((value) => value === "--capability");
    return index >= 0 && candidate.argv[index + 1] === capabilityId;
  });
  return action ?? derivedAction(result, "catalog.invoke", capabilityId, declaration, "invoke", fallback) ?? fail(`${label} invoke Next Action was not returned`);
}

function invokeInput(argv) {
  const index = argv.findIndex((value) => value === "--input");
  return index >= 0 ? argv[index + 1] : undefined;
}

function replaceOption(argv, option, value) {
  const copy = [...argv];
  const index = copy.findIndex((item) => item === option || item.startsWith(`${option}=`));
  if (index < 0) fail(`invoke Next Action is missing ${option}`);
  if (copy[index] === option) copy[index + 1] = value;
  else copy[index] = `${option}=${value}`;
  return copy;
}

function runCatalogScenario(phase, cli, target, contract, evidenceWriter, platform) {
  const { resource, ordinaryAction, destructiveAction, canary } = contract;
  const list = execute(`${phase}.catalog.list`, cli, ["catalog", "list", `--target=${target}`, "--output=json", "--non-interactive"], "", evidenceWriter);
  ensureNoCanary(list.stdout, canary, `${phase} catalog list`);
  const listResult = requireMachineSucceeded(list, `${phase} catalog list`);
  ensureNoCanary(listResult, canary, `${phase} catalog list Machine Result`);
  const listData = requireObject(listResult.data, `${phase} catalog list data`);
  const catalog = requireObject(listData.catalog, `${phase} catalog`);
  if (!Number.isSafeInteger(catalog.generation) || catalog.generation < 1 || typeof catalog.id !== "string") fail(`${phase} catalog identity is invalid`);
  const capabilities = listData.capabilities;
  const expectedMembership = [resource, ordinaryAction, destructiveAction].map(capabilityKey).sort();
  if (!isDeepStrictEqual(catalogMembership(capabilities), expectedMembership)) fail(`${phase} catalog membership is not exactly one resource and two actions`);
  const catalogIdentity = canonicalJson(catalog);
  const continuation = actionForCapability(listResult, resource.id, `${phase} resource`, resource);
  const firstShow = execute(`${phase}.catalog.show.resource`, cli, commandArgsFromNextAction(continuation, cli), "", evidenceWriter);
  const firstShowResult = requireMachineSucceeded(firstShow, `${phase} catalog resource show`);
  ensureNoCanary(firstShowResult, canary, `${phase} catalog resource show`);
  requireCatalogDeclaration(firstShowResult, resource, `${phase} catalog resource show`);

  const resourceSchema = schemaActionFrom(firstShowResult, resource.id, `${phase} resource`, resource, continuation);
  const resourceSchemaResult = execute(`${phase}.catalog.schema.resource`, cli, commandArgsFromNextAction(resourceSchema, cli), "", evidenceWriter);
  const resourceSchemaMachine = requireMachineSucceeded(resourceSchemaResult, `${phase} catalog resource schema`);
  ensureNoCanary(resourceSchemaMachine, canary, `${phase} catalog resource schema`);
  const resourceSchemaData = requireObject(resourceSchemaMachine.data, `${phase} resource schema data`);
  if (!isDeepStrictEqual(resourceSchemaData.schema, resource.value_schema)) fail(`${phase} resource schema handle mismatch`);
  if (resource.document && !isDeepStrictEqual(resourceSchemaData.document, resource.document)) fail(`${phase} resource schema document mismatch`);

  const resourceQuery = queryActionFrom(resourceSchemaMachine, resource.id, `${phase} resource`, resource, resourceSchema);
  const beforeQuery = execute(`${phase}.catalog.query.resource.before`, cli, commandArgsFromNextAction(resourceQuery, cli), "", evidenceWriter);
  const beforeMachine = requireMachineSucceeded(beforeQuery, `${phase} resource query before`);
  ensureNoCanary(beforeMachine, canary, `${phase} resource query before`);
  const beforeData = requireObject(beforeMachine.data, `${phase} resource query before data`);
  const beforeValue = beforeData.value;
  if (!isDeepStrictEqual(beforeData.value_schema, resource.value_schema)) fail(`${phase} resource query schema mismatch`);

  const showOrdinary = actionForCapability(listResult, ordinaryAction.id, `${phase} ordinary action`, ordinaryAction);
  const ordinaryShow = execute(`${phase}.catalog.show.ordinary`, cli, commandArgsFromNextAction(showOrdinary, cli), "", evidenceWriter);
  const ordinaryShowMachine = requireMachineSucceeded(ordinaryShow, `${phase} ordinary show`);
  ensureNoCanary(ordinaryShowMachine, canary, `${phase} ordinary show`);
  requireCatalogDeclaration(ordinaryShowMachine, ordinaryAction, `${phase} ordinary show`);
  const ordinarySchema = schemaActionFrom(ordinaryShowMachine, ordinaryAction.id, `${phase} ordinary`, ordinaryAction, showOrdinary);
  const ordinarySchemaExecution = execute(`${phase}.catalog.schema.ordinary`, cli, commandArgsFromNextAction(ordinarySchema, cli), "", evidenceWriter);
  const ordinarySchemaMachine = requireMachineSucceeded(ordinarySchemaExecution, `${phase} ordinary schema`);
  ensureNoCanary(ordinarySchemaMachine, canary, `${phase} ordinary schema`);
  const ordinaryInvoke = invokeActionFrom(ordinarySchemaMachine, ordinaryAction.id, `${phase} ordinary`, ordinaryAction, ordinarySchema);
  const ordinaryInvokeArgs = commandArgsFromNextAction(ordinaryInvoke, cli);
  const ordinaryInvokeExecution = execute(`${phase}.catalog.invoke.ordinary.1`, cli, ordinaryInvokeArgs, "", evidenceWriter);
  const ordinaryInvokeMachine = requireMachineSucceeded(ordinaryInvokeExecution, `${phase} ordinary invoke`);
  ensureNoCanary(ordinaryInvokeMachine, canary, `${phase} ordinary invoke`);
  if (ordinaryInvokeMachine.side_effect !== undefined && ordinaryInvokeMachine.side_effect === "read_only") fail(`${phase} ordinary invoke was not classified as a mutation`);
  const session = actionArgument(continuation.argv, "--session");
  const countOneList = execute(`${phase}.catalog.list.count1`, cli, ["catalog", "list", `--session=${session}`, `--target=${target}`, "--output=json", "--non-interactive"], "", evidenceWriter);
  const countOneListResult = requireMachineSucceeded(countOneList, `${phase} catalog list count1`);
  ensureNoCanary(countOneListResult, canary, `${phase} catalog list count1`);
  const countOneListData = requireObject(countOneListResult.data, `${phase} catalog list count1 data`);
  const countOneCatalog = requireObject(countOneListData.catalog, `${phase} catalog list count1 catalog`);
  if (countOneCatalog.id !== catalog.id || countOneCatalog.generation !== catalog.generation || !isDeepStrictEqual(catalogMembership(countOneListData.capabilities), expectedMembership)) {
    fail(`${phase} catalog membership changed while the resource was becoming unavailable`);
  }
  const countOneQuery = execute(`${phase}.catalog.query.resource.count1`, cli, commandArgsFromNextAction(resourceQuery, cli), "", evidenceWriter);
  requireExpectedFailure(countOneQuery, "semantic.unavailable", `${phase} count1 query`, canary);

  const secondInvoke = execute(`${phase}.catalog.invoke.ordinary.2`, cli, ordinaryInvokeArgs, "", evidenceWriter);
  const secondInvokeMachine = requireMachineSucceeded(secondInvoke, `${phase} ordinary invoke second`);
  ensureNoCanary(secondInvokeMachine, canary, `${phase} ordinary invoke second`);
  const countTwoQuery = execute(`${phase}.catalog.query.resource.count2`, cli, commandArgsFromNextAction(resourceQuery, cli), "", evidenceWriter);
  const afterMachine = requireMachineSucceeded(countTwoQuery, `${phase} resource query count2`);
  const afterValue = requireObject(afterMachine.data, `${phase} resource query count2 data`).value;
  const beforeCounter = beforeValue?.count ?? beforeValue?.counter;
  const afterCounter = afterValue?.count ?? afterValue?.counter;
  if (!Number.isSafeInteger(beforeCounter) || !Number.isSafeInteger(afterCounter) || beforeCounter !== 0 || afterCounter !== 2 || beforeCounter === afterCounter) fail(`${phase} ordinary action did not change the resource counter from 0 to 2`);
  if (beforeValue.available !== true || afterValue.available !== true) fail(`${phase} resource availability did not restore after the second increment`);
  const countTwoList = execute(`${phase}.catalog.list.count2`, cli, ["catalog", "list", `--session=${session}`, `--target=${target}`, "--output=json", "--non-interactive"], "", evidenceWriter);
  const countTwoListResult = requireMachineSucceeded(countTwoList, `${phase} catalog list count2`);
  ensureNoCanary(countTwoListResult, canary, `${phase} catalog list count2`);
  const countTwoListData = requireObject(countTwoListResult.data, `${phase} catalog list count2 data`);
  const countTwoCatalog = requireObject(countTwoListData.catalog, `${phase} catalog list count2 catalog`);
  if (countTwoCatalog.id !== catalog.id || countTwoCatalog.generation !== catalog.generation || !isDeepStrictEqual(catalogMembership(countTwoListData.capabilities), expectedMembership)) {
    fail(`${phase} catalog membership changed after availability was restored`);
  }

  const destructiveShow = actionForCapability(listResult, destructiveAction.id, `${phase} destructive action`, destructiveAction);
  const destructiveShowExecution = execute(`${phase}.catalog.show.destructive`, cli, commandArgsFromNextAction(destructiveShow, cli), "", evidenceWriter);
  const destructiveShowMachine = requireMachineSucceeded(destructiveShowExecution, `${phase} destructive show`);
  ensureNoCanary(destructiveShowMachine, canary, `${phase} destructive show`);
  const destructiveInvoke = invokeActionFrom(destructiveShowMachine, destructiveAction.id, `${phase} destructive`, destructiveAction, destructiveShow);
  const denied = execute(`${phase}.catalog.invoke.destructive.denied`, cli, commandArgsFromNextAction(destructiveInvoke, cli), "", evidenceWriter);
  requireExpectedFailure(denied, "action.policyDenied", `${phase} destructive denial`, canary);
  const denialQuery = execute(`${phase}.catalog.query.resource.denial`, cli, commandArgsFromNextAction(resourceQuery, cli), "", evidenceWriter);
  const denialValue = requireObject(requireMachineSucceeded(denialQuery, `${phase} denial side-effect query`).data, `${phase} denial query data`).value;
  if (!isDeepStrictEqual(denialValue, afterValue)) fail(`${phase} destructive denial changed resource state`);

  const invalid = (label, args, kinds) => {
    const execution = execute(`${phase}.${label}`, cli, args, "", evidenceWriter);
    if (execution.exitCode === 0 || execution.raw?.status !== "failed" || !kinds.includes(execution.raw?.error?.kind)) fail(`${phase} ${label} must fail before side effects`);
    ensureNoCanary(execution.stdout, canary, `${phase} ${label}`);
    ensureNoCanary(execution.stderr, canary, `${phase} ${label} stderr`);
    ensureNoCanary(execution.raw, canary, `${phase} ${label} Machine Result`);
    return execution;
  };
  invalid("catalog.invoke.ordinary.schema-mismatch", replaceOption(ordinaryInvokeArgs, "--input-schema-digest", "sha256:" + "0".repeat(64)), ["schemaMismatch", "semantic.schemaMismatch", "invalidParams", "cli.invalidInvocation"]);
  let undeclaredArgs = ordinaryInvokeArgs;
  const undeclaredInput = invokeInput(undeclaredArgs);
  if (undeclaredInput !== undefined) {
    let parsed;
    try { parsed = JSON.parse(undeclaredInput); } catch { parsed = {}; }
    undeclaredArgs = replaceOption(undeclaredArgs, "--input", JSON.stringify({ ...parsed, undeclared: "journey7-undeclared-field" }));
  }
  invalid("catalog.invoke.ordinary.undeclared", undeclaredArgs, ["undeclaredFields", "semantic.schemaMismatch", "invalidParams", "cli.invalidInvocation"]);
  const oversized = replaceOption(ordinaryInvokeArgs, "--input", JSON.stringify({ amount: "x".repeat(80 * 1024) }));
  invalid("catalog.invoke.ordinary.oversized", oversized, ["inputTooLarge", "resourceExhausted", "semantic.schemaMismatch", "invalidParams", "cli.invalidInvocation"]);

  const thirdInvoke = execute(`${phase}.catalog.invoke.ordinary.3`, cli, ordinaryInvokeArgs, "", evidenceWriter);
  const thirdInvokeMachine = requireMachineSucceeded(thirdInvoke, `${phase} ordinary invoke third`);
  ensureNoCanary(thirdInvokeMachine, canary, `${phase} ordinary invoke third`);
  const countThreeQuery = execute(`${phase}.catalog.query.resource.count3`, cli, commandArgsFromNextAction(resourceQuery, cli), "", evidenceWriter);
  requireExpectedFailure(countThreeQuery, "semantic.disclosureDenied", `${phase} count3 query`, canary);
  if (JSON.stringify(countThreeQuery.raw).match(/unclassified|secret|canary/i)) fail(`${phase} count3 query disclosed unsafe output`);

  const fourthInvoke = execute(`${phase}.catalog.invoke.ordinary.4`, cli, ordinaryInvokeArgs, "", evidenceWriter);
  const fourthInvokeMachine = requireMachineSucceeded(fourthInvoke, `${phase} ordinary invoke fourth`);
  ensureNoCanary(fourthInvokeMachine, canary, `${phase} ordinary invoke fourth`);
  const countFourQuery = execute(`${phase}.catalog.query.resource.count4`, cli, commandArgsFromNextAction(resourceQuery, cli), "", evidenceWriter);
  requireExpectedFailure(countFourQuery, "resourceExhausted", `${phase} count4 query`, canary);

  return {
    target_sha256: sha256(target),
    session_sha256: sha256(session),
    generation: catalog.generation,
    session: actionArgument(continuation.argv, "--session"),
    continuation: continuation.argv,
    catalog_identity: catalogIdentity,
    membership: expectedMembership,
    membership_observations: {
      initial: expectedMembership,
      unavailable: catalogMembership(countOneListData.capabilities),
      restored: catalogMembership(countTwoListData.capabilities),
    },
    availability_transition: ["available", "unavailable", "available"],
  };
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
  activeCanary = contract.canary;
  const request = requireObject(config.prepare_request, "configuration prepare_request");
  const expectedPrepareRequestKeys = ["app_artifact", "app_id", "artifact_encoding", "device_selector", "platform", "schema_version"];
  if (!isDeepStrictEqual(Object.keys(request).sort(), expectedPrepareRequestKeys)) fail("prepare request must contain exactly six keys");
  if (request.schema_version !== "1.0") fail("prepare request schema_version must be 1.0");
  const allowedPreparePlatforms = allowedPreparePlatformsByHostPlatform[platform];
  if (!allowedPreparePlatforms.includes(request.platform)) {
    fail(`prepare request platform must be ${allowedPreparePlatforms.join(" or ")} for configuration platform ${platform}`);
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
  const isCatalog = contract.scenario.id === "demo.catalog";
  const scenarioRunner = isCatalog ? runCatalogScenario : runScenario;
  const initial = scenarioRunner("initial", cli, oldTarget, contract, evidenceWriter, platform);

  const releaseRequest = canonicalJson({ schema_version: "1.0", target: oldTarget });
  const restartExecution = execute("restart.callback", restart.argv[0], restart.argv.slice(1), releaseRequest, evidenceWriter);
  requireReleaseSucceeded(restartExecution, oldTarget, "platform restart callback");
  const oldSession = execute("restart.old_session", cli, initial.continuation.slice(1), "", evidenceWriter);
  if (oldSession.exitCode === 0 || oldSession.raw?.status !== "failed" || oldSession.raw?.error?.kind !== "sessionExpired") {
    fail("old session remained usable after platform restart");
  }

  const newTarget = prepare("restart", prepareProgram, request, artifact, platform, evidenceWriter);
  const restartScenario = scenarioRunner("restart", cli, newTarget, contract, evidenceWriter, platform);
  if (initial.target_sha256 === restartScenario.target_sha256) fail("restart did not mint a fresh Target reference");
  if (initial.session_sha256 === restartScenario.session_sha256) fail("restart did not mint a fresh Session");
  if (initial.generation === restartScenario.generation) fail("restart did not change process generation");

  evidence.initial = {
    target_sha256: initial.target_sha256,
    session_sha256: initial.session_sha256,
    generation: initial.generation,
    catalog_identity: initial.catalog_identity,
    membership: initial.membership,
    membership_observations: initial.membership_observations,
    availability_transition: initial.availability_transition,
  };
  evidence.restart = {
    callback_command_id: restartExecution.record.command_id,
    old_session_command_id: oldSession.record.command_id,
    target_sha256: restartScenario.target_sha256,
    session_sha256: restartScenario.session_sha256,
    generation: restartScenario.generation,
    catalog_identity: restartScenario.catalog_identity,
    membership: restartScenario.membership,
    membership_observations: restartScenario.membership_observations,
    availability_transition: restartScenario.availability_transition,
  };
  if (isCatalog) {
    if (!isDeepStrictEqual(initial.membership, restartScenario.membership)) fail("restart catalog membership changed unexpectedly");
    if (contract.canary && JSON.stringify(evidence).includes(contract.canary)) fail("public evidence disclosed the secret canary");
  }
  evidenceWriter.persist();
}

main().catch((error) => {
  process.stderr.write(`${redactText(error.message)}\n`);
  process.exitCode = 1;
});
