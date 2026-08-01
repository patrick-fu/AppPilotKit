import Foundation

public enum UIPlatform: String, Codable, Sendable {
  case iOS = "ios"
  case android
}

public enum UIRepresentation: String, Codable, Sendable {
  case native
  case semantics
  case accessibility
}

public enum UICoordinateUnit: String, Codable, Sendable {
  case point
  case pixel
}

public enum UISourceCoverage: String, Codable, Sendable {
  case complete
  case partial
}

public struct UICoordinateSpace: Codable, Equatable, Sendable {
  public let unit: UICoordinateUnit
  public let scale: Double

  public init(unit: UICoordinateUnit, scale: Double) {
    self.unit = unit
    self.scale = scale
  }
}

public struct UIRect: Codable, Equatable, Sendable {
  public let x: Double
  public let y: Double
  public let width: Double
  public let height: Double

  public init(x: Double, y: Double, width: Double, height: Double) {
    self.x = x
    self.y = y
    self.width = width
    self.height = height
  }
}

public struct UIProviderDescriptor: Codable, Equatable, Sendable {
  public let name: String
  public let platform: UIPlatform

  public init(name: String, platform: UIPlatform) {
    self.name = name
    self.platform = platform
  }
}

public enum JSONValue: Equatable, Sendable {
  case null
  case bool(Bool)
  case integer(Int64)
  case unsignedInteger(UInt64)
  case number(Double)
  case string(String)
  case array([JSONValue])
  case object([String: JSONValue])
}

extension JSONValue: Codable {
  public init(from decoder: any Decoder) throws {
    let container = try decoder.singleValueContainer()
    if container.decodeNil() {
      self = .null
    } else if let value = try? container.decode(Bool.self) {
      self = .bool(value)
    } else if let value = try? container.decode(Int64.self) {
      self = .integer(value)
    } else if let value = try? container.decode(UInt64.self) {
      self = .unsignedInteger(value)
    } else if let value = try? container.decode(Double.self) {
      self = .number(value)
    } else if let value = try? container.decode(String.self) {
      self = .string(value)
    } else if let value = try? container.decode([JSONValue].self) {
      self = .array(value)
    } else {
      self = .object(try container.decode([String: JSONValue].self))
    }
  }

  public func encode(to encoder: any Encoder) throws {
    var container = encoder.singleValueContainer()
    switch self {
    case .null:
      try container.encodeNil()
    case .bool(let value):
      try container.encode(value)
    case .integer(let value):
      try container.encode(value)
    case .unsignedInteger(let value):
      try container.encode(value)
    case .number(let value):
      try container.encode(value)
    case .string(let value):
      try container.encode(value)
    case .array(let value):
      try container.encode(value)
    case .object(let value):
      try container.encode(value)
    }
  }
}

public struct UISnapshotScope: Codable, Equatable, Hashable, Sendable {
  public let sessionID: String
  public let processGeneration: UInt64

  public init(sessionID: String, processGeneration: UInt64) {
    self.sessionID = sessionID
    self.processGeneration = processGeneration
  }
}

public struct UISnapshotIdentity: Codable, Equatable, Hashable, Sendable {
  public let id: String
  public let generation: UInt64

  public init(id: String, generation: UInt64) {
    self.id = id
    self.generation = generation
  }
}

public struct RedactedNodeIndex: Codable, Equatable, Sendable {
  public let identifier: String?
  public let text: String?
  public let className: String?
  public let typeName: String?
  public let traits: [String]?
  public let frame: UIRect?
  public let visible: Bool?
  public let interactive: Bool?

  public init(
    identifier: String? = nil,
    text: String? = nil,
    className: String? = nil,
    typeName: String? = nil,
    traits: [String]? = nil,
    frame: UIRect? = nil,
    visible: Bool? = nil,
    interactive: Bool? = nil
  ) {
    self.identifier = identifier
    self.text = text
    self.className = className
    self.typeName = typeName
    self.traits = traits
    self.frame = frame
    self.visible = visible
    self.interactive = interactive
  }
}

