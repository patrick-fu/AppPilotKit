@_spi(AppPilotKitTargetTransportInternal) @testable import AppPilotKitTargetTransportInternal
import AppPilotKit
import CAppPilotKitTargetTransport
import Foundation
import XCTest

final class TargetTransportActorTests: XCTestCase {
  func testActorDrivesRealCABIThroughProductionBrokerExactlyOnce() async throws {
    let broker = try TestBroker()
    let supervisor = try RustTargetTransportSupervisor(descriptor: broker.descriptor)
    let sockets = BrokerSocketHost(broker: broker, request: sessionOpenRequest(generation: 0))
    let transport = AppPilotKitTargetTransport(
      supervisor: supervisor,
      sockets: sockets,
      compositionFactory: { try makeComposition(generation: $0) },
      initialOutcome: supervisor.initialOutcome
    )
    try await transport.activate()
    try await eventually { sockets.responseSnapshot() != nil }

    let response = try XCTUnwrap(sockets.responseSnapshot())
    let context = try sessionContext(from: response)
    XCTAssertEqual(
      (context["generation"] as? NSNumber)?.uint64Value,
      sockets.processGenerationSnapshot()
    )
    XCTAssertEqual(sockets.responseCountSnapshot(), 1)
    await transport.eligibilityLost()
    try await eventually { sockets.stopCountSnapshot() == 1 }
  }

  func testWriteTokensRuntimeHandoffAndEligibilityAreSerializedOnce() async throws {
    let request = sessionOpenRequest(generation: 42)
    let supervisor = ScriptedSupervisor(application: request)
    let sockets = TestSocketHost()
    let transport = AppPilotKitTargetTransport(
      supervisor: supervisor,
      sockets: sockets,
      compositionFactory: { try makeComposition(generation: $0) },
      initialOutcome: supervisor.initialOutcome
    )
    try await transport.activate()
    XCTAssertEqual(sockets.startedPort, 55_001)

    sockets.emit(.accepted(1))
    try await eventually { sockets.sentTokens == [10] }
    sockets.emit(.writeCompleted(1, 10, failed: false))
    try await eventually { supervisor.hasEvent(UInt32(APK_TP_EVENT_FULL_WRITE_COMMITTED), token: 10) }
    try await eventually { sockets.receiveRequests.contains(1) }

    sockets.emit(.accepted(2))
    try await eventually { sockets.sentTokens == [10, 20] }
    sockets.emit(.writeCompleted(2, 20, failed: false))
    try await eventually { sockets.receiveRequests.contains(2) }
    sockets.emit(.received(2, Data([0x01]), end: false, failed: false))

    try await eventually { sockets.sentTokens == [10, 20, 30] }
    XCTAssertEqual(supervisor.runtimeResponseCount, 1)
    let response = try XCTUnwrap(supervisor.runtimeResponse)
    let json = try XCTUnwrap(
      JSONSerialization.jsonObject(with: response) as? [String: Any]
    )
    let result = try XCTUnwrap(json["result"] as? [String: Any])
    let context = try XCTUnwrap(result["context"] as? [String: Any])
    XCTAssertEqual((context["generation"] as? NSNumber)?.uint64Value, 42)

    sockets.emit(.writeCompleted(2, 30, failed: false))
    try await eventually { supervisor.hasEvent(UInt32(APK_TP_EVENT_FULL_WRITE_COMMITTED), token: 30) }
    await transport.eligibilityLost()
    try await eventually { sockets.stopCount == 1 }
    XCTAssertTrue(supervisor.hasEvent(UInt32(APK_TP_EVENT_ELIGIBILITY_LOST), token: 0))
    XCTAssertEqual(supervisor.runtimeResponseCount, 1)
  }

  func testWrongWriteCompletionCannotAdvanceSupervisor() async throws {
    let supervisor = ScriptedSupervisor(application: sessionOpenRequest(generation: 42))
    let sockets = TestSocketHost()
    let transport = AppPilotKitTargetTransport(
      supervisor: supervisor,
      sockets: sockets,
      compositionFactory: { try makeComposition(generation: $0) },
      initialOutcome: supervisor.initialOutcome
    )
    try await transport.activate()
    sockets.emit(.accepted(1))
    try await eventually { sockets.sentTokens == [10] }
    sockets.emit(.writeCompleted(1, 999, failed: false))
    try await Task.sleep(for: .milliseconds(30))
    XCTAssertFalse(supervisor.hasEvent(UInt32(APK_TP_EVENT_FULL_WRITE_COMMITTED), token: 999))
    await transport.stop()
  }

