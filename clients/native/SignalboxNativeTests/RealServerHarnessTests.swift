import Foundation
import XCTest

@testable import SignalboxNative

final class RealServerHarnessTests: XCTestCase {
  func testVersionOneClientCompletesSessionLifecycleAcrossReconnects() async throws {
    let socketPath = try realServerSocketPath()
    let initialService = realServerService(socketPath: socketPath)

    try await initialService.testConnection()
    let aliases = try await initialService.listModelAliases()
    let alias = try XCTUnwrap(aliases.first)
    let creation = try await initialService.prepareSessionCreation(
      modelSelection: .alias(aliasID: alias.aliasID),
      systemPrompt: RealServerFixture.systemPrompt
    )
    let sessionID = try await initialService.createSession(creation)
    let sessions = try await initialService.listSessions(includeArchived: true)
    let created = try XCTUnwrap(sessions.first(where: { $0.id == sessionID }))
    let archived = try await initialService.setArchived(true, session: created)
    let unarchived = try await initialService.setArchived(false, session: archived)

    XCTAssertEqual(aliases, [RealServerFixture.modelAlias])
    XCTAssertEqual(created.id, sessionID)
    XCTAssertEqual(created.modelSelection, .alias(aliasID: alias.aliasID))
    XCTAssertFalse(created.archived)
    XCTAssertTrue(archived.archived)
    XCTAssertFalse(unarchived.archived)

    let reconnectingService = realServerService(socketPath: socketPath)
    try await reconnectingService.testConnection()
    let afterReconnect = try await reconnectingService.listSessions(includeArchived: true)

    XCTAssertEqual(afterReconnect.first(where: { $0.id == sessionID })?.archived, false)
  }

  func testRealDaemonServesEmptyTranscript() async throws {
    let socketPath = try realServerSocketPath()
    let service = realServerService(socketPath: socketPath)
    let aliases = try await service.listModelAliases()
    let alias = try XCTUnwrap(aliases.first)
    let creation = try await service.prepareSessionCreation(
      modelSelection: .alias(aliasID: alias.aliasID),
      systemPrompt: nil
    )
    let sessionID = try await service.createSession(creation)
    let client = realServerClient(socketPath: socketPath)
    let transcript = try await client.open(.readTranscript(sessionID: sessionID))
    let snapshotStart = try await requireFrame(from: transcript)
    let modelCallsEnd = try await requireFrame(from: transcript)
    let snapshotEnd = try await requireFrame(from: transcript)
    await transcript.close()

    XCTAssertEqual(
      try requireSnapshotStart(snapshotStart.message).sessionID,
      sessionID
    )
    XCTAssertEqual(
      try requireModelCallsEnd(modelCallsEnd.message),
      SignalboxCanonicalUInt64(rawValue: 0)
    )
    XCTAssertEqual(try requireSnapshotEnd(snapshotEnd.message).sessionID, sessionID)
  }

  func testRealDaemonRejectsStaleDefaultsBeforeModelExecution() async throws {
    let socketPath = try realServerSocketPath()
    let service = realServerService(socketPath: socketPath)
    let aliases = try await service.listModelAliases()
    let alias = try XCTUnwrap(aliases.first)
    let creation = try await service.prepareSessionCreation(
      modelSelection: .alias(aliasID: alias.aliasID),
      systemPrompt: nil
    )
    let sessionID = try await service.createSession(creation)
    let client = realServerClient(socketPath: socketPath)
    let rejected = try await client.open(
      .submitInput(
        commandID: RealServerFixture.staleDefaultsCommandID,
        sessionID: sessionID,
        content: RealServerFixture.rejectedInput,
        expectedDefaultsVersion: RealServerFixture.staleDefaultsVersion
      )
    )
    let rejection = try requireProtocolError(try await requireFrame(from: rejected).message)

    XCTAssertEqual(rejection.code, .rejected)
    XCTAssertEqual(
      rejection.detail,
      .defaultsVersionMismatch(
        sessionID: sessionID,
        expected: RealServerFixture.staleDefaultsVersion,
        current: RealServerFixture.initialDefaultsVersion
      )
    )
  }