public struct RedactedNodeCapture: Codable, Equatable, Sendable {
  public let id: String
  public let parentID: String?
  public let childIndex: Int?
  public let depth: Int
  public let childCount: Int
  public let index: RedactedNodeIndex?
  public let native: [String: JSONValue]?

  public init(
    id: String,
    parentID: String? = nil,
    childIndex: Int? = nil,
    depth: Int,
    childCount: Int,
    index: RedactedNodeIndex? = nil,
    native: [String: JSONValue]? = nil
  ) {
    self.id = id
    self.parentID = parentID
    self.childIndex = childIndex
    self.depth = depth
    self.childCount = childCount
    self.index = index
    self.native = native
  }
}

public struct RedactedSourceCapture: Codable, Equatable, Sendable {
  public let id: String
  public let provider: String
  public let platform: UIPlatform
  public let representation: UIRepresentation
  public let nativeSchema: String
  public let coordinateSpace: UICoordinateSpace
  public let coverage: UISourceCoverage
  public let limitations: [String]?
  public let nodes: [RedactedNodeCapture]

  public init(
    id: String,
    provider: String,
    platform: UIPlatform,
    representation: UIRepresentation,
    nativeSchema: String,
    coordinateSpace: UICoordinateSpace,
    coverage: UISourceCoverage,
    limitations: [String]? = nil,
    nodes: [RedactedNodeCapture]
  ) {
    self.id = id
    self.provider = provider
    self.platform = platform
    self.representation = representation
    self.nativeSchema = nativeSchema
    self.coordinateSpace = coordinateSpace
    self.coverage = coverage
    self.limitations = limitations
    self.nodes = nodes
  }
}

public struct RedactedProviderCapture: Codable, Equatable, Sendable {
  public let sources: [RedactedSourceCapture]

  public init(sources: [RedactedSourceCapture]) {
    self.sources = sources
  }
}

@MainActor
public protocol UISnapshotProvider: AnyObject, Sendable {
  nonisolated var descriptor: UIProviderDescriptor { get }
  func capture() async throws -> RedactedProviderCapture
}

public struct StoredUISource: Codable, Equatable, Sendable {
  public let id: String
  public let platform: UIPlatform
  public let provider: String
  public let representation: UIRepresentation
  public let nativeSchema: String
  public let coordinateSpace: UICoordinateSpace
  public let rootReference: String
  public let coverage: UISourceCoverage
  public let limitations: [String]?
}

public struct StoredUINode: Codable, Equatable, Sendable {
  public let reference: String
  public let sourceID: String
  public let parentReference: String?
  public let childIndex: Int?
  public let depth: Int
  public let childCount: Int
  public let index: RedactedNodeIndex?
  public let native: [String: JSONValue]?
}

public struct StoredUISnapshot: Codable, Equatable, Sendable {
  public let scope: UISnapshotScope
  public let identity: UISnapshotIdentity
  public let sources: [StoredUISource]
  public let nodes: [StoredUINode]
  public let storedBytes: Int
}

public struct UISnapshotStoreLimits: Equatable, Sendable {
  public let maximumSnapshotCount: Int
  public let maximumStoredBytes: Int

  public init(maximumSnapshotCount: Int, maximumStoredBytes: Int) {
    self.maximumSnapshotCount = maximumSnapshotCount
    self.maximumStoredBytes = maximumStoredBytes
  }

  public static let `default` = UISnapshotStoreLimits(
    maximumSnapshotCount: 8,
    maximumStoredBytes: 16 * 1024 * 1024
  )
}

public enum UISnapshotFailureKind: String, Equatable, Sendable {
  case invalidParams
  case snapshotExpired = "ui.snapshotExpired"
  case resourceExhausted
  case internalError
}

public enum UISnapshotRuntimeError: Error, Equatable, Sendable {
  case invalidParams(String)
  case snapshotExpired
  case resourceExhausted(String)
  case internalError(String)

