import Foundation

#if canImport(SignalboxModels)
  import SignalboxModels
#endif

func signalboxImportedConversationTitleIsAdmissible(_ title: String?) -> Bool {
  title.map {
    !$0.isEmpty
      && !$0.unicodeScalars.contains(where: {
        $0 == "\0" || $0 == "\n" || $0 == "\r"
      })
      && !$0.hasPrefix(" ")
      && !$0.hasPrefix("\t")
      && !$0.hasSuffix(" ")
      && !$0.hasSuffix("\t")
      && $0.unicodeScalars.count
        <= SignalboxProcessProtocol.maximumImportedConversationTitleScalars
  } ?? true
}

public enum SignalboxProcessServiceError: LocalizedError, Equatable {
  case unexpectedMessage(String)
  case invalidPage(String)
  case deadlineExceeded(String)
  case remote(
    code: SignalboxProcessErrorCode,
    message: String,
    detail: SignalboxRejectionDetail?
  )
  case mutationRetryExhausted(code: SignalboxProcessErrorCode, message: String)

  public var errorDescription: String? {
    switch self {
    case .unexpectedMessage(let message), .invalidPage(let message),
      .deadlineExceeded(let message):
      return message
    case .remote(let code, let message, _):
      return SignalboxProcessPresentation.retainedLabel("\(code.rawValue): \(message)")
    case .mutationRetryExhausted(let code, let message):
      return SignalboxProcessPresentation.retainedLabel(
        "\(code.rawValue): \(message)",
        preserving: " The exact command can be retried."
      )
    }
  }
}

public struct SignalboxProcessApplicationPolicy: Equatable, Sendable {
  public let metadataPageSize: SignalboxCanonicalUInt64
  public let maximumMetadataPages: UInt
  public let maximumMetadataListUTF8Bytes: UInt
  public let maximumImportedEntries: UInt
  public let maximumImportedPreviewUTF8Bytes: UInt
  public let ambiguousMutationRetryDelays: [Duration]
  public let oneShotResponseDeadline: Duration
  public let synchronization: SignalboxSessionSynchronizationPolicy

  public init(
    metadataPageSize: SignalboxCanonicalUInt64,
    maximumMetadataPages: UInt,
    maximumMetadataListUTF8Bytes: UInt = 32 * 1_024 * 1_024,
    maximumImportedEntries: UInt = 50_000,
    maximumImportedPreviewUTF8Bytes: UInt = 32 * 1_024 * 1_024,
    ambiguousMutationRetryDelays: [Duration],
    oneShotResponseDeadline: Duration = .seconds(20),
    synchronization: SignalboxSessionSynchronizationPolicy
  ) {
    self.metadataPageSize = metadataPageSize
    self.maximumMetadataPages = maximumMetadataPages
    self.maximumMetadataListUTF8Bytes = maximumMetadataListUTF8Bytes
    self.maximumImportedEntries = maximumImportedEntries
    self.maximumImportedPreviewUTF8Bytes = maximumImportedPreviewUTF8Bytes
    self.ambiguousMutationRetryDelays = ambiguousMutationRetryDelays
    self.oneShotResponseDeadline = oneShotResponseDeadline
    self.synchronization = synchronization
  }

  public static let nativeDefault = Self(
    metadataPageSize: SignalboxCanonicalUInt64(rawValue: 100),
    maximumMetadataPages: 100,
    ambiguousMutationRetryDelays: [
      .milliseconds(250),
      .milliseconds(750),
      .seconds(2),
    ],
    oneShotResponseDeadline: .seconds(20),
    synchronization: SignalboxSessionSynchronizationPolicy(
      deadlines: SignalboxSynchronizationDeadlines(
        connect: .seconds(5),
        hello: .seconds(5),
        history: .seconds(20),
        replay: .seconds(5),
        sideHistory: .seconds(20)
      ),
      retry: SignalboxSynchronizationRetryPolicy(
        delays: [
          .milliseconds(250),
          .seconds(1),
          .seconds(3),
          .seconds(8),
        ]
      ),
      snapshotCapacity: SignalboxSynchronizationSnapshotCapacity(
        maximumRecords: 50_000,
        maximumUTF8Bytes: 32 * 1_024 * 1_024
      ),
      eventBufferCapacity: SignalboxSynchronizationEventBufferCapacity(
        maximumEvents: 2_000,
        maximumUTF8Bytes: 8 * 1_024 * 1_024
      )
    )
  )
}

/// A mutation is prepared once so every outcome-unknown retry reuses the exact
/// command identity and payload the daemon may already have committed.
public struct SignalboxPreparedInputSubmission: Equatable, Sendable {
  public let commandID: SignalboxCommandID
  public let sessionID: SignalboxCanonicalUUID
  public let content: String
  public let expectedDefaultsVersion: SignalboxCanonicalUInt64
  public let modelSelection: SignalboxModelSelection

  public init(
    commandID: SignalboxCommandID,
    sessionID: SignalboxCanonicalUUID,
    content: String,
    expectedDefaultsVersion: SignalboxCanonicalUInt64,
    modelSelection: SignalboxModelSelection
  ) {
    self.commandID = commandID
    self.sessionID = sessionID
    self.content = content
    self.expectedDefaultsVersion = expectedDefaultsVersion
    self.modelSelection = modelSelection
  }

  fileprivate var request: SignalboxProcessClientRequest {
    .submitInput(
      commandID: commandID,
      sessionID: sessionID,
      content: content,
      expectedDefaultsVersion: expectedDefaultsVersion
    )
  }
}

public struct SignalboxPreparedSessionCreation: Equatable, Sendable {
  public let commandID: SignalboxCommandID
  public let modelSelection: SignalboxModelSelection
  public let systemPrompt: String?

  public init(
    commandID: SignalboxCommandID,
    modelSelection: SignalboxModelSelection,
    systemPrompt: String?
  ) {
    self.commandID = commandID
    self.modelSelection = modelSelection
    self.systemPrompt = systemPrompt
  }

  fileprivate var request: SignalboxProcessClientRequest {
    .createSession(
      commandID: commandID,
      initialModelSelection: modelSelection,
      systemPrompt: systemPrompt
    )
  }
}

public struct SignalboxPreparedImportedSessionCreation: Equatable, Sendable {
  public let commandID: SignalboxCommandID
  public let importedConversationID: SignalboxCanonicalUUID
  public let throughPosition: SignalboxCanonicalUInt64
  public let relationship: SignalboxImportedSessionRelationship
  public let modelSelection: SignalboxModelSelection

  public init(
    commandID: SignalboxCommandID,
    importedConversationID: SignalboxCanonicalUUID,
    throughPosition: SignalboxCanonicalUInt64,
    relationship: SignalboxImportedSessionRelationship,
    modelSelection: SignalboxModelSelection
  ) {
    self.commandID = commandID
    self.importedConversationID = importedConversationID
    self.throughPosition = throughPosition
    self.relationship = relationship
    self.modelSelection = modelSelection
  }

