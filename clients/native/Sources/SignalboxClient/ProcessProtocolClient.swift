import Foundation
import Network

#if canImport(SignalboxModels)
  import SignalboxModels
#endif

public protocol SignalboxProcessConnection: Sendable {
  func start() async throws
  func send(_ data: Data) async throws
  func receive() async throws -> Data?
  func close() async
}

public protocol SignalboxProcessConnectionFactory: Sendable {
  func makeConnection() -> any SignalboxProcessConnection
}

public protocol SignalboxProcessExchange: Sendable {
  func next() async throws -> SignalboxProcessServerFrame?
  func close() async
}

public protocol SignalboxProcessRequesting: Sendable {
  func open(
    _ request: SignalboxProcessClientRequest
  ) async throws -> any SignalboxProcessExchange
}

public enum SignalboxProcessClientError: LocalizedError, Equatable {
  case requestIdentityExhausted
  case oversizedFrame
  case unterminatedFrame
  case connectionClosed
  case responseVersionMismatch
  case responseIdentityMismatch

  public var errorDescription: String? {
    switch self {
    case .requestIdentityExhausted:
      return "The process-protocol request identity was exhausted."
    case .oversizedFrame:
      return "The process-protocol frame exceeded 8 MiB."
    case .unterminatedFrame:
      return "The process-protocol frame reached 8 MiB without a newline."
    case .connectionClosed:
      return "The daemon closed the connection before a complete frame."
    case .responseVersionMismatch:
      return "The daemon responded with a mismatched protocol version."
    case .responseIdentityMismatch:
      return "The daemon responded with a mismatched request identity."
    }
  }
}

public enum SignalboxProcessRequestOpenError: LocalizedError, Equatable {
  case definitelyUnsent(String)
  case sendOutcomeUnknown(String)

  public var errorDescription: String? {
    switch self {
    case .definitelyUnsent(let message):
      return "The process request was not sent: \(message)"
    case .sendOutcomeUnknown(let message):
      return "The process request send outcome is unknown: \(message)"
    }
  }
}

public actor SignalboxProcessClient: SignalboxProcessRequesting {
  private let connectionFactory: any SignalboxProcessConnectionFactory
  private var nextRequestID: UInt64

  public init(
    connectionFactory: any SignalboxProcessConnectionFactory,
    firstRequestID: UInt64 = 1
  ) {
    self.connectionFactory = connectionFactory
    self.nextRequestID = firstRequestID
  }

  public func open(
    _ request: SignalboxProcessClientRequest
  ) async throws -> any SignalboxProcessExchange {
    let requestID = try claimRequestID()
    let version = SignalboxProcessProtocol.currentVersion
    let frame = SignalboxProcessClientFrame(
      version: version,
      requestID: requestID,
      request: request
    )
    var encoded = try SignalboxJSONCoding.encoder().encode(frame)
    encoded.append(0x0A)
    guard encoded.count <= SignalboxProcessProtocol.maximumFrameBytes else {
      throw SignalboxProcessClientError.oversizedFrame
    }
    let connection = connectionFactory.makeConnection()
    do {
      try await connection.start()
    } catch {
      await connection.close()
      throw SignalboxProcessRequestOpenError.definitelyUnsent(error.localizedDescription)
    }
    do {
      try await connection.send(encoded)
    } catch {
      await connection.close()
      throw SignalboxProcessRequestOpenError.sendOutcomeUnknown(error.localizedDescription)
    }
    return SignalboxPullProcessExchange(
      connection: connection,
      requestedVersion: version,
      requestID: requestID
    )
  }

  private func claimRequestID() throws -> SignalboxRequestID {
    let claimed = try SignalboxRequestID(validating: nextRequestID)
    guard nextRequestID < UInt64.max else {
      throw SignalboxProcessClientError.requestIdentityExhausted
    }
    nextRequestID += 1
    return claimed
  }

  fileprivate static func admits(
    responseVersion: SignalboxProcessProtocolVersion,
    message: SignalboxProcessServerMessage,
    requestedVersion: SignalboxProcessProtocolVersion
  ) -> Bool {
    guard responseVersion != requestedVersion else {
      return true
    }
    guard responseVersion == .one,
      case .protocolError(let error) = message
    else {
      return false
    }
    return error.code == .malformedFrame || error.code == .unsupportedVersion
  }
}