  public var kind: UISnapshotFailureKind {
    switch self {
    case .invalidParams:
      .invalidParams
    case .snapshotExpired:
      .snapshotExpired
    case .resourceExhausted:
      .resourceExhausted
    case .internalError:
      .internalError
    }
  }
}

public actor UISnapshotRuntime {
  private struct ProviderCaptureValidationError: Error {
    let reason: String
  }

  private struct StoredSnapshotRecord: Encodable {
    let scope: UISnapshotScope
    let identity: UISnapshotIdentity
    let sources: [StoredUISource]
    let nodes: [StoredUINode]
  }

  private struct RegisteredProvider: Sendable {
    let descriptor: UIProviderDescriptor
    let adapter: any UISnapshotProvider
  }

  private let providers: [RegisteredProvider]
  private let limits: UISnapshotStoreLimits
  private var nextGeneration: UInt64 = 1
  private var snapshots: [UISnapshotIdentity: StoredUISnapshot] = [:]
  private var snapshotOrder: [UISnapshotIdentity] = []
  private var totalStoredBytes = 0
  private var captureTail: Task<Void, Never>?

  public init(
    providers: [any UISnapshotProvider],
    limits: UISnapshotStoreLimits = .default
  ) throws {
    guard limits.maximumSnapshotCount > 0, limits.maximumStoredBytes > 0 else {
      throw UISnapshotRuntimeError.invalidParams(
        "Snapshot store limits must be positive"
      )
    }
    var registeredNames = Set<String>()
    var registeredProviders: [RegisteredProvider] = []
    for provider in providers {
      let descriptor = provider.descriptor
      guard descriptor.platform == .iOS else {
        throw UISnapshotRuntimeError.invalidParams(
          "iOS runtime cannot register provider \(descriptor.name) for \(descriptor.platform.rawValue)"
        )
      }
      guard
        Self.matches(
          descriptor.name,
          pattern: "^[a-z][a-z0-9]*(?:[._-][a-z0-9]+)*$",
          maximumLength: 128
        )
      else {
        throw UISnapshotRuntimeError.invalidParams(
          "Invalid provider name: \(descriptor.name)"
        )
      }
      guard registeredNames.insert(descriptor.name).inserted else {
        throw UISnapshotRuntimeError.invalidParams(
          "Duplicate provider name: \(descriptor.name)"
        )
      }
      registeredProviders.append(
        RegisteredProvider(descriptor: descriptor, adapter: provider)
      )
    }
    self.providers = registeredProviders
    self.limits = limits
  }

  private static func matches(
    _ value: String,
    pattern: String,
    maximumLength: Int
  ) -> Bool {
    guard value.count <= maximumLength,
      let match = value.range(of: pattern, options: .regularExpression)
    else {
      return false
    }
    return match == value.startIndex..<value.endIndex
  }

  public func capture(
    providers requestedProviderNames: [String]? = nil,
    in scope: UISnapshotScope
  ) async throws -> StoredUISnapshot {
    guard !scope.sessionID.isEmpty, scope.processGeneration > 0 else {
      throw UISnapshotRuntimeError.invalidParams("Invalid snapshot scope")
    }
    if let requestedProviderNames {
      guard !requestedProviderNames.isEmpty else {
        throw UISnapshotRuntimeError.invalidParams(
          "Provider selection cannot be empty"
        )
      }
      guard Set(requestedProviderNames).count == requestedProviderNames.count else {
        throw UISnapshotRuntimeError.invalidParams(
          "Provider selection contains duplicates"
        )
      }
    }
    let registeredNames = Set(providers.map(\.descriptor.name))
    let requestedProviderList = requestedProviderNames ?? providers.map(\.descriptor.name)
    let requestedNames = Set(requestedProviderList)
    if let unknownName = requestedProviderList.first(where: {
      !registeredNames.contains($0)
    }) {
      throw UISnapshotRuntimeError.invalidParams("Unknown provider: \(unknownName)")
    }
    let selectedProviders = providers.filter { requestedNames.contains($0.descriptor.name) }
    guard !selectedProviders.isEmpty else {
      throw UISnapshotRuntimeError.invalidParams("No providers are registered")
    }
    let previousCapture = captureTail
    let operation = Task { [self] in
      if let previousCapture {
        await previousCapture.value
      }
      try Task.checkCancellation()
      return try await performCapture(selectedProviders, in: scope)
    }
    captureTail = Task {
      _ = await operation.result
    }
    return try await withTaskCancellationHandler {
      try await operation.value
    } onCancel: {
      operation.cancel()
    }
  }

  private func performCapture(
    _ selectedProviders: [RegisteredProvider],
    in scope: UISnapshotScope
  ) async throws -> StoredUISnapshot {
    var providerCaptures:
      [(
        descriptor: UIProviderDescriptor,
        capture: RedactedProviderCapture
      )] = []

    for provider in selectedProviders {
      let captured: RedactedProviderCapture
      do {
        captured = try await provider.adapter.capture()
      } catch is CancellationError {
        throw CancellationError()
      } catch {
        throw UISnapshotRuntimeError.internalError(
          "Provider \(provider.descriptor.name) failed"
        )
      }
      try Task.checkCancellation()
      guard !captured.sources.isEmpty else {
        throw UISnapshotRuntimeError.internalError(
          "Provider \(provider.descriptor.name) returned an invalid capture"
        )
      }
      providerCaptures.append((provider.descriptor, captured))
    }

    var sourceIDs = Set<String>()
    for providerCapture in providerCaptures {
      do {
        for source in providerCapture.capture.sources {
          try validate(
            source: source,
            from: providerCapture.descriptor,
            sourceIDs: &sourceIDs
          )
        }
      } catch is CancellationError {
        throw CancellationError()
      } catch {
        throw UISnapshotRuntimeError.internalError(
          "Provider \(providerCapture.descriptor.name) returned an invalid capture"
        )
      }
      try Task.checkCancellation()
    }

    let generation = nextGeneration
    let token = UUID().uuidString.lowercased().replacingOccurrences(of: "-", with: "")
    let identity = UISnapshotIdentity(id: "snapshot_\(token)", generation: generation)
    var storedSources: [StoredUISource] = []
    var storedNodes: [StoredUINode] = []

    for providerCapture in providerCaptures {
      for source in providerCapture.capture.sources {
        let references = Dictionary(
          uniqueKeysWithValues: source.nodes.enumerated().map { offset, node in
            (node.id, "node_\(token)_\(storedNodes.count + offset + 1)")
          }
        )
        let sourceNodes = source.nodes.map { node in
          StoredUINode(
            reference: references[node.id]!,
            sourceID: source.id,
            parentReference: node.parentID.flatMap { references[$0] },
            childIndex: node.childIndex,
            depth: node.depth,
            childCount: node.childCount,
            index: node.index,
            native: node.native
          )
        }
        storedSources.append(
          StoredUISource(
            id: source.id,
            platform: source.platform,
            provider: source.provider,
            representation: source.representation,
            nativeSchema: source.nativeSchema,
            coordinateSpace: source.coordinateSpace,
            rootReference: sourceNodes[0].reference,
            coverage: source.coverage,
            limitations: source.limitations
          )
        )
        storedNodes.append(contentsOf: sourceNodes)
      }
    }

    try Task.checkCancellation()
    let record = StoredSnapshotRecord(
      scope: scope,
      identity: identity,
      sources: storedSources,
      nodes: storedNodes
    )
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
    let storedBytes: Int
    do {
      storedBytes = try encoder.encode(record).count
    } catch {
      throw UISnapshotRuntimeError.internalError(
        "Validated provider capture could not be encoded"
      )
    }
    try Task.checkCancellation()
    guard storedBytes <= limits.maximumStoredBytes else {
      throw UISnapshotRuntimeError.resourceExhausted(
        "Snapshot exceeds the configured byte capacity"
      )
    }
    let (updatedStoredBytes, overflowed) = totalStoredBytes.addingReportingOverflow(
      storedBytes
    )
    guard !overflowed else {
      throw UISnapshotRuntimeError.resourceExhausted(
        "Snapshot store byte accounting overflowed"
      )
    }
    let snapshot = StoredUISnapshot(
      scope: scope,
      identity: identity,
      sources: storedSources,
      nodes: storedNodes,
      storedBytes: storedBytes
    )
    try Task.checkCancellation()
    snapshots[identity] = snapshot
    snapshotOrder.append(identity)
    totalStoredBytes = updatedStoredBytes
    while snapshotOrder.count > limits.maximumSnapshotCount
      || totalStoredBytes > limits.maximumStoredBytes
    {
      let evictedIdentity = snapshotOrder.removeFirst()
      if let evicted = snapshots.removeValue(forKey: evictedIdentity) {
        totalStoredBytes -= evicted.storedBytes
      }
    }
    nextGeneration += 1
    return snapshot
  }

  public func resolve(
    _ identity: UISnapshotIdentity,
    in scope: UISnapshotScope
  ) throws -> StoredUISnapshot {
    guard let snapshot = snapshots[identity], snapshot.scope == scope else {
      throw UISnapshotRuntimeError.snapshotExpired
    }
    return snapshot
  }

  public func invalidate(scope: UISnapshotScope) {
    let invalidatedBytes = snapshots.values
      .filter { $0.scope == scope }
      .reduce(0) { $0 + $1.storedBytes }
    snapshots = snapshots.filter { $0.value.scope != scope }
    snapshotOrder.removeAll { snapshots[$0] == nil }
    totalStoredBytes -= invalidatedBytes
  }

  private func validate(
    source: RedactedSourceCapture,
    from descriptor: UIProviderDescriptor,
    sourceIDs: inout Set<String>
  ) throws {
    guard source.provider == descriptor.name else {
      throw invalidCapture(
        "Source \(source.id) does not match provider \(descriptor.name)"
      )
    }
    guard
      Self.matches(
        source.id,
        pattern: "^[a-z][a-z0-9._-]{0,63}$",
        maximumLength: 64
      )
    else {
      throw invalidCapture(
        "Invalid source ID: \(source.id)"
      )
    }
    guard
      Self.matches(
        source.nativeSchema,
        pattern: "^[a-z][a-z0-9.-]*@[1-9][0-9]*$",
        maximumLength: 128
      )
    else {
      throw invalidCapture(
        "Invalid native schema for source \(source.id)"
      )
    }
    guard descriptor.platform == .iOS, source.platform == descriptor.platform else {
      throw invalidCapture(
        "Source \(source.id) is not an iOS source"
      )
    }
    guard source.coordinateSpace.unit == .point,
      source.coordinateSpace.scale.isFinite,
      source.coordinateSpace.scale > 0
    else {
      throw invalidCapture(
        "Source \(source.id) has an invalid iOS coordinate space"
      )
    }
    guard sourceIDs.insert(source.id).inserted else {
      throw invalidCapture(
        "Duplicate source ID: \(source.id)"
      )
    }
    switch source.coverage {
    case .complete:
      guard source.limitations == nil else {
        throw invalidCapture(
          "Complete source \(source.id) cannot declare limitations"
        )
      }
    case .partial:
      guard let limitations = source.limitations, !limitations.isEmpty else {
        throw invalidCapture(
          "Partial source \(source.id) must declare limitations"
        )
      }
      guard Set(limitations).count == limitations.count,
        limitations.allSatisfy({
          !$0.isEmpty && $0.unicodeScalars.count <= 256
        })
      else {
        throw invalidCapture(
          "Source \(source.id) has invalid limitations"
        )
      }
    }
    guard !source.nodes.isEmpty else {
      throw invalidCapture(
        "Source \(source.id) returned no nodes"
      )
    }
    try validateGraph(source)
  }

  private func validateGraph(_ source: RedactedSourceCapture) throws {
    var nodeIDs = Set<String>()
    var stack: [RedactedNodeCapture] = []
    var childIndicesByParent: [String: [Int]] = [:]

    for (offset, node) in source.nodes.enumerated() {
      guard !node.id.isEmpty else {
        throw invalidGraph(source, "empty provider-local node ID")
      }
      try validatePayload(of: node, in: source)
      guard nodeIDs.insert(node.id).inserted else {
        throw invalidGraph(source, "duplicate node ID \(node.id)")
      }
      guard node.depth >= 0, node.childCount >= 0 else {
        throw invalidGraph(source, "negative depth or child count")
      }

      if node.depth == 0 {
        guard offset == 0, node.parentID == nil, node.childIndex == nil else {
          throw invalidGraph(source, "missing or extra root")
        }
        stack = [node]
        continue
      }

      guard let parentID = node.parentID,
        let childIndex = node.childIndex,
        childIndex >= 0
      else {
        throw invalidGraph(source, "non-root node is missing adjacency")
      }
      while stack.count > node.depth {
        stack.removeLast()
      }
      guard stack.count == node.depth, stack.last?.id == parentID else {
        throw invalidGraph(source, "nodes are not in native depth-first order")
      }
      let expectedChildIndex = childIndicesByParent[parentID, default: []].count
      guard childIndex == expectedChildIndex else {
        throw invalidGraph(source, "siblings are not in native child order")
      }
      childIndicesByParent[parentID, default: []].append(childIndex)
      stack.append(node)
    }

    guard source.nodes.first?.depth == 0 else {
      throw invalidGraph(source, "missing root")
    }
    for node in source.nodes {
      let capturedChildCount = childIndicesByParent[node.id, default: []].count
      guard capturedChildCount == node.childCount else {
        throw invalidGraph(source, "child count does not match captured nodes")
      }
    }
  }

  private func validatePayload(
    of node: RedactedNodeCapture,
    in source: RedactedSourceCapture
  ) throws {
    if let index = node.index {
      let hasValue =
        index.identifier != nil
        || index.text != nil
        || index.className != nil
        || index.typeName != nil
        || index.traits != nil
        || index.frame != nil
        || index.visible != nil
        || index.interactive != nil
      guard hasValue,
        Self.valid(index.identifier, minimumLength: 1, maximumLength: 512),
        Self.valid(index.text, minimumLength: 0, maximumLength: 4096),
        Self.valid(index.className, minimumLength: 1, maximumLength: 512),
        Self.valid(index.typeName, minimumLength: 1, maximumLength: 512)
      else {
        throw invalidGraph(source, "invalid node index")
      }
      if let traits = index.traits {
        guard Set(traits).count == traits.count,
          traits.allSatisfy({
            !$0.isEmpty && $0.unicodeScalars.count <= 128
          })
        else {
          throw invalidGraph(source, "invalid node traits")
        }
      }
      if let frame = index.frame {
        guard frame.x.isFinite,
          frame.y.isFinite,
          frame.width.isFinite,
          frame.height.isFinite,
          frame.width >= 0,
          frame.height >= 0
        else {
          throw invalidGraph(source, "invalid node frame")
        }
      }
    }
    if let native = node.native,
      !native.values.allSatisfy(Self.isJSONSafe)
    {
      throw invalidGraph(source, "native payload is not JSON-safe")
    }
  }

  private static func valid(
    _ value: String?,
    minimumLength: Int,
    maximumLength: Int
  ) -> Bool {
    guard let value else { return true }
    let length = value.unicodeScalars.count
    return length >= minimumLength && length <= maximumLength
  }

  private static func isJSONSafe(_ value: JSONValue) -> Bool {
    switch value {
    case .null, .bool, .integer, .unsignedInteger, .string:
      true
    case .number(let number):
      number.isFinite
    case .array(let values):
      values.allSatisfy(isJSONSafe)
    case .object(let values):
      values.values.allSatisfy(isJSONSafe)
    }
  }

  private func invalidGraph(
    _ source: RedactedSourceCapture,
    _ reason: String
  ) -> ProviderCaptureValidationError {
    invalidCapture("Invalid graph for source \(source.id): \(reason)")
  }

  private func invalidCapture(_ reason: String) -> ProviderCaptureValidationError {
    ProviderCaptureValidationError(reason: reason)
  }
}