  fileprivate var request: SignalboxProcessClientRequest {
    .createSessionFromImportedFrontier(
      commandID: commandID,
      importedConversationID: importedConversationID,
      throughPosition: throughPosition,
      relationship: relationship,
      initialModelSelection: modelSelection
    )
  }
}

public struct SignalboxPreparedToolRequestDecision: Equatable, Sendable {
  public let commandID: SignalboxCommandID
  public let sessionID: SignalboxCanonicalUUID
  public let toolRequestID: SignalboxCanonicalUUID
  public let decision: SignalboxProcessToolDecision

  public init(
    commandID: SignalboxCommandID,
    sessionID: SignalboxCanonicalUUID,
    toolRequestID: SignalboxCanonicalUUID,
    decision: SignalboxProcessToolDecision
  ) {
    self.commandID = commandID
    self.sessionID = sessionID
    self.toolRequestID = toolRequestID
    self.decision = decision
  }

  fileprivate var request: SignalboxProcessClientRequest {
    .decideToolRequest(
      commandID: commandID,
      sessionID: sessionID,
      toolRequestID: toolRequestID,
      decision: decision
    )
  }
}

public struct SignalboxPreparedTurnStop: Equatable, Sendable {
  public let commandID: SignalboxCommandID
  public let sessionID: SignalboxCanonicalUUID
  public let activeTurnID: SignalboxCanonicalUUID
  public let content: String
  public let expectedDefaultsVersion: SignalboxCanonicalUInt64
  public let descendantScope: SignalboxDescendantTerminationScope
  public let modelSelection: SignalboxModelSelection

  public init(
    commandID: SignalboxCommandID,
    sessionID: SignalboxCanonicalUUID,
    activeTurnID: SignalboxCanonicalUUID,
    content: String,
    expectedDefaultsVersion: SignalboxCanonicalUInt64,
    descendantScope: SignalboxDescendantTerminationScope = .parentAlone,
    modelSelection: SignalboxModelSelection
  ) {
    self.commandID = commandID
    self.sessionID = sessionID
    self.activeTurnID = activeTurnID
    self.content = content
    self.expectedDefaultsVersion = expectedDefaultsVersion
    self.descendantScope = descendantScope
    self.modelSelection = modelSelection
  }

  fileprivate var request: SignalboxProcessClientRequest {
    .stopTurn(
      commandID: commandID,
      sessionID: sessionID,
      expectedActiveTurnID: activeTurnID,
      content: content,
      expectedDefaultsVersion: expectedDefaultsVersion,
      descendantScope: descendantScope
    )
  }
}

public protocol SignalboxProcessServiceProtocol: Sendable {
  func testConnection() async throws
  func listConversations(includeArchived: Bool) async throws -> [SignalboxProcessConversation]
  func listModelAliases() async throws -> [SignalboxModelAliasSummary]
  func listSessions(includeArchived: Bool) async throws -> [SignalboxProcessSession]
  func readSession(
    conversation: SignalboxProcessConversation
  ) async throws -> SignalboxProcessSession
  func readImportedConversation(
    conversation: SignalboxProcessConversation
  ) async throws -> SignalboxImportedConversationTranscript
  func setConversationArchived(
    _ archived: Bool,
    conversation: SignalboxProcessConversation
  ) async throws -> SignalboxProcessConversation
  func setArchived(
    _ archived: Bool,
    session: SignalboxProcessSession
  ) async throws -> SignalboxProcessSession
  func prepareInputSubmission(
    session: SignalboxProcessSession,
    content: String
  ) async throws -> SignalboxPreparedInputSubmission
  func submit(
    _ submission: SignalboxPreparedInputSubmission
  ) async throws -> SignalboxInputSubmitted
  func prepareSessionCreation(
    modelSelection: SignalboxModelSelection,
    systemPrompt: String?
  ) async throws -> SignalboxPreparedSessionCreation
  func createSession(
    _ creation: SignalboxPreparedSessionCreation
  ) async throws -> SignalboxCanonicalUUID
  func prepareImportedSessionCreation(
    conversation: SignalboxProcessConversation,
    throughPosition: SignalboxCanonicalUInt64,
    relationship: SignalboxImportedSessionRelationship,
    modelSelection: SignalboxModelSelection
  ) async throws -> SignalboxPreparedImportedSessionCreation
  func createSessionFromImportedFrontier(
    _ creation: SignalboxPreparedImportedSessionCreation
  ) async throws -> SignalboxCanonicalUUID
  func prepareToolRequestDecision(
    sessionID: SignalboxCanonicalUUID,
    toolRequestID: SignalboxCanonicalUUID,
    decision: SignalboxProcessToolDecision
  ) async throws -> SignalboxPreparedToolRequestDecision
  func decideToolRequest(
    _ prepared: SignalboxPreparedToolRequestDecision
  ) async throws -> SignalboxToolRequestDecided
  func prepareTurnStop(
    session: SignalboxProcessSession,
    activeTurnID: SignalboxCanonicalUUID,
    content: String
  ) async throws -> SignalboxPreparedTurnStop
  func stopTurn(
    _ prepared: SignalboxPreparedTurnStop
  ) async throws -> SignalboxInputSubmitted
  func makeSynchronization(
    sessionID: SignalboxCanonicalUUID,
    updates: @escaping @Sendable (SignalboxSessionSynchronizationDriverUpdate) async -> Void
  ) async -> any SignalboxSessionSynchronizing
}

extension SignalboxProcessServiceProtocol {
  public func listConversations(
    includeArchived _: Bool
  ) async throws -> [SignalboxProcessConversation] {
    throw SignalboxProcessServiceError.unexpectedMessage(
      "This process service does not implement unified conversation listing."
    )
  }

  public func listModelAliases() async throws -> [SignalboxModelAliasSummary] {
    throw SignalboxProcessServiceError.unexpectedMessage(
      "This process service does not implement the model-alias catalog."
    )
  }

  public func readSession(
    conversation _: SignalboxProcessConversation
  ) async throws -> SignalboxProcessSession {
    throw SignalboxProcessServiceError.unexpectedMessage(
      "This process service does not implement current session reads."
    )
  }

  public func readImportedConversation(
    conversation _: SignalboxProcessConversation
  ) async throws -> SignalboxImportedConversationTranscript {
    throw SignalboxProcessServiceError.unexpectedMessage(
      "This process service does not implement imported-conversation reads."
    )
  }

  public func setConversationArchived(
    _ archived: Bool,
    conversation: SignalboxProcessConversation
  ) async throws -> SignalboxProcessConversation {
    _ = archived
    _ = conversation
    throw SignalboxProcessServiceError.unexpectedMessage(
      "This process service does not implement unified archive mutations."
    )
  }

  public func prepareSessionCreation(
    modelSelection _: SignalboxModelSelection,
    systemPrompt _: String?
  ) async throws -> SignalboxPreparedSessionCreation {
    throw SignalboxProcessServiceError.unexpectedMessage(
      "This process service does not implement session creation."
    )
  }

  public func createSession(
    _ creation: SignalboxPreparedSessionCreation
  ) async throws -> SignalboxCanonicalUUID {
    _ = creation
    throw SignalboxProcessServiceError.unexpectedMessage(
      "This process service does not implement session creation."
    )
  }

