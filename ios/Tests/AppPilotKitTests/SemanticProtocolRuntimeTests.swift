import AppPilotKit
import Foundation
import XCTest

final class SemanticProtocolRuntimeTests: XCTestCase {
  func testNegotiationIsolatedBySelectedMinorAndInvokeNeverDispatches() async throws {
    let runtime = try makeRuntime()
    let legacy = try await open(runtime, minimum: 0, maximum: 1, required: ["session.core"])
    XCTAssertEqual(result(legacy.response, "protocol"), .object(["major": .integer(1), "minor": .integer(1)]))
    XCTAssertEqual(resultArray(legacy.response, "capabilities"), [.string("session.core")])

    let unnegotiated = try await call(
      runtime,
      request("semantic.invoke", context: legacy.context, params: [:])
    )
    assertError(
      unnegotiated,
      code: -32601,
      kind: "methodNotFound",
      message: "Method not found",
      retryable: false
    )

    let coreV12 = try await open(runtime, minimum: 2, maximum: 2, required: ["session.core"])
    let unnegotiatedV12 = try await call(
      runtime,
      request("semantic.invoke", context: coreV12.context, params: [:])
    )
    assertError(
      unnegotiatedV12,
      code: -32003,
      kind: "capabilityUnavailable",
      message: "Capability unavailable",
      retryable: false
    )

    let semantic = try await open(runtime, minimum: 0, maximum: 2, required: ["semantic.catalog"])
    XCTAssertEqual(result(semantic.response, "protocol"), .object(["major": .integer(1), "minor": .integer(2)]))
    XCTAssertEqual(
      Set(resultArray(semantic.response, "capabilities").compactMap(string)),
      ["session.core", "semantic.catalog"]
    )
    let invoke = try await call(
      runtime,
      request("semantic.invoke", context: semantic.context, params: [:])
    )
    XCTAssertEqual(errorKind(invoke), "methodNotFound")

    let invalidCapability = try await call(
      runtime,
      .object([
        "jsonrpc": .string("2.0"),
        "id": .string("open-invalid-capability"),
        "method": .string("session.open"),
        "params": .object([
          "client": .object(["name": .string("tests"), "version": .string("1")]),
          "protocol": .object(["major": .integer(1), "minMinor": .integer(2), "maxMinor": .integer(2)]),
          "requiredCapabilities": .array([.string("semantic_catalog")]),
        ]),
      ])
    )
    XCTAssertEqual(errorKind(invalidCapability), "invalidParams")

    let invalidEnvelope = try await call(
      runtime,
      .object([
        "jsonrpc": .string("2.0"),
        "id": .string("invalid-envelope"),
        "method": .string("session.open"),
        "params": .object([:]),
        "extra": .bool(true),
      ])
    )
    assertError(
      invalidEnvelope,
      code: -32600,
      kind: "invalidRequest",
      message: "Invalid request",
      retryable: false
    )
  }

  func testStrictAndOversizedRequestsFailBeforeResourceHandler() async throws {
    let counter = InvocationCounter()
    let runtime = try makeRuntime(counter: counter, maximumRequestBytes: 1_024)
    let session = try await open(runtime, required: ["semantic.catalog"])
    let declaration = try await declaration(from: runtime, context: session.context, id: "config.current")
    let invalid = try await call(
      runtime,
      request(
        "semantic.query",
        context: session.context,
        params: [
          "capability": .string("config.current"),
          "declarationRevision": .unsignedInteger(declaration.declarationRevision),
          "valueSchema": declaration.valueSchema!,
          "unexpected": .bool(true),
        ]
      )
    )
    XCTAssertEqual(errorKind(invalid), "invalidParams")

    let oversized = try await call(
      runtime,
      request(
        "semantic.query",
        context: session.context,
        params: [
          "capability": .string("config.current"),
          "declarationRevision": .unsignedInteger(declaration.declarationRevision),
          "valueSchema": declaration.valueSchema!,
          "padding": .string(String(repeating: "x", count: 1_500)),
        ]
      )
    )
    assertError(
      oversized,
      code: -32004,
      kind: "resourceExhausted",
      message: "Resource exhausted",
      retryable: false
    )
    let invocationCount = await counter.value
    XCTAssertEqual(invocationCount, 0)
  }

