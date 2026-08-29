@testable import AppPilotKit
import XCTest

final class SemanticCatalogTests: XCTestCase {
  func testBuilderFreezesMembershipAndRejectsLaterRegistration() throws {
    let schema = try stringSchema(id: "schema_value0001")
    let builder = SemanticCatalogBuilder()
    try builder.registerResource(
      id: "config.current",
      declarationRevision: 1,
      output: SemanticOutputCodec(schema: schema) { .publicValue(.string($0)) }
    ) {
      "safe"
    }
    let catalog = try builder.freeze(identity: catalogIdentity())

    XCTAssertEqual(
      catalog.items,
      [
        SemanticCapabilityItem(
          id: "config.current",
          kind: .resource,
          declarationRevision: 1
        )
      ]
    )
    XCTAssertThrowsError(
      try builder.registerResource(
        id: "config.other",
        declarationRevision: 1,
        output: SemanticOutputCodec(schema: schema) { .publicValue(.string($0)) }
      ) {
        "later"
      }
    ) { error in
      XCTAssertEqual(error as? SemanticCatalogError, .builderFrozen)
    }
    XCTAssertThrowsError(try builder.freeze(identity: catalogIdentity())) { error in
      XCTAssertEqual(error as? SemanticCatalogError, .builderFrozen)
    }
    XCTAssertEqual(catalog.items.map(\.id), ["config.current"])
  }

  func testDuplicateAndCrossKindIdentifiersFailClosed() throws {
    let valueSchema = try stringSchema(id: "schema_value0001")
    let inputSchema = try stringSchema(id: "schema_input0001")
    let output = SemanticOutputCodec(schema: valueSchema) {
      SemanticDisclosureValue.publicValue(.string($0))
    }
    let input = SemanticInputCodec(schema: inputSchema) { value in
      guard case .string(let decoded) = value else { throw FixtureError.rejected }
      return decoded
    }
    let builder = SemanticCatalogBuilder()
    try builder.registerResource(id: "account.operation", declarationRevision: 1, output: output) {
      "safe"
    }

    XCTAssertThrowsError(
      try builder.registerResource(
        id: "account.operation",
        declarationRevision: 2,
        output: output
      ) {
        "duplicate"
      }
    ) { error in
      XCTAssertEqual(error as? SemanticCatalogError, .duplicateCapabilityID)
    }
    XCTAssertThrowsError(
      try builder.registerAction(
        id: "account.operation",
        declarationRevision: 1,
        input: input,
        policy: ordinaryActionPolicy()
      ) { _ in }
    ) { error in
      XCTAssertEqual(error as? SemanticCatalogError, .crossKindCapabilityID)
    }
  }

  func testInvalidSchemaAndConflictingCodecFailClosed() throws {
    XCTAssertThrowsError(
      try SemanticSchema(
        id: "schema_invalid0001",
        revision: 1,
        document: .object([
          "$schema": .string("https://json-schema.org/draft/2020-12/schema"),
          "$id": .string("app://invalid/value@1"),
          "type": .string("object"),
          "properties": .object(["known": .object(["type": .string("string")])]),
        ])
      )
    ) { error in
      XCTAssertEqual(error as? SemanticCatalogError, .invalidSchema)
    }

    let first = try stringSchema(id: "schema_conflict01", maximumLength: 8)
    let second = try stringSchema(id: "schema_conflict01", maximumLength: 9)
    let builder = SemanticCatalogBuilder()
    try builder.registerResource(
      id: "config.first",
      declarationRevision: 1,
      output: SemanticOutputCodec(schema: first) { .publicValue(.string($0)) }
    ) { "one" }
    XCTAssertThrowsError(
      try builder.registerResource(
        id: "config.second",
        declarationRevision: 1,
        output: SemanticOutputCodec(schema: second) { .publicValue(.string($0)) }
      ) { "two" }
    ) { error in
      XCTAssertEqual(error as? SemanticCatalogError, .invalidCodec)
    }

    let pairBuilder = SemanticCatalogBuilder()
    XCTAssertThrowsError(
      try pairBuilder.registerResource(
        id: "config.pair",
        declarationRevision: 1,
        input: SemanticInputCodec(schema: first) { value in
          guard case .string(let value) = value else { throw FixtureError.rejected }
          return value
        },
        output: SemanticOutputCodec(schema: second) { .publicValue(.string($0)) }
      ) { value in
        value
      }
    ) { error in
      XCTAssertEqual(error as? SemanticCatalogError, .invalidCodec)
    }
    XCTAssertEqual(try pairBuilder.freeze(identity: catalogIdentity()).items, [])
  }

