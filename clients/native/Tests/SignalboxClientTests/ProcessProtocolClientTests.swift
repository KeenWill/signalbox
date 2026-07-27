import Foundation
import XCTest

@testable import SignalboxNative

final class ProcessProtocolClientTests: XCTestCase {
  func testMalformedKnownMessageDoesNotKillFollowingFrame() async throws {
    let connection = ScriptedProcessConnection(
      chunks: [
        Data(
          """
          {"version":5,"request_id":"1","message":{"type":"session_created","session_id":17}}
          {"version":5,"request_id":"1","message":
          """.utf8
        ),
        Data(
          ("""
          {"type":"sessions_end","session_count":"0"}}
          """ + "\n").utf8
        ),
      ]
    )
    let client = SignalboxProcessClient(
      connectionFactory: ScriptedProcessConnectionFactory(connection: connection)
    )

    let exchange = try await client.open(.listSessions)
    let malformed = try await exchange.next()
    let following = try await exchange.next()

    XCTAssertEqual(
      malformed?.message,
      .unknown(
        kind: "session_created",
        payload: [
          "type": .string("session_created"),
          "session_id": .number(17),
        ],
        decodingDiagnostic: SignalboxDecodingDiagnostic(
          message: "Unexpected field type at message.session_id."
        )
      )
    )
    XCTAssertEqual(
      following?.message,
      .sessionsEnd(sessionCount: SignalboxCanonicalUInt64(rawValue: 0))
    )
  }

  func testClientWritesOneNewlineTerminatedVersionFiveFrame() async throws {
    let connection = ScriptedProcessConnection(
      chunks: [
        Data(
          ("""
          {"version":5,"request_id":"1","message":{"type":"sessions_end","session_count":"0"}}
          """ + "\n").utf8
        )
      ]
    )
    let client = SignalboxProcessClient(
      connectionFactory: ScriptedProcessConnectionFactory(connection: connection)
    )

    let exchange = try await client.open(.listSessions)
    _ = try await exchange.next()
    let sent = await connection.sentData

    XCTAssertEqual(
      String(decoding: sent, as: UTF8.self),
      #"{"request":{"type":"list_sessions"},"request_id":"1","version":5}"# + "\n"
    )
  }

  func testPullExchangeReadsNoFollowingChunkBeforeNextRequest() async throws {
    let connection = ScriptedProcessConnection(
      chunks: [
        Data(
          ("""
          {"version":5,"request_id":"1","message":{"type":"sessions_start"}}
          """ + "\n").utf8
        ),
        Data(
          ("""
          {"version":5,"request_id":"1","message":{"type":"sessions_end","session_count":"0"}}
          """ + "\n").utf8
        ),
      ]
    )
    let client = SignalboxProcessClient(
      connectionFactory: ScriptedProcessConnectionFactory(connection: connection)
    )

    let exchange = try await client.open(.listSessions)
    let readsBeforeNext = await connection.receiveCount
    _ = try await exchange.next()
    let readsAfterOneFrame = await connection.receiveCount

    XCTAssertEqual(readsBeforeNext, 0)
    XCTAssertEqual(readsAfterOneFrame, 1)
  }

  func testConnectionStartFailureIsDefinitelyUnsent() async {
    let connection = ScriptedProcessConnection(
      chunks: [],
      startError: ProcessProtocolClientFixtureError.startFailed
    )
    let client = SignalboxProcessClient(
      connectionFactory: ScriptedProcessConnectionFactory(connection: connection)
    )

    let error = await capturedOpenError(client)

    XCTAssertEqual(
      error,
      .definitelyUnsent(ProcessProtocolClientFixtureError.startMessage)
    )
  }

  func testConnectionSendFailureHasUnknownOutcome() async {
    let connection = ScriptedProcessConnection(
      chunks: [],
      sendError: ProcessProtocolClientFixtureError.sendFailed
    )
    let client = SignalboxProcessClient(
      connectionFactory: ScriptedProcessConnectionFactory(connection: connection)
    )

    let error = await capturedOpenError(client)

    XCTAssertEqual(
      error,
      .sendOutcomeUnknown(ProcessProtocolClientFixtureError.sendMessage)
    )
  }

  func testCancellingSuspendedSendClosesConnection() async {
    let connection = ScriptedProcessConnection(chunks: [], suspendsSend: true)
    let client = SignalboxProcessClient(
      connectionFactory: ScriptedProcessConnectionFactory(connection: connection)
    )
    let opening = Task { () -> Bool in
      do {
        _ = try await client.open(.listSessions)
        return false
      } catch is CancellationError {
        return true
      } catch {
        return false
      }
    }
    await connection.waitUntilSendStarted()

    opening.cancel()
    let wasCancelled = await opening.value
    let closeCount = await connection.closeCount

    XCTAssertTrue(wasCancelled)
    XCTAssertEqual(closeCount, 1)
  }