  func testListPaginatesOpaqueMembershipWithoutValues() async throws {
    let runtime = try makeRuntime(extraResource: true)
    let session = try await open(runtime, required: ["semantic.catalog"])
    let first = try await call(
      runtime,
      request("semantic.list", context: session.context, params: ["limits": .object(["maxItems": .integer(1)])])
    )
    let page = result(first, "page")
    XCTAssertEqual(object(page)?["truncated"], .bool(true))
    let items = resultArray(first, "capabilities")
    XCTAssertEqual(items.count, 1)
    XCTAssertNil(object(items[0])?["value"])
    XCTAssertNil(object(items[0])?["input"])
    let cursor = string(object(page)?["nextCursor"])
    XCTAssertNotNil(cursor)

    let modified = try await call(
      runtime,
      request("semantic.list", context: session.context, params: ["cursor": .string("cursor_mutated")])
    )
    XCTAssertEqual(errorKind(modified), "invalidParams")

    let otherSession = try await open(runtime, required: ["semantic.catalog"])
    let crossSession = try await call(
      runtime,
      request("semantic.list", context: otherSession.context, params: ["cursor": .string(cursor!)])
    )
    XCTAssertEqual(errorKind(crossSession), "invalidParams")

    let final = try await call(
      runtime,
      request("semantic.list", context: session.context, params: ["cursor": .string(cursor!)])
    )
    XCTAssertEqual(resultArray(final, "capabilities").count, 1)
    XCTAssertEqual(
      object(result(first, "page"))?["appliedLimits"],
      object(result(final, "page"))?["appliedLimits"]
    )
  }

  func testListContinuationUsesInitialDiscoverySnapshot() async throws {
    let discovery = DiscoverySwitch()
    let runtime = try makeRuntime(extraResource: true, discovery: discovery)
    let session = try await open(runtime, required: ["semantic.catalog"])
    let first = try await call(
      runtime,
      request("semantic.list", context: session.context, params: ["limits": .object(["maxItems": .integer(1)])])
    )
    let cursor = string(object(result(first, "page"))?["nextCursor"])!
    await discovery.setAllowed(false)
    let continuation = try await call(
      runtime,
      request("semantic.list", context: session.context, params: ["cursor": .string(cursor)])
    )
    XCTAssertEqual(resultArray(continuation, "capabilities").count, 1)
  }

  func testShowAndSchemaAreStaticAndUseIndependentSchemaPolicy() async throws {
    let runtime = try makeRuntime()
    let session = try await open(runtime, required: ["semantic.catalog"])
    let show = try await call(
      runtime,
      request(
        "semantic.show",
        context: session.context,
        params: ["capability": .string("config.current"), "declarationRevision": .integer(1)]
      )
    )
    XCTAssertNil(object(result(show))?["value"])
    let valueSchema = object(result(show))?["valueSchema"]
    XCTAssertNotNil(valueSchema)
    let schema = try await call(
      runtime,
      request(
        "semantic.schema",
        context: session.context,
        params: [
          "capability": .string("config.current"),
          "declarationRevision": .integer(1),
          "schema": valueSchema!,
        ]
      )
    )
    XCTAssertEqual(object(result(schema, "document"))?["$schema"], .string("https://json-schema.org/draft/2020-12/schema"))

    let denied = try makeRuntime(schemaAllowed: false)
    let deniedSession = try await open(denied, required: ["semantic.catalog"])
    let deniedSchema = try await call(
      denied,
      request(
        "semantic.schema",
        context: deniedSession.context,
        params: [
          "capability": .string("config.current"),
          "declarationRevision": .integer(1),
          "schema": valueSchema!,
        ]
      )
    )
    XCTAssertEqual(errorKind(deniedSchema), "semantic.disclosureDenied")
  }

  func testInvalidSchemaHandleFailsBeforeDisclosurePolicy() async throws {
    let policyCalls = InvocationCounter()
    let runtime = try makeRuntime(schemaPolicyCounter: policyCalls)
    let session = try await open(runtime, required: ["semantic.catalog"])
    let declaration = try await declaration(
      from: runtime,
      context: session.context,
      id: "config.current"
    )
    var mismatched = object(declaration.valueSchema)!
    mismatched["digest"] = .string("sha256:" + String(repeating: "0", count: 64))
    let response = try await call(
      runtime,
      request(
        "semantic.schema",
        context: session.context,
        params: [
          "capability": .string("config.current"),
          "declarationRevision": .unsignedInteger(declaration.declarationRevision),
          "schema": .object(mismatched),
        ]
      )
    )
    XCTAssertEqual(errorKind(response), "semantic.schemaMismatch")
    let invocationCount = await policyCalls.value
    XCTAssertEqual(invocationCount, 0)
  }