private actor SignalboxPullProcessExchange: SignalboxProcessExchange {
  private let connection: any SignalboxProcessConnection
  private let reader: SignalboxProcessLineReader
  private let requestedVersion: SignalboxProcessProtocolVersion
  private let requestID: SignalboxRequestID
  private var isClosed = false

  init(
    connection: any SignalboxProcessConnection,
    requestedVersion: SignalboxProcessProtocolVersion,
    requestID: SignalboxRequestID
  ) {
    self.connection = connection
    self.reader = SignalboxProcessLineReader(connection: connection)
    self.requestedVersion = requestedVersion
    self.requestID = requestID
  }

  func next() async throws -> SignalboxProcessServerFrame? {
    guard !isClosed else {
      return nil
    }
    do {
      let line = try await reader.nextLine()
      let response = try SignalboxProcessServerFrame.decode(from: line)
      guard
        SignalboxProcessClient.admits(
          responseVersion: response.version,
          message: response.message,
          requestedVersion: requestedVersion
        )
      else {
        throw SignalboxProcessClientError.responseVersionMismatch
      }
      guard response.requestID.rawValue == requestID.rawValue else {
        throw SignalboxProcessClientError.responseIdentityMismatch
      }
      return response
    } catch SignalboxProcessClientError.connectionClosed {
      await close()
      return nil
    } catch {
      await close()
      throw error
    }
  }

  func close() async {
    guard !isClosed else {
      return
    }
    isClosed = true
    await connection.close()
  }
}

private actor SignalboxProcessLineReader {
  private let connection: any SignalboxProcessConnection
  private var buffer = Data()

  init(connection: any SignalboxProcessConnection) {
    self.connection = connection
  }

  func nextLine() async throws -> Data {
    while true {
      if let newline = buffer.firstIndex(of: 0x0A) {
        let end = buffer.index(after: newline)
        let line = buffer[..<end]
        guard line.count <= SignalboxProcessProtocol.maximumFrameBytes else {
          throw SignalboxProcessClientError.oversizedFrame
        }
        buffer.removeSubrange(..<end)
        return Data(line)
      }
      guard buffer.count < SignalboxProcessProtocol.maximumFrameBytes else {
        throw SignalboxProcessClientError.unterminatedFrame
      }
      guard let chunk = try await connection.receive() else {
        if buffer.isEmpty {
          throw SignalboxProcessClientError.connectionClosed
        }
        throw SignalboxProcessClientError.unterminatedFrame
      }
      buffer.append(chunk)
    }
  }
}

public struct SignalboxLocalSocketConnectionFactory: SignalboxProcessConnectionFactory {
  public let socketPath: String

  public init(socketPath: String) {
    self.socketPath = socketPath
  }

  public func makeConnection() -> any SignalboxProcessConnection {
    SignalboxLocalSocketConnection(socketPath: socketPath)
  }
}

private final class SignalboxLocalSocketConnection: SignalboxProcessConnection, @unchecked Sendable
{
  private let connection: NWConnection
  private let stateLock = NSLock()
  private var startContinuation: CheckedContinuation<Void, Error>?

  init(socketPath: String) {
    self.connection = NWConnection(
      to: .unix(path: socketPath),
      using: .tcp
    )
  }

  func start() async throws {
    try await withTaskCancellationHandler {
      try await withCheckedThrowingContinuation { continuation in
        stateLock.lock()
        startContinuation = continuation
        stateLock.unlock()
        connection.stateUpdateHandler = { [weak self] state in
          self?.receive(state: state)
        }
        connection.start(queue: .global(qos: .userInitiated))
      }
    } onCancel: {
      connection.cancel()
    }
  }

  func send(_ data: Data) async throws {
    try await withCheckedThrowingContinuation {
      (continuation: CheckedContinuation<Void, Error>) in
      connection.send(
        content: data,
        completion: .contentProcessed { error in
          if let error {
            continuation.resume(throwing: error)
          } else {
            continuation.resume()
          }
        })
    }
  }

  func receive() async throws -> Data? {
    try await withCheckedThrowingContinuation { continuation in
      connection.receive(
        minimumIncompleteLength: 1,
        maximumLength: 64 * 1024
      ) { data, _, complete, error in
        if let error {
          continuation.resume(throwing: error)
        } else if let data, !data.isEmpty {
          continuation.resume(returning: data)
        } else if complete {
          continuation.resume(returning: nil)
        } else {
          continuation.resume(returning: Data())
        }
      }
    }
  }

  func close() async {
    connection.cancel()
  }

  private func receive(state: NWConnection.State) {
    let result: Result<Void, Error>?
    switch state {
    case .ready:
      result = .success(())
    case .failed(let error):
      result = .failure(error)
    case .cancelled:
      result = .failure(CancellationError())
    case .setup, .preparing, .waiting:
      result = nil
    @unknown default:
      result = .failure(SignalboxProcessClientError.connectionClosed)
    }
    guard let result else {
      return
    }
    stateLock.lock()
    let continuation = startContinuation
    startContinuation = nil
    stateLock.unlock()
    continuation?.resume(with: result)
  }
}
