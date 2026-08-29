import Foundation

/// Hard limits negotiated by the transport-independent semantic protocol runtime.
public struct SemanticProtocolLimits: Equatable, Sendable {
  public let maximumRequestBytes: Int
  public let maximumResponseBytes: Int
  public let maximumPageItems: Int

  public init(
    maximumRequestBytes: Int,
    maximumResponseBytes: Int,
    maximumPageItems: Int
  ) throws {
    guard (1_024...16 * 1024 * 1024).contains(maximumRequestBytes),
      (1_024...64 * 1024 * 1024).contains(maximumResponseBytes),
      (1...10_000).contains(maximumPageItems)
    else {
      throw SemanticProtocolRuntimeError.invalidLimits
    }
    self.maximumRequestBytes = maximumRequestBytes
    self.maximumResponseBytes = maximumResponseBytes
    self.maximumPageItems = maximumPageItems
  }
}

public enum SemanticProtocolRuntimeError: Error, Equatable, Sendable {
  case invalidLimits
}

/// Session identity supplied to App-owned discovery and disclosure policy.
public struct SemanticProtocolSessionContext: Codable, Equatable, Hashable, Sendable {
  public let id: String
  public let generation: UInt64

  public init(id: String, generation: UInt64) {
    self.id = id
    self.generation = generation
  }
}

/// App-owned gates. There is deliberately no permissive default.
public struct SemanticProtocolPolicy: Sendable {
  public let discover: @Sendable (SemanticProtocolSessionContext, SemanticCapabilityItem) async throws -> Bool
  public let discloseSchema: @Sendable (SemanticProtocolSessionContext, SemanticCapabilityDeclaration) async throws -> Bool
  public let discloseResource: @Sendable (SemanticProtocolSessionContext, SemanticCapabilityDeclaration) async throws -> Bool
  public let discloseAction: @Sendable (SemanticProtocolSessionContext, SemanticCapabilityDeclaration) async throws -> Bool

  public init(
    discover: @escaping @Sendable (SemanticProtocolSessionContext, SemanticCapabilityItem) async throws -> Bool,
    discloseSchema: @escaping @Sendable (SemanticProtocolSessionContext, SemanticCapabilityDeclaration) async throws -> Bool,
    discloseResource: @escaping @Sendable (SemanticProtocolSessionContext, SemanticCapabilityDeclaration) async throws -> Bool,
    discloseAction: @escaping @Sendable (SemanticProtocolSessionContext, SemanticCapabilityDeclaration) async throws -> Bool
  ) {
    self.discover = discover
    self.discloseSchema = discloseSchema
    self.discloseResource = discloseResource
    self.discloseAction = discloseAction
  }
}

