import assert from "node:assert/strict";
import { chmod, mkdtemp, readFile, writeFile, mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";

const repository = resolve(import.meta.dirname, "../../..");
const harness = join(repository, "acceptance/harness/installed-cli-harness.mjs");
const contract = join(repository, "acceptance/demo-foundation.contract.json");
const catalogContract = join(repository, "acceptance/demo-catalog.contract.json");

async function fixture({
  oldSessionFails = true,
  platform = "ios",
  firstListAction = "show",
  earlyPrepareExit,
  earlyCliExit,
  releaseExitCode = 0,
  releaseStatus = "succeeded",
  releaseEchoTarget = false,
  diagnostics = false,
  diagnosticCli = false,
} = {}) {
  const root = await mkdtemp(join(tmpdir(), "apppilotkit-acceptance-"));
  const prefix = join(root, "prefix");
  await mkdir(join(prefix, "bin"), { recursive: true });
  await mkdir(join(prefix, "libexec"), { recursive: true });
  const state = join(root, "state.json");
  await writeFile(state, JSON.stringify({
    phase: 0,
    restarted: false,
    oldSessionFails,
    firstListAction,
    earlyPrepareExit,
    earlyCliExit,
    releaseExitCode,
    releaseStatus,
    releaseEchoTarget,
  }));
  const appArtifact = platform === "ios" ? join(root, "AcceptanceHost.app") : join(root, "acceptance-host.apk");
  if (platform === "ios") await mkdir(appArtifact);
  else await writeFile(appArtifact, "acceptance host apk fixture");

  await executable(join(prefix, "libexec/apppilotkit-target-prepare"), `#!/usr/bin/env node
const fs = require("node:fs");
const statePath = process.env.ACCEPTANCE_FAKE_STATE;
const state = JSON.parse(fs.readFileSync(statePath, "utf8"));
process.stdin.resume();
let input = "";
process.stdin.on("data", (chunk) => { input += chunk; });
process.stdin.on("end", () => {
  if (process.argv[2] === "--release-fd=0") {
    const request = JSON.parse(input);
    const expectedKeys = ["schema_version", "target"];
    if (JSON.stringify(Object.keys(request).sort()) !== JSON.stringify(expectedKeys)) process.exit(6);
    if (request.schema_version !== "1.0" || request.target !== "target_OldPublicReference_0123456789abcdef") process.exit(7);
    state.releaseInput = input;
    fs.writeFileSync(statePath, JSON.stringify(state));
    if (state.releaseExitCode) {
      process.stderr.write("release callback failed");
      process.exit(state.releaseExitCode);
    }
    if (state.releaseStatus !== "succeeded") {
      console.log(JSON.stringify({schema_version:"1.0",status:state.releaseStatus,error:{kind:"cleanup.failed",message:"cleanup failed"}}));
      process.exit(1);
    }
    state.restarted = true;
    fs.writeFileSync(statePath, JSON.stringify(state));
    const result = {schema_version:"1.0",status:"succeeded"};
    if (state.releaseEchoTarget) result.target = request.target;
    console.log(JSON.stringify(result));
    return;
  }
  const diagnosticFd = Number(process.env.APPPILOTKIT_INTERNAL_PREPARE_FAILURE_FD);
  if (Number.isSafeInteger(diagnosticFd) && diagnosticFd > 2) {
    fs.writeSync(diagnosticFd, JSON.stringify({schema_version:1,stage:"prepare",close_reason:"internal_error"}) + "\\n");
  }
  if (state.earlyPrepareExit) {
  process.stderr.write("private_prepare_secret_should_not_persist");
  process.exit(state.earlyPrepareExit);
  }
  const request = JSON.parse(input);
  const expectedKeys = ["app_artifact", "app_id", "artifact_encoding", "device_selector", "platform", "schema_version"];
  if (JSON.stringify(Object.keys(request).sort()) !== JSON.stringify(expectedKeys)) process.exit(6);
  if (request.schema_version !== "1.0" || request.platform !== process.env.ACCEPTANCE_FAKE_PREPARE_PLATFORM || !request.app_artifact.startsWith("/")) process.exit(7);
  state.phase += 1;
  fs.writeFileSync(statePath, JSON.stringify(state));
  const target = state.phase === 1 ? "target_OldPublicReference_0123456789abcdef" : "target_NewPublicReference_fedcba9876543210";
  console.log(JSON.stringify({schema_version:"1.0",status:"succeeded",ready_target:{schema_version:"1.0",target,issued_at_unix_ms:1,expires_at_unix_ms:2},private_prepare_stdout:"session_prepare_private_0123456789"}));
});
`);

  await executable(join(prefix, "bin/apppilotkit"), `#!/usr/bin/env node
const fs = require("node:fs");
const state = JSON.parse(fs.readFileSync(process.env.ACCEPTANCE_FAKE_STATE, "utf8"));
const args = process.argv.slice(2);
const value = (name) => args.find((item) => item.startsWith(name + "="))?.slice(name.length + 1) ?? args[args.indexOf(name) + 1];
const target = value("--target");
const session = value("--session");
const result = (status, data, error, next_actions = []) => ({schema_version:"1.0",cli_version:"1.2.3",status,command:args.slice(0, 2),side_effect:"read_only",retry_safety:"safe",...(data ? {data} : {}),...(error ? {error} : {}),disclosure:{truncated:false,returned_items:1},artifacts:[],next_actions});
const handle = {id:"schema_demo_foundation_state_v1",revision:1,digest:"sha256:b63382887a98877b06466df4d6aa2a2fd788e89c5c1db39170720fd6c721a08c"};
if (args[0] !== "catalog") process.exit(9);
if (state.earlyCliExit) {
  const diagnosticFd = Number(process.env.APPPILOTKIT_INTERNAL_PREPARE_FAILURE_FD);
  if (process.env.ACCEPTANCE_FAKE_CLI_DIAGNOSTIC === "1" && Number.isSafeInteger(diagnosticFd) && diagnosticFd > 2) {
    fs.writeSync(diagnosticFd, JSON.stringify({stage:"session_open",close_reason:"peer_closed",reason_code:"target_no_session_frames"}) + "\\n");
  }
  process.stderr.write("private_cli_token_should_not_persist");
  process.exit(state.earlyCliExit);
}
if (target === "target_OldPublicReference_0123456789abcdef" && session && state.restarted && state.oldSessionFails) {
  console.log(JSON.stringify(result("failed", undefined, {kind:"sessionExpired",message:"closed",retryable:false,details:{}})));
  process.exit(4);
}
const generation = target === "target_NewPublicReference_fedcba9876543210" ? 42 : 41;
const openedSession = target === "target_NewPublicReference_fedcba9876543210" ? "session_NewPublicReference_fedcba9876543210" : "session_OldPublicReference_0123456789abcdef";
if (args[1] === "list") {
  const show = {id:"catalog.show",argv:["apppilotkit","catalog","show","--capability","acceptance.foundation.state","--declaration-revision","1","--session",openedSession,"--target",target,"--output","json","--non-interactive","--returned-action-marker","foundation-show"],side_effect:"read_only",retry_safety:"safe",preconditions:[],reason:"show"};
  const next = state.firstListAction === "continue"
    ? {id:"catalog.list.continue",argv:["apppilotkit","catalog","list","--cursor","cursor_next"],side_effect:"read_only",retry_safety:"safe",preconditions:[],reason:"continue"}
    : state.firstListAction === "wrong-binding"
      ? {...show, argv:show.argv.map((value, index) => index === 4 ? "other.capability" : value)}
      : show;
  console.log(JSON.stringify(result("succeeded", {catalog:{id:"catalog_demo_foundation",generation},capabilities:[{id:"acceptance.foundation.state",kind:"resource",declaration_revision:1}]}, undefined, [next])));
}
else if (args[1] === "show") {
  const expected = ["catalog","show","--capability","acceptance.foundation.state","--declaration-revision","1","--session",openedSession,"--target",target,"--output","json","--non-interactive","--returned-action-marker","foundation-show"];
  if (JSON.stringify(args) !== JSON.stringify(expected)) process.exit(10);
  console.log(JSON.stringify(result("succeeded", {id:"acceptance.foundation.state",kind:"resource",declaration_revision:1,value_schema:handle})));
}
else if (args[1] === "schema") console.log(JSON.stringify(result("succeeded", {schema:handle,document:{$schema:"https://json-schema.org/draft/2020-12/schema",$id:"app://acceptance.foundation.state/value@1",type:"object",required:["scenario","seed","platform"],properties:{scenario:{type:"string",const:"demo.foundation"},seed:{type:"string",const:"foundation-v1"},platform:{type:"string"}},additionalProperties:false}})));
else if (args[1] === "query") console.log(JSON.stringify(result("succeeded", {bytes:67,value:{scenario:"demo.foundation",seed:"foundation-v1",platform:process.env.ACCEPTANCE_FAKE_PLATFORM},value_schema:handle})));
else process.exit(8);
`);

  const config = join(root, "run.json");
  const evidence = join(root, "evidence.json");
  await writeFile(config, JSON.stringify({
    prefix,
    platform,
    contract,
    prepare_request: {
      schema_version: "1.0",
      platform: platform === "ios" ? "ios-simulator" : "android-emulator",
      device_selector: platform === "ios" ? "iPhone 17 Pro" : "emulator-5554",
      app_id: "dev.apppilotkit.acceptancehost",
      app_artifact: appArtifact,
      artifact_encoding: platform === "ios" ? "ios-app-tree-v1" : "raw-file-v1",
    },
    restart: { argv: [join(prefix, "libexec/apppilotkit-target-prepare"), "--release-fd=0", "--output=json"] },
  }));
  return {
    root,
    state,
    config,
    evidence,
    platform,
    preparePlatform: platform === "ios" ? "ios-simulator" : "android-emulator",
    appArtifact,
    diagnostics,
    diagnosticCli,
  };
}

async function executable(path, source) {
  await writeFile(path, source);
  await chmod(path, 0o755);
}

function run(fixture) {
  return spawnSync(process.execPath, [harness, "--config", fixture.config, "--evidence", fixture.evidence], {
    encoding: "utf8",
    env: {
      ...process.env,
      ACCEPTANCE_FAKE_STATE: fixture.state,
      ACCEPTANCE_FAKE_PLATFORM: fixture.platform,
      ACCEPTANCE_FAKE_PREPARE_PLATFORM: fixture.preparePlatform,
      ...(fixture.diagnostics ? {
        APPPILOTKIT_INTERNAL_PREPARE_FAILURE_DIAGNOSTICS: "1",
        APPPILOTKIT_INTERNAL_PREPARE_FAILURE_SIDECAR: join(fixture.root, "prepare-failure.ndjson"),
      } : {}),
      ...(fixture.diagnosticCli ? { ACCEPTANCE_FAKE_CLI_DIAGNOSTIC: "1" } : {}),
    },
  });
}

test("explicit diagnostic sidecar reaches both installed prepare invocations without public evidence", async () => {
  const runFixture = await fixture({ diagnostics: true });
  const outcome = run(runFixture);
  assert.equal(outcome.status, 0, outcome.stderr);
  const sidecar = await readFile(join(runFixture.root, "prepare-failure.ndjson"), "utf8");
  const events = sidecar.trim().split("\n").map((line) => JSON.parse(line));
  assert.equal(events.length, 2);
  assert.deepEqual(events, [
    { schema_version: 1, stage: "prepare", close_reason: "internal_error" },
    { schema_version: 1, stage: "prepare", close_reason: "internal_error" },
  ]);
  const evidence = await readFile(runFixture.evidence, "utf8");
  assert.doesNotMatch(evidence, /prepare-failure|internal_error/);
});

test("demo.catalog contract freezes the Journey 7 public capability set and secret observation", async () => {
  const value = JSON.parse(await readFile(catalogContract, "utf8"));
  assert.equal(value.scenario.id, "demo.catalog");
  assert.deepEqual(value.scenario.capabilities.resources.map((item) => item.id), ["acceptance.catalog.state"]);
  assert.deepEqual(value.scenario.capabilities.actions.map((item) => item.id), ["acceptance.catalog.increment", "acceptance.catalog.reset"]);
  assert.equal(value.scenario.capabilities.resources[0].value_schema.digest, "sha256:b3ffcc96d1da03c447d243ba196f9af01a38447bbb03d5139aed9923c7300804");
  assert.equal(value.scenario.capabilities.actions[0].input_schema.digest, "sha256:bd75453e5e97b497e4ee54db7cd82f4e654d04da370d8106e645e0bb9769c2fe");
  assert.equal(value.scenario.capabilities.actions[1].input_schema.digest, "sha256:079b413f2ca72310290f3b07da82503b0dd74c408358570ead26ffeea94efd7c");
  assert.equal(value.scenario.observations.secret.expected, "never_disclosed");
  assert.equal(value.scenario.observations.secret.surfaces.includes("machine_result"), true);
});

test("explicit diagnostic sidecar reaches installed CLI failures without public evidence", async () => {
  const runFixture = await fixture({ diagnostics: true, diagnosticCli: true, earlyCliExit: 4 });
  const outcome = run(runFixture);
  assert.notEqual(outcome.status, 0);
  const sidecar = await readFile(join(runFixture.root, "prepare-failure.ndjson"), "utf8");
  const events = sidecar.trim().split("\n").map((line) => JSON.parse(line));
  assert.deepEqual(events, [
    { schema_version: 1, stage: "prepare", close_reason: "internal_error" },
    { stage: "session_open", close_reason: "peer_closed", reason_code: "target_no_session_frames" },
  ]);
  const evidence = await readFile(runFixture.evidence, "utf8");
  assert.doesNotMatch(evidence, /session_open|target_no_session_frames/);
});

test("retains public installed-CLI evidence across reset and true restart", async () => {
  const runFixture = await fixture();
  const outcome = run(runFixture);
  assert.equal(outcome.status, 0, outcome.stderr);
  const evidence = JSON.parse(await readFile(runFixture.evidence, "utf8"));
  assert.equal(evidence.scenario.id, "demo.foundation");
  assert.equal(evidence.platform, "ios");
  assert.equal(evidence.runs.length, 12);
  assert.equal(evidence.restart.callback_command_id, "restart.callback");
  assert.equal(evidence.restart.old_session_command_id, "restart.old_session");
  assert.notEqual(evidence.initial.target_sha256, evidence.restart.target_sha256);
  assert.notEqual(evidence.initial.session_sha256, evidence.restart.session_sha256);
  assert.notEqual(evidence.initial.generation, evidence.restart.generation);
  assert.equal(JSON.parse(await readFile(runFixture.state, "utf8")).phase, 2);
  assert.equal(
    JSON.parse(await readFile(runFixture.state, "utf8")).releaseInput,
    '{"schema_version":"1.0","target":"target_OldPublicReference_0123456789abcdef"}',
  );
  assert.deepEqual(evidence.runs.map((run) => run.command_id), [
    "initial.prepare",
    "initial.catalog.list",
    "initial.catalog.show",
    "initial.catalog.schema",
    "initial.catalog.query",
    "restart.callback",
    "restart.old_session",
    "restart.prepare",
    "restart.catalog.list",
    "restart.catalog.show",
    "restart.catalog.schema",
    "restart.catalog.query",
  ]);
  const query = evidence.runs.find((run) => run.command_id === "initial.catalog.query");
  const callback = evidence.runs.find((run) => run.command_id === "restart.callback");
  const staleContinuation = evidence.runs.find((run) => run.command_id === "restart.old_session");
  assert.ok(query.argv_shape.includes("--value-schema-id"));
  assert.ok(query.argv_shape.includes("--target=<redacted>"));
  assert.ok(query.argv_shape.includes("--session=<redacted>"));
  assert.deepEqual(staleContinuation.argv_shape.slice(0, 2), ["catalog", "show"]);
  assert.equal(query.single_machine_result, true);
  assert.equal(query.safe_kind, "succeeded");
  assert.equal(callback.stdin.json_type, "object");
  assert.match(callback.stdin.canonical_sha256, /^sha256:/);
  assert.equal(callback.single_machine_result, true);
  assert.equal(callback.safe_kind, "succeeded");
  assert.equal(staleContinuation.safe_kind, "sessionExpired");
  assert.match(JSON.stringify(evidence), /<redacted>/);
  assert.doesNotMatch(JSON.stringify(evidence), /target_(?:Old|New)PublicReference/);
  assert.doesNotMatch(JSON.stringify(evidence), /session_(?:Old|New|prepare_private)/);
  assert.doesNotMatch(JSON.stringify(evidence), /private_prepare_stdout/);
  assert.doesNotMatch(JSON.stringify(evidence), /AcceptanceHost\.app|restart complete|private_cli_token/);
  const prepare = evidence.runs.find((run) => run.command_id === "initial.prepare");
  assert.deepEqual(prepare.prepare_request.key_names, ["app_artifact", "app_id", "artifact_encoding", "device_selector", "platform", "schema_version"]);
  assert.equal(prepare.prepare_request.value_types.app_artifact, "string");
  assert.equal(prepare.prepare_request.artifact.type, "directory");
  assert.equal(prepare.prepare_request.artifact.exists, true);
  assert.equal(prepare.prepare_request.artifact.symlink, false);
  assert.match(prepare.prepare_request.artifact.controlled_tree_sha256, /^sha256:/);
  assert.match(prepare.stdin.canonical_sha256, /^sha256:/);
  assert.equal(prepare.stdin.json_type, "object");
  assert.equal(prepare.stdout.byte_count > 0, true);
  assert.match(prepare.stdout.sha256, /^sha256:/);
});

test("fails when the old public session remains usable after restart", async () => {
  const runFixture = await fixture({ oldSessionFails: false });
  const outcome = run(runFixture);
  assert.notEqual(outcome.status, 0);
  assert.match(outcome.stderr, /old session/i);
});

test("fails when the platform restart callback exits unsuccessfully", async () => {
  const runFixture = await fixture({ releaseExitCode: 5 });
  const outcome = run(runFixture);
  assert.notEqual(outcome.status, 0);
  assert.match(outcome.stderr, /platform restart callback exited 5/);
  const evidence = JSON.parse(await readFile(runFixture.evidence, "utf8"));
  assert.deepEqual(evidence.runs.map((run) => run.command_id), [
    "initial.prepare",
    "initial.catalog.list",
    "initial.catalog.show",
    "initial.catalog.schema",
    "initial.catalog.query",
    "restart.callback",
  ]);
});

test("fails closed when the release callback returns a failed machine result", async () => {
  const runFixture = await fixture({ releaseStatus: "failed" });
  const outcome = run(runFixture);
  assert.notEqual(outcome.status, 0);
  assert.match(outcome.stderr, /platform restart callback exited 1|platform restart callback did not report succeeded/i);
  const evidence = JSON.parse(await readFile(runFixture.evidence, "utf8"));
  assert.deepEqual(evidence.runs.map((run) => run.command_id), [
    "initial.prepare",
    "initial.catalog.list",
    "initial.catalog.show",
    "initial.catalog.schema",
    "initial.catalog.query",
    "restart.callback",
  ]);
  assert.equal(JSON.parse(await readFile(runFixture.state, "utf8")).phase, 1);
});

test("rejects a release callback result that echoes the opaque Target", async () => {
  const runFixture = await fixture({ releaseEchoTarget: true });
  const outcome = run(runFixture);
  assert.notEqual(outcome.status, 0);
  assert.match(outcome.stderr, /target/i);
  const evidence = JSON.parse(await readFile(runFixture.evidence, "utf8"));
  assert.equal(evidence.runs.at(-1).command_id, "restart.callback");
  assert.doesNotMatch(JSON.stringify(evidence), /target_(?:Old|New)PublicReference/);
});

test("rejects a restart config that does not use the installed release mode", async () => {
  const runFixture = await fixture();
  const config = JSON.parse(await readFile(runFixture.config, "utf8"));
  config.restart.argv = [join(runFixture.root, "platform-restart"), "--release-fd=0", "--output=json"];
  await writeFile(runFixture.config, JSON.stringify(config));
  const outcome = run(runFixture);
  assert.notEqual(outcome.status, 0);
  assert.match(outcome.stderr, /installed target-prepare release mode/i);
  assert.equal(JSON.parse(await readFile(runFixture.state, "utf8")).phase, 0);
});

test("records an early prepare exit before parsing or retaining private output", async () => {
  const runFixture = await fixture({ earlyPrepareExit: 1 });
  const outcome = run(runFixture);
  assert.notEqual(outcome.status, 0);
  const evidence = JSON.parse(await readFile(runFixture.evidence, "utf8"));
  assert.equal(evidence.runs.length, 1);
  assert.equal(evidence.runs[0].command_id, "initial.prepare");
  assert.equal(evidence.runs[0].exit_code, 1);
  assert.equal(evidence.runs[0].single_machine_result, false);
  assert.equal(evidence.runs[0].stderr.byte_count > 0, true);
  assert.doesNotMatch(JSON.stringify(evidence), /private_prepare_secret_should_not_persist/);
});

test("records an early CLI exit before parsing or retaining private output", async () => {
  const runFixture = await fixture({ earlyCliExit: 2 });
  const outcome = run(runFixture);
  assert.notEqual(outcome.status, 0);
  const evidence = JSON.parse(await readFile(runFixture.evidence, "utf8"));
  assert.equal(evidence.runs.length, 2);
  const list = evidence.runs[1];
  assert.equal(list.command_id, "initial.catalog.list");
  assert.equal(list.exit_code, 2);
  assert.equal(list.single_machine_result, false);
  assert.equal(list.stderr.byte_count > 0, true);
  assert.doesNotMatch(JSON.stringify(evidence), /private_cli_token_should_not_persist/);
});

test("records a bad artifact preflight without spawning target prepare", async () => {
  const runFixture = await fixture();
  const config = JSON.parse(await readFile(runFixture.config, "utf8"));
  config.prepare_request.app_artifact = join(runFixture.root, "missing.app");
  await writeFile(runFixture.config, JSON.stringify(config));
  const outcome = run(runFixture);
  assert.notEqual(outcome.status, 0);
  assert.match(outcome.stderr, /app artifact/i);
  assert.equal(JSON.parse(await readFile(runFixture.state, "utf8")).phase, 0);
  const evidence = JSON.parse(await readFile(runFixture.evidence, "utf8"));
  assert.equal(evidence.runs.length, 1);
  assert.equal(evidence.runs[0].prepare_request.artifact.type, "missing");
  assert.equal(evidence.runs[0].exit_code, undefined);
});

test("fails closed when the pre-spawn evidence write cannot be created", async () => {
  const runFixture = await fixture();
  const missingEvidenceDirectory = join(runFixture.root, "missing-evidence-directory");
  const outcome = spawnSync(process.execPath, [harness, "--config", runFixture.config, "--evidence", join(missingEvidenceDirectory, "evidence.json")], {
    encoding: "utf8",
    env: {
      ...process.env,
      ACCEPTANCE_FAKE_STATE: runFixture.state,
      ACCEPTANCE_FAKE_PLATFORM: runFixture.platform,
      ACCEPTANCE_FAKE_PREPARE_PLATFORM: runFixture.preparePlatform,
    },
  });
  assert.notEqual(outcome.status, 0);
  assert.match(outcome.stderr, /atomically write evidence/i);
  assert.equal(JSON.parse(await readFile(runFixture.state, "utf8")).phase, 0);
});

test("maps an Android public host platform to the production emulator prepare platform", async () => {
  const runFixture = await fixture({ platform: "android" });
  const outcome = run(runFixture);
  assert.equal(outcome.status, 0, outcome.stderr);
  const evidence = JSON.parse(await readFile(runFixture.evidence, "utf8"));
  assert.equal(evidence.platform, "android");
});

test("accepts an ios-device production prepare platform for the ios host", async () => {
  const runFixture = await fixture();
  const config = JSON.parse(await readFile(runFixture.config, "utf8"));
  config.prepare_request.platform = "ios-device";
  await writeFile(runFixture.config, JSON.stringify(config));
  runFixture.preparePlatform = "ios-device";
  const outcome = run(runFixture);
  assert.equal(outcome.status, 0, outcome.stderr);
  const evidence = JSON.parse(await readFile(runFixture.evidence, "utf8"));
  assert.equal(evidence.platform, "ios");
});

test("rejects an abstract platform in a production prepare request", async () => {
  const runFixture = await fixture();
  const config = JSON.parse(await readFile(runFixture.config, "utf8"));
  config.prepare_request.platform = "ios";
  await writeFile(runFixture.config, JSON.stringify(config));
  const outcome = run(runFixture);
  assert.notEqual(outcome.status, 0);
  assert.match(outcome.stderr, /ios-simulator/);
});

test("rejects a prepare request that is not the exact production six-key shape", async () => {
  const runFixture = await fixture();
  const config = JSON.parse(await readFile(runFixture.config, "utf8"));
  config.prepare_request.extra = "not accepted";
  await writeFile(runFixture.config, JSON.stringify(config));
  const outcome = run(runFixture);
  assert.notEqual(outcome.status, 0);
  assert.match(outcome.stderr, /exactly six keys/i);
  assert.equal(JSON.parse(await readFile(runFixture.state, "utf8")).phase, 0);
});

test("rejects an artifact encoding that does not match the production prepare platform", async () => {
  const runFixture = await fixture();
  const config = JSON.parse(await readFile(runFixture.config, "utf8"));
  config.prepare_request.artifact_encoding = "raw-file-v1";
  await writeFile(runFixture.config, JSON.stringify(config));
  const outcome = run(runFixture);
  assert.notEqual(outcome.status, 0);
  assert.match(outcome.stderr, /artifact_encoding.*ios-app-tree-v1/i);
});

test("rejects a truncated catalog continuation before it can stand in for foundation show", async () => {
  const runFixture = await fixture({ firstListAction: "continue" });
  const outcome = run(runFixture);
  assert.notEqual(outcome.status, 0);
  assert.match(outcome.stderr, /first.*catalog\.show/i);
});

test("rejects a returned catalog show action not bound to the listed foundation capability", async () => {
  const runFixture = await fixture({ firstListAction: "wrong-binding" });
  const outcome = run(runFixture);
  assert.notEqual(outcome.status, 0);
  assert.match(outcome.stderr, /bound to the listed/i);
});

test("rejects a fake public value whose native platform differs from the selected host", async () => {
  const runFixture = await fixture();
  const config = JSON.parse(await readFile(runFixture.config, "utf8"));
  config.platform = "android";
  config.prepare_request.platform = "android-emulator";
  config.prepare_request.artifact_encoding = "raw-file-v1";
  config.prepare_request.app_artifact = join(runFixture.root, "acceptance-host.apk");
  await writeFile(config.prepare_request.app_artifact, "android fixture");
  await writeFile(runFixture.config, JSON.stringify(config));
  runFixture.preparePlatform = "android-emulator";
  const outcome = run(runFixture);
  assert.notEqual(outcome.status, 0);
  assert.match(outcome.stderr, /queried public value/i);
});