  func testListenerFailureUsesAcceptedInternalErrorTerminal() async throws {
    let supervisor = ScriptedSupervisor(application: sessionOpenRequest(generation: 42))
    let sockets = TestSocketHost()
    let transport = AppPilotKitTargetTransport(
      supervisor: supervisor,
      sockets: sockets,
      compositionFactory: { try makeComposition(generation: $0) },
      initialOutcome: supervisor.initialOutcome
    )
    try await transport.activate()
    sockets.emit(.listenerFailed)
    try await eventually { sockets.stopCount == 1 }
    XCTAssertTrue(supervisor.hasEvent(UInt32(APK_TP_EVENT_INTERNAL_ERROR), token: 0))
  }

  func testDeadlineFlagsSelectExactOneShotTokenLocation() {
    let valueToken = SupervisorOutcome(
      kind: UInt32(APK_TP_OUTCOME_WRITE_FRAMES),
      flags: UInt32(APK_TP_OUTCOME_FLAG_DEADLINE_TOKEN_VALUE0),
      streamID: 1,
      writeToken: 9,
      bytes: Data([1]),
      value0: 10,
      value1: 0,
      nextDeadlineMilliseconds: 2_000,
      closeReason: 0,
      handoffState: 0,
      peerCloseReason: nil,
      peerHandoffState: nil
    )
    XCTAssertEqual(valueToken.deadlineToken, 10)

    let writeToken = SupervisorOutcome(
      kind: UInt32(APK_TP_OUTCOME_NEED_INPUT),
      flags: UInt32(APK_TP_OUTCOME_FLAG_DEADLINE_TOKEN_WRITE_TOKEN),
      streamID: 1,
      writeToken: 11,
      bytes: nil,
      value0: 0,
      value1: 0,
      nextDeadlineMilliseconds: 30_000,
      closeReason: 0,
      handoffState: 0,
      peerCloseReason: nil,
      peerHandoffState: nil
    )
    XCTAssertEqual(writeToken.deadlineToken, 11)
  }

  func testSessionTerminalInvalidatesOnlyItsInFlightRuntimeBeforeDispatch() async throws {
    let gate = PolicyGate()
    let handlerCount = HandlerCount()
    let supervisor = ScriptedSupervisor(application: sessionOpenRequest(generation: 42))
    let sockets = TestSocketHost()
    let transport = AppPilotKitTargetTransport(
      supervisor: supervisor,
      sockets: sockets,
      compositionFactory: {
        try makeBlockingComposition(generation: $0, gate: gate, handlerCount: handlerCount)
      },
      initialOutcome: supervisor.initialOutcome
    )
    try await transport.activate()
    sockets.emit(.accepted(1))
    try await eventually { sockets.sentTokens == [10] }
    sockets.emit(.writeCompleted(1, 10, failed: false))
    try await eventually { sockets.receiveRequests.contains(1) }
    sockets.emit(.accepted(2))
    try await eventually { sockets.sentTokens == [10, 20] }
    sockets.emit(.writeCompleted(2, 20, failed: false))
    try await eventually { sockets.receiveCount(streamID: 2) == 1 }
    sockets.emit(.received(2, Data([0x01]), end: false, failed: false))
    try await eventually { sockets.sentTokens == [10, 20, 30] }

    let openResponse = try XCTUnwrap(supervisor.runtimeResponseSnapshot())
    let context = try sessionContext(from: openResponse)
    supervisor.setApplication(try queryRequest(context: context))
    sockets.emit(.writeCompleted(2, 30, failed: false))
    try await eventually { sockets.receiveCount(streamID: 2) == 2 }
    sockets.emit(.received(2, Data([0x02]), end: false, failed: false))
    try await waitUntil { await gate.entered }

    sockets.emit(.received(2, Data(), end: true, failed: false))
    try await eventually {
      supervisor.hasEvent(UInt32(APK_TP_EVENT_STREAM_EOF), token: 0)
    }
    await gate.release()
    try await Task.sleep(for: .milliseconds(30))
    let dispatchedCount = await handlerCount.value
    XCTAssertEqual(dispatchedCount, 0)
    XCTAssertEqual(supervisor.runtimeResponseCountSnapshot(), 1)
    await transport.stop()
  }