  func testUnknownUnavailableAndResourcePolicyDenyFailClosed() async throws {
    let unknownRuntime = try makeRuntime()
    let unknownSession = try await open(unknownRuntime, required: ["semantic.catalog"])
    let unknown = try await call(
      unknownRuntime,
      request(
        "semantic.show",
        context: unknownSession.context,
        params: ["capability": .string("hidden.operation"), "declarationRevision": .integer(1)]
      )
    )
    XCTAssertEqual(errorKind(unknown), "semantic.capabilityNotFound")

    let hiddenRuntime = try makeRuntime(discoverAllowed: false)
    let hiddenSession = try await open(hiddenRuntime, required: ["semantic.catalog"])
    let hidden = try await call(
      hiddenRuntime,
      request(
        "semantic.show",
        context: hiddenSession.context,
        params: ["capability": .string("config.current"), "declarationRevision": .integer(1)]
      )
    )
    XCTAssertEqual(errorKind(hidden), "semantic.capabilityNotFound")

    let hiddenStale = try await call(
      hiddenRuntime,
      request(
        "semantic.show",
        context: hiddenSession.context,
        params: ["capability": .string("config.current"), "declarationRevision": .integer(99)]
      )
    )
    XCTAssertEqual(errorKind(hiddenStale), "semantic.capabilityNotFound")

    let unavailableRuntime = try makeRuntime(available: false)
    let unavailableSession = try await open(unavailableRuntime, required: ["semantic.catalog"])
    let unavailable = try await query(unavailableRuntime, context: unavailableSession.context)
    assertError(
      unavailable,
      code: -32022,
      kind: "semantic.unavailable",
      message: "Semantic capability is unavailable",
      retryable: true
    )

    let counter = InvocationCounter()
    let deniedRuntime = try makeRuntime(counter: counter, resourceAllowed: false)
    let deniedSession = try await open(deniedRuntime, required: ["semantic.catalog"])
    let denied = try await query(deniedRuntime, context: deniedSession.context)
    assertError(
      denied,
      code: -32023,
      kind: "semantic.disclosureDenied",
      message: "Semantic disclosure is denied",
      retryable: false
    )
    let invocationCount = await counter.value
    XCTAssertEqual(invocationCount, 0)
  }

  func testQueryRejectsActionAndDoesNotCreateActionAuthority() async throws {
    let runtime = try makeRuntime()
    let session = try await open(runtime, required: ["semantic.catalog"])
    let action = try await declaration(from: runtime, context: session.context, id: "account.delete")
    let query = try await call(
      runtime,
      request(
        "semantic.query",
        context: session.context,
        params: [
          "capability": .string("account.delete"),
          "declarationRevision": .unsignedInteger(action.declarationRevision),
          "inputSchema": action.inputSchema!,
          "input": .object(["confirm": .bool(true)]),
          "valueSchema": action.inputSchema!,
        ]
      )
    )
    XCTAssertEqual(errorKind(query), "semantic.capabilityNotFound")

    let resource = try await declaration(from: runtime, context: session.context, id: "config.current")
    let mismatch = try await call(
      runtime,
      request(
        "semantic.query",
        context: session.context,
        params: [
          "capability": .string("config.current"),
          "declarationRevision": .unsignedInteger(resource.declarationRevision),
          "valueSchema": action.inputSchema!,
        ]
      )
    )
    assertError(
      mismatch,
      code: -32021,
      kind: "semantic.schemaMismatch",
      message: "Semantic schema does not match",
      retryable: false
    )
  }

  func testQueryReportsCanonicalValueBytesAndBoundsCompleteResponse() async throws {
    let runtime = try makeRuntime()
    let session = try await open(runtime, required: ["semantic.catalog"])
    let response = try await query(runtime, context: session.context)
    XCTAssertEqual(result(response, "bytes"), .integer(15))

    let bounded = try makeRuntime(maximumResponseBytes: 1_024, outputLength: 900)
    let boundedSession = try await open(bounded, required: ["semantic.catalog"])
    let overflow = try await query(bounded, context: boundedSession.context)
    XCTAssertEqual(errorKind(overflow), "resourceExhausted")
  }