  public func prepareImportedSessionCreation(
    conversation _: SignalboxProcessConversation,
    throughPosition _: SignalboxCanonicalUInt64,
    relationship _: SignalboxImportedSessionRelationship,
    modelSelection _: SignalboxModelSelection
  ) async throws -> SignalboxPreparedImportedSessionCreation {
    throw SignalboxProcessServiceError.unexpectedMessage(
      "This process service does not implement imported-conversation continuation."
    )
  }

  public func createSessionFromImportedFrontier(
    _ creation: SignalboxPreparedImportedSessionCreation
  ) async throws -> SignalboxCanonicalUUID {
    _ = creation
    throw SignalboxProcessServiceError.unexpectedMessage(
      "This process service does not implement imported-conversation continuation."
    )
  }

  public func prepareToolRequestDecision(
    sessionID _: SignalboxCanonicalUUID,
    toolRequestID _: SignalboxCanonicalUUID,
    decision _: SignalboxProcessToolDecision
  ) async throws -> SignalboxPreparedToolRequestDecision {
    throw SignalboxProcessServiceError.unexpectedMessage(
      "This process service does not implement tool decisions."
    )
  }

  public func decideToolRequest(
    _ prepared: SignalboxPreparedToolRequestDecision
  ) async throws -> SignalboxToolRequestDecided {
    _ = prepared
    throw SignalboxProcessServiceError.unexpectedMessage(
      "This process service does not implement tool decisions."
    )
  }

  public func prepareTurnStop(
    session _: SignalboxProcessSession,
    activeTurnID _: SignalboxCanonicalUUID,
    content _: String
  ) async throws -> SignalboxPreparedTurnStop {
    throw SignalboxProcessServiceError.unexpectedMessage(
      "This process service does not implement turn stops."
    )
  }

  public func stopTurn(
    _ prepared: SignalboxPreparedTurnStop
  ) async throws -> SignalboxInputSubmitted {
    _ = prepared
    throw SignalboxProcessServiceError.unexpectedMessage(
      "This process service does not implement turn stops."
    )
  }
}