  func testTypedHandlersAreErasedAndValuesAndAvailabilityRemainDynamic() async throws {
    let state = FixtureState(value: "first", available: true)
    let actionRecorder = ActionRecorder()
    let string = try stringSchema(id: "schema_value0001")
    let inputSchema = try objectSchema(
      id: "schema_input0001",
      properties: ["count": .object(["type": .string("integer")])],
      required: ["count"]
    )
    let builder = SemanticCatalogBuilder()
    try builder.registerResource(
      id: "config.current",
      declarationRevision: 1,
      output: SemanticOutputCodec(schema: string) { .publicValue(.string($0)) },
      availability: { await state.isAvailable() }
    ) {
      await state.currentValue()
    }
    try builder.registerAction(
      id: "counter.set",
      declarationRevision: 3,
      input: SemanticInputCodec(schema: inputSchema) { value in
        guard case .object(let object) = value,
          case .integer(let count)? = object["count"]
        else {
          throw FixtureError.rejected
        }
        return Int(count)
      },
      policy: ordinaryActionPolicy()
    ) { count in
      await actionRecorder.record(count)
    }
    let catalog = try builder.freeze(identity: catalogIdentity())
    let membership = catalog.items

    let query = try resourceQuery(catalog, id: "config.current")
    let first = try await catalog.queryResource(query, maximumOutputBytes: 128)
    await state.update(value: "second", available: false)
    XCTAssertEqual(first.value, .string("first"))
    let isAvailable = try await catalog.isAvailable("config.current")
    XCTAssertFalse(isAvailable)
    do {
      _ = try await catalog.queryResource(query, maximumOutputBytes: 128)
      XCTFail("Expected unavailable resource query to fail")
    } catch {
      XCTAssertEqual(error as? SemanticCatalogError, .unavailable)
    }
    await state.update(value: "second", available: true)
    let second = try await catalog.queryResource(query, maximumOutputBytes: 128)
    XCTAssertEqual(second.value, .string("second"))
    XCTAssertEqual(catalog.items, membership)

    let prepared = try await catalog.prepareAction(
      SemanticActionInvocation(
        capability: "counter.set",
        declarationRevision: 3,
        inputSchema: inputSchema.handle,
        input: .object(["count": .integer(7)])
      )
    )
    try await prepared.dispatch()
    let recordedValue = await actionRecorder.lastValue()
    XCTAssertEqual(recordedValue, 7)

    do {
      _ = try await catalog.queryResource(
        SemanticResourceQuery(
          capability: "counter.set",
          declarationRevision: 3,
          valueSchema: inputSchema.handle
        ),
        maximumOutputBytes: 128
      )
      XCTFail("Expected an Action queried as a Resource to fail")
    } catch {
      XCTAssertEqual(error as? SemanticCatalogError, .kindMismatch)
      XCTAssertEqual((error as? SemanticCatalogError)?.kind, .capabilityNotFound)
    }
  }

  func testDeclarationsAndSchemasNeverContainLiveValues() async throws {
    let schema = try objectSchema(
      id: "schema_value0001",
      properties: [
        "public": .object(["type": .string("string")]),
        "secret": .object(["type": .string("string")]),
      ],
      required: ["public", "secret"]
    )
    let builder = SemanticCatalogBuilder()
    try builder.registerResource(
      id: "profile.current",
      declarationRevision: 2,
      output: SemanticOutputCodec(schema: schema) { _ in
        .object([
          "public": .publicValue(.string("safe")),
          "secret": .redacted(.string("[redacted]")),
        ])
      }
    ) { "raw-secret" }
    let catalog = try builder.freeze(identity: catalogIdentity())

    let declaration = try catalog.declaration(for: "profile.current")
    XCTAssertEqual(declaration.valueSchema, schema.handle)
    XCTAssertNil(declaration.inputSchema)
    XCTAssertNil(declaration.actionPolicy)
    XCTAssertEqual(
      try catalog.schema(
        capabilityID: "profile.current",
        declarationRevision: 2,
        handle: schema.handle
      ),
      schema
    )
    let result = try await catalog.queryResource(
      try resourceQuery(catalog, id: "profile.current"),
      maximumOutputBytes: 128
    )
    XCTAssertEqual(
      result.value,
      .object(["public": .string("safe"), "secret": .string("[redacted]")])
    )
    XCTAssertFalse(String(describing: result.value).contains("raw-secret"))
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
    XCTAssertEqual(result.bytes, try encoder.encode(result.value).count)

    do {
      _ = try catalog.schema(
        capabilityID: "profile.current",
        declarationRevision: 1,
        handle: schema.handle
      )
      XCTFail("Expected stale declaration to fail")
    } catch {
      XCTAssertEqual(error as? SemanticCatalogError, .schemaMismatch)
    }
  }