  func testConcurrentReadQueriesRemainIndependent() async throws {
    let counter = InvocationCounter()
    let runtime = try makeRuntime(counter: counter)
    let session = try await open(runtime, required: ["semantic.catalog"])
    let declaration = try await declaration(from: runtime, context: session.context, id: "config.current")
    let queryRequest = request(
      "semantic.query",
      context: session.context,
      params: [
        "capability": .string("config.current"),
        "declarationRevision": .unsignedInteger(declaration.declarationRevision),
        "valueSchema": declaration.valueSchema!,
      ]
    )
    let bytes = try JSONEncoder().encode(queryRequest)
    async let first = runtime.handle(bytes)
    async let second = runtime.handle(bytes)
    let responses = try await [
      JSONDecoder().decode(JSONValue.self, from: first),
      JSONDecoder().decode(JSONValue.self, from: second),
    ]
    XCTAssertTrue(responses.allSatisfy { errorKind($0) == nil })
    let invocationCount = await counter.value
    XCTAssertEqual(invocationCount, 2)
  }

  func testListenerInvalidationExpiresSessionsButNotTheCatalog() async throws {
    let runtime = try makeRuntime()
    let session = try await open(runtime, required: ["semantic.catalog"])
    let before = try await call(runtime, request("semantic.list", context: session.context, params: [:]))
    XCTAssertEqual(errorKind(before), nil)
    await runtime.invalidateSessions()
    let expired = try await call(runtime, request("semantic.list", context: session.context, params: [:]))
    XCTAssertEqual(errorKind(expired), "sessionExpired")

    let renewed = try await open(runtime, required: ["semantic.catalog"])
    let after = try await call(runtime, request("semantic.list", context: renewed.context, params: [:]))
    XCTAssertEqual(result(before, "catalog"), result(after, "catalog"))

    let wrongGeneration = try await call(
      runtime,
      request(
        "semantic.list",
        context: SemanticProtocolSessionContext(id: renewed.context.id, generation: renewed.context.generation + 1),
        params: [:]
      )
    )
    XCTAssertEqual(errorKind(wrongGeneration), "sessionExpired")
  }

  func testInvalidationDuringResourcePolicyPreventsHandlerAndDisclosure() async throws {
    let counter = InvocationCounter()
    let gate = ResourceDisclosureGate()
    let runtime = try makeRuntime(counter: counter, resourceGate: gate)
    let session = try await open(runtime, required: ["semantic.catalog"])
    let declaration = try await declaration(from: runtime, context: session.context, id: "config.current")
    let bytes = try JSONEncoder().encode(
      request(
        "semantic.query",
        context: session.context,
        params: [
          "capability": .string("config.current"),
          "declarationRevision": .unsignedInteger(declaration.declarationRevision),
          "valueSchema": declaration.valueSchema!,
        ]
      )
    )
    async let rawResponse = runtime.handle(bytes)
    await gate.waitUntilEntered()
    await runtime.invalidateSessions()
    await gate.allow()
    let rawResponseData = await rawResponse
    let response = try JSONDecoder().decode(JSONValue.self, from: rawResponseData)
    XCTAssertEqual(errorKind(response), "sessionExpired")
    let invocationCount = await counter.value
    XCTAssertEqual(invocationCount, 0)
  }

  func testHandlerFailureProducesOnlyStockSafeError() async throws {
    let runtime = try makeRuntime(handlerFails: true)
    let session = try await open(runtime, required: ["semantic.catalog"])
    let response = try await query(runtime, context: session.context)
    XCTAssertEqual(errorKind(response), "internalError")
    XCTAssertFalse(encoded(response).contains("customer-secret"))
    XCTAssertFalse(encoded(response).contains("input"))
  }

