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
  private(set) var sentData = Data()
  private(set) var receiveCount = 0

  init(
    chunks: [Data],
    startError: (any Error)? = nil,
    sendError: (any Error)? = nil
  ) {
    self.chunks = chunks
    self.startError = startError
    self.sendError = sendError
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
    sentData.append(data)
  }

  func receive() async throws -> Data? {
    receiveCount += 1
    guard !chunks.isEmpty else {
      return nil
    }
    return chunks.removeFirst()
  }

  func close() async {}
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