  func testConcurrentReadsUseIndependentTypeErasedInvocations() async throws {
    let counter = InvocationCounter()
    let schema = try integerSchema(id: "schema_value0001")
    let builder = SemanticCatalogBuilder()
    try builder.registerResource(
      id: "counter.current",
      declarationRevision: 1,
      output: SemanticOutputCodec(schema: schema) { .publicValue(.integer($0)) }
    ) {
      await counter.next()
    }
    let catalog = try builder.freeze(identity: catalogIdentity())
    let query = try resourceQuery(catalog, id: "counter.current")

    let values = try await withThrowingTaskGroup(of: Int64.self) { group in
      for _ in 0..<50 {
        group.addTask {
          let value = try await catalog.queryResource(
            query,
            maximumOutputBytes: 64
          ).value
          guard case .integer(let integer) = value else { throw FixtureError.rejected }
          return integer
        }
      }
      var results: [Int64] = []
      for try await value in group {
        results.append(value)
      }
      return results
    }
    let expected = Set((1...50).map(Int64.init))
    XCTAssertEqual(Set(values), expected)
    XCTAssertEqual(catalog.items.count, 1)
  }

  func testStaleResourceRevisionAndSchemasFailBeforeHandler() async throws {
    let counter = InvocationCounter()
    let declared = try stringSchema(id: "schema_value0001")
    let other = try stringSchema(id: "schema_value0002")
    let builder = SemanticCatalogBuilder()
    try builder.registerResource(
      id: "guarded.value",
      declarationRevision: 4,
      output: SemanticOutputCodec(schema: declared) { .publicValue(.string($0)) }
    ) {
      _ = await counter.next()
      return "safe"
    }
    let catalog = try builder.freeze(identity: catalogIdentity())
    let invalidRequests = [
      SemanticResourceQuery(
        capability: "guarded.value",
        declarationRevision: 3,
        valueSchema: declared.handle
      ),
      SemanticResourceQuery(
        capability: "guarded.value",
        declarationRevision: 4,
        valueSchema: other.handle
      ),
      SemanticResourceQuery(
        capability: "guarded.value",
        declarationRevision: 4,
        inputSchema: other.handle,
        input: .string("unexpected"),
        valueSchema: declared.handle
      ),
    ]

    for request in invalidRequests {
      do {
        _ = try await catalog.queryResource(request, maximumOutputBytes: 128)
        XCTFail("Expected stale semantic request to fail")
      } catch {
        XCTAssertEqual(error as? SemanticCatalogError, .schemaMismatch)
      }
    }
    let invocationCount = await counter.current()
    XCTAssertEqual(invocationCount, 0)
  }

  func testDisclosureFailuresAreAtomicAndErrorsNeverEchoBusinessData() async throws {
    let secret = "fixture-secret-canary"
    for (id, disclosure, expected) in [
      (
        "disclosure.unclassified",
        SemanticDisclosureValue.object([
          "safe": .publicValue(.string("ok")),
          "secret": .unclassified(.string(secret)),
        ]),
        SemanticCatalogError.disclosureDenied
      ),
      (
        "disclosure.incomplete",
        SemanticDisclosureValue.object([
          "safe": .publicValue(.string("ok")),
          "secret": .sensitive(.string(secret)),
        ]),
        SemanticCatalogError.disclosureDenied
      ),
      (
        "disclosure.undeclared",
        SemanticDisclosureValue.object([
          "safe": .publicValue(.string("ok")),
          "extra": .publicValue(.string(secret)),
        ]),
        SemanticCatalogError.invalidOutput
      ),
      (
        "disclosure.invalid",
        SemanticDisclosureValue.object([
          "safe": .publicValue(.integer(1))
        ]),
        SemanticCatalogError.invalidOutput
      ),
      (
        "disclosure.container",
        SemanticDisclosureValue.publicValue(.object([
          "safe": .string("ok"),
          "secret": .string(secret),
        ])),
        SemanticCatalogError.disclosureDenied
      ),
    ] {
      let catalog = try disclosureCatalog(id: id, disclosure: disclosure)
      do {
        _ = try await catalog.queryResource(
          try resourceQuery(catalog, id: id),
          maximumOutputBytes: 4_096
        )
        XCTFail("Expected disclosure to fail")
      } catch {
        XCTAssertEqual(error as? SemanticCatalogError, expected)
        if expected == .invalidOutput {
          XCTAssertEqual((error as? SemanticCatalogError)?.kind, .disclosureDenied)
        }
        XCTAssertFalse(String(describing: error).contains(secret))
        XCTAssertFalse(error.localizedDescription.contains(secret))
      }
    }

    let oversized = try disclosureCatalog(
      id: "disclosure.oversized",
      disclosure: .object(["safe": .publicValue(.string(String(repeating: "x", count: 64)))])
    )
    do {
      _ = try await oversized.queryResource(
        try resourceQuery(oversized, id: "disclosure.oversized"),
        maximumOutputBytes: 16
      )
      XCTFail("Expected output limit failure")
    } catch {
      XCTAssertEqual(error as? SemanticCatalogError, .resourceExhausted)
      XCTAssertFalse(String(describing: error).contains(secret))
    }
  }