/// A raw UTF-8 JSON-RPC runtime for the negotiated Semantic Catalog family.
/// Transport authentication, framing, and listener ownership intentionally stay outside this type.
public actor SemanticProtocolRuntime {
  private static let semanticCapability = "semantic.catalog"
  private static let coreCapability = "session.core"

  private struct SessionRecord: Sendable {
    let context: SemanticProtocolSessionContext
    let minor: Int
    let capabilities: Set<String>
  }

  private struct RequestedListLimits: Equatable, Sendable {
    let maxItems: Int?
    let maxBytes: Int?
  }

  private struct WireSchemaHandle: Equatable, Sendable {
    let id: String
    let revision: UInt64
    let digest: String

    func matches(_ handle: SemanticSchemaHandle) -> Bool {
      id == handle.id && revision == handle.revision && digest == handle.digest
    }
  }

  private struct CursorRecord: Sendable {
    let context: SemanticProtocolSessionContext
    let catalog: SemanticCatalogIdentity
    let originalLimits: RequestedListLimits
    let visibleItems: [SemanticCapabilityItem]
    let nextIndex: Int
  }

  private struct WireFault: Error {
    let code: Int
    let kind: String
    let message: String
    let retryable: Bool
    let details: [String: JSONValue]?

    static let parseError = WireFault(
      code: -32700, kind: "parseError", message: "Parse error", retryable: false, details: nil
    )
    static let invalidRequest = WireFault(
      code: -32600, kind: "invalidRequest", message: "Invalid request", retryable: false, details: nil
    )
    static let invalidParams = WireFault(
      code: -32602, kind: "invalidParams", message: "Invalid params", retryable: false, details: nil
    )
    static let incompatibleProtocol = WireFault(
      code: -32001, kind: "incompatibleProtocol", message: "No compatible protocol version", retryable: false, details: nil
    )
    static let sessionExpired = WireFault(
      code: -32002, kind: "sessionExpired", message: "Session expired", retryable: false, details: nil
    )
    static let capabilityUnavailable = WireFault(
      code: -32003, kind: "capabilityUnavailable", message: "Capability unavailable", retryable: false, details: nil
    )
    static let resourceExhausted = WireFault(
      code: -32004, kind: "resourceExhausted", message: "Resource exhausted", retryable: false, details: nil
    )
    static let cursorExpired = WireFault(
      code: -32006, kind: "cursorExpired", message: "Cursor expired", retryable: false, details: nil
    )

    static func methodNotFound(_ method: String) -> WireFault {
      WireFault(
        code: -32601,
        kind: "methodNotFound",
        message: "Method not found",
        retryable: false,
        details: ["method": .string(method)]
      )
    }

    static func capabilityNotFound(_ capability: String) -> WireFault {
      WireFault(
        code: -32020,
        kind: "semantic.capabilityNotFound",
        message: "Semantic capability is unavailable",
        retryable: false,
        details: ["capability": .string(capability)]
      )
    }

    static func schemaMismatch(_ capability: String) -> WireFault {
      WireFault(
        code: -32021,
        kind: "semantic.schemaMismatch",
        message: "Semantic schema does not match",
        retryable: false,
        details: ["capability": .string(capability)]
      )
    }

    static func unavailable(_ capability: String) -> WireFault {
      WireFault(
        code: -32022,
        kind: "semantic.unavailable",
        message: "Semantic capability is unavailable",
        retryable: true,
        details: ["capability": .string(capability)]
      )
    }

    static func disclosureDenied(_ capability: String) -> WireFault {
      WireFault(
        code: -32023,
        kind: "semantic.disclosureDenied",
        message: "Semantic disclosure is denied",
        retryable: false,
        details: ["capability": .string(capability)]
      )
    }

    static let internalError = WireFault(
      code: -32603, kind: "internalError", message: "Internal error", retryable: false, details: nil
    )
  }

  private let catalog: SemanticCatalog
  private let limits: SemanticProtocolLimits
  private let policy: SemanticProtocolPolicy
  private let actionCoordinator: TargetActionCoordinator
  private var sessions: [String: SessionRecord] = [:]
  private var cursors: [String: CursorRecord] = [:]

  public init(
    catalog: SemanticCatalog,
    limits: SemanticProtocolLimits,
    policy: SemanticProtocolPolicy,
    actionCoordinator: TargetActionCoordinator
  ) {
    self.catalog = catalog
    self.limits = limits
    self.policy = policy
    self.actionCoordinator = actionCoordinator
  }

  /// Clears listener-epoch-bound protocol state while retaining the process Catalog.
  public func invalidateSessions() {
    sessions.removeAll()
    cursors.removeAll()
  }

  /// Handles exactly one raw UTF-8 JSON-RPC message.
  public func handle(_ bytes: Data) async -> Data {
    let decoded: JSONValue
    do {
      decoded = try JSONDecoder().decode(JSONValue.self, from: bytes)
    } catch {
      return encodeError(.null, .parseError)
    }

    let requestID = Self.requestID(in: decoded) ?? .null
    guard bytes.count <= limits.maximumRequestBytes else {
      return encodeError(requestID, .resourceExhausted)
    }

    do {
      let response = try await dispatch(decoded)
      let data = encode(response)
      guard data.count <= limits.maximumResponseBytes else {
        return encodeError(requestID, .resourceExhausted)
      }
      return data
    } catch let fault as WireFault {
      return encodeError(requestID, fault)
    } catch {
      return encodeError(requestID, .internalError)
    }
  }

  private func dispatch(_ raw: JSONValue) async throws -> JSONValue {
    guard case .object(let envelope) = raw else {
      throw WireFault.invalidRequest
    }
    try Self.requireEnvelopeKeys(envelope, allowed: ["jsonrpc", "id", "method", "params", "context"])
    guard envelope["jsonrpc"] == .string("2.0"),
      let id = Self.string(envelope["id"], maximum: 128),
      !id.isEmpty,
      let method = Self.validMethod(envelope["method"])
    else {
      throw WireFault.invalidRequest
    }
    let requestID = JSONValue.string(id)

    if method == "session.open" {
      guard envelope["context"] == nil else { throw WireFault.invalidRequest }
      guard let params = Self.object(envelope["params"]) else { throw WireFault.invalidParams }
      return try open(requestID: requestID, params: params)
    }

    guard let rawContext = Self.object(envelope["context"]),
      let context = try? Self.context(rawContext)
    else {
      throw WireFault.invalidRequest
    }
    guard let session = sessions[context.id], session.context == context else {
      throw WireFault.sessionExpired
    }
    guard let params = Self.object(envelope["params"]) else { throw WireFault.invalidParams }

    if method.hasPrefix("semantic.") {
      guard session.minor >= 2 else { throw WireFault.methodNotFound(method) }
      guard session.capabilities.contains(Self.semanticCapability) else {
        throw WireFault.capabilityUnavailable
      }
    }
    switch method {
    case "semantic.list":
      return try await list(requestID: requestID, session: session, params: params)
    case "semantic.show":
      return try await show(requestID: requestID, session: session, params: params)
    case "semantic.schema":
      return try await schema(requestID: requestID, session: session, params: params)
    case "semantic.query":
      return try await query(requestID: requestID, session: session, params: params)
    case "semantic.invoke":
      return try await invoke(requestID: requestID, session: session, params: params)
    default:
      throw WireFault.methodNotFound(method)
    }
  }

  private func invoke(
    requestID: JSONValue,
    session: SessionRecord,
    params: [String: JSONValue]
  ) async throws -> JSONValue {
    try Self.requireExactKeys(
      params,
      allowed: ["capability", "declarationRevision", "inputSchema", "input", "authorizationGrant"]
    )
    guard params["capability"] != nil,
      params["declarationRevision"] != nil,
      params["inputSchema"] != nil,
      params["input"] != nil
    else { throw WireFault.invalidParams }
    let capability = try Self.capability(params["capability"])
    let revision = try Self.positiveInteger(params["declarationRevision"])
    let handle = try Self.schemaHandle(params["inputSchema"])
    let grant = params["authorizationGrant"].flatMap { Self.string($0, maximum: 256) }
    if params["authorizationGrant"] != nil && (grant?.isEmpty ?? true) {
      throw WireFault.invalidParams
    }
    let declaration = try await declaration(capability, revision: revision, context: session.context)
    try ensureActive(session)
    guard declaration.kind == .action else {
      throw WireFault.capabilityNotFound(capability)
    }
    guard let inputSchema = declaration.inputSchema,
      handle.matches(inputSchema)
    else { throw WireFault.schemaMismatch(capability) }
    guard (try? await policy.discloseAction(session.context, declaration)) == true else {
      throw WireFault.disclosureDenied(capability)
    }
    try ensureActive(session)
    do {
      try await actionCoordinator.invoke(
        SemanticActionInvocation(
          capability: capability,
          declarationRevision: revision,
          inputSchema: inputSchema,
          input: params["input"]!
        ),
        authorizationGrant: grant,
        session: session.context,
        sessionIsActive: { [weak self] in
          guard let self else { return false }
          return await self.isSessionActive(session.context)
        }
      )
    } catch let error as TargetActionCoordinatorError {
      switch error {
      case .policyDenied:
        throw WireFault(
          code: -32024,
          kind: "action.policyDenied",
          message: "Action policy is denied",
          retryable: false,
          details: [
            "capability": .string(capability),
            "field": .string("authorizationGrant"),
          ]
        )
      case .conflict:
        throw WireFault(
          code: -32025,
          kind: "action.conflict",
          message: "Action conflicts with an in-flight mutation",
          retryable: false,
          details: ["capability": .string(capability)]
        )
      case .outcomeUnknown:
        throw WireFault(
          code: -32026,
          kind: "action.outcomeUnknown",
          message: "Action outcome is unknown",
          retryable: false,
          details: ["capability": .string(capability)]
        )
      case .sessionExpired:
        throw WireFault.sessionExpired
      case .preDispatchFailed:
        throw WireFault.internalError
      }
    } catch {
      throw Self.map(error, capability: capability)
    }
    return Self.success(
      requestID,
      result: .object([
        "capability": .string(capability),
        "declarationRevision": .unsignedInteger(revision),
        "completed": .bool(true),
      ])
    )
  }

  private func isSessionActive(_ context: SemanticProtocolSessionContext) -> Bool {
    sessions[context.id]?.context == context
  }

  private func open(requestID: JSONValue, params: [String: JSONValue]) throws -> JSONValue {
    try Self.requireExactKeys(params, allowed: ["client", "protocol", "requiredCapabilities"])
    guard let client = Self.object(params["client"]),
      Self.validClient(client),
      let protocolRange = Self.object(params["protocol"])
    else {
      throw WireFault.invalidParams
    }
    let selectedMinor = try Self.selectMinor(protocolRange)
    let offered: Set<String> = selectedMinor == 2
      ? [Self.coreCapability, Self.semanticCapability]
      : [Self.coreCapability]
    let required = try Self.capabilities(params["requiredCapabilities"])
    guard required.isSubset(of: offered) else { throw WireFault.capabilityUnavailable }

    let granted: Set<String> = required.isEmpty ? [Self.coreCapability] : required.union([Self.coreCapability])
    let id = "session_\(UUID().uuidString.lowercased().replacingOccurrences(of: "-", with: ""))"
    let context = SemanticProtocolSessionContext(id: id, generation: catalog.identity.generation)
    let session = SessionRecord(context: context, minor: selectedMinor, capabilities: granted)
    sessions[id] = session
    let result: [String: JSONValue] = [
      "context": Self.contextValue(context),
      "protocol": .object(["major": .integer(1), "minor": .integer(Int64(selectedMinor))]),
      "capabilities": .array(granted.sorted().map(JSONValue.string)),
      "limits": .object([
        "maxRequestBytes": .integer(Int64(limits.maximumRequestBytes)),
        "maxResponseBytes": .integer(Int64(limits.maximumResponseBytes)),
        "maxPageItems": .integer(Int64(limits.maximumPageItems)),
      ]),
    ]
    return Self.success(requestID, result: .object(result))
  }

  private func list(
    requestID: JSONValue,
    session: SessionRecord,
    params: [String: JSONValue]
  ) async throws -> JSONValue {
    let start: Int
    let requested: RequestedListLimits
    let visible: [SemanticCapabilityItem]
    if let cursorValue = params["cursor"] {
      try Self.requireExactKeys(params, allowed: ["cursor"])
      guard let token = Self.string(cursorValue, maximum: 4_096), !token.isEmpty else {
        throw WireFault.invalidParams
      }
      guard let cursor = cursors[token] else { throw WireFault.invalidParams }
      guard cursor.context == session.context else { throw WireFault.invalidParams }
      guard cursor.catalog == catalog.identity else { throw WireFault.cursorExpired }
      requested = cursor.originalLimits
      visible = cursor.visibleItems
      start = cursor.nextIndex
      cursors.removeValue(forKey: token)
    } else {
      try Self.requireExactKeys(params, allowed: ["limits"])
      requested = try Self.requestedLimits(params["limits"])
      visible = await visibleItems(for: session.context)
      try ensureActive(session)
      start = 0
    }
    let appliedItems = min(requested.maxItems ?? limits.maximumPageItems, limits.maximumPageItems)
    let appliedBytes = min(requested.maxBytes ?? limits.maximumResponseBytes, limits.maximumResponseBytes)
    guard start <= visible.count else { throw WireFault.cursorExpired }

    var end = min(start + appliedItems, visible.count)
    var byteLimited = false
    var cursorToken: String?
    var response: JSONValue
    while true {
      let hasMore = end < visible.count
      let provisionalToken = hasMore ? Self.newCursorToken() : nil
      let reasons = Self.listReasons(hasMore: hasMore, itemLimited: end == start + appliedItems, byteLimited: byteLimited)
      response = Self.listResponse(
        requestID: requestID,
        catalog: catalog.identity,
        items: Array(visible[start..<end]),
        maxItems: appliedItems,
        maxBytes: appliedBytes,
        cursor: provisionalToken,
        reasons: reasons
      )
      let count = encode(response).count
      guard count <= limits.maximumResponseBytes else { throw WireFault.resourceExhausted }
      if count <= appliedBytes {
        cursorToken = provisionalToken
        break
      }
      guard end > start else { throw WireFault.resourceExhausted }
      end -= 1
      byteLimited = true
    }
    if let cursorToken {
      cursors[cursorToken] = CursorRecord(
        context: session.context,
        catalog: catalog.identity,
        originalLimits: requested,
        visibleItems: visible,
        nextIndex: end
      )
    }
    return response
  }

  private func show(
    requestID: JSONValue,
    session: SessionRecord,
    params: [String: JSONValue]
  ) async throws -> JSONValue {
    let (capability, revision) = try Self.capabilityRevision(params)
    let declaration = try await declaration(capability, revision: revision, context: session.context)
    try ensureActive(session)
    return Self.success(requestID, result: Self.declarationValue(declaration))
  }

  private func schema(
    requestID: JSONValue,
    session: SessionRecord,
    params: [String: JSONValue]
  ) async throws -> JSONValue {
    try Self.requireExactKeys(params, allowed: ["capability", "declarationRevision", "schema"])
    let capability = try Self.capability(params["capability"])
    let revision = try Self.positiveInteger(params["declarationRevision"])
    let requestedHandle = try Self.schemaHandle(params["schema"])
    let declaration = try await declaration(capability, revision: revision, context: session.context)
    try ensureActive(session)
    guard let handle = [declaration.inputSchema, declaration.valueSchema]
      .compactMap({ $0 })
      .first(where: { requestedHandle.matches($0) })
    else {
      throw WireFault.schemaMismatch(capability)
    }
    guard await permitsSchema(session.context, declaration) else {
      throw WireFault.disclosureDenied(capability)
    }
    try ensureActive(session)
    let document: SemanticSchema
    do {
      document = try catalog.schema(
        capabilityID: capability,
        declarationRevision: revision,
        handle: handle
      )
    } catch {
      throw Self.map(error, capability: capability)
    }
    return Self.success(requestID, result: .object([
      "schema": Self.schemaHandleValue(document.handle),
      "document": document.document,
    ]))
  }

  private func query(
    requestID: JSONValue,
    session: SessionRecord,
    params: [String: JSONValue]
  ) async throws -> JSONValue {
    try Self.requireExactKeys(
      params,
      allowed: ["capability", "declarationRevision", "inputSchema", "input", "valueSchema"]
    )
    let capability = try Self.capability(params["capability"])
    let revision = try Self.positiveInteger(params["declarationRevision"])
    let requestedValueSchema = try Self.schemaHandle(params["valueSchema"])
    let requestedInputSchema: WireSchemaHandle?
    if let rawInputSchema = params["inputSchema"] {
      requestedInputSchema = try Self.schemaHandle(rawInputSchema)
    } else {
      requestedInputSchema = nil
    }
    let input = params["input"]
    guard (requestedInputSchema == nil) == (input == nil) else { throw WireFault.invalidParams }
    let declaration = try await declaration(capability, revision: revision, context: session.context)
    try ensureActive(session)
    let inputSchemaMatches: Bool
    if let declaredInputSchema = declaration.inputSchema {
      inputSchemaMatches = requestedInputSchema?.matches(declaredInputSchema) ?? false
    } else {
      inputSchemaMatches = requestedInputSchema == nil
    }
    guard declaration.kind == .resource else {
      throw WireFault.capabilityNotFound(capability)
    }
    guard
      let valueSchema = declaration.valueSchema,
      requestedValueSchema.matches(valueSchema),
      inputSchemaMatches
    else {
      throw WireFault.schemaMismatch(capability)
    }
    let inputSchema = declaration.inputSchema
    guard await permitsResource(session.context, declaration) else {
      throw WireFault.disclosureDenied(capability)
    }
    try ensureActive(session)
    let output: DetachedSemanticValue
    do {
      output = try await catalog.queryResource(
        SemanticResourceQuery(
          capability: capability,
          declarationRevision: revision,
          inputSchema: inputSchema,
          input: input,
          valueSchema: valueSchema
        ),
        maximumOutputBytes: limits.maximumResponseBytes
      )
    } catch {
      throw Self.map(error, capability: capability)
    }
    try ensureActive(session)
    return Self.success(requestID, result: .object([
      "value": output.value,
      "valueSchema": Self.schemaHandleValue(output.valueSchema),
      "bytes": .integer(Int64(output.bytes)),
    ]))
  }

  private func declaration(
    _ capability: String,
    revision: UInt64,
    context: SemanticProtocolSessionContext
  ) async throws -> SemanticCapabilityDeclaration {
    let declaration: SemanticCapabilityDeclaration
    do {
      declaration = try catalog.declaration(for: capability)
    } catch {
      throw WireFault.capabilityNotFound(capability)
    }
    let item = SemanticCapabilityItem(
      id: declaration.id,
      kind: declaration.kind,
      declarationRevision: declaration.declarationRevision
    )
    guard await permitsDiscovery(context, item) else {
      throw WireFault.capabilityNotFound(capability)
    }
    guard declaration.declarationRevision == revision else {
      throw WireFault.schemaMismatch(capability)
    }
    return declaration
  }

  private func visibleItems(for context: SemanticProtocolSessionContext) async -> [SemanticCapabilityItem] {
    var visible: [SemanticCapabilityItem] = []
    for item in catalog.items {
      if await permitsDiscovery(context, item) {
        visible.append(item)
      }
    }
    return visible
  }

  private func ensureActive(_ session: SessionRecord) throws {
    guard sessions[session.context.id]?.context == session.context else {
      throw WireFault.sessionExpired
    }
  }

  private func permitsDiscovery(
    _ context: SemanticProtocolSessionContext,
    _ item: SemanticCapabilityItem
  ) async -> Bool {
    (try? await policy.discover(context, item)) ?? false
  }

  private func permitsSchema(
    _ context: SemanticProtocolSessionContext,
    _ declaration: SemanticCapabilityDeclaration
  ) async -> Bool {
    (try? await policy.discloseSchema(context, declaration)) ?? false
  }

  private func permitsResource(
    _ context: SemanticProtocolSessionContext,
    _ declaration: SemanticCapabilityDeclaration
  ) async -> Bool {
    (try? await policy.discloseResource(context, declaration)) ?? false
  }

  private static func selectMinor(_ range: [String: JSONValue]) throws -> Int {
    try requireExactKeys(range, allowed: ["major", "minMinor", "maxMinor"])
    let major = try positiveInteger(range["major"])
    let minimum = try nonnegativeInteger(range["minMinor"])
    let maximum = try nonnegativeInteger(range["maxMinor"])
    guard major == 1, minimum <= maximum else { throw WireFault.incompatibleProtocol }
    guard minimum <= 2 else {
      throw WireFault.incompatibleProtocol
    }
    return min(maximum, 2)
  }

  private static func capabilities(_ value: JSONValue?) throws -> Set<String> {
    guard let value else { return [] }
    guard case .array(let entries) = value else { throw WireFault.invalidParams }
    var result = Set<String>()
    for entry in entries {
      guard let capability = validSessionCapability(entry), result.insert(capability).inserted else {
        throw WireFault.invalidParams
      }
    }
    return result
  }

  private static func requestedLimits(_ value: JSONValue?) throws -> RequestedListLimits {
    guard let value else { return RequestedListLimits(maxItems: nil, maxBytes: nil) }
    guard let object = object(value) else { throw WireFault.invalidParams }
    try requireExactKeys(object, allowed: ["maxItems", "maxBytes"])
    guard !object.isEmpty else { throw WireFault.invalidParams }
    let items = try object["maxItems"].map(positiveInteger)
    let bytes = try object["maxBytes"].map(positiveInteger)
    guard items.map({ (1...10_000).contains($0) }) ?? true,
      bytes.map({ (1_024...64 * 1024 * 1024).contains($0) }) ?? true
    else { throw WireFault.invalidParams }
    return RequestedListLimits(
      maxItems: items.map { Int($0) },
      maxBytes: bytes.map { Int($0) }
    )
  }

  private static func capabilityRevision(_ params: [String: JSONValue]) throws -> (String, UInt64) {
    try requireExactKeys(params, allowed: ["capability", "declarationRevision"])
    return (try capability(params["capability"]), try positiveInteger(params["declarationRevision"]))
  }

  private static func capability(_ value: JSONValue?) throws -> String {
    guard let value = validCapability(value) else { throw WireFault.invalidParams }
    return value
  }

  private static func validCapability(_ value: JSONValue?) -> String? {
    guard let value = string(value, maximum: 128),
      value.range(of: "^[a-z][a-z0-9]*(?:[._-][a-z0-9]+)*$", options: .regularExpression)
        == value.startIndex..<value.endIndex
    else { return nil }
    return value
  }

  private static func validSessionCapability(_ value: JSONValue?) -> String? {
    guard let value = string(value, maximum: 128),
      value.range(of: "^[a-z][a-z0-9]*(?:\\.[a-z][a-z0-9]*)+$", options: .regularExpression)
        == value.startIndex..<value.endIndex
    else { return nil }
    return value
  }

  private static func schemaHandle(_ value: JSONValue?) throws -> WireSchemaHandle {
    guard let object = object(value) else { throw WireFault.invalidParams }
    try requireExactKeys(object, allowed: ["id", "revision", "digest"])
    guard let id = string(object["id"], maximum: 128),
      id.range(of: "^schema_[A-Za-z0-9._~-]{8,120}$", options: .regularExpression)
        == id.startIndex..<id.endIndex,
      let revision = try? positiveInteger(object["revision"]),
      let digest = string(object["digest"], maximum: 71),
      digest.range(of: "^sha256:[a-f0-9]{64}$", options: .regularExpression)
        == digest.startIndex..<digest.endIndex
    else { throw WireFault.invalidParams }
    return WireSchemaHandle(id: id, revision: revision, digest: digest)
  }

  private static func context(_ object: [String: JSONValue]) throws -> SemanticProtocolSessionContext {
    try requireExactKeys(object, allowed: ["id", "generation"])
    guard let id = string(object["id"], maximum: 128),
      id.range(of: "^[A-Za-z0-9._~-]{16,128}$", options: .regularExpression)
        == id.startIndex..<id.endIndex
    else { throw WireFault.invalidRequest }
    return SemanticProtocolSessionContext(id: id, generation: try positiveInteger(object["generation"]))
  }

  private static func validClient(_ value: [String: JSONValue]) -> Bool {
    do {
      try requireExactKeys(value, allowed: ["name", "version"])
    } catch {
      return false
    }
    guard let name = string(value["name"], maximum: 128), !name.isEmpty,
      let version = string(value["version"], maximum: 64), !version.isEmpty
    else { return false }
    return true
  }

  private static func validMethod(_ value: JSONValue?) -> String? {
    guard let method = string(value, maximum: 128),
      method.range(of: "^(?!rpc\\.)[a-z][a-z0-9]*(?:\\.[a-z][a-z0-9]*)+$", options: .regularExpression)
        == method.startIndex..<method.endIndex
    else { return nil }
    return method
  }

  private static func positiveInteger(_ value: JSONValue?) throws -> UInt64 {
    let integer = try unsignedInteger(value)
    guard integer > 0 else { throw WireFault.invalidParams }
    return integer
  }

  private static func nonnegativeInteger(_ value: JSONValue?) throws -> Int {
    let integer = try unsignedInteger(value)
    guard integer <= UInt64(Int.max) else { throw WireFault.invalidParams }
    return Int(integer)
  }

  private static func unsignedInteger(_ value: JSONValue?) throws -> UInt64 {
    switch value {
    case .integer(let value) where value >= 0:
      return UInt64(value)
    case .unsignedInteger(let value):
      return value
    default:
      throw WireFault.invalidParams
    }
  }

  private static func object(_ value: JSONValue?) -> [String: JSONValue]? {
    guard case .object(let value) = value else { return nil }
    return value
  }

  private static func string(_ value: JSONValue?, maximum: Int) -> String? {
    guard case .string(let value) = value, value.unicodeScalars.count <= maximum else { return nil }
    return value
  }

  private static func requireExactKeys(_ object: [String: JSONValue], allowed: Set<String>) throws {
    guard Set(object.keys).isSubset(of: allowed) else { throw WireFault.invalidParams }
  }

  private static func requireEnvelopeKeys(_ object: [String: JSONValue], allowed: Set<String>) throws {
    guard Set(object.keys).isSubset(of: allowed) else { throw WireFault.invalidRequest }
  }

  private static func requestID(in raw: JSONValue) -> JSONValue? {
    guard case .object(let object) = raw,
      let id = string(object["id"], maximum: 128), !id.isEmpty
    else { return nil }
    return .string(id)
  }

  private static func contextValue(_ context: SemanticProtocolSessionContext) -> JSONValue {
    .object(["id": .string(context.id), "generation": .unsignedInteger(context.generation)])
  }

  private static func schemaHandleValue(_ handle: SemanticSchemaHandle) -> JSONValue {
    .object([
      "id": .string(handle.id),
      "revision": .unsignedInteger(handle.revision),
      "digest": .string(handle.digest),
    ])
  }

  private static func declarationValue(_ declaration: SemanticCapabilityDeclaration) -> JSONValue {
    var value: [String: JSONValue] = [
      "id": .string(declaration.id),
      "kind": .string(declaration.kind.rawValue),
      "declarationRevision": .unsignedInteger(declaration.declarationRevision),
    ]
    if let input = declaration.inputSchema { value["inputSchema"] = schemaHandleValue(input) }
    if let output = declaration.valueSchema { value["valueSchema"] = schemaHandleValue(output) }
    if let policy = declaration.actionPolicy {
      value["policy"] = .object([
        "authorization": .string(policy.authorization.rawValue),
        "retrySafety": .string(policy.retrySafety.rawValue),
      ])
    }
    return .object(value)
  }

  private static func listResponse(
    requestID: JSONValue,
    catalog: SemanticCatalogIdentity,
    items: [SemanticCapabilityItem],
    maxItems: Int,
    maxBytes: Int,
    cursor: String?,
    reasons: [String]
  ) -> JSONValue {
    var page: [String: JSONValue] = [
      "truncated": .bool(cursor != nil),
      "returnedItems": .integer(Int64(items.count)),
      "appliedLimits": .object([
        "maxItems": .integer(Int64(maxItems)),
        "maxBytes": .integer(Int64(maxBytes)),
      ]),
    ]
    if let cursor {
      page["nextCursor"] = .string(cursor)
      page["reasons"] = .array(reasons.map(JSONValue.string))
    }
    return success(requestID, result: .object([
      "catalog": .object([
        "id": .string(catalog.id),
        "generation": .unsignedInteger(catalog.generation),
      ]),
      "capabilities": .array(items.map {
        .object([
          "id": .string($0.id),
          "kind": .string($0.kind.rawValue),
          "declarationRevision": .unsignedInteger($0.declarationRevision),
        ])
      }),
      "page": .object(page),
    ]))
  }

  private static func listReasons(hasMore: Bool, itemLimited: Bool, byteLimited: Bool) -> [String] {
    guard hasMore else { return [] }
    var reasons: [String] = []
    if itemLimited { reasons.append("maxItems") }
    if byteLimited { reasons.append("maxBytes") }
    return reasons.isEmpty ? ["maxBytes"] : reasons
  }

  private static func success(_ id: JSONValue, result: JSONValue) -> JSONValue {
    .object(["jsonrpc": .string("2.0"), "id": id, "result": result])
  }

  private static func map(_ error: Error, capability: String) -> WireFault {
    guard let error = error as? SemanticCatalogError else { return .internalError }
    switch error {
    case .capabilityNotFound, .kindMismatch:
      return .capabilityNotFound(capability)
    case .schemaMismatch, .invalidInput:
      return .schemaMismatch(capability)
    case .unavailable:
      return .unavailable(capability)
    case .disclosureDenied, .invalidOutput:
      return .disclosureDenied(capability)
    case .resourceExhausted:
      return .resourceExhausted
    default:
      return .internalError
    }
  }

  private static func newCursorToken() -> String {
    "cursor_\(UUID().uuidString.lowercased().replacingOccurrences(of: "-", with: ""))"
  }

  private func encodeError(_ id: JSONValue, _ fault: WireFault) -> Data {
    var data: [String: JSONValue] = [
      "kind": .string(fault.kind),
      "retryable": .bool(fault.retryable),
    ]
    if let details = fault.details { data["details"] = .object(details) }
    return encode(.object([
      "jsonrpc": .string("2.0"),
      "id": id,
      "error": .object([
        "code": .integer(Int64(fault.code)),
        "message": .string(fault.message),
        "data": .object(data),
      ]),
    ]))
  }

  private func encode(_ value: JSONValue) -> Data {
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
    return (try? encoder.encode(value)) ?? Data("{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32603,\"message\":\"Internal error\",\"data\":{\"kind\":\"internalError\",\"retryable\":false}}}".utf8)
  }
}