  func testEligibilityLossInvalidatesInFlightRuntimeBeforeDispatch() async throws {
    let gate = PolicyGate()
    let handlerCount = HandlerCount()
    let supervisor = ScriptedSupervisor(application: sessionOpenRequest(generation: 42))
    let sockets = TestSocketHost()
    let transport = AppPilotKitTargetTransport(
      supervisor: supervisor,
      sockets: sockets,
      compositionFactory: {
        try makeBlockingComposition(generation: $0, gate: gate, handlerCount: handlerCount)
      },
      initialOutcome: supervisor.initialOutcome
    )
    try await transport.activate()
    sockets.emit(.accepted(1))
    try await eventually { sockets.sentTokens == [10] }
    sockets.emit(.writeCompleted(1, 10, failed: false))
    try await eventually { sockets.receiveRequests.contains(1) }
    sockets.emit(.accepted(2))
    try await eventually { sockets.sentTokens == [10, 20] }
    sockets.emit(.writeCompleted(2, 20, failed: false))
    try await eventually { sockets.receiveCount(streamID: 2) == 1 }
    sockets.emit(.received(2, Data([0x01]), end: false, failed: false))
    try await eventually { sockets.sentTokens == [10, 20, 30] }

    let context = try sessionContext(from: try XCTUnwrap(supervisor.runtimeResponseSnapshot()))
    supervisor.setApplication(try queryRequest(context: context))
    sockets.emit(.writeCompleted(2, 30, failed: false))
    try await eventually { sockets.receiveCount(streamID: 2) == 2 }
    sockets.emit(.received(2, Data([0x02]), end: false, failed: false))
    try await waitUntil { await gate.entered }

    let eligibility = Task { await transport.eligibilityLost() }
    try await eventually {
      supervisor.hasEvent(UInt32(APK_TP_EVENT_ELIGIBILITY_LOST), token: 0)
    }
    await gate.release()
    await eligibility.value
    try await Task.sleep(for: .milliseconds(30))
    let dispatchedCount = await handlerCount.value
    XCTAssertEqual(dispatchedCount, 0)
    XCTAssertEqual(supervisor.runtimeResponseCountSnapshot(), 1)
    XCTAssertEqual(sockets.stopCount, 1)
  }

  private func eventually(
    timeout: Duration = .seconds(2),
    _ predicate: @escaping @Sendable () -> Bool
  ) async throws {
    let clock = ContinuousClock()
    let deadline = clock.now.advanced(by: timeout)
    while !predicate() {
      guard clock.now < deadline else { throw TargetTransportInternalError.invariantViolation }
      try await Task.sleep(for: .milliseconds(5))
    }
  }

  private func waitUntil(
    timeout: Duration = .seconds(2),
    _ predicate: @escaping @Sendable () async -> Bool
  ) async throws {
    let clock = ContinuousClock()
    let deadline = clock.now.advanced(by: timeout)
    while !(await predicate()) {
      guard clock.now < deadline else { throw TargetTransportInternalError.invariantViolation }
      try await Task.sleep(for: .milliseconds(5))
    }
  }

  private func sessionContext(from response: Data) throws -> [String: Any] {
    let object = try XCTUnwrap(
      JSONSerialization.jsonObject(with: response) as? [String: Any]
    )
    let result = try XCTUnwrap(object["result"] as? [String: Any])
    return try XCTUnwrap(result["context"] as? [String: Any])
  }
}

private final class ScriptedSupervisor: TargetTransportSupervising, @unchecked Sendable {
  private let lock = NSLock()
  private var phase = 0
  private var events: [SupervisorEvent] = []
  private var application: Data
  private(set) var runtimeResponse: Data?
  private(set) var runtimeResponseCount = 0

  let initialOutcome = SupervisorOutcome(
    kind: UInt32(APK_TP_OUTCOME_ENDPOINT_READY),
    flags: 0,
    streamID: 0,
    writeToken: 0,
    bytes: nil,
    value0: 0,
    value1: 55_001,
    nextDeadlineMilliseconds: 0,
    closeReason: 0,
    handoffState: 0,
    peerCloseReason: nil,
    peerHandoffState: nil
  )

  init(application: Data) {
    self.application = application
  }

