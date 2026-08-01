import assert from "node:assert/strict";
import { readFile, readdir } from "node:fs/promises";
import path from "node:path";
import test from "node:test";
import { isDeepStrictEqual } from "node:util";
import { fileURLToPath } from "node:url";
import Ajv2020 from "ajv/dist/2020.js";

const protocolDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "..",
);
const protocolVersions = ["v1", "v1.1"];

const semanticChecks = {
  versionRange(message) {
    const { minMinor, maxMinor } = message.params.protocol;
    return minMinor <= maxMinor;
  },
  returnedItems(page) {
    return page.returnedItems <= page.appliedLimits.maxItems;
  },
  snapshotResult(message) {
    return validSnapshotResult(message);
  },
  inspectResult(message) {
    return validInspectResult(message);
  },
};

async function readJson(file) {
  return JSON.parse(await readFile(file, "utf8"));
}

function validNegotiation({ client, server, response }) {
  const lowestMinor = Math.max(client.minMinor, server.minMinor);
  const highestMinor = Math.min(client.maxMinor, server.maxMinor);
  const availableCapabilities = new Set(
    server.capabilitiesByMinor[String(highestMinor)] ?? [],
  );
  const responseCapabilities = new Set(response.capabilities);

  return (
    client.requestId === response.requestId &&
    client.major === server.major &&
    lowestMinor <= highestMinor &&
    response.major === client.major &&
    response.minor === highestMinor &&
    client.requiredCapabilities.every((capability) => responseCapabilities.has(capability)) &&
    response.capabilities.every((capability) => availableCapabilities.has(capability)) &&
    availableCapabilities.size === responseCapabilities.size
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

function uniqueNodeRefs(nodes) {
  return new Set(nodes.map((node) => node.ref)).size === nodes.length;
}

function validPage(message, nodes, page) {
  return (
    page.returnedItems === nodes.length &&
    page.returnedItems <= page.appliedLimits.maxItems &&
    Buffer.byteLength(JSON.stringify(message), "utf8") <= page.appliedLimits.maxBytes
  );
}

function validSnapshotResult(message) {
  const { result } = message;
  const nodesByRef = new Map(result.nodes.map((node) => [node.ref, node]));
  const sourcesById = new Map(result.sources.map((source) => [source.id, source]));
  const sourceOrder = new Map(result.sources.map((source, index) => [source.id, index]));
  const sourceIds = result.sources.map((source) => source.id);
  const rootRefs = result.sources.map((source) => source.rootRef);
  const platforms = new Set(result.sources.map((source) => source.platform));
  if (
    !uniqueNodeRefs(result.nodes) ||
    !validPage(message, result.nodes, result.page) ||
    new Set(sourceIds).size !== sourceIds.length ||
    new Set(rootRefs).size !== rootRefs.length ||
    platforms.size !== 1
  ) {
    return false;
  }
  if (
    result.selection.selectedNodes > result.selection.totalNodes ||
    result.selection.selectedNodes < result.sources.length ||
    result.nodes.length > result.selection.selectedNodes
  ) {
    return false;
  }
  if (result.selection.mode === "full") {
    if (
      result.selection.selectedNodes !== result.selection.totalNodes ||
      result.selection.criteria.length !== 1 ||
      result.selection.criteria[0] !== "all"
    ) {
      return false;
    }
  } else {
    const criteria = new Set(result.selection.criteria);
    const agentCriteria = ["root", "visible", "interactive", "ancestor"];
    if (
      criteria.size !== agentCriteria.length ||
      !agentCriteria.every((criterion) => criteria.has(criterion))
    ) {
      return false;
    }
  }

  for (const source of result.sources) {
    const root = nodesByRef.get(source.rootRef);
    if (!root || root.sourceId !== source.id || root.depth !== 0 || root.parentRef) {
      return false;
    }
  }
  const siblingPositions = new Set();
  const lastSiblingIndex = new Map();
  const ancestryStack = [];
  let activeSource = -1;
  for (const [position, node] of result.nodes.entries()) {
    if (!sourcesById.has(node.sourceId)) {
      return false;
    }
    const nodeSource = sourceOrder.get(node.sourceId);
    if (nodeSource < activeSource) {
      return false;
    }
    if (nodeSource !== activeSource) {
      activeSource = nodeSource;
      ancestryStack.length = 0;
    }
    if (node.depth === 0) {
      if (node.parentRef || node.childIndex !== undefined) {
        return false;
      }
      ancestryStack.length = 0;
      ancestryStack.push(node);
      continue;
    }
    const parent = nodesByRef.get(node.parentRef);
    const parentPosition = result.nodes.findIndex((candidate) => candidate.ref === node.parentRef);
    const siblingPosition = `${node.parentRef}:${node.childIndex}`;
    if (
      !parent ||
      node.childIndex === undefined ||
      node.childIndex >= parent.childCount ||
      parent.sourceId !== node.sourceId ||
      parent.depth + 1 !== node.depth ||
      parentPosition >= position ||
      ancestryStack[node.depth - 1]?.ref !== node.parentRef ||
      siblingPositions.has(siblingPosition) ||
      node.childIndex <= (lastSiblingIndex.get(node.parentRef) ?? -1)
    ) {
      return false;
    }
    siblingPositions.add(siblingPosition);
    lastSiblingIndex.set(node.parentRef, node.childIndex);
    ancestryStack.length = node.depth;
    ancestryStack.push(node);
  }
  return result.sources.every(
    (source) =>
      result.nodes.filter(
        (node) => node.sourceId === source.id && node.depth === 0,
      ).length === 1,
  );
}

function validInspectResult(message) {
  const { result } = message;
  if (!uniqueNodeRefs(result.nodes) || !validPage(message, result.nodes, result.page)) {
    return false;
  }
  const refs = new Set(result.nodes.map((node) => node.ref));
  const positions = new Map(result.nodes.map((node, index) => [node.ref, index]));
  const siblingPositions = new Set();
  const lastSiblingIndex = new Map();
  const seenSources = new Set();
  const rootsBySource = new Set();
  const ancestryStack = [];
  let activeSource;
  let activeSourceNodes = 0;
  for (const node of result.nodes) {
    if (node.sourceId !== activeSource) {
      if (seenSources.has(node.sourceId)) {
        return false;
      }
      activeSource = node.sourceId;
      activeSourceNodes = 0;
      seenSources.add(node.sourceId);
      ancestryStack.length = 0;
    }
    if (node.depth === 0 && (node.parentRef || node.childIndex !== undefined)) {
      return false;
    }
    if (node.depth === 0) {
      if (activeSourceNodes > 0 || rootsBySource.has(node.sourceId)) {
        return false;
      }
      rootsBySource.add(node.sourceId);
      ancestryStack.push(node);
      activeSourceNodes += 1;
      continue;
    }
    const siblingPosition = `${node.parentRef}:${node.childIndex}`;
    if (
      siblingPositions.has(siblingPosition) ||
      node.childIndex <= (lastSiblingIndex.get(node.parentRef) ?? -1)
    ) {
      return false;
    }
    siblingPositions.add(siblingPosition);
    lastSiblingIndex.set(node.parentRef, node.childIndex);
    const parent = node.parentRef ? result.nodes[positions.get(node.parentRef)] : undefined;
    if (parent) {
      if (
        node.childIndex >= parent.childCount ||
        parent.sourceId !== node.sourceId ||
        parent.depth + 1 !== node.depth ||
        positions.get(node.parentRef) >= positions.get(node.ref) ||
        ancestryStack[node.depth - 1]?.ref !== node.parentRef
      ) {
        return false;
      }
    }
    ancestryStack.length = node.depth;
    ancestryStack[node.depth] = node;
    activeSourceNodes += 1;
  }
  return result.matchedRefs.every((ref) => refs.has(ref));
}

function validRequestedLimits(request, response) {
  const requested = request.params?.limits;
  const applied = response.result.page.appliedLimits;
  return (
    (!requested?.maxItems || applied.maxItems <= requested.maxItems) &&
    (!requested?.maxBytes || applied.maxBytes <= requested.maxBytes)
  );
}

function validDetail(request, response) {
  const detail = request.params?.detail ?? "compact";
  const containsNative = response.result.nodes.some((node) => node.native !== undefined);
  return detail === "native" || !containsNative;
}

function sameSet(left, right) {
  return left.length === right.length && left.every((value) => right.includes(value));
}

function validSnapshotExchange(request, response, correlateId = true) {
  const requestedSelection = request.params?.selection ?? "agent";
  const requestedProviders = request.params?.providers;
  const responseProviders = [...new Set(response.result.sources.map((source) => source.provider))];
  return (
    (!correlateId || request.id === response.id) &&
    response.result.selection.mode === requestedSelection &&
    (!requestedProviders || sameSet(requestedProviders, responseProviders)) &&
    validRequestedLimits(request, response) &&
    validDetail(request, response)
  );
}

function validInspectExchange(
  request,
  response,
  { correlateId = true, correlateRefs = true } = {},
) {
  const requestedRefs = request.params.target?.refs;
  return (
    (!correlateId || request.id === response.id) &&
    sameJson(request.params.snapshot, response.result.snapshot) &&
    (!correlateRefs || !requestedRefs || sameSet(requestedRefs, response.result.matchedRefs)) &&
    validRequestedLimits(request, response) &&
    validDetail(request, response)
  );
}

function validUIExchange(request, response) {
  if (request.method === "ui.snapshot") {
    return validSnapshotExchange(request, response);
  }
  if (request.method === "ui.inspect") {
    return validInspectExchange(request, response);
  }
  return false;
}

function sameJson(left, right) {
  return isDeepStrictEqual(left, right);
}

function matchesStringPredicate(predicate, candidate) {
  const foldAscii = (value) => value.replace(/[A-Z]/g, (character) => character.toLowerCase());
  const expected = predicate.caseSensitive ? predicate.value : foldAscii(predicate.value);
  const actual = predicate.caseSensitive ? candidate : foldAscii(candidate);
  switch (predicate.operator) {
    case "exact":
      return actual === expected;
    case "prefix":
      return actual.startsWith(expected);
    case "suffix":
      return actual.endsWith(expected);
    case "contains":
      return actual.includes(expected);
    default:
      return false;
  }
}

async function validPaginationExchange(exchange, fixtureDirectory) {
  const initialRequest = await readJson(path.join(fixtureDirectory, exchange.initialRequest));
  const initialResponse = await readJson(path.join(fixtureDirectory, exchange.initialResponse));
  const continuationRequest = await readJson(
    path.join(fixtureDirectory, exchange.continuationRequest),
  );
  const finalResponse = await readJson(path.join(fixtureDirectory, exchange.finalResponse));
  const firstNodes = new Map(
    initialResponse.result.nodes.map((node) => [node.ref, node]),
  );
  const firstRefs = new Set(initialResponse.result.nodes.map((node) => node.ref));
  const newFinalRefs = finalResponse.result.nodes.filter((node) => !firstRefs.has(node.ref));
  const allRefs = new Set([
    ...initialResponse.result.nodes.map((node) => node.ref),
    ...finalResponse.result.nodes.map((node) => node.ref),
  ]);
  const repeatedNodesAreImmutable = finalResponse.result.nodes.every(
    (node) => !firstNodes.has(node.ref) || sameJson(firstNodes.get(node.ref), node),
  );
  const isSnapshot = initialRequest.method === "ui.snapshot";
  const initialResponseMatches = isSnapshot
    ? validSnapshotExchange(initialRequest, initialResponse)
    : validInspectExchange(initialRequest, initialResponse, { correlateRefs: false });
  const finalResponseMatches = isSnapshot
    ? validSnapshotExchange(initialRequest, finalResponse, false)
    : validInspectExchange(initialRequest, finalResponse, {
        correlateId: false,
        correlateRefs: false,
      });
  const methodSpecificResult = isSnapshot
    ? sameJson(finalResponse.result.sources, initialResponse.result.sources) &&
      sameJson(finalResponse.result.selection, initialResponse.result.selection) &&
      allRefs.size === initialResponse.result.selection.selectedNodes
    : !initialRequest.params.target.refs ||
      sameSet(
        initialRequest.params.target.refs,
        [...initialResponse.result.matchedRefs, ...finalResponse.result.matchedRefs],
      );

  return (
    initialResponseMatches &&
    finalResponseMatches &&
    initialResponse.result.page.truncated === true &&
    continuationRequest.params.cursor === initialResponse.result.page.nextCursor &&
    sameJson(continuationRequest.params.snapshot, initialResponse.result.snapshot) &&
    Object.keys(continuationRequest.params).sort().join(",") === "cursor,snapshot" &&
    continuationRequest.method === initialRequest.method &&
    sameJson(continuationRequest.context, initialRequest.context) &&
    continuationRequest.id === finalResponse.id &&
    sameJson(finalResponse.result.snapshot, initialResponse.result.snapshot) &&
    finalResponse.result.page.truncated === false &&
    newFinalRefs.length > 0 &&
    repeatedNodesAreImmutable &&
    methodSpecificResult
  );
}

async function createValidator() {
  const ajv = new Ajv2020({ allErrors: true, strict: true });
  for (const version of protocolVersions) {
    const schemaDirectory = path.join(protocolDirectory, version, "schema");
    const schemaFiles = (await readdir(schemaDirectory))
      .filter((file) => file.endsWith(".schema.json"))
      .sort();
    for (const file of schemaFiles) {
      ajv.addSchema(await readJson(path.join(schemaDirectory, file)));
    }
  }
  return ajv;
}

async function runFixtureCases(version, suite, ajv) {
  const fixtureDirectory = path.join(protocolDirectory, version, "fixtures");

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
      if (contractCase.semantic && !contractCase.valid) {
        assert.equal(
          schemaValid,
          true,
          `semantic-invalid fixture must pass its schema: ${contractCase.fixture}`,
        );
      }
      const semanticValid = semanticCheck ? semanticCheck(fixture) : true;
      const request = contractCase.requestFixture
        ? await readJson(path.join(fixtureDirectory, contractCase.requestFixture))
        : undefined;
      const exchangeValid = request ? validUIExchange(request, fixture) : true;
      const actual = schemaValid && semanticValid && exchangeValid;

      if (contractCase.expectedSchemaValid !== undefined) {
        assert.equal(schemaValid, contractCase.expectedSchemaValid);
      }
      if (contractCase.expectedSemanticValid !== undefined) {
        assert.equal(semanticValid, contractCase.expectedSemanticValid);
      }
      if (contractCase.expectedExchangeValid !== undefined) {
        assert.equal(exchangeValid, contractCase.expectedExchangeValid);
      }

      assert.equal(
        actual,
        contractCase.valid,
        JSON.stringify(
          { schemaErrors: validate.errors, semanticValid, exchangeValid },
          null,
          2,
        ),
      );
    });
  }
}