/// Converts pull-framed wire sequences into bounded application values and
/// validates every identity, ordering, terminator, and capacity claim before
/// publishing a result. Partial sequences never escape this actor.
public actor SignalboxProcessService: SignalboxProcessServiceProtocol {
  private let requester: any SignalboxProcessRequesting
  private let policy: SignalboxProcessApplicationPolicy
  private let commandID: @Sendable () throws -> SignalboxCommandID
  private let wait: @Sendable (Duration) async throws -> Void

  public init(
    requester: any SignalboxProcessRequesting,
    policy: SignalboxProcessApplicationPolicy,
    commandID: @escaping @Sendable () throws -> SignalboxCommandID = {
      try SignalboxCommandID(validating: UUID().uuidString.lowercased())
    },
    wait: @escaping @Sendable (Duration) async throws -> Void = {
      try await Task.sleep(for: $0)
    }
  ) {
    self.requester = requester
    self.policy = policy
    self.commandID = commandID
    self.wait = wait
  }

  public func testConnection() async throws {
    _ = try await metadataPage(
      includeArchived: false,
      after: nil,
      pageSize: SignalboxCanonicalUInt64(rawValue: 1)
    )
  }

  public func listConversations(
    includeArchived: Bool
  ) async throws -> [SignalboxProcessConversation] {
    var conversations: [SignalboxProcessConversation] = []
    var cursor: SignalboxConversationCursor?
    var pageCount: UInt = 0
    var retainedUTF8Bytes: UInt = 0
    while true {
      guard pageCount < policy.maximumMetadataPages else {
        throw SignalboxProcessServiceError.invalidPage(
          "The native conversation-list page cap was reached."
        )
      }
      let page = try await conversationPage(
        includeArchived: includeArchived,
        after: cursor,
        pageSize: policy.metadataPageSize,
        maximumRetainedUTF8Bytes:
          policy.maximumMetadataListUTF8Bytes - retainedUTF8Bytes
      )
      retainedUTF8Bytes += page.retainedUTF8Bytes
      conversations.append(contentsOf: page.conversations)
      pageCount += 1
      guard let next = page.nextAfter else {
        return conversations
      }
      guard cursor.map({ conversationCursorPrecedes($0, next) }) ?? true else {
        throw SignalboxProcessServiceError.invalidPage(
          "The conversation page cursor did not advance."
        )
      }
      cursor = next
    }
  }

  public func listModelAliases() async throws -> [SignalboxModelAliasSummary] {
    try await withExchange(request: .listModelAliases) { exchange in
      var aliases: [SignalboxModelAliasSummary] = []
      var started = false
      while let frame = try await nextFrame(from: exchange) {
        switch frame.message {
        case .modelAliasesStart where !started:
          started = true
        case .modelAliasSummary(let alias) where started:
          guard
            aliases.count < SignalboxProcessProtocol.maximumModelAliasCatalogEntries
          else {
            throw SignalboxProcessServiceError.invalidPage(
              "The model-alias catalog exceeded the native retention cap."
            )
          }
          guard aliases.last.map({ $0.aliasID.rawValue < alias.aliasID.rawValue }) ?? true else {
            throw SignalboxProcessServiceError.invalidPage(
              "Model aliases were not in strict identity order."
            )
          }
          aliases.append(alias)
        case .modelAliasesEnd(let aliasCount) where started:
          guard aliasCount.rawValue == UInt64(aliases.count) else {
            throw SignalboxProcessServiceError.invalidPage(
              "The model-alias count did not match the sequence."
            )
          }
          return aliases
        case .protocolError(let error):
          throw remote(error)
        default:
          throw SignalboxProcessServiceError.unexpectedMessage(
            "The model-alias sequence was malformed."
          )
        }
      }
      throw SignalboxProcessServiceError.unexpectedMessage(
        "The model-alias sequence ended before its terminator."
      )
    }
  }

  public func listSessions(
    includeArchived: Bool
  ) async throws -> [SignalboxProcessSession] {
    var sessions: [SignalboxProcessSession] = []
    var cursor: SignalboxCanonicalUUID?
    var pageCount: UInt = 0
    var retainedUTF8Bytes: UInt = 0
    while true {
      guard pageCount < policy.maximumMetadataPages else {
        throw SignalboxProcessServiceError.invalidPage(
          "The native session-list page cap was reached."
        )
      }
      let page = try await metadataPage(
        includeArchived: includeArchived,
        after: cursor,
        pageSize: policy.metadataPageSize
      )
      for session in page.sessions {
        let titleBytes = UInt(session.title?.utf8.count ?? 0)
        let tagBytes = session.tags.reduce(UInt(0)) { partial, tag in
          partial + UInt(tag.utf8.count)
        }
        let (summaryBytes, summaryOverflowed) = titleBytes.addingReportingOverflow(tagBytes)
        let (nextBytes, listOverflowed) =
          retainedUTF8Bytes.addingReportingOverflow(summaryBytes)
        guard
          !summaryOverflowed,
          !listOverflowed,
          nextBytes <= policy.maximumMetadataListUTF8Bytes
        else {
          throw SignalboxProcessServiceError.invalidPage(
            "The native session list exceeded its retained UTF-8 byte limit."
          )
        }
        retainedUTF8Bytes = nextBytes
      }
      sessions.append(contentsOf: page.sessions)
      pageCount += 1
      guard let next = page.nextAfterSessionID else {
        return sessions
      }
      guard cursor.map({ $0.rawValue < next.rawValue }) ?? true else {
        throw SignalboxProcessServiceError.invalidPage(
          "The metadata page cursor did not advance."
        )
      }
      cursor = next
    }
  }

  public func readSession(
    conversation: SignalboxProcessConversation
  ) async throws -> SignalboxProcessSession {
    guard case .native(let native) = conversation.record else {
      throw SignalboxProcessServiceError.unexpectedMessage(
        "Imported conversations do not have live session defaults."
      )
    }
    async let metadata = readMetadata(sessionID: native.sessionID)
    async let defaults = readDefaults(sessionID: native.sessionID)
    let currentMetadata = try await metadata
    let currentDefaults = try await defaults
    guard currentMetadata.sessionID == native.sessionID,
      currentDefaults.sessionID == native.sessionID
    else {
      throw SignalboxProcessServiceError.unexpectedMessage(
        "The session read named a different session."
      )
    }
    return SignalboxProcessSession(
      id: native.sessionID,
      defaults: currentDefaults,
      metadata: currentMetadata.metadata
    )
  }

  public func readImportedConversation(
    conversation: SignalboxProcessConversation
  ) async throws -> SignalboxImportedConversationTranscript {
    guard case .imported(let imported) = conversation.record else {
      throw SignalboxProcessServiceError.unexpectedMessage(
        "Native sessions do not have imported transcript entries."
      )
    }
    guard imported.entryCount.rawValue <= UInt64(policy.maximumImportedEntries) else {
      throw SignalboxProcessServiceError.invalidPage(
        "The imported conversation exceeded the native entry-retention cap."
      )
    }
    return try await withExchange(
      request: .readImportedConversation(
        importedConversationID: imported.importedConversationID
      )
    ) { exchange in
      var entries: [SignalboxImportedConversationEntry] = []
      var entryIDs: Set<SignalboxCanonicalUUID> = []
      var retainedPreviewBytes: UInt = 0
      var started = false
      while let frame = try await nextFrame(from: exchange) {
        switch frame.message {
        case .importedConversationStart(let conversationID) where !started:
          guard conversationID == imported.importedConversationID else {
            throw SignalboxProcessServiceError.invalidPage(
              "The imported transcript start named a different conversation."
            )
          }
          started = true
        case .importedConversationEntry(let entry) where started:
          guard entries.count < policy.maximumImportedEntries else {
            throw SignalboxProcessServiceError.invalidPage(
              "The imported transcript exceeded the native entry-retention cap."
            )
          }
          guard entry.position.rawValue == UInt64(entries.count) + 1 else {
            throw SignalboxProcessServiceError.invalidPage(
              "Imported transcript positions were not contiguous and one-based."
            )
          }
          guard entryIDs.insert(entry.importedEntryID).inserted else {
            throw SignalboxProcessServiceError.invalidPage(
              "The imported transcript repeated an entry identity."
            )
          }
          let previewBytes = UInt(entry.textPreview?.preview.utf8.count ?? 0)
          let entryBytes = previewBytes.saturatedAdding(entry.retainedUnknownUTF8Bytes)
          let (nextBytes, overflowed) =
            retainedPreviewBytes.addingReportingOverflow(entryBytes)
          guard !overflowed, nextBytes <= policy.maximumImportedPreviewUTF8Bytes else {
            throw SignalboxProcessServiceError.invalidPage(
              "The imported transcript exceeded the native preview-retention cap."
            )
          }
          retainedPreviewBytes = nextBytes
          entries.append(entry)
        case .importedConversationEnd(let end) where started:
          guard end.importedConversationID == imported.importedConversationID else {
            throw SignalboxProcessServiceError.invalidPage(
              "The imported transcript end named a different conversation."
            )
          }
          guard end.entryCount.rawValue == UInt64(entries.count),
            end.entryCount == imported.entryCount
          else {
            throw SignalboxProcessServiceError.invalidPage(
              "The imported transcript count did not match its sequence and summary."
            )
          }
          return SignalboxImportedConversationTranscript(
            importedConversationID: imported.importedConversationID,
            entries: entries
          )
        case .protocolError(let error):
          throw remote(error)
        case .unknown(let kind, _, let diagnostic):
          throw SignalboxProcessServiceError.invalidPage(
            diagnostic?.message
              ?? "The imported transcript contained an unrecognized \(kind) message."
          )
        case .sessionCreated, .inputSubmitted, .toolRequestDecided, .sessionDefaults,
          .sessionsStart, .sessionSummary, .sessionsEnd, .sessionMetadataPageStart,
          .sessionMetadataSummary, .sessionMetadataPageEnd, .sessionMetadata,
          .sessionMetadataReplaced, .conversationImportInserted,
          .conversationImportAlreadyImported, .conversationPageStart,
          .conversationSummary, .conversationPageEnd, .importedConversationStart,
          .importedConversationEntry, .importedConversationEnd, .modelAliasesStart,
          .modelAliasSummary, .modelAliasesEnd, .transcriptSnapshotStart,
          .transcriptTurn, .transcriptModelCallUsage, .transcriptModelCallsEnd,
          .transcriptEntry, .transcriptUserEntry, .transcriptTextEntry, .transcriptContent,
          .transcriptSnapshotEnd, .sessionEvent, .providerTextDelta:
          throw SignalboxProcessServiceError.unexpectedMessage(
            "The imported transcript sequence was malformed."
          )
        }
      }
      throw SignalboxProcessServiceError.unexpectedMessage(
        "The imported transcript ended before its terminator."
      )
    }
  }

  public func setConversationArchived(
    _ archived: Bool,
    conversation: SignalboxProcessConversation
  ) async throws -> SignalboxProcessConversation {
    let session = try await readSession(conversation: conversation)
    _ = try await setArchived(archived, session: session)
    guard
      let refreshed = try await listConversations(includeArchived: true).first(where: {
        $0.id == conversation.id
      })
    else {
      throw SignalboxProcessServiceError.unexpectedMessage(
        "The archived conversation was absent from the refreshed unified list."
      )
    }
    return refreshed
  }

  public func setArchived(
    _ archived: Bool,
    session: SignalboxProcessSession
  ) async throws -> SignalboxProcessSession {
    let current = try await readMetadata(sessionID: session.id)
    guard current.sessionID == session.id else {
      throw SignalboxProcessServiceError.unexpectedMessage(
        "The metadata read named a different session."
      )
    }
    let replacement = SignalboxProcessSessionMetadata(
      title: current.metadata.title,
      tags: current.metadata.tags,
      attributes: current.metadata.attributes,
      archived: archived
    )
    let request = SignalboxProcessClientRequest.replaceSessionMetadata(
      commandID: try commandID(),
      sessionID: session.id,
      metadata: replacement
    )
    let receipt: SignalboxProcessSessionMetadataRead = try await mutation(
      request,
      success: { message in
        guard case .sessionMetadataReplaced(let receipt) = message else {
          return nil
        }
        return receipt
      }
    )
    guard receipt.sessionID == session.id else {
      throw SignalboxProcessServiceError.unexpectedMessage(
        "The metadata replacement receipt named a different session."
      )
    }
    guard metadataIsAdmissible(receipt.metadata) else {
      throw SignalboxProcessServiceError.unexpectedMessage(
        "The metadata replacement receipt violated the metadata contract."
      )
    }
    guard
      let refreshed = try await listSessions(includeArchived: true).first(where: {
        $0.id == session.id
      })
    else {
      throw SignalboxProcessServiceError.unexpectedMessage(
        "The archived session was absent from the refreshed metadata list."
      )
    }
    return refreshed
  }

  public func prepareInputSubmission(
    session: SignalboxProcessSession,
    content: String
  ) async throws -> SignalboxPreparedInputSubmission {
    SignalboxPreparedInputSubmission(
      commandID: try commandID(),
      sessionID: session.id,
      content: content,
      expectedDefaultsVersion: session.defaultsVersion,
      modelSelection: session.modelSelection
    )
  }

  public func submit(
    _ submission: SignalboxPreparedInputSubmission
  ) async throws -> SignalboxInputSubmitted {
    let submitted: SignalboxInputSubmitted = try await mutation(
      submission.request,
      success: { message in
        guard case .inputSubmitted(let submitted) = message else {
          return nil
        }
        return submitted
      }
    )
    guard submitted.sessionID == submission.sessionID else {
      throw SignalboxProcessServiceError.unexpectedMessage(
        "The input-submission receipt named a different session."
      )
    }
    guard submitted.modelSettings.matches(submission.modelSelection) else {
      throw SignalboxProcessServiceError.unexpectedMessage(
        "The input-submission receipt settings named a different direct model."
      )
    }
    return submitted
  }

  public func prepareSessionCreation(
    modelSelection: SignalboxModelSelection,
    systemPrompt: String?
  ) async throws -> SignalboxPreparedSessionCreation {
    SignalboxPreparedSessionCreation(
      commandID: try commandID(),
      modelSelection: modelSelection,
      systemPrompt: systemPrompt
    )
  }

  public func createSession(
    _ creation: SignalboxPreparedSessionCreation
  ) async throws -> SignalboxCanonicalUUID {
    try await mutation(
      creation.request,
      success: { message in
        guard case .sessionCreated(let sessionID, let modelSettings) = message,
          modelSettings.matches(creation.modelSelection)
        else {
          return nil
        }
        return sessionID
      }
    )
  }

  public func prepareImportedSessionCreation(
    conversation: SignalboxProcessConversation,
    throughPosition: SignalboxCanonicalUInt64,
    relationship: SignalboxImportedSessionRelationship,
    modelSelection: SignalboxModelSelection
  ) async throws -> SignalboxPreparedImportedSessionCreation {
    guard case .imported(let imported) = conversation.record else {
      throw SignalboxProcessServiceError.unexpectedMessage(
        "Only imported conversations can be continued from an imported frontier."
      )
    }
    guard throughPosition.rawValue > 0,
      throughPosition.rawValue <= imported.entryCount.rawValue
    else {
      throw SignalboxProcessServiceError.unexpectedMessage(
        "The selected imported frontier is outside the conversation."
      )
    }
    return SignalboxPreparedImportedSessionCreation(
      commandID: try commandID(),
      importedConversationID: imported.importedConversationID,
      throughPosition: throughPosition,
      relationship: relationship,
      modelSelection: modelSelection
    )
  }

  public func createSessionFromImportedFrontier(
    _ creation: SignalboxPreparedImportedSessionCreation
  ) async throws -> SignalboxCanonicalUUID {
    try await mutation(
      creation.request,
      success: { message in
        guard case .sessionCreated(let sessionID, let modelSettings) = message,
          modelSettings.matches(creation.modelSelection)
        else {
          return nil
        }
        return sessionID
      }
    )
  }

  public func prepareToolRequestDecision(
    sessionID: SignalboxCanonicalUUID,
    toolRequestID: SignalboxCanonicalUUID,
    decision: SignalboxProcessToolDecision
  ) async throws -> SignalboxPreparedToolRequestDecision {
    SignalboxPreparedToolRequestDecision(
      commandID: try commandID(),
      sessionID: sessionID,
      toolRequestID: toolRequestID,
      decision: decision
    )
  }

  public func decideToolRequest(
    _ prepared: SignalboxPreparedToolRequestDecision
  ) async throws -> SignalboxToolRequestDecided {
    let decided: SignalboxToolRequestDecided = try await mutation(
      prepared.request,
      success: { message in
        guard case .toolRequestDecided(let decided) = message else {
          return nil
        }
        return decided
      }
    )
    guard decided.toolRequestID == prepared.toolRequestID,
      decided.decision == prepared.decision
    else {
      throw SignalboxProcessServiceError.unexpectedMessage(
        "The tool-decision receipt did not echo the requested decision."
      )
    }
    return decided
  }

  public func prepareTurnStop(
    session: SignalboxProcessSession,
    activeTurnID: SignalboxCanonicalUUID,
    content: String
  ) async throws -> SignalboxPreparedTurnStop {
    SignalboxPreparedTurnStop(
      commandID: try commandID(),
      sessionID: session.id,
      activeTurnID: activeTurnID,
      content: content,
      expectedDefaultsVersion: session.defaultsVersion,
      modelSelection: session.modelSelection
    )
  }

  public func stopTurn(
    _ prepared: SignalboxPreparedTurnStop
  ) async throws -> SignalboxInputSubmitted {
    let submitted: SignalboxInputSubmitted = try await mutation(
      prepared.request,
      success: { message in
        guard case .inputSubmitted(let submitted) = message else {
          return nil
        }
        return submitted
      }
    )
    guard submitted.sessionID == prepared.sessionID else {
      throw SignalboxProcessServiceError.unexpectedMessage(
        "The stop receipt named a different session."
      )
    }
    guard submitted.modelSettings.matches(prepared.modelSelection) else {
      throw SignalboxProcessServiceError.unexpectedMessage(
        "The stop receipt settings named a different direct model."
      )
    }
    return submitted
  }

  public func makeSynchronization(
    sessionID: SignalboxCanonicalUUID,
    updates: @escaping @Sendable (SignalboxSessionSynchronizationDriverUpdate) async -> Void
  ) async -> any SignalboxSessionSynchronizing {
    SignalboxSessionSynchronizationDriver(
      requester: requester,
      sessionID: sessionID,
      policy: policy.synchronization,
      updates: updates
    )
  }

  private struct MetadataPage {
    let sessions: [SignalboxProcessSession]
    let nextAfterSessionID: SignalboxCanonicalUUID?
  }

  private struct ConversationPage {
    let conversations: [SignalboxProcessConversation]
    let nextAfter: SignalboxConversationCursor?
    let retainedUTF8Bytes: UInt
  }

  private func conversationPage(
    includeArchived: Bool,
    after cursor: SignalboxConversationCursor?,
    pageSize: SignalboxCanonicalUInt64,
    maximumRetainedUTF8Bytes: UInt
  ) async throws -> ConversationPage {
    try await withExchange(
      request: .listConversations(
        titleContains: nil,
        origin: .all,
        includeArchived: includeArchived,
        pageSize: pageSize,
        after: cursor
      )
    ) { exchange in
      var conversations: [SignalboxProcessConversation] = []
      var retainedUTF8Bytes: UInt = 0
      var started = false
      var priorCursor: SignalboxConversationCursor?
      while let frame = try await nextFrame(from: exchange) {
        switch frame.message {
        case .conversationPageStart where !started:
          started = true
        case .conversationSummary(let summary) where started:
          let conversation = SignalboxProcessConversation(summary: summary)
          guard conversationIsAdmissible(conversation) else {
            throw SignalboxProcessServiceError.invalidPage(
              "A unified conversation summary violated its protocol contract."
            )
          }
          let summaryCursor = conversationCursor(for: conversation)
          guard cursor.map({ conversationCursorPrecedes($0, summaryCursor) }) ?? true else {
            throw SignalboxProcessServiceError.invalidPage(
              "A conversation summary did not advance beyond the request cursor."
            )
          }
          guard priorCursor.map({ conversationCursorPrecedes($0, summaryCursor) }) ?? true else {
            throw SignalboxProcessServiceError.invalidPage(
              "Conversation summaries were not in strict unified order."
            )
          }
          guard conversations.count < Int(pageSize.rawValue) else {
            throw SignalboxProcessServiceError.invalidPage(
              "The conversation page exceeded its requested row limit."
            )
          }
          let conversationBytes = conversation.retainedUTF8Bytes
          let (nextBytes, overflowed) =
            retainedUTF8Bytes.addingReportingOverflow(conversationBytes)
          guard !overflowed, nextBytes <= maximumRetainedUTF8Bytes else {
            throw SignalboxProcessServiceError.invalidPage(
              "The native conversation list exceeded its retained UTF-8 byte limit."
            )
          }
          retainedUTF8Bytes = nextBytes
          priorCursor = summaryCursor
          conversations.append(conversation)
        case .conversationPageEnd(let end) where started:
          guard end.conversationCount.rawValue == UInt64(conversations.count) else {
            throw SignalboxProcessServiceError.invalidPage(
              "The conversation page count did not match its summaries."
            )
          }
          if let next = end.nextAfter {
            guard priorCursor == next else {
              throw SignalboxProcessServiceError.invalidPage(
                "The conversation page cursor did not match its last emitted summary."
              )
            }
          } else if conversations.count == Int(pageSize.rawValue) {
            // A full terminal page is valid: the server proved no later match
            // existed in the same repeatable-read snapshot.
          }
          return ConversationPage(
            conversations: conversations,
            nextAfter: end.nextAfter,
            retainedUTF8Bytes: retainedUTF8Bytes
          )
        case .protocolError(let error):
          throw remote(error)
        case .unknown(let kind, _, let diagnostic):
          throw SignalboxProcessServiceError.invalidPage(
            diagnostic?.message
              ?? "The conversation page contained an unrecognized \(kind) message."
          )
        default:
          throw SignalboxProcessServiceError.unexpectedMessage(
            "The conversation page contained an out-of-order message."
          )
        }
      }
      throw SignalboxProcessServiceError.invalidPage(
        "The conversation page ended before its terminal boundary."
      )
    }
  }

  private func conversationIsAdmissible(
    _ conversation: SignalboxProcessConversation
  ) -> Bool {
    switch conversation.record {
    case .native(let native):
      return native.defaultsVersion.rawValue > 0
        && metadataTitleIsAdmissible(native.title)
    case .imported(let imported):
      return imported.entryCount.rawValue > 0
        && signalboxImportedConversationTitleIsAdmissible(imported.title)
    }
  }

  private func metadataTitleIsAdmissible(_ title: String?) -> Bool {
    title.map {
      !$0.isEmpty
        && !$0.unicodeScalars.contains("\0")
        && $0.utf8.count <= SignalboxProcessProtocol.maximumConversationTitleUTF8Bytes
    } ?? true
  }

  private func conversationCursor(
    for conversation: SignalboxProcessConversation
  ) -> SignalboxConversationCursor {
    switch conversation.record {
    case .native(let native):
      return SignalboxConversationCursor(
        origin: .nativeSession,
        conversationID: native.sessionID
      )
    case .imported(let imported):
      return SignalboxConversationCursor(
        origin: .importedConversation,
        conversationID: imported.importedConversationID
      )
    }
  }

  private func conversationCursorPrecedes(
    _ earlier: SignalboxConversationCursor,
    _ later: SignalboxConversationCursor
  ) -> Bool {
    if earlier.conversationID.rawValue != later.conversationID.rawValue {
      return earlier.conversationID.rawValue < later.conversationID.rawValue
    }
    return earlier.origin == .nativeSession && later.origin == .importedConversation
  }

  private func metadataPage(
    includeArchived: Bool,
    after cursor: SignalboxCanonicalUUID?,
    pageSize: SignalboxCanonicalUInt64
  ) async throws -> MetadataPage {
    try await withExchange(
      request: .listSessionMetadata(
        requiredTags: [],
        titleContains: nil,
        includeArchived: includeArchived,
        pageSize: pageSize,
        afterSessionID: cursor
      )
    ) { exchange in
      var sessions: [SignalboxProcessSession] = []
      var skippedMalformedSummaries: UInt64 = 0
      var started = false
      var priorSessionID: String?
      var cursorValidationIsComplete = true
      while let frame = try await nextFrame(from: exchange) {
        switch frame.message {
        case .sessionMetadataPageStart where !started:
          started = true
        case .sessionMetadataSummary(let summary) where started:
          guard cursor.map({ $0.rawValue < summary.sessionID.rawValue }) ?? true else {
            throw SignalboxProcessServiceError.invalidPage(
              "A metadata summary did not advance beyond the request cursor."
            )
          }
          guard priorSessionID.map({ $0 < summary.sessionID.rawValue }) ?? true else {
            throw SignalboxProcessServiceError.invalidPage(
              "Metadata summaries were not in strict session-identity order."
            )
          }
          priorSessionID = summary.sessionID.rawValue
          if metadataSummaryIsAdmissible(summary) {
            sessions.append(SignalboxProcessSession(summary: summary))
          } else {
            skippedMalformedSummaries += 1
          }
          try validateMetadataPageCapacity(
            admittedCount: sessions.count,
            malformedCount: skippedMalformedSummaries,
            pageSize: pageSize
          )
        case .sessionMetadataPageEnd(let end) where started:
          guard end.sessionCount.rawValue == UInt64(sessions.count) + skippedMalformedSummaries
          else {
            throw SignalboxProcessServiceError.invalidPage(
              "The metadata page count did not match its admitted summaries."
            )
          }
          if let next = end.nextAfterSessionID {
            guard cursor.map({ $0.rawValue < next.rawValue }) ?? true else {
              throw SignalboxProcessServiceError.invalidPage(
                "The metadata page cursor did not advance beyond its request cursor."
              )
            }
            guard priorSessionID.map({ $0 <= next.rawValue }) ?? true else {
              throw SignalboxProcessServiceError.invalidPage(
                "The metadata page cursor regressed behind an admitted summary."
              )
            }
            if cursorValidationIsComplete, next.rawValue != priorSessionID {
              throw SignalboxProcessServiceError.invalidPage(
                "The metadata page cursor did not match its last emitted identity."
              )
            }
          }
          return MetadataPage(
            sessions: sessions,
            nextAfterSessionID: end.nextAfterSessionID
          )
        case .unknown(let kind, let payload, _) where started:
          if kind == "session_metadata_summary" {
            skippedMalformedSummaries += 1
            if case .string(let rawSessionID) = payload["session_id"],
              let sessionID = try? SignalboxCanonicalUUID(validating: rawSessionID),
              cursor.map({ $0.rawValue < sessionID.rawValue }) ?? true,
              priorSessionID.map({ $0 < sessionID.rawValue }) ?? true
            {
              priorSessionID = sessionID.rawValue
            } else {
              cursorValidationIsComplete = false
            }
            try validateMetadataPageCapacity(
              admittedCount: sessions.count,
              malformedCount: skippedMalformedSummaries,
              pageSize: pageSize
            )
          }
        case .unknown:
          continue
        case .protocolError(let error):
          throw remote(error)
        default:
          throw SignalboxProcessServiceError.unexpectedMessage(
            "The metadata page contained an out-of-order message."
          )
        }
      }
      throw SignalboxProcessServiceError.invalidPage(
        "The metadata page ended before its terminal boundary."
      )
    }
  }

  private func metadataSummaryIsAdmissible(
    _ summary: SignalboxProcessSessionMetadataSummary
  ) -> Bool {
    guard summary.tags.count <= SignalboxProcessProtocol.maximumMetadataTags else {
      return false
    }
    guard tagsAreStrictlyIncreasingUTF8(summary.tags) else {
      return false
    }
    guard summary.title.map(metadataStringIsAdmissible) ?? true,
      summary.tags.allSatisfy(indexedMetadataStringIsAdmissible)
    else {
      return false
    }
    let titleBytes = summary.title?.utf8.count ?? 0
    let tagBytes = summary.tags.reduce(0) { $0 + $1.utf8.count }
    return titleBytes + tagBytes <= SignalboxProcessProtocol.maximumMetadataSummaryUTF8Bytes
  }

  private func metadataIsAdmissible(
    _ metadata: SignalboxProcessSessionMetadata
  ) -> Bool {
    guard metadata.tags.count <= SignalboxProcessProtocol.maximumMetadataTags,
      metadata.attributes.count <= SignalboxProcessProtocol.maximumMetadataAttributes,
      tagsHaveUniqueUTF8(metadata.tags),
      metadata.title.map(metadataStringIsAdmissible) ?? true,
      metadata.tags.allSatisfy(indexedMetadataStringIsAdmissible),
      metadata.attributes.keys.allSatisfy(indexedMetadataStringIsAdmissible),
      metadata.attributes.values.allSatisfy(metadataValueIsAdmissible)
    else {
      return false
    }
    let titleBytes = metadata.title?.utf8.count ?? 0
    let tagBytes = metadata.tags.reduce(0) { $0 + $1.utf8.count }
    let attributeBytes = metadata.attributes.reduce(0) {
      $0 + $1.key.utf8.count + $1.value.utf8.count
    }
    return titleBytes + tagBytes + attributeBytes
      <= SignalboxProcessProtocol.maximumMetadataUTF8Bytes
  }

  private func metadataStringIsAdmissible(_ value: String) -> Bool {
    !value.isEmpty && !value.unicodeScalars.contains("\0")
  }

  private func indexedMetadataStringIsAdmissible(_ value: String) -> Bool {
    metadataStringIsAdmissible(value)
      && value.utf8.count <= SignalboxProcessProtocol.maximumIndexedMetadataUTF8Bytes
  }

  private func metadataValueIsAdmissible(_ value: String) -> Bool {
    !value.unicodeScalars.contains("\0")
  }

  private func tagsAreStrictlyIncreasingUTF8(_ tags: [String]) -> Bool {
    zip(tags, tags.dropFirst()).allSatisfy { earlier, later in
      earlier.utf8.lexicographicallyPrecedes(later.utf8)
    }
  }

  private func tagsHaveUniqueUTF8(_ tags: [String]) -> Bool {
    Set(tags.map { Data($0.utf8) }).count == tags.count
  }

  private func validateMetadataPageCapacity(
    admittedCount: Int,
    malformedCount: UInt64,
    pageSize: SignalboxCanonicalUInt64
  ) throws {
    guard UInt64(admittedCount) + malformedCount <= pageSize.rawValue else {
      throw SignalboxProcessServiceError.invalidPage(
        "The metadata page exceeded its requested row limit."
      )
    }
  }

  private func readMetadata(
    sessionID: SignalboxCanonicalUUID
  ) async throws -> SignalboxProcessSessionMetadataRead {
    try await withExchange(
      request: .readSessionMetadata(sessionID: sessionID)
    ) { exchange in
      while let frame = try await nextFrame(from: exchange) {
        switch frame.message {
        case .sessionMetadata(let metadata):
          guard metadataIsAdmissible(metadata.metadata) else {
            throw SignalboxProcessServiceError.unexpectedMessage(
              "The metadata read violated the metadata contract."
            )
          }
          return metadata
        case .unknown:
          continue
        case .protocolError(let error):
          throw remote(error)
        default:
          throw SignalboxProcessServiceError.unexpectedMessage(
            "The metadata read returned an unrelated message."
          )
        }
      }
      throw SignalboxProcessServiceError.unexpectedMessage(
        "The metadata read closed without a current snapshot."
      )
    }
  }

  private func readDefaults(
    sessionID: SignalboxCanonicalUUID
  ) async throws -> SignalboxSessionDefaultsRead {
    try await withExchange(
      request: .readSessionDefaults(
        sessionID: sessionID,
        defaultsVersion: nil
      )
    ) { exchange in
      while let frame = try await nextFrame(from: exchange) {
        switch frame.message {
        case .sessionDefaults(let defaults):
          guard defaults.sessionID == sessionID else {
            throw SignalboxProcessServiceError.unexpectedMessage(
              "The defaults read named a different session."
            )
          }
          return defaults
        case .protocolError(let error):
          throw remote(error)
        case .unknown(let kind, _, let diagnostic):
          throw SignalboxProcessServiceError.unexpectedMessage(
            diagnostic?.message
              ?? "The defaults read returned an unrecognized \(kind) message."
          )
        default:
          throw SignalboxProcessServiceError.unexpectedMessage(
            "The defaults read returned an unrelated message."
          )
        }
      }
      throw SignalboxProcessServiceError.unexpectedMessage(
        "The defaults read closed without a current snapshot."
      )
    }
  }

  private enum DeadlineResult<Success: Sendable>: Sendable {
    case value(Success)
    case expired
  }

  private func nextFrame(
    from exchange: any SignalboxProcessExchange
  ) async throws -> SignalboxProcessServerFrame? {
    try await withTaskCancellationHandler {
      try await withThrowingTaskGroup(
        of: DeadlineResult<SignalboxProcessServerFrame?>.self
      ) { group in
        group.addTask {
          .value(try await exchange.next())
        }
        group.addTask {
          try await Task.sleep(for: self.policy.oneShotResponseDeadline)
          return .expired
        }
        guard let first = try await group.next() else {
          throw CancellationError()
        }
        group.cancelAll()
        switch first {
        case .value(let frame):
          return frame
        case .expired:
          await exchange.close()
          throw SignalboxProcessServiceError.deadlineExceeded(
            "The process request exceeded its response deadline."
          )
        }
      }
    } onCancel: {
      Task {
        await exchange.close()
      }
    }
  }

  private func withExchange<Success: Sendable>(
    request: SignalboxProcessClientRequest,
    body: (any SignalboxProcessExchange) async throws -> Success
  ) async throws -> Success {
    let exchange = try await openExchange(request)
    do {
      let result = try await body(exchange)
      await exchange.close()
      return result
    } catch {
      await exchange.close()
      throw error
    }
  }

  private func openExchange(
    _ request: SignalboxProcessClientRequest
  ) async throws -> any SignalboxProcessExchange {
    try await withThrowingTaskGroup(
      of: DeadlineResult<any SignalboxProcessExchange>.self
    ) { group in
      group.addTask {
        .value(try await self.requester.open(request))
      }
      group.addTask {
        try await Task.sleep(for: self.policy.oneShotResponseDeadline)
        return .expired
      }
      guard let first = try await group.next() else {
        throw CancellationError()
      }
      group.cancelAll()
      switch first {
      case .value(let exchange):
        return exchange
      case .expired:
        do {
          while let remaining = try await group.next() {
            if case .value(let exchange) = remaining {
              await exchange.close()
            }
          }
        } catch {
          // The opening task observed cancellation after the deadline won.
        }
        throw SignalboxProcessServiceError.deadlineExceeded(
          "The process request exceeded its response deadline while opening."
        )
      }
    }
  }

  private func mutation<Success: Sendable>(
    _ request: SignalboxProcessClientRequest,
    success: (SignalboxProcessServerMessage) -> Success?
  ) async throws -> Success {
    var retryIndex = 0
    while true {
      let frame: SignalboxProcessServerFrame?
      do {
        frame = try await withExchange(request: request) { exchange in
          try await nextFrame(from: exchange)
        }
      } catch let error as CancellationError {
        throw error
      } catch let error as SignalboxProcessRequestOpenError {
        if case .definitelyUnsent = error {
          throw error
        }
        let delay = try ambiguousMutationRetryDelay(
          at: retryIndex,
          message: error.localizedDescription
        )
        retryIndex += 1
        try await wait(delay)
        continue
      } catch {
        let delay = try ambiguousMutationRetryDelay(
          at: retryIndex,
          message: error.localizedDescription
        )
        retryIndex += 1
        try await wait(delay)
        continue
      }
      guard let frame else {
        let delay = try ambiguousMutationRetryDelay(
          at: retryIndex,
          message: "The mutation connection closed without a receipt."
        )
        retryIndex += 1
        try await wait(delay)
        continue
      }
      if let value = success(frame.message) {
        return value
      }
      guard case .protocolError(let error) = frame.message else {
        if case .unknown = frame.message {
          let delay = try ambiguousMutationRetryDelay(
            at: retryIndex,
            message: "The mutation receipt could not be decoded."
          )
          retryIndex += 1
          try await wait(delay)
          continue
        }
        throw SignalboxProcessServiceError.unexpectedMessage(
          "The mutation returned an unrelated message."
        )
      }
      switch error.code {
      case .commitAmbiguous:
        break
      case .malformedFrame, .unsupportedVersion, .invalidRequest, .notFound,
        .conflictingReuse, .rejected, .resyncRequired, .unavailable, .internal, .unknown:
        throw remote(error)
      }
      let delay = try ambiguousMutationRetryDelay(
        at: retryIndex,
        code: error.code,
        message: error.message
      )
      retryIndex += 1
      try await wait(delay)
    }
  }

  private func ambiguousMutationRetryDelay(
    at retryIndex: Int,
    code: SignalboxProcessErrorCode = .commitAmbiguous,
    message: String
  ) throws -> Duration {
    guard policy.ambiguousMutationRetryDelays.indices.contains(retryIndex) else {
      throw SignalboxProcessServiceError.mutationRetryExhausted(
        code: code,
        message: message
      )
    }
    return policy.ambiguousMutationRetryDelays[retryIndex]
  }

  private func remote(
    _ error: SignalboxProcessError
  ) -> SignalboxProcessServiceError {
    .remote(code: error.code, message: error.message, detail: error.detail)
  }
}