  func drive(_ event: SupervisorEvent) throws -> SupervisorOutcome {
    lock.lock()
    defer { lock.unlock() }
    events.append(event)
    switch event.tag {
    case UInt32(APK_TP_EVENT_BOOTSTRAP_CONNECTED):
      phase = 1
      return write(stream: event.streamID, token: 10)
    case UInt32(APK_TP_EVENT_FULL_WRITE_COMMITTED) where event.writeToken == 10:
      phase = 2
      return outcome(kind: UInt32(APK_TP_OUTCOME_LEASE_READY), stream: event.streamID, value0: 42, value1: 1)
    case UInt32(APK_TP_EVENT_SESSION_ACCEPTED) where phase >= 2:
      phase = 3
      return write(stream: event.streamID, token: 20)
    case UInt32(APK_TP_EVENT_FULL_WRITE_COMMITTED) where event.writeToken == 20:
      phase = 4
      return outcome(kind: UInt32(APK_TP_OUTCOME_NEED_INPUT), stream: event.streamID)
    case UInt32(APK_TP_EVENT_STREAM_BYTES) where phase == 4:
      phase = 5
      return outcome(kind: UInt32(APK_TP_OUTCOME_APPLICATION), stream: event.streamID, bytes: application)
    case UInt32(APK_TP_EVENT_RUNTIME_RESPONSE) where phase == 5:
      runtimeResponse = event.bytes
      runtimeResponseCount += 1
      phase = 6
      return write(stream: event.streamID, token: 30)
    case UInt32(APK_TP_EVENT_FULL_WRITE_COMMITTED) where event.writeToken == 30:
      phase = 7
      return outcome(kind: UInt32(APK_TP_OUTCOME_NEED_INPUT), stream: event.streamID)
    case UInt32(APK_TP_EVENT_STREAM_BYTES) where phase == 7:
      phase = 5
      return outcome(kind: UInt32(APK_TP_OUTCOME_APPLICATION), stream: event.streamID, bytes: application)
    case UInt32(APK_TP_EVENT_STREAM_EOF) where phase == 5:
      phase = 10
      return outcome(
        kind: UInt32(APK_TP_OUTCOME_SESSION_TERMINAL),
        stream: event.streamID,
        closeReason: UInt32(APK_TP_CLOSE_PEER_CLOSED)
      )
    case UInt32(APK_TP_EVENT_ELIGIBILITY_LOST):
      phase = 8
      return outcome(
        kind: UInt32(APK_TP_OUTCOME_LEASE_TERMINAL),
        closeReason: UInt32(APK_TP_CLOSE_ELIGIBILITY_LOST)
      )
    case UInt32(APK_TP_EVENT_INTERNAL_ERROR):
      phase = 9
      return outcome(
        kind: UInt32(APK_TP_OUTCOME_LEASE_TERMINAL),
        closeReason: UInt32(APK_TP_CLOSE_INTERNAL_ERROR)
      )
    default:
      throw TargetTransportInternalError.invariantViolation
    }
  }

  func close() throws -> SupervisorOutcome {
    lock.withLock { outcome(kind: UInt32(APK_TP_OUTCOME_CLOSED)) }
  }

  func hasEvent(_ tag: UInt32, token: UInt64) -> Bool {
    lock.withLock { events.contains { $0.tag == tag && $0.writeToken == token } }
  }

  func setApplication(_ bytes: Data) {
    lock.withLock { application = bytes }
  }

  func runtimeResponseSnapshot() -> Data? {
    lock.withLock { runtimeResponse }
  }

  func runtimeResponseCountSnapshot() -> Int {
    lock.withLock { runtimeResponseCount }
  }

  private func write(stream: UInt64, token: UInt64) -> SupervisorOutcome {
    outcome(
      kind: UInt32(APK_TP_OUTCOME_WRITE_FRAMES),
      stream: stream,
      writeToken: token,
      bytes: Data([0, 1, 2])
    )
  }

  private func outcome(
    kind: UInt32,
    stream: UInt64 = 0,
    writeToken: UInt64 = 0,
    bytes: Data? = nil,
    value0: UInt64 = 0,
    value1: UInt64 = 0,
    closeReason: UInt32 = 0
  ) -> SupervisorOutcome {
    SupervisorOutcome(
      kind: kind,
      flags: 0,
      streamID: stream,
      writeToken: writeToken,
      bytes: bytes,
      value0: value0,
      value1: value1,
      nextDeadlineMilliseconds: 0,
      closeReason: closeReason,
      handoffState: 0,
      peerCloseReason: nil,
      peerHandoffState: nil
    )
  }
}

private final class TestSocketHost: TargetSocketHosting, @unchecked Sendable {
  private let lock = NSLock()
  private var handler: (@Sendable (SocketEvent) -> Void)?
  private(set) var startedPort: UInt16?
  private(set) var sentTokens: [UInt64] = []
  private(set) var receiveRequests: [UInt64] = []
  private(set) var stopCount = 0