  private func makeRuntime(
    counter: InvocationCounter? = nil,
    available: Bool = true,
    discoverAllowed: Bool = true,
    schemaAllowed: Bool = true,
    resourceAllowed: Bool = true,
    extraResource: Bool = false,
    handlerFails: Bool = false,
    maximumRequestBytes: Int = 4_096,
    maximumResponseBytes: Int = 4_096,
    outputLength: Int? = nil,
    discovery: DiscoverySwitch? = nil,
    resourceGate: ResourceDisclosureGate? = nil,
    schemaPolicyCounter: InvocationCounter? = nil
  ) throws -> SemanticProtocolRuntime {
    let value = try valueSchema()
    let actionInput = try actionSchema()
    let output = outputLength.map { String(repeating: "x", count: $0) } ?? "safe"
    let builder = SemanticCatalogBuilder()
    try builder.registerResource(
      id: "config.current",
      declarationRevision: 1,
      output: SemanticOutputCodec(schema: value) { value in
        .object(["mode": .publicValue(.string(value))])
      },
      availability: { available },
      handler: {
        if let counter { await counter.increment() }
        if handlerFails { throw FixtureError.sensitive("customer-secret") }
        return output
      }
    )
    if extraResource {
      try builder.registerResource(
        id: "profile.summary",
        declarationRevision: 2,
        output: SemanticOutputCodec(schema: value) { value in
          .object(["mode": .publicValue(.string(value))])
        },
        handler: { "summary" }
      )
    }
    try builder.registerAction(
      id: "account.delete",
      declarationRevision: 3,
      input: SemanticInputCodec(schema: actionInput) { _ in true },
      policy: SemanticActionPolicy(authorization: .destructiveAuthorization, retrySafety: .retryWithProofOnly),
      handler: { _ in }
    )
    let catalog = try builder.freeze(
      identity: SemanticCatalogIdentity(id: "catalog_fixture0001", generation: 7)
    )
    let limits = try SemanticProtocolLimits(
      maximumRequestBytes: maximumRequestBytes,
      maximumResponseBytes: maximumResponseBytes,
      maximumPageItems: 1
    )
    return SemanticProtocolRuntime(
      catalog: catalog,
      limits: limits,
      policy: SemanticProtocolPolicy(
        discover: { _, _ in
          if let discovery { return await discovery.isAllowed }
          return discoverAllowed
        },
        discloseSchema: { _, _ in
          if let schemaPolicyCounter { await schemaPolicyCounter.increment() }
          return schemaAllowed
        },
        discloseResource: { _, _ in
          if let resourceGate { return await resourceGate.waitForPermission() }
          return resourceAllowed
        }
      )
    )
  }

  private func valueSchema() throws -> SemanticSchema {
    try SemanticSchema(
      id: "schema_value0001",
      revision: 1,
      document: .object([
        "$schema": .string("https://json-schema.org/draft/2020-12/schema"),
        "$id": .string("app://config.current/value@1"),
        "type": .string("object"),
        "properties": .object(["mode": .object(["type": .string("string")])]),
        "required": .array([.string("mode")]),
        "additionalProperties": .bool(false),
      ])
    )
  }

  private func actionSchema() throws -> SemanticSchema {
    try SemanticSchema(
      id: "schema_action0001",
      revision: 1,
      document: .object([
        "$schema": .string("https://json-schema.org/draft/2020-12/schema"),
        "$id": .string("app://account.delete/input@1"),
        "type": .string("object"),
        "properties": .object(["confirm": .object(["type": .string("boolean")])]),
        "required": .array([.string("confirm")]),
        "additionalProperties": .bool(false),
      ])
    )
  }

  private func open(
    _ runtime: SemanticProtocolRuntime,
    minimum: Int = 2,
    maximum: Int = 2,
    required: [String]
  ) async throws -> (response: JSONValue, context: SemanticProtocolSessionContext) {
    let response = try await call(
      runtime,
      .object([
        "jsonrpc": .string("2.0"),
        "id": .string("open-1"),
        "method": .string("session.open"),
        "params": .object([
          "client": .object(["name": .string("tests"), "version": .string("1")]),
          "protocol": .object([
            "major": .integer(1),
            "minMinor": .integer(Int64(minimum)),
            "maxMinor": .integer(Int64(maximum)),
          ]),
          "requiredCapabilities": .array(required.map(JSONValue.string)),
        ]),
      ])
    )
    let context = object(result(response, "context"))!
    return (
      response,
      SemanticProtocolSessionContext(
        id: string(context["id"])!,
        generation: unsigned(context["generation"])!
      )
    )
  }

  private func declaration(
    from runtime: SemanticProtocolRuntime,
    context: SemanticProtocolSessionContext,
    id: String
  ) async throws -> WireDeclaration {
    let response = try await call(
      runtime,
      request("semantic.show", context: context, params: ["capability": .string(id), "declarationRevision": .integer(id == "account.delete" ? 3 : 1)])
    )
    let value = object(result(response))!
    return WireDeclaration(
      id: id,
      kind: string(value["kind"])!,
      declarationRevision: unsigned(value["declarationRevision"])!,
      inputSchema: value["inputSchema"],
      valueSchema: value["valueSchema"]
    )
  }

