@_spi(AppPilotKitTargetTransportInternal) @testable import AppPilotKitTargetTransportInternal
import CAppPilotKitTargetTransport
import Foundation
import XCTest

final class RustSupervisorTests: XCTestCase {
  func testD0DescriptorIsAcceptedButReplayedTranscriptFailsAuthentication() throws {
    let vector = try loadVector("bootstrap-nk-success.json")
    let canonical = try XCTUnwrap(vector["canonical_input"] as? [String: Any])
    let expected = try XCTUnwrap(vector["expected"] as? [String: Any])
    let descriptor = try hex(try XCTUnwrap(canonical["launch_descriptor_cbor_hex"] as? String))
    let m2 = try hex(try XCTUnwrap(expected["m2_outer_hex"] as? String))

    XCTAssertEqual(apppilotkit_tp_v1_abi_version(), 0x0001_0000)
    let supervisor = try RustTargetTransportSupervisor(descriptor: descriptor)
    XCTAssertEqual(supervisor.initialOutcome.kind, UInt32(APK_TP_OUTCOME_ENDPOINT_READY))
    XCTAssertEqual(supervisor.initialOutcome.value0, 0)
    XCTAssertEqual(supervisor.initialOutcome.value1, 55_001)

    let m1 = try supervisor.drive(
      SupervisorEvent(tag: UInt32(APK_TP_EVENT_BOOTSTRAP_CONNECTED), streamID: 7)
    )
    XCTAssertEqual(m1.kind, UInt32(APK_TP_OUTCOME_WRITE_FRAMES))
    XCTAssertFalse(try XCTUnwrap(m1.bytes).isEmpty)
    _ = try supervisor.drive(
      SupervisorEvent(
        tag: UInt32(APK_TP_EVENT_FULL_WRITE_COMMITTED),
        streamID: 7,
        writeToken: m1.writeToken
      )
    )

    var terminal: SupervisorOutcome?
    for byte in m2 {
      terminal = try supervisor.drive(
        SupervisorEvent(
          tag: UInt32(APK_TP_EVENT_STREAM_BYTES),
          streamID: 7,
          bytes: Data([byte])
        )
      )
    }
    XCTAssertEqual(terminal?.kind, UInt32(APK_TP_OUTCOME_LEASE_TERMINAL))
    XCTAssertEqual(terminal?.closeReason, UInt32(APK_TP_CLOSE_AUTHENTICATION_FAILED))
    XCTAssertNil(terminal?.bytes)
    XCTAssertEqual(terminal?.handoffState, UInt32(APK_TP_HANDOFF_NOT_HANDED_OFF))
  }

