import Foundation

#if canImport(SignalboxModels)
  import SignalboxModels
#endif

public enum SignalboxProcessServiceError: LocalizedError, Equatable {
  case unexpectedMessage(String)
  case invalidPage(String)
  case remote(code: SignalboxProcessErrorCode, message: String)
  case mutationRetryExhausted(code: SignalboxProcessErrorCode, message: String)

  public var errorDescription: String? {
    switch self {
    case .unexpectedMessage(let message), .invalidPage(let message):
      return message
    case .remote(let code, let message):
      return "\(code.rawValue): \(message)"
    case .mutationRetryExhausted(let code, let message):
      return "\(code.rawValue): \(message) The exact command can be retried."
    }
  }
}

public struct SignalboxProcessApplicationPolicy: Equatable, Sendable {
  public let metadataPageSize: SignalboxCanonicalUInt64
  public let maximumMetadataPages: UInt
  public let ambiguousMutationRetryDelays: [Duration]
  public let synchronization: SignalboxSessionSynchronizationPolicy

  public init(
    metadataPageSize: SignalboxCanonicalUInt64,
    maximumMetadataPages: UInt,
    ambiguousMutationRetryDelays: [Duration],
    synchronization: SignalboxSessionSynchronizationPolicy
  ) {
    self.metadataPageSize = metadataPageSize
    self.maximumMetadataPages = maximumMetadataPages
    self.ambiguousMutationRetryDelays = ambiguousMutationRetryDelays
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

public struct SignalboxPreparedInputSubmission: Equatable, Sendable {
  public let commandID: SignalboxCommandID
  public let sessionID: SignalboxCanonicalUUID
  public let content: String
  public let expectedDefaultsVersion: SignalboxCanonicalUInt64

  public init(
    commandID: SignalboxCommandID,
    sessionID: SignalboxCanonicalUUID,
    content: String,
    expectedDefaultsVersion: SignalboxCanonicalUInt64
  ) {
    self.commandID = commandID
    self.sessionID = sessionID
    self.content = content
    self.expectedDefaultsVersion = expectedDefaultsVersion
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

public protocol SignalboxProcessServiceProtocol: Sendable {
  func testConnection() async throws
  func listSessions(includeArchived: Bool) async throws -> [SignalboxProcessSession]
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
  func makeSynchronization(
    sessionID: SignalboxCanonicalUUID,
    updates: @escaping @Sendable (SignalboxSessionSynchronizationDriverUpdate) async -> Void
  ) async -> any SignalboxSessionSynchronizing
}

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

  public func listSessions(
    includeArchived: Bool
  ) async throws -> [SignalboxProcessSession] {
    var sessions: [SignalboxProcessSession] = []
    var cursor: SignalboxCanonicalUUID?
    var pageCount: UInt = 0
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

  public func setArchived(
    _ archived: Bool,
    session: SignalboxProcessSession
  ) async throws -> SignalboxProcessSession {
    let current = try await readMetadata(sessionID: session.id)
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
    return SignalboxProcessSession(
      summary: SignalboxProcessSessionMetadataSummary(
        sessionID: session.id,
        defaultsVersion: session.defaultsVersion,
        modelSelection: session.modelSelection,
        dangerousToolAutoApproval: session.dangerousToolAutoApproval,
        title: receipt.metadata.title,
        tags: receipt.metadata.tags,
        archived: receipt.metadata.archived,
        lastWriter: receipt.lastWriter
      )
    )
  }

  public func prepareInputSubmission(
    session: SignalboxProcessSession,
    content: String
  ) async throws -> SignalboxPreparedInputSubmission {
    SignalboxPreparedInputSubmission(
      commandID: try commandID(),
      sessionID: session.id,
      content: content,
      expectedDefaultsVersion: session.defaultsVersion
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
      while let frame = try await exchange.next() {
        switch frame.message {
        case .sessionMetadataPageStart where !started:
          started = true
        case .sessionMetadataSummary(let summary) where started:
          guard priorSessionID.map({ $0 < summary.sessionID.rawValue }) ?? true else {
            throw SignalboxProcessServiceError.invalidPage(
              "Metadata summaries were not in strict session-identity order."
            )
          }
          priorSessionID = summary.sessionID.rawValue
          sessions.append(SignalboxProcessSession(summary: summary))
        case .sessionMetadataPageEnd(let end) where started:
          guard end.sessionCount.rawValue == UInt64(sessions.count) + skippedMalformedSummaries
          else {
            throw SignalboxProcessServiceError.invalidPage(
              "The metadata page count did not match its admitted summaries."
            )
          }
          return MetadataPage(
            sessions: sessions,
            nextAfterSessionID: end.nextAfterSessionID
          )
        case .unknown(let kind, _, _):
          if kind == "session_metadata_summary" {
            skippedMalformedSummaries += 1
          }
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

  private func readMetadata(
    sessionID: SignalboxCanonicalUUID
  ) async throws -> SignalboxProcessSessionMetadataRead {
    try await withExchange(
      request: .readSessionMetadata(sessionID: sessionID)
    ) { exchange in
      while let frame = try await exchange.next() {
        switch frame.message {
        case .sessionMetadata(let metadata):
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

  private func withExchange<Success: Sendable>(
    request: SignalboxProcessClientRequest,
    body: (any SignalboxProcessExchange) async throws -> Success
  ) async throws -> Success {
    let exchange = try await requester.open(request)
    do {
      let result = try await body(exchange)
      await exchange.close()
      return result
    } catch {
      await exchange.close()
      throw error
    }
  }

  private func mutation<Success: Sendable>(
    _ request: SignalboxProcessClientRequest,
    success: (SignalboxProcessServerMessage) -> Success?
  ) async throws -> Success {
    var retryIndex = 0
    while true {
      let frame = try await withExchange(request: request) { exchange in
        try await exchange.next()
      }
      guard let frame else {
        throw SignalboxProcessServiceError.unexpectedMessage(
          "The mutation connection closed without a receipt."
        )
      }
      if let value = success(frame.message) {
        return value
      }
      guard case .protocolError(let error) = frame.message else {
        if case .unknown = frame.message {
          throw SignalboxProcessServiceError.unexpectedMessage(
            "The mutation receipt could not be decoded."
          )
        }
        throw SignalboxProcessServiceError.unexpectedMessage(
          "The mutation returned an unrelated message."
        )
      }
      guard error.code == .commitAmbiguous else {
        throw remote(error)
      }
      guard policy.ambiguousMutationRetryDelays.indices.contains(retryIndex) else {
        throw SignalboxProcessServiceError.mutationRetryExhausted(
          code: error.code,
          message: error.message
        )
      }
      let delay = policy.ambiguousMutationRetryDelays[retryIndex]
      retryIndex += 1
      try await wait(delay)
    }
  }

  private func remote(
    _ error: SignalboxProcessError
  ) -> SignalboxProcessServiceError {
    .remote(code: error.code, message: error.message)
  }
}
