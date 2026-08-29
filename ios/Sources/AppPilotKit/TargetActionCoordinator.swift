import Foundation

public struct TargetActionContext: Equatable, Sendable {
  public let targetID: String
  public let processGeneration: UInt64
  public let sessionID: String

  public init(targetID: String, processGeneration: UInt64, sessionID: String) {
    self.targetID = targetID
    self.processGeneration = processGeneration
    self.sessionID = sessionID
  }
}

public struct DestructiveGrantCheck: Equatable, Sendable {
  public let binding: CanonicalActionBinding
  public let grant: String
}

public typealias DestructiveAuthorizationRequest = DestructiveGrantCheck

public struct ActionPolicySubject: Equatable, Sendable {
  public let id: String
  public let declaredAuthorization: SemanticActionAuthorization
  public let retrySafety: SemanticActionRetrySafety

  public init(
    id: String,
    declaredAuthorization: SemanticActionAuthorization,
    retrySafety: SemanticActionRetrySafety
  ) {
    self.id = id
    self.declaredAuthorization = declaredAuthorization
    self.retrySafety = retrySafety
  }
}

/// Target-owned evidence lifecycle. There is deliberately no no-op default.
public protocol ActionEvidencePort: Sendable {
  func captureBefore(context: TargetActionContext) async throws
  func observeStability(context: TargetActionContext) async throws
  func captureAfter(context: TargetActionContext) async throws
}

/// App-owned gates used to resolve the one Effective Action Policy and consume
/// a destructive grant bound to the exact Target and detached input.
public struct TargetActionPolicy: Sendable {
  public let resolve: @Sendable (
    TargetActionContext,
    ActionPolicySubject
  ) async throws -> SemanticActionPolicy?
  public let validateDestructive: @Sendable (
    DestructiveGrantCheck
  ) async throws -> Bool
  /// Atomically revalidates every grant binding and expiry, then consumes it at most once.
  public let consumeDestructive: @Sendable (
    DestructiveGrantCheck
  ) async throws -> Bool

  public init(
    resolve: @escaping @Sendable (
      TargetActionContext,
      ActionPolicySubject
    ) async throws -> SemanticActionPolicy?,
    validateDestructive: @escaping @Sendable (
      DestructiveGrantCheck
    ) async throws -> Bool,
    consumeDestructive: @escaping @Sendable (
      DestructiveGrantCheck
    ) async throws -> Bool
  ) {
    self.resolve = resolve
    self.validateDestructive = validateDestructive
    self.consumeDestructive = consumeDestructive
  }
}

final class DispatchAuthority: @unchecked Sendable {
  fileprivate init() {}
}

final class DispatchClaim: @unchecked Sendable {
  private let lock = NSLock()
  private var handedOff = false
  private let handler: @Sendable () async throws -> Void

  init(
    handler: @escaping @Sendable () async throws -> Void,
    authority: DispatchAuthority
  ) {
    self.handler = handler
    _ = authority
  }

  func handoff() async throws {
    let acquired = lock.withLock {
      guard !handedOff else { return false }
      handedOff = true
      return true
    }
    guard acquired else { throw TargetActionCoordinatorError.outcomeUnknown }
    try await handler()
  }
}

public enum TargetActionCoordinatorError: Error, Equatable, Sendable {
  case policyDenied
  case conflict
  case outcomeUnknown
  case sessionExpired
  case preDispatchFailed
}

/// The sole owner of Semantic Action preparation/dispatch and Target mutation serialization.
public final class TargetActionCoordinator: @unchecked Sendable {
  private let catalog: SemanticCatalog
  private let targetID: String
  private let evidence: any ActionEvidencePort
  private let policy: TargetActionPolicy

  public init(
    catalog: SemanticCatalog,
    targetID: String,
    evidence: any ActionEvidencePort,
    policy: TargetActionPolicy
  ) {
    self.catalog = catalog
    self.targetID = targetID
    self.evidence = evidence
    self.policy = policy
  }

