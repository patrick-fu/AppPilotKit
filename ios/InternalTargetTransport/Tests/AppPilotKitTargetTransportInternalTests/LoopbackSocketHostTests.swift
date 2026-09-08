@_spi(AppPilotKitTargetTransportInternal) @testable import AppPilotKitTargetTransportInternal
import Foundation
import XCTest

#if os(macOS)
import Darwin
import Network

final class LoopbackSocketHostTests: XCTestCase {
  func testProductionHostBindsExactLoopbackPortAndAcceptsLoopbackOnly() async throws {
    let port = try availableHighLoopbackPort()
    let host = LoopbackSocketHost()
    let probe = SocketProbe()
    try await host.start(port: port) { event in probe.record(event) }

    let connection = NWConnection(
      host: NWEndpoint.Host("127.0.0.1"),
      port: try XCTUnwrap(NWEndpoint.Port(rawValue: port)),
      using: .tcp
    )
    connection.start(queue: DispatchQueue(label: "app.appilotkit.target-transport.loopback-test"))
    defer {
      connection.cancel()
      host.stop()
    }

    try await eventually { probe.acceptedStreamID() != nil }
    XCTAssertFalse(probe.listenerFailed())
    XCTAssertGreaterThan(try XCTUnwrap(probe.acceptedStreamID()), 0)
  }

  private func availableHighLoopbackPort() throws -> UInt16 {
    for _ in 0..<32 {
      let descriptor = Darwin.socket(AF_INET, SOCK_STREAM, 0)
      guard descriptor >= 0 else { continue }
      defer { Darwin.close(descriptor) }
      var address = sockaddr_in()
      address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
      address.sin_family = sa_family_t(AF_INET)
      address.sin_addr = in_addr(s_addr: inet_addr("127.0.0.1"))
      let bound = withUnsafePointer(to: &address) {
        $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
          Darwin.bind(descriptor, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
        }
      }
      guard bound == 0 else { continue }
      var length = socklen_t(MemoryLayout<sockaddr_in>.size)
      let named = withUnsafeMutablePointer(to: &address) {
        $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
          Darwin.getsockname(descriptor, $0, &length)
        }
      }
      guard named == 0 else { continue }
      let port = UInt16(bigEndian: address.sin_port)
      if port >= 49_152 { return port }
    }
    throw TargetTransportInternalError.listenerFailed
  }

  private func eventually(
    timeout: Duration = .seconds(2),
    _ predicate: @escaping @Sendable () -> Bool
  ) async throws {
    let clock = ContinuousClock()
    let deadline = clock.now.advanced(by: timeout)
    while !predicate() {
      guard clock.now < deadline else {
        throw TargetTransportInternalError.invariantViolation
      }
      try await Task.sleep(for: .milliseconds(5))
    }
  }
}

private final class SocketProbe: @unchecked Sendable {
  private let lock = NSLock()
  private var streamID: UInt64?
  private var failed = false

  func record(_ event: SocketEvent) {
    lock.withLock {
      switch event {
      case .accepted(let value): streamID = value
      case .listenerFailed: failed = true
      default: break
      }
    }
  }

  func acceptedStreamID() -> UInt64? { lock.withLock { streamID } }
  func listenerFailed() -> Bool { lock.withLock { failed } }
}
#endif
