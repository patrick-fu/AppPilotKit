import assert from "node:assert/strict";
import { readFile, readdir } from "node:fs/promises";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import Ajv2020 from "ajv/dist/2020.js";

const protocolDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "..",
);
const schemaDirectory = path.join(protocolDirectory, "v1", "schema");
const fixtureDirectory = path.join(protocolDirectory, "v1", "fixtures");

const semanticChecks = {
  versionRange(message) {
    const { minMinor, maxMinor } = message.params.protocol;
    return minMinor <= maxMinor;
  },
  returnedItems(page) {
    return page.returnedItems <= page.appliedLimits.maxItems;
  },
};

async function readJson(file) {
  return JSON.parse(await readFile(file, "utf8"));
}

function validNegotiation({ client, server, response }) {
  const lowestMinor = Math.max(client.minMinor, server.minMinor);
  const highestMinor = Math.min(client.maxMinor, server.maxMinor);
  const availableCapabilities = new Set(server.capabilities);
  const responseCapabilities = new Set(response.capabilities);

  return (
    client.requestId === response.requestId &&
    client.major === server.major &&
    lowestMinor <= highestMinor &&
    response.major === client.major &&
    response.minor === highestMinor &&
    client.requiredCapabilities.every((capability) => responseCapabilities.has(capability)) &&
    response.capabilities.every((capability) => availableCapabilities.has(capability)) &&
    server.capabilities.every((capability) => responseCapabilities.has(capability))
  );
}

function validAppliedLimits({ negotiated, requested, applied }) {
  const requestedMaxItems = requested.maxItems ?? negotiated.maxPageItems;
  const requestedMaxBytes = requested.maxBytes ?? negotiated.maxResponseBytes;
  return (
    applied.maxItems <= requestedMaxItems &&
    applied.maxItems <= negotiated.maxPageItems &&
    applied.maxBytes <= requestedMaxBytes &&
    applied.maxBytes <= negotiated.maxResponseBytes
  );
}

test("v1 contract fixtures", async (suite) => {
  const ajv = new Ajv2020({ allErrors: true, strict: true });
  const schemaFiles = (await readdir(schemaDirectory))
    .filter((file) => file.endsWith(".schema.json"))
    .sort();

  for (const file of schemaFiles) {
    ajv.addSchema(await readJson(path.join(schemaDirectory, file)));
  }

  const cases = await readJson(path.join(fixtureDirectory, "cases.json"));
  const listedFixtures = cases.map((contractCase) => contractCase.fixture).sort();
  const fixtureFiles = (
    await Promise.all(
      ["valid", "invalid"].map(async (directory) =>
        (await readdir(path.join(fixtureDirectory, directory)))
          .filter((file) => file.endsWith(".json"))
          .map((file) => `${directory}/${file}`),
      ),
    )
  )
    .flat()
    .sort();
  assert.deepEqual(listedFixtures, fixtureFiles, "every fixture must appear exactly once");

  for (const contractCase of cases) {
    await suite.test(contractCase.name, async () => {
      const validate = ajv.getSchema(contractCase.schema);
      assert.ok(validate, `schema not found: ${contractCase.schema}`);

      const fixture = await readJson(path.join(fixtureDirectory, contractCase.fixture));
      const schemaValid = validate(fixture);
      const semanticCheck = contractCase.semantic
        ? semanticChecks[contractCase.semantic]
        : undefined;
      assert.ok(
        !contractCase.semantic || semanticCheck,
        `unknown semantic check: ${contractCase.semantic}`,
      );
      const semanticValid = semanticCheck ? semanticCheck(fixture) : true;
      const actual = schemaValid && semanticValid;

      assert.equal(
        actual,
        contractCase.valid,
        JSON.stringify({ schemaErrors: validate.errors, semanticValid }, null, 2),
      );
    });
  }
});

test("v1 session negotiation exchanges", async (suite) => {
  const cases = await readJson(
    path.join(protocolDirectory, "v1", "negotiation-cases.json"),
  );
  for (const negotiationCase of cases) {
    await suite.test(negotiationCase.name, () => {
      assert.equal(validNegotiation(negotiationCase), negotiationCase.valid);
    });
  }
});

test("v1 disclosure limits", async (suite) => {
  const cases = await readJson(path.join(protocolDirectory, "v1", "disclosure-cases.json"));
  for (const disclosureCase of cases) {
    await suite.test(disclosureCase.name, () => {
      assert.equal(validAppliedLimits(disclosureCase), disclosureCase.valid);
    });
  }
});
