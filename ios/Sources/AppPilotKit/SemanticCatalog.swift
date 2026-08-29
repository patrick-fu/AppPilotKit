import CryptoKit
import Foundation

public enum SemanticCapabilityKind: String, Codable, Equatable, Sendable {
  case resource
  case action
}

public enum SemanticActionAuthorization: String, Codable, Equatable, Sendable {
  case none
  case destructiveAuthorization
}

public enum SemanticActionRetrySafety: String, Codable, Equatable, Sendable {
  case noAutomaticRetry
  case retryWithProofOnly
}

public struct SemanticActionPolicy: Codable, Equatable, Sendable {
  public let authorization: SemanticActionAuthorization
  public let retrySafety: SemanticActionRetrySafety

  public init(
    authorization: SemanticActionAuthorization,
    retrySafety: SemanticActionRetrySafety
  ) {
    self.authorization = authorization
    self.retrySafety = retrySafety
  }
}

public enum SemanticCatalogFailureKind: String, Codable, Equatable, Sendable {
  case invalidRegistration
  case builderFrozen
  case capabilityNotFound = "semantic.capabilityNotFound"
  case schemaMismatch = "semantic.schemaMismatch"
  case unavailable = "semantic.unavailable"
  case disclosureDenied = "semantic.disclosureDenied"
  case resourceExhausted
  case internalError
}

public enum SemanticCatalogError: Error, Equatable, Sendable {
  case invalidCatalogIdentity
  case invalidCapabilityID
  case invalidDeclarationRevision
  case invalidSchema
  case invalidCodec
  case duplicateCapabilityID
  case crossKindCapabilityID
  case builderFrozen
  case capabilityNotFound
  case kindMismatch
  case schemaMismatch
  case unavailable
  case invalidInput
  case invalidOutput
  case disclosureDenied
  case resourceExhausted
  case handlerFailed

  public var kind: SemanticCatalogFailureKind {
    switch self {
    case .builderFrozen:
      .builderFrozen
    case .capabilityNotFound, .kindMismatch:
      .capabilityNotFound
    case .schemaMismatch, .invalidInput:
      .schemaMismatch
    case .unavailable:
      .unavailable
    case .disclosureDenied, .invalidOutput:
      .disclosureDenied
    case .resourceExhausted:
      .resourceExhausted
    case .handlerFailed:
      .internalError
    default:
      .invalidRegistration
    }
  }
}

extension SemanticCatalogError: LocalizedError {
  public var errorDescription: String? {
    switch self {
    case .invalidCatalogIdentity:
      "Invalid semantic catalog identity"
    case .invalidCapabilityID:
      "Invalid semantic capability identifier"
    case .invalidDeclarationRevision:
      "Invalid semantic declaration revision"
    case .invalidSchema:
      "Invalid semantic schema"
    case .invalidCodec:
      "Invalid semantic codec"
    case .duplicateCapabilityID:
      "Duplicate semantic capability identifier"
    case .crossKindCapabilityID:
      "Semantic capability identifier is already used by another kind"
    case .builderFrozen:
      "Semantic catalog builder is frozen"
    case .capabilityNotFound:
      "Semantic capability is unavailable"
    case .kindMismatch:
      "Semantic capability kind does not match the operation"
    case .schemaMismatch:
      "Semantic schema does not match the declaration"
    case .unavailable:
      "Semantic capability is unavailable"
    case .invalidInput:
      "Semantic input is invalid"
    case .invalidOutput:
      "Semantic output is invalid"
    case .disclosureDenied:
      "Semantic disclosure is denied"
    case .resourceExhausted:
      "Semantic output exceeds a limit"
    case .handlerFailed:
      "Semantic handler failed"
    }
  }
}

public struct SemanticCatalogIdentity: Codable, Equatable, Sendable {
  public let id: String
  public let generation: UInt64

  public init(id: String, generation: UInt64) throws {
    guard
      generation > 0,
      SemanticValidation.matches(
        id,
        pattern: "^catalog_[A-Za-z0-9._~-]{8,120}$"
      )
    else {
      throw SemanticCatalogError.invalidCatalogIdentity
    }
    self.id = id
    self.generation = generation
  }
}

public struct SemanticSchemaHandle: Codable, Equatable, Hashable, Sendable {
  public let id: String
  public let revision: UInt64
  public let digest: String

  fileprivate init(id: String, revision: UInt64, digest: String) {
    self.id = id
    self.revision = revision
    self.digest = digest
  }
}

