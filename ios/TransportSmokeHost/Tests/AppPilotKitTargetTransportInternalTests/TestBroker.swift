@_spi(AppPilotKitTargetTransportInternal) @testable import AppPilotKitTargetTransportInternal
import CAppPilotKitTargetTransportTestBroker
import Foundation

final class TestBroker: @unchecked Sendable {
  private var handle: apk_tp_test_broker_handle = 0
  let descriptor: Data

  init(port: UInt16 = 55_001) throws {
    var output: apk_tp_test_broker_output = 0
    guard apk_tp_test_broker_create(port, &handle, &output) == 0 else {
      throw TargetTransportInternalError.invariantViolation
    }
    descriptor = try Self.copyAndDrop(&output)
  }

  deinit {
    if handle != 0 {
      _ = apk_tp_test_broker_drop(&handle)
    }
  }

  func bootstrapM1(_ bytes: Data) throws -> Data {
    try output { output in
      bytes.withUnsafeBytes { buffer in
        apk_tp_test_broker_bootstrap_m1(
          handle,
          buffer.bindMemory(to: UInt8.self).baseAddress,
          UInt64(buffer.count),
          output
        )
      }
    }
  }

  func bootstrapAck(_ bytes: Data) throws {
    let status = bytes.withUnsafeBytes { buffer in
      apk_tp_test_broker_bootstrap_ack(
        handle,
        buffer.bindMemory(to: UInt8.self).baseAddress,
        UInt64(buffer.count)
      )
    }
    guard status == 0 else { throw TargetTransportInternalError.invariantViolation }
  }

  func heartbeat(_ counter: UInt64) throws -> Data {
    try output { apk_tp_test_broker_heartbeat(handle, counter, $0) }
  }

  func heartbeatReply(_ bytes: Data, expectedCounter: UInt64) throws {
    let status = bytes.withUnsafeBytes { buffer in
      apk_tp_test_broker_heartbeat_reply(
        handle,
        buffer.bindMemory(to: UInt8.self).baseAddress,
        UInt64(buffer.count),
        expectedCounter
      )
    }
    guard status == 0 else { throw TargetTransportInternalError.invariantViolation }
  }

  func sessionM1(_ bytes: Data) throws -> Data {
    try output { output in
      bytes.withUnsafeBytes { buffer in
        apk_tp_test_broker_session_m1(
          handle,
          buffer.bindMemory(to: UInt8.self).baseAddress,
          UInt64(buffer.count),
          output
        )
      }
    }
  }

  func targetFinished(_ bytes: Data) throws -> Data {
    try output { output in
      bytes.withUnsafeBytes { buffer in
        apk_tp_test_broker_target_finished(
          handle,
          buffer.bindMemory(to: UInt8.self).baseAddress,
          UInt64(buffer.count),
          output
        )
      }
    }
  }

  func sessionOpen(_ bytes: Data) throws -> Data {
    try output { output in
      bytes.withUnsafeBytes { buffer in
        apk_tp_test_broker_session_open(
          handle,
          buffer.bindMemory(to: UInt8.self).baseAddress,
          UInt64(buffer.count),
          output
        )
      }
    }
  }

  func sessionResponse(_ bytes: Data) throws -> Data {
    try output { output in
      bytes.withUnsafeBytes { buffer in
        apk_tp_test_broker_session_response(
          handle,
          buffer.bindMemory(to: UInt8.self).baseAddress,
          UInt64(buffer.count),
          output
        )
      }
    }
  }

  private func output(
    _ operation: (UnsafeMutablePointer<apk_tp_test_broker_output>) -> Int32
  ) throws -> Data {
    var output: apk_tp_test_broker_output = 0
    guard operation(&output) == 0 else {
      throw TargetTransportInternalError.invariantViolation
    }
    return try Self.copyAndDrop(&output)
  }

  private static func copyAndDrop(
    _ output: inout apk_tp_test_broker_output
  ) throws -> Data {
    defer {
      if output != 0 { _ = apk_tp_test_broker_output_drop(&output) }
    }
    var length: UInt64 = 0
    guard apk_tp_test_broker_output_len(output, &length) == 0,
      let count = Int(exactly: length)
    else {
      throw TargetTransportInternalError.invariantViolation
    }
    var bytes = Data(count: count)
    let status = bytes.withUnsafeMutableBytes { buffer in
      apk_tp_test_broker_output_copy(
        output,
        buffer.bindMemory(to: UInt8.self).baseAddress,
        length
      )
    }
    guard status == 0 else {
      bytes.resetBytes(in: 0..<bytes.count)
      throw TargetTransportInternalError.invariantViolation
    }
    return bytes
  }
}