  func testRealDaemonRejectsUnsupportedProtocolVersion() async throws {
    let socketPath = try realServerSocketPath()
    let unsupportedConnection =
      SignalboxLocalSocketConnectionFactory(socketPath: socketPath).makeConnection()
    try await unsupportedConnection.start()
    try await unsupportedConnection.send(RealServerFixture.unsupportedVersionRequest)
    let unsupportedResponse = try SignalboxProcessServerFrame.decode(
      from: try await readLine(from: unsupportedConnection)
    )
    await unsupportedConnection.close()

    XCTAssertEqual(unsupportedResponse.version, .one)
    XCTAssertEqual(
      unsupportedResponse.requestID,
      RealServerFixture.unsupportedVersionRequestID
    )
    XCTAssertEqual(
      try requireProtocolError(unsupportedResponse.message).code,
      .unsupportedVersion
    )
  }

  func testRealDaemonRejectsMalformedFrameThenAcceptsReconnect() async throws {
    let socketPath = try realServerSocketPath()
    let malformedConnection =
      SignalboxLocalSocketConnectionFactory(socketPath: socketPath).makeConnection()
    try await malformedConnection.start()
    try await malformedConnection.send(RealServerFixture.malformedRequest)
    let malformedResponse = try SignalboxProcessServerFrame.decode(
      from: try await readLine(from: malformedConnection)
    )
    await malformedConnection.close()

    XCTAssertEqual(try requireProtocolError(malformedResponse.message).code, .malformedFrame)

    let reconnectingClient = realServerClient(socketPath: socketPath)
    let exchange = try await reconnectingClient.open(.listSessions)
    let firstFrameAfterReconnect = try await requireFrame(from: exchange)
    await exchange.close()

    XCTAssertEqual(firstFrameAfterReconnect.message, .sessionsStart)
  }

  /// S28: the native client inspects and continues a real daemon import without a model call.
  func testRealDaemonCompletesImportedTranscriptContinuationWithoutAModelCall() async throws {
    let socketPath = try realServerSocketPath()
    let client = realServerClient(socketPath: socketPath)
    let importExchange = try await client.open(
      .importConversation(
        format: .codexRolloutJSONLV1,
        source: RealServerFixture.importedRollout
      )
    )
    let importedConversationID = try requireImportedConversationID(
      try await requireFrame(from: importExchange).message
    )
    await importExchange.close()
    let service = realServerService(socketPath: socketPath)
    let conversations = try await service.listConversations(includeArchived: true)
    let conversation = try XCTUnwrap(
      conversations.first(where: { $0.conversationID == importedConversationID })
    )
    let transcript = try await service.readImportedConversation(conversation: conversation)
    let aliases = try await service.listModelAliases()
    let alias = try XCTUnwrap(aliases.first)
    let creation = try await service.prepareImportedSessionCreation(
      conversation: conversation,
      throughPosition: try XCTUnwrap(transcript.entries.last?.position),
      relationship: .resume,
      modelSelection: .alias(aliasID: alias.aliasID)
    )

    let sessionID = try await service.createSessionFromImportedFrontier(creation)
    let sessions = try await service.listSessions(includeArchived: true)

    XCTAssertEqual(transcript.importedConversationID, importedConversationID)
    XCTAssertEqual(transcript.entries.count, RealServerFixture.importedEntryCount)
    XCTAssertEqual(
      transcript.entries[1].textPreview?.preview,
      RealServerFixture.importedQuestion
    )
    XCTAssertEqual(
      transcript.entries[2].textPreview?.preview,
      RealServerFixture.importedAnswer
    )
    XCTAssertNotNil(sessions.first(where: { $0.id == sessionID }))
  }
}

private enum RealServerFixture {
  static let systemPrompt = "Synthetic real-server harness system prompt."
  static let rejectedInput = "This input must be rejected before model execution."
  static let staleDefaultsVersion = SignalboxCanonicalUInt64(rawValue: 99)
  static let initialDefaultsVersion = SignalboxCanonicalUInt64(rawValue: 1)
  static let importedEntryCount = 3
  static let importedQuestion = "User fixture question"
  static let importedAnswer = "User fixture answer"
  static let modelAlias = SignalboxModelAliasSummary(
    aliasID: try! SignalboxCanonicalUUID(
      validating: "30000000-0000-4000-8000-000000000001"
    ),
    selectionID: try! SignalboxCanonicalUUID(
      validating: "10000000-0000-4000-8000-000000000001"
    )
  )
  static let staleDefaultsCommandID = try! SignalboxCommandID(
    validating: "40000000-0000-4000-8000-000000000001"
  )
  static let unsupportedVersionRequestID = SignalboxCanonicalUInt64(rawValue: 51)
  static let unsupportedVersionRequest = Data(
    #"{"version":2,"request_id":"51","request":{"type":"list_sessions"}}"#.utf8
      + [0x0A]
  )
  static let malformedRequest = Data(
    #"{"version":1,"request_id":"52","request":{"type":"list_sessions"}"#.utf8
      + [0x0A]
  )
  static var importedRollout: Data {
    Data(
      """
      {"timestamp":"t0","type":"session_meta","payload":{"id":"user-fixture","session_id":"user-fixture"}}
      {"timestamp":"t1","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"\(importedQuestion)"}]}}
      {"timestamp":"t2","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"\(importedAnswer)"}]}}
      """.utf8
    )
  }
}