extension SignalboxImportedConversationSourceFormat {
  fileprivate var retainedUnknownUTF8Bytes: UInt {
    if case .unknown(let value) = self {
      return UInt(value.utf8.count)
    }
    return 0
  }
}

extension SignalboxProcessConversation {
  fileprivate var retainedUTF8Bytes: UInt {
    UInt(title?.utf8.count ?? 0).saturatedAdding(
      importedSourceFormat?.retainedUnknownUTF8Bytes ?? 0
    )
  }
}

extension SignalboxImportedConversationEntry {
  fileprivate var retainedUnknownUTF8Bytes: UInt {
    contentKind.retainedUnknownUTF8Bytes.saturatedAdding(
      sourceSpeaker.retainedUnknownUTF8Bytes
    )
  }
}

extension SignalboxImportedContentKind {
  fileprivate var retainedUnknownUTF8Bytes: UInt {
    if case .unknown(let value) = self {
      return UInt(value.utf8.count)
    }
    return 0
  }
}

extension SignalboxImportedSourceSpeaker {
  fileprivate var retainedUnknownUTF8Bytes: UInt {
    switch self {
    case .attested(.unknown(let value)):
      return UInt(value.utf8.count)
    case .unknown(let kind, let payload):
      let payloadBytes =
        (try? SignalboxJSONCoding.encoder().encode(payload)).map { UInt($0.count) } ?? .max
      return UInt(kind.utf8.count).saturatedAdding(payloadBytes)
    case .notAttested, .attestedAbsent, .attested:
      return 0
    }
  }
}

extension UInt {
  fileprivate func saturatedAdding(_ other: UInt) -> UInt {
    let (sum, overflowed) = addingReportingOverflow(other)
    return overflowed ? .max : sum
  }
}