  func start(port: UInt16, handler: @escaping @Sendable (SocketEvent) -> Void) async throws {
    lock.withLock {
      startedPort = port
      self.handler = handler
    }
  }

  func receive(streamID: UInt64) {
    lock.withLock { receiveRequests.append(streamID) }
  }

  func send(streamID: UInt64, writeToken: UInt64, bytes: Data) {
    lock.withLock { sentTokens.append(writeToken) }
    _ = streamID
    _ = bytes
  }

  func close(streamID: UInt64) {
    _ = streamID
  }

  func stop() {
    lock.withLock {
      stopCount += 1
      handler = nil
    }
  }

  func emit(_ event: SocketEvent) {
    let callback = lock.withLock { handler }
    callback?(event)
  }

  func receiveCount(streamID: UInt64) -> Int {
    lock.withLock { receiveRequests.filter { $0 == streamID }.count }
  }
}

private final class BrokerSocketHost: TargetSocketHosting, @unchecked Sendable {
  private let lock = NSLock()
  private let broker: TestBroker
  private let request: Data
  private var handler: (@Sendable (SocketEvent) -> Void)?
  private var pending: [UInt64: Data] = [:]
  private var outboundStage = 0
  private var openSent = false
  private var response: Data?
  private var responseCount = 0
  private var processGeneration: UInt64?
  private var stopCount = 0

  init(broker: TestBroker, request: Data) {
    self.broker = broker
    self.request = request
  }

  func start(port: UInt16, handler: @escaping @Sendable (SocketEvent) -> Void) async throws {
    guard port == 55_001 else { throw TargetTransportInternalError.listenerFailed }
    lock.withLock { self.handler = handler }
    handler(.accepted(1))
  }

  func receive(streamID: UInt64) {
    let action: (@Sendable (SocketEvent) -> Void, SocketEvent)? = lock.withLock {
      guard let handler else { return nil }
      if pending[streamID]?.isEmpty != false,
        streamID == 2, outboundStage == 4, !openSent
      {
        do {
          pending[streamID] = try broker.sessionOpen(request)
          openSent = true
        } catch {
          return (handler, .received(streamID, Data(), end: false, failed: true))
        }
      }
      guard var bytes = pending[streamID], !bytes.isEmpty else {
        if streamID == 1, outboundStage == 2 {
          return (handler, .accepted(2))
        }
        return nil
      }
      let byte = bytes.removeFirst()
      pending[streamID] = bytes
      return (handler, .received(streamID, Data([byte]), end: false, failed: false))
    }
    if let action { action.0(action.1) }
  }

  func send(streamID: UInt64, writeToken: UInt64, bytes: Data) {
    let action: (@Sendable (SocketEvent) -> Void, SocketEvent)? = lock.withLock {
      guard let handler else { return nil }
      do {
        outboundStage += 1
        switch outboundStage {
        case 1:
          pending[streamID] = try broker.bootstrapM1(bytes)
        case 2:
          try broker.bootstrapAck(bytes)
        case 3:
          pending[streamID] = try broker.sessionM1(bytes)
        case 4:
          pending[streamID] = try broker.targetFinished(bytes)
        case 5:
          response = try broker.sessionResponse(bytes)
          responseCount += 1
          if let object = try JSONSerialization.jsonObject(with: response!) as? [String: Any],
            let result = object["result"] as? [String: Any],
            let context = result["context"] as? [String: Any]
          {
            processGeneration = (context["generation"] as? NSNumber)?.uint64Value
          }
        default:
          return (handler, .writeCompleted(streamID, writeToken, failed: true))
        }
        return (handler, .writeCompleted(streamID, writeToken, failed: false))
      } catch {
        return (handler, .writeCompleted(streamID, writeToken, failed: true))
      }
    }
    if let action { action.0(action.1) }
  }

  func close(streamID: UInt64) {
    lock.withLock { _ = pending.removeValue(forKey: streamID) }
  }

  func stop() {
    lock.withLock {
      stopCount += 1
      handler = nil
      pending.removeAll()
    }
  }

  func responseSnapshot() -> Data? { lock.withLock { response } }
  func responseCountSnapshot() -> Int { lock.withLock { responseCount } }
  func processGenerationSnapshot() -> UInt64? { lock.withLock { processGeneration } }
  func stopCountSnapshot() -> Int { lock.withLock { stopCount } }
}

private struct TestEvidence: ActionEvidencePort {
  func captureBefore(context: TargetActionContext) async throws {}
  func observeStability(context: TargetActionContext) async throws {}
  func captureAfter(context: TargetActionContext) async throws {}
}