  func invoke(
    _ request: SemanticActionInvocation,
    authorizationGrant: String?,
    session: SemanticProtocolSessionContext,
    sessionIsActive: @escaping @Sendable () async -> Bool
  ) async throws {
    try await ensureLive(sessionIsActive)
    let context = TargetActionContext(
      targetID: targetID,
      processGeneration: session.generation,
      sessionID: session.id
    )
    let validated = try await catalog.validateAction(request, context: context)
    let subject = try policySubject(for: validated.declaration)
    let effective = try await resolveEffective(context: context, subject: subject)
    guard matches(effective, declaration: validated.declaration) else {
      throw TargetActionCoordinatorError.policyDenied
    }
    let check = try await readOnlyGrant(
      effective: effective,
      binding: validated.binding,
      grant: authorizationGrant
    )
    try await performValidatedMutation(
      context: context,
      sessionIsActive: sessionIsActive,
      authorization: check
    ) {
      let claim = catalog.claimDispatch(validated, authority: DispatchAuthority())
      try await claim.handoff()
    }
  }

  /// Internal ordinary-mutation seam. It shares policy, evidence, handoff ambiguity,
  /// and the same non-queueing writer with Semantic Actions.
  func invokeOrdinary<T: Sendable>(
    subject: ActionPolicySubject,
    authorizationGrant: String?,
    context: TargetActionContext,
    sessionIsActive: @escaping @Sendable () async -> Bool,
    body: @escaping @Sendable () async throws -> T
  ) async throws -> T {
    guard context.targetID == targetID else {
      throw TargetActionCoordinatorError.sessionExpired
    }
    try await ensureLive(sessionIsActive)
    let effective = try await resolveEffective(context: context, subject: subject)
    let check = try await readOnlyGrant(
      effective: effective,
      binding: ordinaryBinding(context: context, subject: subject),
      grant: authorizationGrant
    )
    return try await performValidatedMutation(
      context: context,
      sessionIsActive: sessionIsActive,
      authorization: check
    ) {
      let box = OnceValue<T>()
      let claim = DispatchClaim(
        handler: {
          box.value = try await body()
        },
        authority: DispatchAuthority()
      )
      try await claim.handoff()
      guard let value = box.value else {
        throw TargetActionCoordinatorError.outcomeUnknown
      }
      return value
    }
  }

  private func performValidatedMutation<T: Sendable>(
    context: TargetActionContext,
    sessionIsActive: @escaping @Sendable () async -> Bool,
    authorization: DestructiveGrantCheck?,
    handoff: @Sendable () async throws -> T
  ) async throws -> T {
    guard TargetWriterTable.shared.tryAcquire(targetID) else {
      throw TargetActionCoordinatorError.conflict
    }
    defer { TargetWriterTable.shared.release(targetID) }

    try await ensureLive(sessionIsActive)
    if let authorization {
      let consumed: Bool
      do {
        consumed = try await policy.consumeDestructive(authorization)
      } catch {
        throw TargetActionCoordinatorError.preDispatchFailed
      }
      guard consumed else { throw TargetActionCoordinatorError.policyDenied }
    }
    try await ensureLive(sessionIsActive)
    do {
      try await evidence.captureBefore(context: context)
    } catch {
      throw TargetActionCoordinatorError.preDispatchFailed
    }
    try await ensureLive(sessionIsActive)

    // Past this handoff, no thrown outcome is Retry Proof.
    let result: T
    do {
      result = try await handoff()
    } catch {
      throw TargetActionCoordinatorError.outcomeUnknown
    }
    guard await sessionIsActive() else {
      throw TargetActionCoordinatorError.outcomeUnknown
    }
    do {
      try await evidence.observeStability(context: context)
    } catch {
      throw TargetActionCoordinatorError.outcomeUnknown
    }
    guard await sessionIsActive() else {
      throw TargetActionCoordinatorError.outcomeUnknown
    }
    do {
      try await evidence.captureAfter(context: context)
    } catch {
      throw TargetActionCoordinatorError.outcomeUnknown
    }
    guard await sessionIsActive() else {
      throw TargetActionCoordinatorError.outcomeUnknown
    }
    return result
  }

