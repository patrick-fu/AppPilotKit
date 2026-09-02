import CAppPilotKitTargetTransport
import Darwin
import Foundation

final class RustTargetTransportSupervisor: TargetTransportSupervising, @unchecked Sendable {
  private static let abiVersion: UInt32 = 0x0001_0000
  private var handle: apk_tp_handle_v1 = 0
  let initialOutcome: SupervisorOutcome

  init(descriptor: Data) throws {
    guard apppilotkit_tp_v1_abi_version() == Self.abiVersion else {
      throw TargetTransportInternalError.ffiFailure(Int32(APK_TP_STATUS_ABI_MISMATCH))
    }
    var input = apk_tp_create_input_v1()
    input.abi_version = Self.abiVersion
    input.struct_size = UInt32(MemoryLayout<apk_tp_create_input_v1>.size)
    input.descriptor_len = UInt64(descriptor.count)
    var outcome = apk_tp_outcome_v1()
    var created: apk_tp_handle_v1 = 0
    let status = descriptor.withUnsafeBytes { raw in
      input.descriptor_cbor = raw.bindMemory(to: UInt8.self).baseAddress
      return apppilotkit_tp_v1_create(&input, &created, &outcome)
    }
    guard status == APK_TP_STATUS_EVENT, created != 0 else {
      throw TargetTransportInternalError.ffiFailure(status)
    }
    handle = created
    do {
      initialOutcome = try Self.copyOutcome(outcome)
    } catch {
      var value = handle
      _ = Self.retryBusy { apppilotkit_tp_v1_drop(&value) }
      handle = 0
      throw error
    }
  }

  deinit {
    var value = handle
    _ = Self.retryBusy { apppilotkit_tp_v1_drop(&value) }
  }

  func drive(_ event: SupervisorEvent) throws -> SupervisorOutcome {
    guard handle != 0 else { throw TargetTransportInternalError.invariantViolation }
    var input = apk_tp_event_v1()
    input.abi_version = Self.abiVersion
    input.struct_size = UInt32(MemoryLayout<apk_tp_event_v1>.size)
    input.tag = event.tag
    input.stream_id = event.streamID
    input.write_token = event.writeToken
    input.bytes_len = UInt64(event.bytes.count)
    var outcome = apk_tp_outcome_v1()
    let status = event.bytes.withUnsafeBytes { raw in
      input.bytes = raw.bindMemory(to: UInt8.self).baseAddress
      return Self.retryBusy {
        apppilotkit_tp_v1_drive(handle, &input, &outcome)
      }
    }
    guard status >= APK_TP_STATUS_OK else {
      throw TargetTransportInternalError.ffiFailure(status)
    }
    return try Self.copyOutcome(outcome)
  }

  func close() throws -> SupervisorOutcome {
    var outcome = apk_tp_outcome_v1()
    let status = Self.retryBusy {
      apppilotkit_tp_v1_close(&handle, &outcome)
    }
    guard status == APK_TP_STATUS_OK else {
      throw TargetTransportInternalError.ffiFailure(status)
    }
    return try Self.copyOutcome(outcome)
  }

  private static func copyOutcome(_ raw: apk_tp_outcome_v1) throws -> SupervisorOutcome {
    var output = raw.output
    var bytes: Data?
    if output != 0 {
      defer { _ = apppilotkit_tp_v1_output_drop(&output) }
      var length: UInt64 = 0
      let lengthStatus = apppilotkit_tp_v1_output_len(output, &length)
      guard lengthStatus == APK_TP_STATUS_OK, let count = Int(exactly: length) else {
        throw TargetTransportInternalError.ffiFailure(lengthStatus)
      }
      var copied = Data(count: count)
      var written: UInt64 = 0
      let copyStatus = copied.withUnsafeMutableBytes { raw in
        apppilotkit_tp_v1_output_copy(
          output,
          raw.bindMemory(to: UInt8.self).baseAddress,
          UInt64(count),
          &written
        )
      }
      guard copyStatus == APK_TP_STATUS_OK, written == length else {
        copied.resetBytes(in: 0..<copied.count)
        throw TargetTransportInternalError.ffiFailure(copyStatus)
      }
      bytes = copied
    }
    let hasPeerClose = raw.flags & UInt32(APK_TP_OUTCOME_FLAG_PEER_CLOSE) != 0
    return SupervisorOutcome(
      kind: raw.kind,
      flags: raw.flags,
      streamID: raw.stream_id,
      writeToken: raw.write_token,
      bytes: bytes,
      value0: raw.value0,
      value1: raw.value1,
      nextDeadlineMilliseconds: raw.next_deadline_ms,
      closeReason: raw.close_reason,
      handoffState: raw.handoff_state,
      peerCloseReason: hasPeerClose ? raw.peer_close_reason : nil,
      peerHandoffState: hasPeerClose ? raw.peer_handoff_state : nil
    )
  }

  private static func retryBusy(
    _ operation: () -> apk_tp_status_v1
  ) -> apk_tp_status_v1 {
    var status = operation()
    for _ in 0..<64 where status == APK_TP_STATUS_BUSY {
      sched_yield()
      status = operation()
    }
    return status
  }
}

func decodeCanonicalDescriptor(_ encoded: String) throws -> Data {
  guard !encoded.isEmpty,
    !encoded.contains("="),
    encoded.utf8.allSatisfy({
      ($0 >= 65 && $0 <= 90) || ($0 >= 97 && $0 <= 122) || ($0 >= 48 && $0 <= 57)
        || $0 == 45 || $0 == 95
    }),
    encoded.count % 4 != 1
  else {
    throw TargetTransportInternalError.invalidDescriptor
  }
  let translated = encoded.replacingOccurrences(of: "-", with: "+")
    .replacingOccurrences(of: "_", with: "/")
  let padding = String(repeating: "=", count: (4 - translated.count % 4) % 4)
  guard let decoded = Data(base64Encoded: translated + padding) else {
    throw TargetTransportInternalError.invalidDescriptor
  }
  let canonical = decoded.base64EncodedString()
    .replacingOccurrences(of: "+", with: "-")
    .replacingOccurrences(of: "/", with: "_")
    .replacingOccurrences(of: "=", with: "")
  guard canonical == encoded else { throw TargetTransportInternalError.invalidDescriptor }
  return decoded
}