  private func query(
    _ runtime: SemanticProtocolRuntime,
    context: SemanticProtocolSessionContext
  ) async throws -> JSONValue {
    let declaration = try await declaration(from: runtime, context: context, id: "config.current")
    return try await call(
      runtime,
      request(
        "semantic.query",
        context: context,
        params: [
          "capability": .string("config.current"),
          "declarationRevision": .unsignedInteger(declaration.declarationRevision),
          "valueSchema": declaration.valueSchema!,
        ]
      )
    )
  }

  private func request(
    _ method: String,
    context: SemanticProtocolSessionContext,
    params: [String: JSONValue]
  ) -> JSONValue {
    .object([
      "jsonrpc": .string("2.0"),
      "id": .string("request-1"),
      "method": .string(method),
      "context": .object([
        "id": .string(context.id),
        "generation": .unsignedInteger(context.generation),
      ]),
      "params": .object(params),
    ])
  }

  private func call(_ runtime: SemanticProtocolRuntime, _ request: JSONValue) async throws -> JSONValue {
    let response = await runtime.handle(try JSONEncoder().encode(request))
    return try JSONDecoder().decode(JSONValue.self, from: response)
  }

  private func result(_ response: JSONValue, _ key: String? = nil) -> JSONValue {
    let whole = object(response)!["result"]!
    guard let key else { return whole }
    return object(whole)![key]!
  }

  private func resultArray(_ response: JSONValue, _ key: String) -> [JSONValue] {
    guard case .array(let values) = result(response, key) else { return [] }
    return values
  }

  private func errorKind(_ response: JSONValue) -> String? {
    guard let error = object(object(response)?["error"]),
      let data = object(error["data"])
    else { return nil }
    return string(data["kind"])
  }

  private func assertError(
    _ response: JSONValue,
    code: Int64,
    kind: String,
    message: String,
    retryable: Bool,
    file: StaticString = #filePath,
    line: UInt = #line
  ) {
    let error = object(object(response)?["error"])
    XCTAssertEqual(error?["code"], .integer(code), file: file, line: line)
    XCTAssertEqual(string(error?["message"]), message, file: file, line: line)
    let data = object(error?["data"])
    XCTAssertEqual(string(data?["kind"]), kind, file: file, line: line)
    XCTAssertEqual(data?["retryable"], .bool(retryable), file: file, line: line)
  }

  private func object(_ value: JSONValue?) -> [String: JSONValue]? {
    guard case .object(let value) = value else { return nil }
    return value
  }

  private func string(_ value: JSONValue?) -> String? {
    guard case .string(let value) = value else { return nil }
    return value
  }

  private func unsigned(_ value: JSONValue?) -> UInt64? {
    switch value {
    case .integer(let value) where value >= 0: UInt64(value)
    case .unsignedInteger(let value): value
    default: nil
    }
  }

  private func encoded(_ value: JSONValue) -> String {
    String(data: try! JSONEncoder().encode(value), encoding: .utf8)!
  }
}

private struct WireDeclaration {
  let id: String
  let kind: String
  let declarationRevision: UInt64
  let inputSchema: JSONValue?
  let valueSchema: JSONValue?
}

private actor InvocationCounter {
  private(set) var value = 0

  func increment() {
    value += 1
  }
}

private actor DiscoverySwitch {
  private var allowed = true

  var isAllowed: Bool {
    allowed
  }

  func setAllowed(_ allowed: Bool) {
    self.allowed = allowed
  }
}

private actor ResourceDisclosureGate {
  private var entered = false
  private var entryWaiter: CheckedContinuation<Void, Never>?
  private var permissionWaiter: CheckedContinuation<Void, Never>?

  func waitForPermission() async -> Bool {
    entered = true
    entryWaiter?.resume()
    entryWaiter = nil
    await withCheckedContinuation { continuation in
      permissionWaiter = continuation
    }
    return true
  }

  func waitUntilEntered() async {
    guard !entered else { return }
    await withCheckedContinuation { continuation in
      entryWaiter = continuation
    }
  }

  func allow() {
    permissionWaiter?.resume()
    permissionWaiter = nil
  }
}

private enum FixtureError: Error {
  case sensitive(String)
}