public struct SemanticSchema: Equatable, Sendable {
  public let handle: SemanticSchemaHandle
  public let document: JSONValue

  public init(id: String, revision: UInt64, document: JSONValue) throws {
    guard
      revision > 0,
      SemanticValidation.matches(
        id,
        pattern: "^schema_[A-Za-z0-9._~-]{8,120}$"
      )
    else {
      throw SemanticCatalogError.invalidSchema
    }
    try SemanticJSONSchema.validateDocument(document)
    let bytes: Data
    do {
      bytes = try SemanticValidation.canonicalData(for: document)
    } catch {
      throw SemanticCatalogError.invalidSchema
    }
    let digest = SHA256.hash(data: bytes).map { String(format: "%02x", $0) }.joined()
    self.handle = SemanticSchemaHandle(
      id: id,
      revision: revision,
      digest: "sha256:\(digest)"
    )
    self.document = document
  }
}

/// A classified, App-owned representation of a typed business value.
/// Unclassified or still-sensitive branches are rejected before detached JSON is returned.
public indirect enum SemanticDisclosureValue: Equatable, Sendable {
  case publicValue(JSONValue)
  case redacted(JSONValue)
  case unclassified(JSONValue)
  case sensitive(JSONValue)
  case array([SemanticDisclosureValue])
  case object([String: SemanticDisclosureValue])
}

public struct SemanticInputCodec<Value: Sendable>: Sendable {
  public let schema: SemanticSchema
  fileprivate let decodeValue: @Sendable (JSONValue) throws -> Value

  public init(
    schema: SemanticSchema,
    decode: @escaping @Sendable (JSONValue) throws -> Value
  ) {
    self.schema = schema
    self.decodeValue = decode
  }
}

public struct SemanticOutputCodec<Value: Sendable>: Sendable {
  public let schema: SemanticSchema
  fileprivate let encodeValue: @Sendable (Value) throws -> SemanticDisclosureValue

  public init(
    schema: SemanticSchema,
    encode: @escaping @Sendable (Value) throws -> SemanticDisclosureValue
  ) {
    self.schema = schema
    self.encodeValue = encode
  }
}

public struct SemanticCapabilityItem: Codable, Equatable, Sendable {
  public let id: String
  public let kind: SemanticCapabilityKind
  public let declarationRevision: UInt64
}

public struct SemanticCapabilityDeclaration: Equatable, Sendable {
  public let id: String
  public let kind: SemanticCapabilityKind
  public let declarationRevision: UInt64
  public let inputSchema: SemanticSchemaHandle?
  public let valueSchema: SemanticSchemaHandle?
  public let actionPolicy: SemanticActionPolicy?
}

public struct SemanticResourceQuery: Equatable, Sendable {
  public let capability: String
  public let declarationRevision: UInt64
  public let inputSchema: SemanticSchemaHandle?
  public let input: JSONValue?
  public let valueSchema: SemanticSchemaHandle

  public init(
    capability: String,
    declarationRevision: UInt64,
    inputSchema: SemanticSchemaHandle? = nil,
    input: JSONValue? = nil,
    valueSchema: SemanticSchemaHandle
  ) {
    self.capability = capability
    self.declarationRevision = declarationRevision
    self.inputSchema = inputSchema
    self.input = input
    self.valueSchema = valueSchema
  }
}

public struct SemanticActionInvocation: Equatable, Sendable {
  public let capability: String
  public let declarationRevision: UInt64
  public let inputSchema: SemanticSchemaHandle
  public let input: JSONValue

  public init(
    capability: String,
    declarationRevision: UInt64,
    inputSchema: SemanticSchemaHandle,
    input: JSONValue
  ) {
    self.capability = capability
    self.declarationRevision = declarationRevision
    self.inputSchema = inputSchema
    self.input = input
  }
}

public struct DetachedSemanticValue: Equatable, Sendable {
  public let value: JSONValue
  public let valueSchema: SemanticSchemaHandle
  public let bytes: Int

  fileprivate init(value: JSONValue, valueSchema: SemanticSchemaHandle, bytes: Int) {
    self.value = value
    self.valueSchema = valueSchema
    self.bytes = bytes
  }
}

private struct SemanticSchemaVersion: Hashable {
  let id: String
  let revision: UInt64
}

private struct AnySemanticResource: Sendable {
  let declaration: SemanticCapabilityDeclaration
  let schemas: [SemanticSchema]
  let availability: @Sendable () async throws -> Bool
  let query: @Sendable (JSONValue?, Int) async throws -> DetachedSemanticValue
}