private actor PolicyGate {
  private var continuation: CheckedContinuation<Void, Never>?
  private(set) var entered = false

  func wait() async {
    entered = true
    await withCheckedContinuation { continuation = $0 }
  }

  func release() {
    continuation?.resume()
    continuation = nil
  }
}

private actor HandlerCount {
  private(set) var value = 0

  func increment() {
    value += 1
  }
}

private func makeComposition(generation: UInt64) throws -> TargetRuntimeComposition {
  let builder = SemanticCatalogBuilder()
  let catalog = try builder.freeze(
    identity: SemanticCatalogIdentity(id: "catalog_transport_test", generation: generation)
  )
  let coordinator = TargetActionCoordinator(
    catalog: catalog,
    targetID: "target_transport_test",
    evidence: TestEvidence(),
    policy: TargetActionPolicy(
      resolve: { _, _ in nil },
      validateDestructive: { _ in false },
      consumeDestructive: { _ in false }
    )
  )
  return try TargetRuntimeComposition(
    catalog: catalog,
    limits: SemanticProtocolLimits(
      maximumRequestBytes: 65_536,
      maximumResponseBytes: 65_536,
      maximumPageItems: 10
    ),
    policy: SemanticProtocolPolicy(
      discover: { _, _ in false },
      discloseSchema: { _, _ in false },
      discloseResource: { _, _ in false },
      discloseAction: { _, _ in false }
    ),
    actionCoordinator: coordinator,
    processGeneration: generation
  )
}

private func makeBlockingComposition(
  generation: UInt64,
  gate: PolicyGate,
  handlerCount: HandlerCount
) throws -> TargetRuntimeComposition {
  let schema = try resourceSchema()
  let builder = SemanticCatalogBuilder()
  try builder.registerResource(
    id: "slow.value",
    declarationRevision: 1,
    output: SemanticOutputCodec(schema: schema) { value in
      .object(["value": .publicValue(.string(value))])
    },
    handler: {
      await handlerCount.increment()
      return "dispatched"
    }
  )
  let catalog = try builder.freeze(
    identity: SemanticCatalogIdentity(id: "catalog_transport_slow", generation: generation)
  )
  let coordinator = TargetActionCoordinator(
    catalog: catalog,
    targetID: "target_transport_slow",
    evidence: TestEvidence(),
    policy: TargetActionPolicy(
      resolve: { _, _ in nil },
      validateDestructive: { _ in false },
      consumeDestructive: { _ in false }
    )
  )
  return try TargetRuntimeComposition(
    catalog: catalog,
    limits: SemanticProtocolLimits(
      maximumRequestBytes: 65_536,
      maximumResponseBytes: 65_536,
      maximumPageItems: 10
    ),
    policy: SemanticProtocolPolicy(
      discover: { _, _ in true },
      discloseSchema: { _, _ in true },
      discloseResource: { _, _ in
        await gate.wait()
        return true
      },
      discloseAction: { _, _ in false }
    ),
    actionCoordinator: coordinator,
    processGeneration: generation
  )
}

private func resourceSchema() throws -> SemanticSchema {
  try SemanticSchema(
    id: "schema_slow_value",
    revision: 1,
    document: .object([
      "$schema": .string("https://json-schema.org/draft/2020-12/schema"),
      "$id": .string("app://slow.value/value@1"),
      "type": .string("object"),
      "properties": .object(["value": .object(["type": .string("string")])]),
      "required": .array([.string("value")]),
      "additionalProperties": .bool(false),
    ])
  )
}

private func queryRequest(context: [String: Any]) throws -> Data {
  let handle = try JSONSerialization.jsonObject(
    with: JSONEncoder().encode(try resourceSchema().handle)
  )
  return try JSONSerialization.data(withJSONObject: [
    "jsonrpc": "2.0",
    "id": "query-after-open",
    "method": "semantic.query",
    "context": context,
    "params": [
      "capability": "slow.value",
      "declarationRevision": 1,
      "valueSchema": handle,
    ],
  ])
}

private func sessionOpenRequest(generation: UInt64) -> Data {
  _ = generation
  return Data(
    #"{"jsonrpc":"2.0","id":"open-transport","method":"session.open","params":{"client":{"name":"tests","version":"1"},"protocol":{"major":1,"minMinor":2,"maxMinor":2},"requiredCapabilities":["semantic.catalog"]}}"#.utf8
  )
}
