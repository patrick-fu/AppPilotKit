import AppPilotKit
import CAppPilotKitTargetTransport
import Foundation

@_spi(AppPilotKitTargetTransportInternal)
public actor AppPilotKitTargetTransport {
  public static let descriptorEnvironmentKey = "APPPILOTKIT_TRANSPORT_DESCRIPTOR"

  private let supervisor: any TargetTransportSupervising
  private let sockets: any TargetSocketHosting
  private let compositionFactory: TargetRuntimeCompositionFactory
  private let initialOutcome: SupervisorOutcome
  private let initialObservedAtNanoseconds: UInt64

  private var bootstrapStreamID: UInt64?
  private var composition: TargetRuntimeComposition?
  private var runtimes: [UInt64: SemanticProtocolRuntime] = [:]
  private var pendingWrites: [UInt64: Data] = [:]
  private var pendingWriteTokens: [UInt64: UInt64] = [:]
  private var timerTasks: [UInt64: Task<Void, Never>] = [:]
  private var lifecycleMonitor: EligibilityMonitor?
  private var started = false
  private var stopped = false

  @_spi(AppPilotKitTargetTransportInternal)
  public static func start(
    descriptor: String,
    compositionFactory: @escaping TargetRuntimeCompositionFactory
  ) async throws -> AppPilotKitTargetTransport {
#if !DEBUG && !APPPILOTKIT_INTERNAL
    throw TargetTransportInternalError.unavailableInBuildConfiguration
#else
    let decoded = try decodeCanonicalDescriptor(descriptor)
    let observed = DispatchTime.now().uptimeNanoseconds
    let supervisor = try RustTargetTransportSupervisor(descriptor: decoded)
    let transport = AppPilotKitTargetTransport(
      supervisor: supervisor,
      sockets: LoopbackSocketHost(),
      compositionFactory: compositionFactory,
      initialOutcome: supervisor.initialOutcome,
      initialObservedAtNanoseconds: observed
    )
    try await transport.activate()
    return transport
#endif
  }

  @_spi(AppPilotKitTargetTransportInternal)
  public static func startFromEnvironment(
    compositionFactory: @escaping TargetRuntimeCompositionFactory
  ) async throws -> AppPilotKitTargetTransport {
    guard let descriptor = ProcessInfo.processInfo.environment[descriptorEnvironmentKey] else {
      throw TargetTransportInternalError.invalidDescriptor
    }
    return try await start(descriptor: descriptor, compositionFactory: compositionFactory)
  }

  init(
    supervisor: any TargetTransportSupervising,
    sockets: any TargetSocketHosting,
    compositionFactory: @escaping TargetRuntimeCompositionFactory,
    initialOutcome: SupervisorOutcome,
    initialObservedAtNanoseconds: UInt64 = DispatchTime.now().uptimeNanoseconds
  ) {
    self.supervisor = supervisor
    self.sockets = sockets
    self.compositionFactory = compositionFactory
    self.initialOutcome = initialOutcome
    self.initialObservedAtNanoseconds = initialObservedAtNanoseconds
  }

  deinit {
    // Explicit lifecycle termination remains required; this is a final safety net
    // so an abandoned owner cannot leave the loopback listener bound.
    sockets.stop()
  }

  @_spi(AppPilotKitTargetTransportInternal)
  public func eligibilityLost() async {
    guard started, !stopped else { return }
    await driveAndProcess(
      SupervisorEvent(tag: UInt32(APK_TP_EVENT_ELIGIBILITY_LOST))
    )
  }

  @_spi(AppPilotKitTargetTransportInternal)
  public func stop() async {
    guard !stopped else { return }
    stopped = true
    lifecycleMonitor = nil
    cancelTimers()
    wipePendingWrites()
    sockets.stop()
    let live = Array(runtimes.values)
    runtimes.removeAll()
    for runtime in live {
      await runtime.invalidateSessions()
    }
    _ = try? supervisor.close()
  }

  func activate() async throws {
    guard !started, !stopped,
      initialOutcome.kind == UInt32(APK_TP_OUTCOME_ENDPOINT_READY),
      initialOutcome.value0 == 0,
      let port = UInt16(exactly: initialOutcome.value1),
      port >= 49_152
    else {
      throw TargetTransportInternalError.unsupportedPlatform
    }
    started = true
    scheduleDeadline(from: initialOutcome, observedAtNanoseconds: initialObservedAtNanoseconds)
    guard let monitor = await EligibilityMonitor.install(callback: { [weak self] in
      Task { await self?.eligibilityLost() }
    }) else {
      await driveAndProcess(
        SupervisorEvent(tag: UInt32(APK_TP_EVENT_ELIGIBILITY_LOST))
      )
      throw TargetTransportInternalError.ineligibleLifecycle
    }
    lifecycleMonitor = monitor
    do {
      try await sockets.start(port: port) { [weak self] event in
        Task { await self?.socketEvent(event) }
      }
    } catch {
      await internalFailure()
      throw TargetTransportInternalError.listenerFailed
    }
    guard !stopped else { throw TargetTransportInternalError.listenerFailed }
  }

  private func socketEvent(_ event: SocketEvent) async {
    guard started, !stopped else { return }
    switch event {
    case .accepted(let streamID):
      await accepted(streamID: streamID)
    case .received(let streamID, var bytes, let end, let failed):
      if !bytes.isEmpty {
        await driveAndProcess(
          SupervisorEvent(
            tag: UInt32(APK_TP_EVENT_STREAM_BYTES),
            streamID: streamID,
            bytes: bytes
          )
        )
        bytes.resetBytes(in: 0..<bytes.count)
      }
      guard !stopped else { return }
      if end || failed {
        await driveAndProcess(
          SupervisorEvent(
            tag: UInt32(failed ? APK_TP_EVENT_STREAM_IO_FAILED : APK_TP_EVENT_STREAM_EOF),
            streamID: streamID
          )
        )
      }
    case .writeCompleted(let streamID, let writeToken, let failed):
      guard pendingWriteTokens[streamID] == writeToken else { return }
      wipePendingWrite(streamID: streamID)
      await driveAndProcess(
        SupervisorEvent(
          tag: UInt32(failed ? APK_TP_EVENT_STREAM_IO_FAILED : APK_TP_EVENT_FULL_WRITE_COMMITTED),
          streamID: streamID,
          writeToken: failed ? 0 : writeToken
        )
      )
    case .listenerFailed:
      await internalFailure()
    }
  }

  private func accepted(streamID: UInt64) async {
    guard streamID != 0 else {
      await internalFailure()
      return
    }
    if bootstrapStreamID == nil {
      bootstrapStreamID = streamID
      await driveAndProcess(
        SupervisorEvent(tag: UInt32(APK_TP_EVENT_BOOTSTRAP_CONNECTED), streamID: streamID)
      )
      return
    }
    guard composition != nil else {
      sockets.close(streamID: streamID)
      return
    }
    await driveAndProcess(
      SupervisorEvent(tag: UInt32(APK_TP_EVENT_SESSION_ACCEPTED), streamID: streamID)
    )
  }

  private func driveAndProcess(_ event: SupervisorEvent) async {
    guard !stopped else { return }
    do {
      let outcome = try supervisor.drive(event)
      let observed = DispatchTime.now().uptimeNanoseconds
      await process(outcome, observedAtNanoseconds: observed)
    } catch {
      await internalFailure()
    }
  }

  private func process(
    _ outcome: SupervisorOutcome,
    observedAtNanoseconds: UInt64
  ) async {
    guard !stopped else {
      if var bytes = outcome.bytes {
        bytes.resetBytes(in: 0..<bytes.count)
      }
      return
    }
    scheduleDeadline(from: outcome, observedAtNanoseconds: observedAtNanoseconds)
    switch outcome.kind {
    case UInt32(APK_TP_OUTCOME_NEED_INPUT):
      if outcome.streamID != 0 { sockets.receive(streamID: outcome.streamID) }
    case UInt32(APK_TP_OUTCOME_WRITE_FRAMES):
      await write(outcome)
    case UInt32(APK_TP_OUTCOME_APPLICATION):
      await application(outcome)
    case UInt32(APK_TP_OUTCOME_LEASE_READY):
      await leaseReady(outcome)
    case UInt32(APK_TP_OUTCOME_SESSION_TERMINAL):
      await sessionTerminal(streamID: outcome.streamID)
    case UInt32(APK_TP_OUTCOME_LEASE_TERMINAL):
      await leaseTerminal()
    case UInt32(APK_TP_OUTCOME_CLOSED):
      await leaseTerminal()
    default:
      await internalFailure()
    }
  }

  private func write(_ outcome: SupervisorOutcome) async {
    guard outcome.streamID != 0, outcome.writeToken != 0,
      var bytes = outcome.bytes, !bytes.isEmpty,
      pendingWrites[outcome.streamID] == nil
    else {
      await internalFailure()
      return
    }
    pendingWriteTokens[outcome.streamID] = outcome.writeToken
    pendingWrites[outcome.streamID] = bytes
    sockets.send(streamID: outcome.streamID, writeToken: outcome.writeToken, bytes: bytes)
    bytes.resetBytes(in: 0..<bytes.count)
  }

  private func application(_ outcome: SupervisorOutcome) async {
    guard outcome.streamID != 0, var request = outcome.bytes, !request.isEmpty,
      let composition
    else {
      await internalFailure()
      return
    }
    let runtime: SemanticProtocolRuntime
    if let existing = runtimes[outcome.streamID] {
      runtime = existing
    } else {
      runtime = composition.makeRuntime()
      runtimes[outcome.streamID] = runtime
    }

    // There is deliberately no suspension between accepting the C1 Application
    // outcome and invoking the existing runtime.
    var response = await runtime.handle(request)
    request.resetBytes(in: 0..<request.count)
    guard !stopped, runtimes[outcome.streamID] === runtime else {
      response.resetBytes(in: 0..<response.count)
      return
    }
    await driveAndProcess(
      SupervisorEvent(
        tag: UInt32(APK_TP_EVENT_RUNTIME_RESPONSE),
        streamID: outcome.streamID,
        bytes: response
      )
    )
    response.resetBytes(in: 0..<response.count)
  }

  private func leaseReady(_ outcome: SupervisorOutcome) async {
    guard outcome.streamID == bootstrapStreamID,
      outcome.value0 > 0,
      outcome.value1 > 0,
      composition == nil
    else {
      await internalFailure()
      return
    }
    do {
      let created = try compositionFactory(outcome.value0)
      guard created.catalog.identity.generation == outcome.value0 else {
        throw TargetTransportInternalError.runtimeCompositionFailed
      }
      composition = created
      sockets.receive(streamID: outcome.streamID)
    } catch {
      await internalFailure()
    }
  }

  private func sessionTerminal(streamID: UInt64) async {
    guard streamID != 0 else { return }
    sockets.close(streamID: streamID)
    wipePendingWrite(streamID: streamID)
    // Each connection owns one runtime, so invalidation here affects only A and
    // prevents an in-flight reentrant handle from dispatching after A closes.
    if let runtime = runtimes.removeValue(forKey: streamID) {
      await runtime.invalidateSessions()
    }
  }

  private func leaseTerminal() async {
    guard !stopped else { return }
    stopped = true
    lifecycleMonitor = nil
    cancelTimers()
    wipePendingWrites()
    sockets.stop()
    let live = Array(runtimes.values)
    runtimes.removeAll()
    for runtime in live {
      await runtime.invalidateSessions()
    }
    _ = try? supervisor.close()
  }

  private func internalFailure() async {
    guard !stopped else { return }
    do {
      let outcome = try supervisor.drive(
        SupervisorEvent(tag: UInt32(APK_TP_EVENT_INTERNAL_ERROR))
      )
      await process(
        outcome,
        observedAtNanoseconds: DispatchTime.now().uptimeNanoseconds
      )
    } catch {
      await leaseTerminal()
    }
  }

  private func scheduleDeadline(
    from outcome: SupervisorOutcome,
    observedAtNanoseconds: UInt64
  ) {
    guard outcome.nextDeadlineMilliseconds > 0,
      let token = outcome.deadlineToken,
      token != 0,
      timerTasks[token] == nil
    else { return }
    let (delta, multiplyOverflow) = outcome.nextDeadlineMilliseconds
      .multipliedReportingOverflow(by: 1_000_000)
    let (deadline, addOverflow) = observedAtNanoseconds.addingReportingOverflow(delta)
    guard !multiplyOverflow, !addOverflow else { return }
    timerTasks[token] = Task { [weak self] in
      do {
        let now = DispatchTime.now().uptimeNanoseconds
        if now < deadline {
          try await Task.sleep(nanoseconds: deadline - now)
        }
      } catch {
        return
      }
      await self?.timerFired(token: token)
    }
  }

  private func timerFired(token: UInt64) async {
    timerTasks.removeValue(forKey: token)
    guard !stopped else { return }
    await driveAndProcess(
      SupervisorEvent(tag: UInt32(APK_TP_EVENT_TIMER_FIRED), writeToken: token)
    )
  }

  private func cancelTimers() {
    for task in timerTasks.values { task.cancel() }
    timerTasks.removeAll()
  }

  private func wipePendingWrite(streamID: UInt64) {
    pendingWriteTokens.removeValue(forKey: streamID)
    if var bytes = pendingWrites.removeValue(forKey: streamID) {
      bytes.resetBytes(in: 0..<bytes.count)
    }
  }

  private func wipePendingWrites() {
    let ids = Array(pendingWrites.keys)
    for id in ids { wipePendingWrite(streamID: id) }
  }
}