  func testConcurrentExchangeReadIsRejectedBeforeSecondReceive() async throws {
    let connection = ScriptedProcessConnection(chunks: [], suspendsReceive: true)
    let client = SignalboxProcessClient(
      connectionFactory: ScriptedProcessConnectionFactory(connection: connection)
    )
    let exchange = try await client.open(.listSessions)
    let first = Task { try await exchange.next() }
    await connection.waitUntilReceiveStarted()

    let secondError = await capturedNextError(exchange)
    await exchange.close()
    _ = try? await first.value

    XCTAssertEqual(secondError, .concurrentRead)
  }

  private func capturedOpenError(
    _ client: SignalboxProcessClient
  ) async -> SignalboxProcessRequestOpenError? {
    do {
      _ = try await client.open(.listSessions)
      return nil
    } catch let error as SignalboxProcessRequestOpenError {
      return error
    } catch {
      return nil
    }
  }

  private func capturedNextError(
    _ exchange: any SignalboxProcessExchange
  ) async -> SignalboxProcessClientError? {
    do {
      _ = try await exchange.next()
      return nil
    } catch let error as SignalboxProcessClientError {
      return error
    } catch {
      return nil
    }
  }
}

private struct ScriptedProcessConnectionFactory: SignalboxProcessConnectionFactory {
  let connection: ScriptedProcessConnection

  func makeConnection() -> any SignalboxProcessConnection {
    connection
  }
}

private actor ScriptedProcessConnection: SignalboxProcessConnection {
  private var chunks: [Data]
  private let startError: (any Error)?
  private let sendError: (any Error)?
  private let suspendsSend: Bool
  private let suspendsReceive: Bool
  private(set) var sentData = Data()
  private(set) var receiveCount = 0
  private(set) var closeCount = 0
  private var sendContinuation: CheckedContinuation<Void, Error>?
  private var sendStartedContinuation: CheckedContinuation<Void, Never>?
  private var receiveContinuation: CheckedContinuation<Data?, Error>?
  private var receiveStartedContinuation: CheckedContinuation<Void, Never>?

  init(
    chunks: [Data],
    startError: (any Error)? = nil,
    sendError: (any Error)? = nil,
    suspendsSend: Bool = false,
    suspendsReceive: Bool = false
  ) {
    self.chunks = chunks
    self.startError = startError
    self.sendError = sendError
    self.suspendsSend = suspendsSend
    self.suspendsReceive = suspendsReceive
  }

  func start() async throws {
    if let startError {
      throw startError
    }
  }

  func send(_ data: Data) async throws {
    if let sendError {
      throw sendError
    }
    if suspendsSend {
      try await withCheckedThrowingContinuation { continuation in
        sendContinuation = continuation
        sendStartedContinuation?.resume()
        sendStartedContinuation = nil
      }
      return
    }
    sentData.append(data)
  }

  func waitUntilSendStarted() async {
    guard sendContinuation == nil else {
      return
    }
    await withCheckedContinuation { continuation in
      sendStartedContinuation = continuation
    }
  }

  func receive() async throws -> Data? {
    receiveCount += 1
    if suspendsReceive {
      return try await withCheckedThrowingContinuation { continuation in
        receiveContinuation = continuation
        receiveStartedContinuation?.resume()
        receiveStartedContinuation = nil
      }
    }
    guard !chunks.isEmpty else {
      return nil
    }
    return chunks.removeFirst()
  }

  func waitUntilReceiveStarted() async {
    guard receiveContinuation == nil else {
      return
    }
    await withCheckedContinuation { continuation in
      receiveStartedContinuation = continuation
    }
  }

  func close() async {
    guard closeCount == 0 else {
      return
    }
    closeCount = 1
    sendContinuation?.resume(throwing: CancellationError())
    sendContinuation = nil
    receiveContinuation?.resume(throwing: CancellationError())
    receiveContinuation = nil
  }
}

private enum ProcessProtocolClientFixtureError: LocalizedError {
  case startFailed
  case sendFailed

  static let startMessage = "Fixture start failed."
  static let sendMessage = "Fixture send failed."

  var errorDescription: String? {
    switch self {
    case .startFailed:
      Self.startMessage
    case .sendFailed:
      Self.sendMessage
    }
  }
}