private struct AnySemanticAction: Sendable {
  let declaration: SemanticCapabilityDeclaration
  let schemas: [SemanticSchema]
  let availability: @Sendable () async throws -> Bool
  let prepare: @Sendable (JSONValue) throws -> PreparedSemanticAction
}

private enum AnySemanticCapability: Sendable {
  case resource(AnySemanticResource)
  case action(AnySemanticAction)

  var declaration: SemanticCapabilityDeclaration {
    switch self {
    case .resource(let resource):
      resource.declaration
    case .action(let action):
      action.declaration
    }
  }

  var schemas: [SemanticSchema] {
    switch self {
    case .resource(let resource):
      resource.schemas
    case .action(let action):
      action.schemas
    }
  }

  func isAvailable() async throws -> Bool {
    switch self {
    case .resource(let resource):
      try await resource.availability()
    case .action(let action):
      try await action.availability()
    }
  }
}

/// A composition-root-only builder. It is intentionally non-Sendable so registration
/// cannot become a runtime service or race with request handling.
public final class SemanticCatalogBuilder {
  private var registrations: [String: AnySemanticCapability] = [:]
  private var schemas: [SemanticSchemaVersion: SemanticSchema] = [:]
  private var frozen = false

  public init() {}

  public func registerResource<Output: Sendable>(
    id: String,
    declarationRevision: UInt64,
    output: SemanticOutputCodec<Output>,
    availability: @escaping @Sendable () async throws -> Bool = { true },
    handler: @escaping @Sendable () async throws -> Output
  ) throws {
    try validateRegistration(
      id: id,
      kind: .resource,
      declarationRevision: declarationRevision,
      schemas: [output.schema]
    )
    let declaration = SemanticCapabilityDeclaration(
      id: id,
      kind: .resource,
      declarationRevision: declarationRevision,
      inputSchema: nil,
      valueSchema: output.schema.handle,
      actionPolicy: nil
    )
    registrations[id] = .resource(
      AnySemanticResource(
        declaration: declaration,
        schemas: [output.schema],
        availability: availability,
        query: { input, maximumOutputBytes in
          guard input == nil else {
            throw SemanticCatalogError.invalidInput
          }
          return try await Self.query(
            maximumOutputBytes: maximumOutputBytes,
            output: output,
            handler: handler
          )
        }
      )
    )
    recordSchemas([output.schema])
  }

  public func registerResource<Input: Sendable, Output: Sendable>(
    id: String,
    declarationRevision: UInt64,
    input: SemanticInputCodec<Input>,
    output: SemanticOutputCodec<Output>,
    availability: @escaping @Sendable () async throws -> Bool = { true },
    handler: @escaping @Sendable (Input) async throws -> Output
  ) throws {
    try validateRegistration(
      id: id,
      kind: .resource,
      declarationRevision: declarationRevision,
      schemas: [input.schema, output.schema]
    )
    let declaration = SemanticCapabilityDeclaration(
      id: id,
      kind: .resource,
      declarationRevision: declarationRevision,
      inputSchema: input.schema.handle,
      valueSchema: output.schema.handle,
      actionPolicy: nil
    )
    registrations[id] = .resource(
      AnySemanticResource(
        declaration: declaration,
        schemas: [input.schema, output.schema],
        availability: availability,
        query: { rawInput, maximumOutputBytes in
          guard let rawInput else {
            throw SemanticCatalogError.invalidInput
          }
          let typedInput = try Self.decode(rawInput, with: input)
          return try await Self.query(
            maximumOutputBytes: maximumOutputBytes,
            output: output
          ) {
            try await handler(typedInput)
          }
        }
      )
    )
    recordSchemas([input.schema, output.schema])
  }

  public func registerAction<Input: Sendable>(
    id: String,
    declarationRevision: UInt64,
    input: SemanticInputCodec<Input>,
    policy: SemanticActionPolicy,
    availability: @escaping @Sendable () async throws -> Bool = { true },
    handler: @escaping @Sendable (Input) async throws -> Void
  ) throws {
    try validateRegistration(
      id: id,
      kind: .action,
      declarationRevision: declarationRevision,
      schemas: [input.schema]
    )
    let declaration = SemanticCapabilityDeclaration(
      id: id,
      kind: .action,
      declarationRevision: declarationRevision,
      inputSchema: input.schema.handle,
      valueSchema: nil,
      actionPolicy: policy
    )
    registrations[id] = .action(
      AnySemanticAction(
        declaration: declaration,
        schemas: [input.schema],
        availability: availability,
        prepare: { rawInput in
          let typedInput = try Self.decode(rawInput, with: input)
          return PreparedSemanticAction(declaration: declaration) {
            do {
              try await handler(typedInput)
            } catch is CancellationError {
              throw CancellationError()
            } catch {
              throw SemanticCatalogError.handlerFailed
            }
          }
        }
      )
    )
    recordSchemas([input.schema])
  }