  func testRealCABICompletesProductionNKAndNNpsk0WithArbitraryChunking() throws {
    let broker = try TestBroker()
    let supervisor = try RustTargetTransportSupervisor(descriptor: broker.descriptor)
    let bootstrapStream: UInt64 = 7
    let sessionStream: UInt64 = 9

    let m1 = try supervisor.drive(
      SupervisorEvent(tag: UInt32(APK_TP_EVENT_BOOTSTRAP_CONNECTED), streamID: bootstrapStream)
    )
    let m2 = try broker.bootstrapM1(try XCTUnwrap(m1.bytes))
    let bootstrapInput = try supervisor.drive(
      SupervisorEvent(
        tag: UInt32(APK_TP_EVENT_FULL_WRITE_COMMITTED),
        streamID: bootstrapStream,
        writeToken: m1.writeToken
      )
    )
    XCTAssertEqual(bootstrapInput.kind, UInt32(APK_TP_OUTCOME_NEED_INPUT))

    let ack = try feedBytewise(m2, streamID: bootstrapStream, supervisor: supervisor)
    XCTAssertEqual(ack.kind, UInt32(APK_TP_OUTCOME_WRITE_FRAMES))
    try broker.bootstrapAck(try XCTUnwrap(ack.bytes))
    let lease = try supervisor.drive(
      SupervisorEvent(
        tag: UInt32(APK_TP_EVENT_FULL_WRITE_COMMITTED),
        streamID: bootstrapStream,
        writeToken: ack.writeToken
      )
    )
    XCTAssertEqual(lease.kind, UInt32(APK_TP_OUTCOME_LEASE_READY))
    XCTAssertGreaterThan(lease.value0, 0)
    XCTAssertEqual(lease.value1, 1)

    let heartbeat = try broker.heartbeat(1)
    let heartbeatReply = try feedBytewise(
      heartbeat,
      streamID: bootstrapStream,
      supervisor: supervisor
    )
    XCTAssertEqual(heartbeatReply.kind, UInt32(APK_TP_OUTCOME_WRITE_FRAMES))
    try broker.heartbeatReply(try XCTUnwrap(heartbeatReply.bytes), expectedCounter: 1)
    let heartbeatCommitted = try supervisor.drive(
      SupervisorEvent(
        tag: UInt32(APK_TP_EVENT_FULL_WRITE_COMMITTED),
        streamID: bootstrapStream,
        writeToken: heartbeatReply.writeToken
      )
    )
    XCTAssertEqual(heartbeatCommitted.kind, UInt32(APK_TP_OUTCOME_NEED_INPUT))

    let sessionM1 = try supervisor.drive(
      SupervisorEvent(tag: UInt32(APK_TP_EVENT_SESSION_ACCEPTED), streamID: sessionStream)
    )
    let sessionM2 = try broker.sessionM1(try XCTUnwrap(sessionM1.bytes))
    _ = try supervisor.drive(
      SupervisorEvent(
        tag: UInt32(APK_TP_EVENT_FULL_WRITE_COMMITTED),
        streamID: sessionStream,
        writeToken: sessionM1.writeToken
      )
    )
    let targetFinished = try feedBytewise(
      sessionM2,
      streamID: sessionStream,
      supervisor: supervisor
    )
    let brokerFinished = try broker.targetFinished(try XCTUnwrap(targetFinished.bytes))
    _ = try supervisor.drive(
      SupervisorEvent(
        tag: UInt32(APK_TP_EVENT_FULL_WRITE_COMMITTED),
        streamID: sessionStream,
        writeToken: targetFinished.writeToken
      )
    )
    let ready = try feedBytewise(
      brokerFinished,
      streamID: sessionStream,
      supervisor: supervisor
    )
    XCTAssertEqual(ready.kind, UInt32(APK_TP_OUTCOME_NEED_INPUT))

    let request = Data(
      #"{"jsonrpc":"2.0","id":"open-transport","method":"session.open","params":{"client":{"name":"tests","version":"1"},"protocol":{"major":1,"minMinor":2,"maxMinor":2},"requiredCapabilities":["semantic.catalog"]}}"#.utf8
    )
    let requestFrames = try broker.sessionOpen(request)
    let application = try feedBytewise(
      requestFrames,
      streamID: sessionStream,
      supervisor: supervisor
    )
    XCTAssertEqual(application.kind, UInt32(APK_TP_OUTCOME_APPLICATION))
    XCTAssertEqual(application.bytes, request)
    XCTAssertEqual(
      application.handoffState,
      UInt32(APK_TP_HANDOFF_POSSIBLE_OR_CONFIRMED)
    )

    let runtimeResponse = Data(#"{"jsonrpc":"2.0","id":"open-transport","result":{"ok":true}}"#.utf8)
    let encryptedResponse = try supervisor.drive(
      SupervisorEvent(
        tag: UInt32(APK_TP_EVENT_RUNTIME_RESPONSE),
        streamID: sessionStream,
        bytes: runtimeResponse
      )
    )
    XCTAssertEqual(
      try broker.sessionResponse(try XCTUnwrap(encryptedResponse.bytes)),
      runtimeResponse
    )
    let committed = try supervisor.drive(
      SupervisorEvent(
        tag: UInt32(APK_TP_EVENT_FULL_WRITE_COMMITTED),
        streamID: sessionStream,
        writeToken: encryptedResponse.writeToken
      )
    )
    XCTAssertEqual(committed.kind, UInt32(APK_TP_OUTCOME_NEED_INPUT))
  }

  func testCanonicalBase64urlDecoderRejectsAliasesAndPadding() throws {
    let bytes = Data([0xfb, 0xff, 0x00])
    let canonical = "-_8A"
    XCTAssertEqual(try decodeCanonicalDescriptor(canonical), bytes)
    XCTAssertThrowsError(try decodeCanonicalDescriptor("+/8A"))
    XCTAssertThrowsError(try decodeCanonicalDescriptor("-_8A="))
    XCTAssertThrowsError(try decodeCanonicalDescriptor("a"))
  }

  func testRealCABIMonotonicDeadlineTokenTimesOutAndCannotRevive() throws {
    let broker = try TestBroker()
    let supervisor = try RustTargetTransportSupervisor(descriptor: broker.descriptor)
    XCTAssertNil(supervisor.initialOutcome.deadlineToken)
    let bootstrap = try supervisor.drive(
      SupervisorEvent(tag: UInt32(APK_TP_EVENT_BOOTSTRAP_CONNECTED), streamID: 44)
    )
    XCTAssertEqual(bootstrap.kind, UInt32(APK_TP_OUTCOME_WRITE_FRAMES))
    let token = try XCTUnwrap(bootstrap.deadlineToken)
    let terminal = try supervisor.drive(
      SupervisorEvent(
        tag: UInt32(APK_TP_EVENT_TIMER_FIRED),
        writeToken: token
      )
    )
    XCTAssertEqual(terminal.kind, UInt32(APK_TP_OUTCOME_LEASE_TERMINAL))
    XCTAssertEqual(terminal.closeReason, UInt32(APK_TP_CLOSE_TIMEOUT))
    let stale = try supervisor.drive(
      SupervisorEvent(tag: UInt32(APK_TP_EVENT_BOOTSTRAP_CONNECTED), streamID: 44)
    )
    XCTAssertEqual(stale.kind, UInt32(APK_TP_OUTCOME_LEASE_TERMINAL))
    XCTAssertEqual(stale.closeReason, UInt32(APK_TP_CLOSE_TIMEOUT))
  }

  private func loadVector(_ name: String) throws -> [String: Any] {
    let testFile = URL(fileURLWithPath: #filePath)
    let repository = testFile
      .deletingLastPathComponent()
      .deletingLastPathComponent()
      .deletingLastPathComponent()
      .deletingLastPathComponent()
      .deletingLastPathComponent()
    let url = repository
      .appendingPathComponent("transport/contracts/v1/vectors", isDirectory: true)
      .appendingPathComponent(name)
    return try XCTUnwrap(
      JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any]
    )
  }

  private func hex(_ value: String) throws -> Data {
    guard value.count.isMultiple(of: 2) else { throw TargetTransportInternalError.invalidDescriptor }
    var bytes = Data()
    bytes.reserveCapacity(value.count / 2)
    var index = value.startIndex
    while index < value.endIndex {
      let next = value.index(index, offsetBy: 2)
      guard let byte = UInt8(value[index..<next], radix: 16) else {
        throw TargetTransportInternalError.invalidDescriptor
      }
      bytes.append(byte)
      index = next
    }
    return bytes
  }

  private func feedBytewise(
    _ bytes: Data,
    streamID: UInt64,
    supervisor: RustTargetTransportSupervisor
  ) throws -> SupervisorOutcome {
    var outcome: SupervisorOutcome?
    for byte in bytes {
      outcome = try supervisor.drive(
        SupervisorEvent(
          tag: UInt32(APK_TP_EVENT_STREAM_BYTES),
          streamID: streamID,
          bytes: Data([byte])
        )
      )
    }
    return try XCTUnwrap(outcome)
  }
}
