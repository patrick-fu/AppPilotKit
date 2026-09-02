import Foundation
import Network

final class LoopbackSocketHost: TargetSocketHosting, @unchecked Sendable {
  private struct Stream {
    let connection: NWConnection
    var receiving: Bool
  }

  private let queue = DispatchQueue(label: "app.appilotkit.target-transport.loopback")
  private var listener: NWListener?
  private var streams: [UInt64: Stream] = [:]
  private var nextStreamID: UInt64 = 1
  private var handler: (@Sendable (SocketEvent) -> Void)?
  private var startingContinuation: CheckedContinuation<Void, any Error>?
  private var ready = false
  private var stopped = false

  func start(port: UInt16, handler: @escaping @Sendable (SocketEvent) -> Void) async throws {
    try await withCheckedThrowingContinuation {
      (continuation: CheckedContinuation<Void, any Error>) in
      queue.async { [self] in
        guard listener == nil, !stopped,
          let networkPort = NWEndpoint.Port(rawValue: port)
        else {
          continuation.resume(throwing: TargetTransportInternalError.listenerFailed)
          return
        }
        self.handler = handler
        startingContinuation = continuation
        let parameters = NWParameters.tcp
        parameters.requiredLocalEndpoint = .hostPort(
          host: NWEndpoint.Host("127.0.0.1"),
          port: networkPort
        )
        do {
          let listener = try NWListener(using: parameters)
          self.listener = listener
          listener.service = nil
          listener.newConnectionHandler = { [weak self] connection in
            self?.accept(connection)
          }
          listener.stateUpdateHandler = { [weak self] state in
            self?.listenerStateChanged(state, expectedPort: networkPort)
          }
          listener.start(queue: queue)
        } catch {
          finishStart(.failure(TargetTransportInternalError.listenerFailed))
        }
      }
    }
  }

  func receive(streamID: UInt64) {
    queue.async { [weak self] in self?.beginReceive(streamID: streamID) }
  }

  func send(streamID: UInt64, writeToken: UInt64, bytes: Data) {
    queue.async { [weak self] in
      guard let self, let stream = streams[streamID], !stopped else {
        self?.handler?(.writeCompleted(streamID, writeToken, failed: true))
        return
      }
      stream.connection.send(content: bytes, completion: .contentProcessed { [weak self] error in
        self?.handler?(.writeCompleted(streamID, writeToken, failed: error != nil))
      })
    }
  }

  func close(streamID: UInt64) {
    queue.async { [weak self] in
      guard let stream = self?.streams.removeValue(forKey: streamID) else { return }
      stream.connection.stateUpdateHandler = nil
      stream.connection.cancel()
    }
  }

  func stop() {
    queue.sync {
      stopped = true
      ready = false
      listener?.newConnectionHandler = nil
      listener?.stateUpdateHandler = nil
      listener?.cancel()
      listener = nil
      for stream in streams.values {
        stream.connection.stateUpdateHandler = nil
        stream.connection.cancel()
      }
      streams.removeAll()
      finishStart(.failure(TargetTransportInternalError.listenerFailed))
      handler = nil
    }
  }

  private func listenerStateChanged(_ state: NWListener.State, expectedPort: NWEndpoint.Port) {
    switch state {
    case .ready:
      guard listener?.port == expectedPort else {
        finishStart(.failure(TargetTransportInternalError.listenerFailed))
        handler?(.listenerFailed)
        return
      }
      ready = true
      finishStart(.success(()))
    case .failed:
      let wasReady = ready
      ready = false
      finishStart(.failure(TargetTransportInternalError.listenerFailed))
      if wasReady { handler?(.listenerFailed) }
    case .cancelled:
      ready = false
      if !stopped {
        finishStart(.failure(TargetTransportInternalError.listenerFailed))
        handler?(.listenerFailed)
      }
    default:
      break
    }
  }

  private func accept(_ connection: NWConnection) {
    guard ready, !stopped, isLoopback(connection.endpoint), nextStreamID != 0 else {
      connection.cancel()
      return
    }
    let streamID = nextStreamID
    let (following, overflow) = nextStreamID.addingReportingOverflow(1)
    guard !overflow, following != 0 else {
      connection.cancel()
      handler?(.listenerFailed)
      return
    }
    nextStreamID = following
    streams[streamID] = Stream(connection: connection, receiving: false)
    connection.stateUpdateHandler = { [weak self] state in
      self?.connectionStateChanged(streamID: streamID, state: state)
    }
    connection.start(queue: queue)
  }

  private func connectionStateChanged(streamID: UInt64, state: NWConnection.State) {
    guard streams[streamID] != nil else { return }
    switch state {
    case .ready:
      handler?(.accepted(streamID))
    case .failed:
      finishStream(streamID: streamID, failed: true)
    case .cancelled:
      finishStream(streamID: streamID, failed: false)
    default:
      break
    }
  }

  private func beginReceive(streamID: UInt64) {
    guard var stream = streams[streamID], !stream.receiving, !stopped else { return }
    stream.receiving = true
    streams[streamID] = stream
    stream.connection.receive(minimumIncompleteLength: 1, maximumLength: 1_048_576) {
      [weak self] content, _, isComplete, error in
        guard let self, var current = self.streams[streamID] else { return }
        current.receiving = false
        self.streams[streamID] = current
        let data = content ?? Data()
        self.handler?(.received(streamID, data, end: isComplete, failed: error != nil))
    }
  }

  private func finishStream(streamID: UInt64, failed: Bool) {
    guard let stream = streams.removeValue(forKey: streamID) else { return }
    stream.connection.stateUpdateHandler = nil
    stream.connection.cancel()
    handler?(.received(streamID, Data(), end: true, failed: failed))
  }

  private func finishStart(_ result: Result<Void, any Error>) {
    guard let continuation = startingContinuation else { return }
    startingContinuation = nil
    continuation.resume(with: result)
  }

  private func isLoopback(_ endpoint: NWEndpoint) -> Bool {
    guard case .hostPort(let host, _) = endpoint else { return false }
    return host == NWEndpoint.Host("127.0.0.1")
  }
}