  public func freeze(identity: SemanticCatalogIdentity) throws -> SemanticCatalog {
    guard !frozen else {
      throw SemanticCatalogError.builderFrozen
    }
    frozen = true
    return SemanticCatalog(identity: identity, registrations: registrations)
  }

  private func validateRegistration(
    id: String,
    kind: SemanticCapabilityKind,
    declarationRevision: UInt64,
    schemas newSchemas: [SemanticSchema]
  ) throws {
    guard !frozen else {
      throw SemanticCatalogError.builderFrozen
    }
    guard
      id.count <= 128,
      SemanticValidation.matches(
        id,
        pattern: "^[a-z][a-z0-9]*(?:[._-][a-z0-9]+)*$"
      )
    else {
      throw SemanticCatalogError.invalidCapabilityID
    }
    guard declarationRevision > 0 else {
      throw SemanticCatalogError.invalidDeclarationRevision
    }
    if let existing = registrations[id] {
      if existing.declaration.kind == kind {
        throw SemanticCatalogError.duplicateCapabilityID
      }
      throw SemanticCatalogError.crossKindCapabilityID
    }
    var candidateSchemas: [SemanticSchemaVersion: SemanticSchema] = [:]
    for schema in newSchemas {
      let version = SemanticSchemaVersion(
        id: schema.handle.id,
        revision: schema.handle.revision
      )
      if let existing = candidateSchemas[version] ?? schemas[version], existing != schema {
        throw SemanticCatalogError.invalidCodec
      }
      candidateSchemas[version] = schema
    }
  }

  private func recordSchemas(_ newSchemas: [SemanticSchema]) {
    for schema in newSchemas {
      schemas[
        SemanticSchemaVersion(id: schema.handle.id, revision: schema.handle.revision)
      ] = schema
    }
  }

  private static func decode<Input: Sendable>(
    _ rawInput: JSONValue,
    with codec: SemanticInputCodec<Input>
  ) throws -> Input {
    do {
      try SemanticJSONSchema.validate(rawInput, against: codec.schema.document)
      return try codec.decodeValue(rawInput)
    } catch is CancellationError {
      throw CancellationError()
    } catch {
      throw SemanticCatalogError.invalidInput
    }
  }

  private static func query<Output: Sendable>(
    maximumOutputBytes: Int,
    output: SemanticOutputCodec<Output>,
    handler: @escaping @Sendable () async throws -> Output
  ) async throws -> DetachedSemanticValue {
    guard maximumOutputBytes > 0 else {
      throw SemanticCatalogError.resourceExhausted
    }
    let typedOutput: Output
    do {
      typedOutput = try await handler()
    } catch is CancellationError {
      throw CancellationError()
    } catch {
      throw SemanticCatalogError.handlerFailed
    }
    let disclosure: SemanticDisclosureValue
    do {
      disclosure = try output.encodeValue(typedOutput)
    } catch is CancellationError {
      throw CancellationError()
    } catch {
      throw SemanticCatalogError.invalidOutput
    }
    let value = try disclosure.detachedValue()
    do {
      try SemanticJSONSchema.validate(value, against: output.schema.document)
    } catch {
      throw SemanticCatalogError.invalidOutput
    }
    let data: Data
    do {
      data = try SemanticValidation.canonicalData(for: value)
    } catch {
      throw SemanticCatalogError.invalidOutput
    }
    guard data.count <= maximumOutputBytes else {
      throw SemanticCatalogError.resourceExhausted
    }
    return DetachedSemanticValue(
      value: value,
      valueSchema: output.schema.handle,
      bytes: data.count
    )
  }
}

public struct SemanticCatalog: Sendable {
  public let identity: SemanticCatalogIdentity
  private let registrations: [String: AnySemanticCapability]

  fileprivate init(
    identity: SemanticCatalogIdentity,
    registrations: [String: AnySemanticCapability]
  ) {
    self.identity = identity
    self.registrations = registrations
  }