  private func resolveEffective(
    context: TargetActionContext,
    subject: ActionPolicySubject
  ) async throws -> SemanticActionPolicy {
    let effective: SemanticActionPolicy
    do {
      guard let resolved = try await policy.resolve(context, subject) else {
        throw TargetActionCoordinatorError.policyDenied
      }
      effective = resolved
    } catch let error as TargetActionCoordinatorError {
      throw error
    } catch {
      throw TargetActionCoordinatorError.policyDenied
    }
    guard
      effective.authorization == subject.declaredAuthorization,
      effective.retrySafety == subject.retrySafety
    else {
      throw TargetActionCoordinatorError.policyDenied
    }
    return effective
  }

  private func readOnlyGrant(
    effective: SemanticActionPolicy,
    binding: CanonicalActionBinding,
    grant: String?
  ) async throws -> DestructiveGrantCheck? {
    guard effective.authorization == .destructiveAuthorization else { return nil }
    guard let grant, !grant.isEmpty else {
      throw TargetActionCoordinatorError.policyDenied
    }
    let candidate = DestructiveGrantCheck(binding: binding, grant: grant)
    let authorized: Bool
    do {
      authorized = try await policy.validateDestructive(candidate)
    } catch {
      throw TargetActionCoordinatorError.policyDenied
    }
    guard authorized else { throw TargetActionCoordinatorError.policyDenied }
    return candidate
  }

  private func ordinaryBinding(
    context: TargetActionContext,
    subject: ActionPolicySubject
  ) throws -> CanonicalActionBinding {
    let digest = try SemanticCatalog.canonicalDigest(for: .object([:]))
    return CanonicalActionBinding(
      targetID: context.targetID,
      processGeneration: context.processGeneration,
      sessionID: context.sessionID,
      capability: subject.id,
      declarationRevision: 1,
      inputSchema: SemanticSchemaHandle(id: "schema_ordinary01", revision: 1, digest: digest),
      inputDigest: digest
    )
  }

  private func policySubject(for declaration: SemanticCapabilityDeclaration) throws -> ActionPolicySubject {
    guard let policy = declaration.actionPolicy else { throw TargetActionCoordinatorError.policyDenied }
    return ActionPolicySubject(
      id: declaration.id,
      declaredAuthorization: policy.authorization,
      retrySafety: policy.retrySafety
    )
  }

  private func matches(_ effective: SemanticActionPolicy, declaration: SemanticCapabilityDeclaration) -> Bool {
    guard let declared = declaration.actionPolicy else { return false }
    return effective.authorization == declared.authorization && effective.retrySafety == declared.retrySafety
  }

  private func ensureLive(_ sessionIsActive: @escaping @Sendable () async -> Bool) async throws {
    guard await sessionIsActive() else {
      throw TargetActionCoordinatorError.sessionExpired
    }
    do {
      try Task.checkCancellation()
    } catch {
      throw TargetActionCoordinatorError.preDispatchFailed
    }
  }
}

private final class TargetWriterTable: @unchecked Sendable {
  static let shared = TargetWriterTable()
  private let lock = NSLock()
  private var busy: Set<String> = []

  func tryAcquire(_ targetID: String) -> Bool {
    lock.lock()
    defer { lock.unlock() }
    guard !busy.contains(targetID) else { return false }
    busy.insert(targetID)
    return true
  }

  func release(_ targetID: String) {
    lock.lock()
    busy.remove(targetID)
    lock.unlock()
  }
}

private final class OnceValue<T: Sendable>: @unchecked Sendable {
  var value: T?
}