  func testTypedInputAndHandlerErrorsAreSanitized() async throws {
    struct BusinessInput: Sendable { let customer: String }
    let schema = try objectSchema(
      id: "schema_input0001",
      properties: ["customer": .object(["type": .string("string")])],
      required: ["customer"]
    )
    let builder = SemanticCatalogBuilder()
    try builder.registerAction(
      id: "customer.reject",
      declarationRevision: 1,
      input: SemanticInputCodec(schema: schema) { value in
        guard case .object(let fields) = value,
          case .string(let customer)? = fields["customer"]
        else {
          throw FixtureError.rejected
        }
        return BusinessInput(customer: customer)
      },
      policy: ordinaryActionPolicy()
    ) { input in
      throw FixtureError.business(input.customer)
    }
    let catalog = try builder.freeze(identity: catalogIdentity())
    let canary = "must-not-echo"

    do {
      let prepared = try await catalog.prepareAction(
        SemanticActionInvocation(
          capability: "customer.reject",
          declarationRevision: 1,
          inputSchema: schema.handle,
          input: .object(["customer": .string(canary)])
        )
      )
      try await prepared.dispatch()
      XCTFail("Expected handler failure")
    } catch {
      XCTAssertEqual(error as? SemanticCatalogError, .handlerFailed)
      XCTAssertFalse(String(describing: error).contains(canary))
      XCTAssertFalse(error.localizedDescription.contains(canary))
    }

    do {
      _ = try await catalog.prepareAction(
        SemanticActionInvocation(
          capability: "customer.reject",
          declarationRevision: 1,
          inputSchema: schema.handle,
          input: .object([
            "customer": .string("safe"),
            "undeclared": .string(canary),
          ])
        )
      )
      XCTFail("Expected input validation failure")
    } catch {
      XCTAssertEqual(error as? SemanticCatalogError, .invalidInput)
      XCTAssertEqual((error as? SemanticCatalogError)?.kind, .schemaMismatch)
      XCTAssertFalse(String(describing: error).contains(canary))
      XCTAssertFalse(error.localizedDescription.contains(canary))
    }
  }

  func testSchemaDigestAndOutputBytesUseCanonicalJSON() async throws {
    let first = try SemanticSchema(
      id: "schema_canonical01",
      revision: 1,
      document: .object([
        "$schema": .string("https://json-schema.org/draft/2020-12/schema"),
        "$id": .string("app://fixture/canonical@1"),
        "type": .string("number"),
        "minimum": .number(1e-7),
      ])
    )
    let reordered = try SemanticSchema(
      id: "schema_canonical01",
      revision: 1,
      document: .object([
        "minimum": .number(1e-7),
        "type": .string("number"),
        "$id": .string("app://fixture/canonical@1"),
        "$schema": .string("https://json-schema.org/draft/2020-12/schema"),
      ])
    )
    XCTAssertEqual(first.handle.digest, reordered.handle.digest)

    let outputSchema = try SemanticSchema(
      id: "schema_number0001",
      revision: 1,
      document: .object([
        "$schema": .string("https://json-schema.org/draft/2020-12/schema"),
        "$id": .string("app://fixture/number@1"),
        "type": .string("number"),
      ])
    )

    for (id, value, expectedBytes) in [
      ("number.negative-zero", JSONValue.number(-0.0), 1),
      ("number.small", JSONValue.number(1e-7), 4),
      ("number.large", JSONValue.number(1e21), 5),
      ("number.wide-integer", JSONValue.integer(999_999_999_999_999_999), 19),
    ] {
      let builder = SemanticCatalogBuilder()
      try builder.registerResource(
        id: id,
        declarationRevision: 1,
        output: SemanticOutputCodec(schema: outputSchema) { .publicValue($0) }
      ) { value }
      let catalog = try builder.freeze(identity: catalogIdentity())
      let result = try await catalog.queryResource(
        try resourceQuery(catalog, id: id),
        maximumOutputBytes: 128
      )
      XCTAssertEqual(result.bytes, expectedBytes)
    }
  }