  public var items: [SemanticCapabilityItem] {
    registrations.values
      .map { capability in
        let declaration = capability.declaration
        return SemanticCapabilityItem(
          id: declaration.id,
          kind: declaration.kind,
          declarationRevision: declaration.declarationRevision
        )
      }
      .sorted { $0.id < $1.id }
  }

  public func declaration(for id: String) throws -> SemanticCapabilityDeclaration {
    guard let capability = registrations[id] else {
      throw SemanticCatalogError.capabilityNotFound
    }
    return capability.declaration
  }

  public func schema(
    capabilityID: String,
    declarationRevision: UInt64,
    handle: SemanticSchemaHandle
  ) throws -> SemanticSchema {
    guard let capability = registrations[capabilityID] else {
      throw SemanticCatalogError.capabilityNotFound
    }
    guard capability.declaration.declarationRevision == declarationRevision else {
      throw SemanticCatalogError.schemaMismatch
    }
    guard let schema = capability.schemas.first(where: { $0.handle == handle }) else {
      throw SemanticCatalogError.schemaMismatch
    }
    return schema
  }

  public func isAvailable(_ id: String) async throws -> Bool {
    guard let capability = registrations[id] else {
      throw SemanticCatalogError.capabilityNotFound
    }
    do {
      return try await capability.isAvailable()
    } catch is CancellationError {
      throw CancellationError()
    } catch {
      throw SemanticCatalogError.unavailable
    }
  }

  public func queryResource(
    _ request: SemanticResourceQuery,
    maximumOutputBytes: Int
  ) async throws -> DetachedSemanticValue {
    guard let capability = registrations[request.capability] else {
      throw SemanticCatalogError.capabilityNotFound
    }
    guard case .resource(let resource) = capability else {
      throw SemanticCatalogError.kindMismatch
    }
    let declaration = resource.declaration
    guard
      request.declarationRevision == declaration.declarationRevision,
      request.valueSchema == declaration.valueSchema,
      request.inputSchema == declaration.inputSchema,
      (request.inputSchema == nil) == (request.input == nil)
    else {
      throw SemanticCatalogError.schemaMismatch
    }
    do {
      guard try await resource.availability() else {
        throw SemanticCatalogError.unavailable
      }
    } catch is CancellationError {
      throw CancellationError()
    } catch let error as SemanticCatalogError {
      throw error
    } catch {
      throw SemanticCatalogError.unavailable
    }
    return try await resource.query(request.input, maximumOutputBytes)
  }

  /// Internal preparation seam for the future Target Action Coordinator.
  /// Only that coordinator may dispatch the returned typed invocation.
  func prepareAction(_ request: SemanticActionInvocation) async throws -> PreparedSemanticAction {
    guard let capability = registrations[request.capability] else {
      throw SemanticCatalogError.capabilityNotFound
    }
    guard case .action(let action) = capability else {
      throw SemanticCatalogError.kindMismatch
    }
    guard
      request.declarationRevision == action.declaration.declarationRevision,
      request.inputSchema == action.declaration.inputSchema
    else {
      throw SemanticCatalogError.schemaMismatch
    }
    do {
      guard try await action.availability() else {
        throw SemanticCatalogError.unavailable
      }
    } catch is CancellationError {
      throw CancellationError()
    } catch let error as SemanticCatalogError {
      throw error
    } catch {
      throw SemanticCatalogError.unavailable
    }
    return try action.prepare(request.input)
  }
}

struct PreparedSemanticAction: Sendable {
  let declaration: SemanticCapabilityDeclaration
  private let dispatchHandler: @Sendable () async throws -> Void

  fileprivate init(
    declaration: SemanticCapabilityDeclaration,
    dispatch: @escaping @Sendable () async throws -> Void
  ) {
    self.declaration = declaration
    self.dispatchHandler = dispatch
  }

  func dispatch() async throws {
    try await dispatchHandler()
  }
}

private extension SemanticDisclosureValue {
  func detachedValue() throws -> JSONValue {
    switch self {
    case .publicValue(let value), .redacted(let value):
      // Containers must use `.object` or `.array` so every child crosses an
      // explicit classification boundary instead of inheriting one broad label.
      switch value {
      case .array, .object:
        throw SemanticCatalogError.disclosureDenied
      default:
        try SemanticValidation.validateFiniteJSON(value)
        return value
      }
    case .unclassified, .sensitive:
      throw SemanticCatalogError.disclosureDenied
    case .array(let values):
      return .array(try values.map { try $0.detachedValue() })
    case .object(let fields):
      var detached: [String: JSONValue] = [:]
      detached.reserveCapacity(fields.count)
      for (key, value) in fields {
        detached[key] = try value.detachedValue()
      }
      return .object(detached)
    }
  }
}

