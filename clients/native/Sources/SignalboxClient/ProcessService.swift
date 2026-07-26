import Foundation

#if canImport(SignalboxModels)
  import SignalboxModels
#endif

public enum SignalboxProcessServiceError: LocalizedError, Equatable {
  case unexpectedMessage(String)
  case invalidPage(String)
  case deadlineExceeded(String)
  case remote(code: SignalboxProcessErrorCode, message: String)
  case mutationRetryExhausted(code: SignalboxProcessErrorCode, message: String)

  public var errorDescription: String? {
    switch self {
    case .unexpectedMessage(let message), .invalidPage(let message),
      .deadlineExceeded(let message):
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
  public let oneShotResponseDeadline: Duration
  public let synchronization: SignalboxSessionSynchronizationPolicy

  public init(
    metadataPageSize: SignalboxCanonicalUInt64,
    maximumMetadataPages: UInt,
    ambiguousMutationRetryDelays: [Duration],
    oneShotResponseDeadline: Duration = .seconds(20),
    synchronization: SignalboxSessionSynchronizationPolicy
  ) {
    self.metadataPageSize = metadataPageSize
    self.maximumMetadataPages = maximumMetadataPages
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
    guard submitted.sessionID == submission.sessionID else {
      throw SignalboxProcessServiceError.unexpectedMessage(
        "The input-submission receipt named a different session."
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
      guard error.code == .commitAmbiguous else {
        throw remote(error)
      }
      let delay = try ambiguousMutationRetryDelay(
        at: retryIndex,
        message: error.message
      )
      retryIndex += 1
      try await wait(delay)
    }
  }

  private func ambiguousMutationRetryDelay(
    at retryIndex: Int,
    message: String
  ) throws -> Duration {
    guard policy.ambiguousMutationRetryDelays.indices.contains(retryIndex) else {
      throw SignalboxProcessServiceError.mutationRetryExhausted(
        code: .commitAmbiguous,
        message: message
      )
    }
    return policy.ambiguousMutationRetryDelays[retryIndex]
  }

  private func remote(
    _ error: SignalboxProcessError
  ) -> SignalboxProcessServiceError {
    .remote(code: error.code, message: error.message)
  }
}