private func realServerSocketPath() throws -> String {
  let path = ProcessInfo.processInfo.environment["SIGNALBOX_SOCKET_PATH"]
  try XCTSkipIf(path == nil, "The real-server harness supplies SIGNALBOX_SOCKET_PATH.")
  return try XCTUnwrap(path)
}

private func realServerClient(socketPath: String) -> SignalboxProcessClient {
  SignalboxProcessClient(
    connectionFactory: SignalboxLocalSocketConnectionFactory(socketPath: socketPath)
  )
}

private func realServerService(socketPath: String) -> SignalboxProcessService {
  SignalboxProcessService(
    requester: realServerClient(socketPath: socketPath),
    policy: .nativeDefault
  )
}

private func requireFrame(
  from exchange: any SignalboxProcessExchange
) async throws -> SignalboxProcessServerFrame {
  let frame = try await exchange.next()
  return try XCTUnwrap(frame)
}

private func requireSnapshotStart(
  _ message: SignalboxProcessServerMessage
) throws -> SignalboxTranscriptSnapshotBoundary {
  guard case .transcriptSnapshotStart(let boundary) = message else {
    throw RealServerFixtureError.unexpectedMessage
  }
  return boundary
}

private func requireModelCallsEnd(
  _ message: SignalboxProcessServerMessage
) throws -> SignalboxCanonicalUInt64 {
  guard case .transcriptModelCallsEnd(let count) = message else {
    throw RealServerFixtureError.unexpectedMessage
  }
  return count
}

private func requireSnapshotEnd(
  _ message: SignalboxProcessServerMessage
) throws -> SignalboxTranscriptSnapshotEnd {
  guard case .transcriptSnapshotEnd(let boundary) = message else {
    throw RealServerFixtureError.unexpectedMessage
  }
  return boundary
}

private func requireProtocolError(
  _ message: SignalboxProcessServerMessage
) throws -> SignalboxProcessError {
  guard case .protocolError(let error) = message else {
    throw RealServerFixtureError.unexpectedMessage
  }
  return error
}

private func requireImportedConversationID(
  _ message: SignalboxProcessServerMessage
) throws -> SignalboxCanonicalUUID {
  switch message {
  case .conversationImportInserted(let importedConversationID),
    .conversationImportAlreadyImported(let importedConversationID):
    return importedConversationID
  case .sessionCreated, .inputSubmitted, .toolRequestDecided, .sessionDefaults,
    .sessionsStart, .sessionSummary, .sessionsEnd, .sessionMetadataPageStart,
    .sessionMetadataSummary, .sessionMetadataPageEnd, .sessionMetadata,
    .sessionMetadataReplaced, .conversationPageStart, .conversationSummary,
    .conversationPageEnd, .importedConversationStart, .importedConversationEntry,
    .importedConversationEnd, .modelAliasesStart, .modelAliasSummary,
    .modelAliasesEnd, .transcriptSnapshotStart, .transcriptTurn,
    .transcriptModelCallUsage, .transcriptModelCallsEnd, .transcriptEntry,
    .transcriptUserEntry, .transcriptTextEntry, .transcriptContent,
    .transcriptSnapshotEnd,
    .sessionEvent, .providerTextDelta, .protocolError, .unknown:
    throw RealServerFixtureError.unexpectedMessage
  }
}

private func readLine(
  from connection: any SignalboxProcessConnection
) async throws -> Data {
  var buffered = Data()
  while buffered.firstIndex(of: 0x0A) == nil {
    let chunk = try await connection.receive()
    buffered.append(try XCTUnwrap(chunk))
  }
  let newline = try XCTUnwrap(buffered.firstIndex(of: 0x0A))
  return Data(buffered[...newline])
}

private enum RealServerFixtureError: Error {
  case unexpectedMessage
}