private enum SemanticValidation {
  static func matches(_ value: String, pattern: String) -> Bool {
    value.range(of: pattern, options: .regularExpression) != nil
  }

  static func canonicalData(for value: JSONValue) throws -> Data {
    try validateFiniteJSON(value)
    return Data(try canonicalString(for: value).utf8)
  }

  private static func canonicalString(for value: JSONValue) throws -> String {
    switch value {
    case .null:
      return "null"
    case .bool(let value):
      return value ? "true" : "false"
    case .integer(let value):
      return try ecmaNumber(Double(value))
    case .unsignedInteger(let value):
      return try ecmaNumber(Double(value))
    case .number(let value):
      return try ecmaNumber(value)
    case .string(let value):
      return try encodedJSONString(value)
    case .array(let values):
      return "[\(try values.map(canonicalString).joined(separator: ","))]"
    case .object(let fields):
      let members = try fields.keys.sorted().map { key in
        "\(try encodedJSONString(key)):\(try canonicalString(for: fields[key]!))"
      }
      return "{\(members.joined(separator: ","))}"
    }
  }

  private static func encodedJSONString(_ value: String) throws -> String {
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.withoutEscapingSlashes]
    let data = try encoder.encode(value)
    guard let string = String(data: data, encoding: .utf8) else {
      throw SemanticCatalogError.invalidOutput
    }
    return string
  }

  private static func ecmaNumber(_ value: Double) throws -> String {
    guard value.isFinite else {
      throw SemanticCatalogError.invalidOutput
    }
    if value == 0 {
      return "0"
    }

    let raw = String(value).lowercased()
    let absolute = abs(value)
    if absolute >= 1e-6, absolute < 1e21 {
      return expandScientificNumber(raw)
    }
    return normalizeScientificNumber(raw)
  }

  private static func expandScientificNumber(_ raw: String) -> String {
    guard let exponentIndex = raw.firstIndex(of: "e") else {
      return raw.hasSuffix(".0") ? String(raw.dropLast(2)) : raw
    }
    let mantissa = String(raw[..<exponentIndex])
    let exponent = Int(raw[raw.index(after: exponentIndex)...])!
    let negative = mantissa.hasPrefix("-")
    let unsignedMantissa = negative ? String(mantissa.dropFirst()) : mantissa
    let parts = unsignedMantissa.split(separator: ".", omittingEmptySubsequences: false)
    let digits = parts.joined()
    let integerDigits = parts[0].count + exponent
    let expanded: String
    if integerDigits <= 0 {
      expanded = "0." + String(repeating: "0", count: -integerDigits) + digits
    } else if integerDigits >= digits.count {
      expanded = digits + String(repeating: "0", count: integerDigits - digits.count)
    } else {
      let split = digits.index(digits.startIndex, offsetBy: integerDigits)
      expanded = String(digits[..<split]) + "." + String(digits[split...])
    }
    return negative ? "-\(expanded)" : expanded
  }

  private static func normalizeScientificNumber(_ raw: String) -> String {
    guard let exponentIndex = raw.firstIndex(of: "e") else {
      return raw.hasSuffix(".0") ? String(raw.dropLast(2)) : raw
    }
    var mantissa = String(raw[..<exponentIndex])
    if mantissa.hasSuffix(".0") {
      mantissa.removeLast(2)
    }
    let exponent = Int(raw[raw.index(after: exponentIndex)...])!
    let sign = exponent >= 0 ? "+" : ""
    return "\(mantissa)e\(sign)\(exponent)"
  }

  static func validateFiniteJSON(_ value: JSONValue) throws {
    switch value {
    case .number(let number):
      guard number.isFinite else {
        throw SemanticCatalogError.invalidOutput
      }
    case .array(let values):
      for value in values {
        try validateFiniteJSON(value)
      }
    case .object(let fields):
      for (key, value) in fields {
        guard !key.isEmpty else {
          throw SemanticCatalogError.invalidOutput
        }
        try validateFiniteJSON(value)
      }
    default:
      break
    }
  }
}