  func testStringSchemaLengthUsesUnicodeCodePoints() async throws {
    let schema = try SemanticSchema(
      id: "schema_codepoints01",
      revision: 1,
      document: .object([
        "$schema": .string("https://json-schema.org/draft/2020-12/schema"),
        "$id": .string("app://fixture/code-points@1"),
        "type": .string("string"),
        "minLength": .integer(2),
        "maxLength": .integer(2),
      ])
    )
    let builder = SemanticCatalogBuilder()
    try builder.registerResource(
      id: "string.codepoints",
      declarationRevision: 1,
      output: SemanticOutputCodec(schema: schema) { .publicValue(.string($0)) }
    ) { "e\u{301}" }
    let catalog = try builder.freeze(identity: catalogIdentity())

    let result = try await catalog.queryResource(
      try resourceQuery(catalog, id: "string.codepoints"),
      maximumOutputBytes: 128
    )
    XCTAssertEqual(result.value, .string("e\u{301}"))
  }

  private func disclosureCatalog(
    id: String,
    disclosure: SemanticDisclosureValue
  ) throws -> SemanticCatalog {
    let schema = try objectSchema(
      id: "schema_disclose01",
      properties: ["safe": .object(["type": .string("string")])],
      required: ["safe"]
    )
    let builder = SemanticCatalogBuilder()
    try builder.registerResource(
      id: id,
      declarationRevision: 1,
      output: SemanticOutputCodec(schema: schema) { _ in disclosure }
    ) { true }
    return try builder.freeze(identity: catalogIdentity())
  }

  private func resourceQuery(
    _ catalog: SemanticCatalog,
    id: String
  ) throws -> SemanticResourceQuery {
    let declaration = try catalog.declaration(for: id)
    guard let valueSchema = declaration.valueSchema else {
      throw FixtureError.rejected
    }
    return SemanticResourceQuery(
      capability: id,
      declarationRevision: declaration.declarationRevision,
      inputSchema: declaration.inputSchema,
      valueSchema: valueSchema
    )
  }

  private func catalogIdentity() throws -> SemanticCatalogIdentity {
    try SemanticCatalogIdentity(id: "catalog_fixture0001", generation: 7)
  }

  private func stringSchema(
    id: String,
    maximumLength: Int? = nil
  ) throws -> SemanticSchema {
    var document: [String: JSONValue] = [
      "$schema": .string("https://json-schema.org/draft/2020-12/schema"),
      "$id": .string("app://fixture/string@1"),
      "type": .string("string"),
    ]
    if let maximumLength {
      document["maxLength"] = .integer(Int64(maximumLength))
    }
    return try SemanticSchema(id: id, revision: 1, document: .object(document))
  }

  private func integerSchema(id: String) throws -> SemanticSchema {
    try SemanticSchema(
      id: id,
      revision: 1,
      document: .object([
        "$schema": .string("https://json-schema.org/draft/2020-12/schema"),
        "$id": .string("app://fixture/integer@1"),
        "type": .string("integer"),
      ])
    )
  }

  private func objectSchema(
    id: String,
    properties: [String: JSONValue],
    required: [String]
  ) throws -> SemanticSchema {
    try SemanticSchema(
      id: id,
      revision: 1,
      document: .object([
        "$schema": .string("https://json-schema.org/draft/2020-12/schema"),
        "$id": .string("app://fixture/object@1"),
        "type": .string("object"),
        "required": .array(required.map(JSONValue.string)),
        "properties": .object(properties),
        "additionalProperties": .bool(false),
      ])
    )
  }

  private func ordinaryActionPolicy() -> SemanticActionPolicy {
    SemanticActionPolicy(authorization: .none, retrySafety: .noAutomaticRetry)
  }
}

private enum FixtureError: Error {
  case rejected
  case business(String)
}

private actor FixtureState {
  private var value: String
  private var available: Bool

  init(value: String, available: Bool) {
    self.value = value
    self.available = available
  }

  func currentValue() -> String { value }
  func isAvailable() -> Bool { available }

  func update(value: String, available: Bool) {
    self.value = value
    self.available = available
  }
}

private actor InvocationCounter {
  private var value: Int64 = 0

  func next() -> Int64 {
    value += 1
    return value
  }

  func current() -> Int64 { value }
}

private actor ActionRecorder {
  private var value: Int?

  func record(_ value: Int) { self.value = value }
  func lastValue() -> Int? { value }
}