test("protocol contract fixtures", async (suite) => {
  const ajv = await createValidator();
  for (const version of protocolVersions) {
    await suite.test(version, async (versionSuite) => {
      await runFixtureCases(version, versionSuite, ajv);
    });
  }
});

test("session negotiation exchanges", async (suite) => {
  for (const version of protocolVersions) {
    await suite.test(version, async (versionSuite) => {
      const cases = await readJson(
        path.join(protocolDirectory, version, "negotiation-cases.json"),
      );
      for (const negotiationCase of cases) {
        await versionSuite.test(negotiationCase.name, () => {
          assert.equal(validNegotiation(negotiationCase), negotiationCase.valid);
        });
      }
    });
  }
});

test("v1.1 pagination exchanges", async (suite) => {
  const fixtureDirectory = path.join(protocolDirectory, "v1.1", "fixtures");
  const cases = await readJson(path.join(protocolDirectory, "v1.1", "pagination-cases.json"));
  for (const paginationCase of cases) {
    await suite.test(paginationCase.name, async () => {
      assert.equal(
        await validPaginationExchange(paginationCase, fixtureDirectory),
        paginationCase.valid,
      );
    });
  }
});

test("v1.1 string matching semantics", async (suite) => {
  const cases = await readJson(
    path.join(protocolDirectory, "v1.1", "string-matching-cases.json"),
  );
  for (const matchingCase of cases) {
    await suite.test(matchingCase.name, () => {
      assert.equal(
        matchesStringPredicate(matchingCase.predicate, matchingCase.candidate),
        matchingCase.matches,
      );
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