private enum SemanticJSONSchema {
  private static let supportedKeywords: Set<String> = [
    "$schema", "$id", "title", "description", "deprecated", "readOnly", "writeOnly",
    "type", "enum", "const", "required", "properties", "additionalProperties", "items",
    "minLength", "maxLength", "minimum", "maximum", "minItems", "maxItems",
  ]
  private static let supportedTypes: Set<String> = [
    "null", "boolean", "integer", "number", "string", "array", "object",
  ]

  static func validateDocument(_ document: JSONValue) throws {
    try SemanticValidation.validateFiniteJSON(document)
    guard case .object(let root) = document,
      root["$schema"] == .string("https://json-schema.org/draft/2020-12/schema"),
      case .string(let identifier)? = root["$id"],
      SemanticValidation.matches(identifier, pattern: "^[a-z][a-z0-9+.-]*:")
    else {
      throw SemanticCatalogError.invalidSchema
    }
    try validateSchemaObject(root, isRoot: true)
  }

  static func validate(_ value: JSONValue, against document: JSONValue) throws {
    guard case .object(let schema) = document else {
      throw SemanticCatalogError.invalidSchema
    }
    try validate(value, schema: schema)
  }

  private static func validateSchemaObject(
    _ schema: [String: JSONValue],
    isRoot: Bool = false
  ) throws {
    guard Set(schema.keys).isSubset(of: supportedKeywords) else {
      throw SemanticCatalogError.invalidSchema
    }
    if !isRoot, schema["$schema"] != nil || schema["$id"] != nil {
      throw SemanticCatalogError.invalidSchema
    }
    guard case .string(let type)? = schema["type"], supportedTypes.contains(type) else {
      throw SemanticCatalogError.invalidSchema
    }
    if let enumValue = schema["enum"] {
      guard case .array(let values) = enumValue, !values.isEmpty else {
        throw SemanticCatalogError.invalidSchema
      }
      for value in values {
        try SemanticValidation.validateFiniteJSON(value)
      }
    }
    if let constant = schema["const"] {
      try SemanticValidation.validateFiniteJSON(constant)
    }
    switch type {
    case "object":
      guard schema["additionalProperties"] == .bool(false) else {
        throw SemanticCatalogError.invalidSchema
      }
      let properties: [String: JSONValue]
      if let propertyValue = schema["properties"] {
        guard case .object(let declared) = propertyValue else {
          throw SemanticCatalogError.invalidSchema
        }
        properties = declared
      } else {
        properties = [:]
      }
      for propertySchema in properties.values {
        guard case .object(let object) = propertySchema else {
          throw SemanticCatalogError.invalidSchema
        }
        try validateSchemaObject(object)
      }
      if let requiredValue = schema["required"] {
        guard case .array(let requiredValues) = requiredValue else {
          throw SemanticCatalogError.invalidSchema
        }
        var required = Set<String>()
        for value in requiredValues {
          guard case .string(let name) = value,
            properties[name] != nil,
            required.insert(name).inserted
          else {
            throw SemanticCatalogError.invalidSchema
          }
        }
      }
    case "array":
      guard case .object(let items)? = schema["items"] else {
        throw SemanticCatalogError.invalidSchema
      }
      try validateSchemaObject(items)
      try validateNonnegativeInteger(schema["minItems"])
      try validateNonnegativeInteger(schema["maxItems"])
      try validateOrderedBounds(schema["minItems"], schema["maxItems"])
    case "string":
      try validateNonnegativeInteger(schema["minLength"])
      try validateNonnegativeInteger(schema["maxLength"])
      try validateOrderedBounds(schema["minLength"], schema["maxLength"])
    case "integer", "number":
      try validateNumeric(schema["minimum"])
      try validateNumeric(schema["maximum"])
      try validateOrderedNumericBounds(schema["minimum"], schema["maximum"])
    default:
      break
    }
    let objectOnly = ["required", "properties", "additionalProperties"]
    let arrayOnly = ["items", "minItems", "maxItems"]
    let stringOnly = ["minLength", "maxLength"]
    let numberOnly = ["minimum", "maximum"]
    if type != "object", objectOnly.contains(where: { schema[$0] != nil })
      || type != "array" && arrayOnly.contains(where: { schema[$0] != nil })
      || type != "string" && stringOnly.contains(where: { schema[$0] != nil })
      || type != "integer" && type != "number"
        && numberOnly.contains(where: { schema[$0] != nil })
    {
      throw SemanticCatalogError.invalidSchema
    }
  }

