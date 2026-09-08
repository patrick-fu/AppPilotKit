import AppPilotKit
import CAppPilotKitTargetTransport
import Foundation

@_spi(AppPilotKitTargetTransportInternal)
public enum TargetTransportInternalError: Error, Equatable, Sendable {
  case unavailableInBuildConfiguration
  case ineligibleLifecycle
  case invalidDescriptor
  case unsupportedPlatform
  case listenerFailed
  case runtimeCompositionFailed
  case ffiFailure(Int32)
  case invariantViolation
}

@_spi(AppPilotKitTargetTransportInternal)
public struct TargetRuntimeComposition: Sendable {
  public let catalog: SemanticCatalog
  public let limits: SemanticProtocolLimits
  public let policy: SemanticProtocolPolicy
  public let actionCoordinator: TargetActionCoordinator

  public init(
    catalog: SemanticCatalog,
    limits: SemanticProtocolLimits,
    policy: SemanticProtocolPolicy,
    actionCoordinator: TargetActionCoordinator,
    processGeneration: UInt64
  ) throws {
    guard catalog.identity.generation == processGeneration else {
      throw TargetTransportInternalError.runtimeCompositionFailed
    }
    self.catalog = catalog
    self.limits = limits
    self.policy = policy
    self.actionCoordinator = actionCoordinator
  }

  func makeRuntime() -> SemanticProtocolRuntime {
    SemanticProtocolRuntime(
      catalog: catalog,
      limits: limits,
      policy: policy,
      actionCoordinator: actionCoordinator
    )
  }
}

@_spi(AppPilotKitTargetTransportInternal)
public typealias TargetRuntimeCompositionFactory = @Sendable (UInt64) throws -> TargetRuntimeComposition

struct SupervisorEvent: Sendable {
  let tag: UInt32
  let streamID: UInt64
  let writeToken: UInt64
  var bytes: Data

  init(tag: UInt32, streamID: UInt64 = 0, writeToken: UInt64 = 0, bytes: Data = Data()) {
    self.tag = tag
    self.streamID = streamID
    self.writeToken = writeToken
    self.bytes = bytes
  }
}

struct SupervisorOutcome: Sendable {
  let kind: UInt32
  let flags: UInt32
  let streamID: UInt64
  let writeToken: UInt64
  var bytes: Data?
  let value0: UInt64
  let value1: UInt64
  let nextDeadlineMilliseconds: UInt64
  let closeReason: UInt32
  let handoffState: UInt32
  let peerCloseReason: UInt32?
  let peerHandoffState: UInt32?

  var deadlineToken: UInt64? {
    if flags & UInt32(APK_TP_OUTCOME_FLAG_DEADLINE_TOKEN_VALUE0) != 0 {
      return value0
    }
    if flags & UInt32(APK_TP_OUTCOME_FLAG_DEADLINE_TOKEN_WRITE_TOKEN) != 0 {
      return writeToken
    }
    return nil
  }
}

protocol TargetTransportSupervising: AnyObject, Sendable {
  var initialOutcome: SupervisorOutcome { get }
  func drive(_ event: SupervisorEvent) throws -> SupervisorOutcome
  func close() throws -> SupervisorOutcome
}

enum SocketEvent: Sendable {
  case accepted(UInt64)
  case received(UInt64, Data, end: Bool, failed: Bool)
  case writeCompleted(UInt64, UInt64, failed: Bool)
  case listenerFailed
}

protocol TargetSocketHosting: AnyObject, Sendable {
  func start(port: UInt16, handler: @escaping @Sendable (SocketEvent) -> Void) async throws
  func receive(streamID: UInt64)
  func send(streamID: UInt64, writeToken: UInt64, bytes: Data)
  func close(streamID: UInt64)
  func stop()
}
