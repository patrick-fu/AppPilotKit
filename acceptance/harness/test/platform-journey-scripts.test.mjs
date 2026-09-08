import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import test from "node:test";
import { spawnSync } from "node:child_process";

const repository = resolve(import.meta.dirname, "../../..");

test("iOS tests select an available shutdown or booted simulator by UUID", async () => {
  const script = await readFile(join(repository, "ios/AcceptanceHost/Scripts/run-simulator-tests.sh"), "utf8");
  const expression = script.match(/sed -nE '([^']+)'/)?.[1];
  assert.ok(expression);
  const uuid = "12345678-1234-1234-1234-123456789ABC";
  for (const state of ["Shutdown", "Booted"]) {
    const result = spawnSync("sed", ["-nE", expression], {
      input: `    iPhone (${uuid}) (${state})\n`,
      encoding: "utf8",
    });
    assert.equal(result.status, 0);
    assert.equal(result.stdout.trim(), uuid);
  }
});

test("iOS and Android journeys use the installed target-prepare release mode", async () => {
  const scripts = await Promise.all([
    readFile(join(repository, "ios/AcceptanceHost/Scripts/run-installed-cli-journey.sh"), "utf8"),
    readFile(join(repository, "android/acceptance-host/Scripts/run-emulator-journey.sh"), "utf8"),
  ]);
  const harness = await readFile(join(repository, "acceptance/harness/installed-cli-harness.mjs"), "utf8");

  assert.match(scripts[0], /argv:\s*\[require\("node:path"\)\.join\(prefix, "libexec", "apppilotkit-target-prepare"\), "--release-fd=0", "--output=json"\]/);
  assert.match(scripts[1], /prepare_program=.*\$prefix\/libexec\/apppilotkit-target-prepare/);
  assert.match(scripts[1], /"argv": \["\$prepare_program", "--release-fd=0", "--output=json"\]/);
  assert.match(scripts[1], /cargo=\$\{cargo:a\}/);
  assert.match(scripts[1], /work_root=\$\(mktemp -d [\s\S]*\)\nwork_root=\$\{work_root:A\}/);

  for (const script of scripts) {
    assert.doesNotMatch(script, /\bkillall\b|\bam\s+force-stop\b/);
    assert.doesNotMatch(script, /simctl\s+terminate/);
  }

  assert.match(scripts[1], /APPPILOTKIT_INTERNAL_PREPARE_FAILURE_DIAGNOSTICS/);
  assert.match(scripts[1], /APPPILOTKIT_INTERNAL_PREPARE_FAILURE_SIDECAR/);
  assert.match(harness, /openSync\(sidecar, "a", 0o600\)/);
  assert.match(harness, /stdio\[fd\] = fd/);
  assert.match(harness, /APPPILOTKIT_INTERNAL_PREPARE_FAILURE_FD/);
});