  private static func validate(_ value: JSONValue, schema: [String: JSONValue]) throws {
    if let constant = schema["const"], value != constant {
      throw SemanticCatalogError.invalidOutput
    }
    if case .array(let allowed)? = schema["enum"], !allowed.contains(value) {
      throw SemanticCatalogError.invalidOutput
    }
    guard case .string(let type)? = schema["type"] else {
      throw SemanticCatalogError.invalidSchema
    }
    switch (type, value) {
    case ("null", .null), ("boolean", .bool):
      break
    case ("integer", .integer), ("integer", .unsignedInteger):
      try validateNumber(value, schema: schema)
    case ("number", .integer), ("number", .unsignedInteger), ("number", .number):
      try validateNumber(value, schema: schema)
    case ("string", .string(let string)):
      let codePointCount = string.unicodeScalars.count
      if let minimum = integer(schema["minLength"]), codePointCount < minimum {
        throw SemanticCatalogError.invalidOutput
      }
      if let maximum = integer(schema["maxLength"]), codePointCount > maximum {
        throw SemanticCatalogError.invalidOutput
      }
    case ("array", .array(let values)):
      if let minimum = integer(schema["minItems"]), values.count < minimum {
        throw SemanticCatalogError.invalidOutput
      }
      if let maximum = integer(schema["maxItems"]), values.count > maximum {
        throw SemanticCatalogError.invalidOutput
      }
      guard case .object(let itemSchema)? = schema["items"] else {
        throw SemanticCatalogError.invalidSchema
      }
      for value in values {
        try validate(value, schema: itemSchema)
      }
    case ("object", .object(let fields)):
      let properties: [String: JSONValue]
      if case .object(let declared)? = schema["properties"] {
        properties = declared
      } else {
        properties = [:]
      }
      guard Set(fields.keys).isSubset(of: Set(properties.keys)) else {
        throw SemanticCatalogError.invalidOutput
      }
      if case .array(let required)? = schema["required"] {
        for value in required {
          guard case .string(let name) = value, fields[name] != nil else {
            throw SemanticCatalogError.invalidOutput
          }
        }
      }
      for (name, value) in fields {
        guard case .object(let propertySchema)? = properties[name] else {
          throw SemanticCatalogError.invalidOutput
        }
        try validate(value, schema: propertySchema)
      }
    default:
      throw SemanticCatalogError.invalidOutput
    }
  }

  private static func validateNumber(
    _ value: JSONValue,
    schema: [String: JSONValue]
  ) throws {
    guard let number = decimal(value) else {
      throw SemanticCatalogError.invalidOutput
    }
    if let minimum = decimal(schema["minimum"]), number < minimum {
      throw SemanticCatalogError.invalidOutput
    }
    if let maximum = decimal(schema["maximum"]), number > maximum {
      throw SemanticCatalogError.invalidOutput
    }
  }

  private static func validateNonnegativeInteger(_ value: JSONValue?) throws {
    guard let value else { return }
    guard let integer = integer(value), integer >= 0 else {
      throw SemanticCatalogError.invalidSchema
    }
  }

  private static func validateNumeric(_ value: JSONValue?) throws {
    guard let value else { return }
    guard decimal(value) != nil else {
      throw SemanticCatalogError.invalidSchema
    }
  }

  private static func validateOrderedBounds(
    _ minimum: JSONValue?,
    _ maximum: JSONValue?
  ) throws {
    if let minimum = integer(minimum), let maximum = integer(maximum), minimum > maximum {
      throw SemanticCatalogError.invalidSchema
    }
  }

  private static func validateOrderedNumericBounds(
    _ minimum: JSONValue?,
    _ maximum: JSONValue?
  ) throws {
    if let minimum = decimal(minimum), let maximum = decimal(maximum), minimum > maximum {
      throw SemanticCatalogError.invalidSchema
    }
  }

  private static func integer(_ value: JSONValue?) -> Int? {
    switch value {
    case .integer(let value) where value >= 0 && value <= Int64(Int.max):
      Int(value)
    case .unsignedInteger(let value) where value <= UInt64(Int.max):
      Int(value)
    default:
      nil
    }
  }

  private static func decimal(_ value: JSONValue?) -> Decimal? {
    switch value {
    case .integer(let value):
      Decimal(value)
    case .unsignedInteger(let value):
      Decimal(value)
    case .number(let value) where value.isFinite:
      Decimal(value)
    default:
      nil
    }
  }
}
