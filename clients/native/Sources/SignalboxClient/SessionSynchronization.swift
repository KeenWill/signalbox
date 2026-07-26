import Foundation

#if canImport(SignalboxModels)
  import SignalboxModels
#endif

public struct SignalboxSynchronizationDeadlines: Equatable, Sendable {
  public let connect: Duration
  public let hello: Duration
  public let history: Duration
  public let replay: Duration
  public let sideHistory: Duration

  public init(
    connect: Duration,
    hello: Duration,
    history: Duration,
    replay: Duration,
    sideHistory: Duration
  ) {
    self.connect = connect
    self.hello = hello
    self.history = history
    self.replay = replay
    self.sideHistory = sideHistory
  }
}

/// A finite retry schedule. Its count is the reconnect cap and its values are
/// the complete bounded backoff policy; the machine has no implicit retry path.
public struct SignalboxSynchronizationRetryPolicy: Equatable, Sendable {
  public let delays: [Duration]

  public init(delays: [Duration]) {
    self.delays = delays
  }

  fileprivate func delay(afterFailure failureCount: Int) -> Duration? {
    let index = failureCount - 1
    guard delays.indices.contains(index) else {
      return nil
    }
    return delays[index]
  }
}

public struct SignalboxSessionSynchronizationPolicy: Equatable, Sendable {
  public let deadlines: SignalboxSynchronizationDeadlines
  public let retry: SignalboxSynchronizationRetryPolicy
  public let snapshotCapacity: SignalboxSynchronizationSnapshotCapacity
  public let eventBufferCapacity: SignalboxSynchronizationEventBufferCapacity

  public init(
    deadlines: SignalboxSynchronizationDeadlines,
    retry: SignalboxSynchronizationRetryPolicy,
    snapshotCapacity: SignalboxSynchronizationSnapshotCapacity,
    eventBufferCapacity: SignalboxSynchronizationEventBufferCapacity
  ) {
    self.deadlines = deadlines
    self.retry = retry
    self.snapshotCapacity = snapshotCapacity
    self.eventBufferCapacity = eventBufferCapacity
  }
}

/// The caller-selected heap bound for one validated snapshot.
///
/// Record count bounds fixed per-record overhead. UTF-8 bytes bound retained
/// wire strings and unknown JSON payloads. Zero permits only an empty snapshot.
public struct SignalboxSynchronizationSnapshotCapacity: Equatable, Sendable {
  public let maximumRecords: UInt
  public let maximumUTF8Bytes: UInt

  public init(maximumRecords: UInt, maximumUTF8Bytes: UInt) {
    self.maximumRecords = maximumRecords
    self.maximumUTF8Bytes = maximumUTF8Bytes
  }
}

/// The caller-selected heap bound for events waiting behind snapshot work.
///
/// Event count bounds fixed per-event overhead. UTF-8 bytes bound retained
/// variable-size content and future-event JSON. Zero permits no buffered event.
public struct SignalboxSynchronizationEventBufferCapacity: Equatable, Sendable {
  public let maximumEvents: UInt
  public let maximumUTF8Bytes: UInt

  public init(maximumEvents: UInt, maximumUTF8Bytes: UInt) {
    self.maximumEvents = maximumEvents
    self.maximumUTF8Bytes = maximumUTF8Bytes
  }
}

public enum SignalboxSynchronizationStage: String, Equatable, Sendable {
  case connect
  case hello
  case history
  case replay
  case steady
  case sideHistory
}

public enum SignalboxSynchronizationDeadlineToken: Equatable, Sendable {
  case connect(generation: UInt64)
  case hello(generation: UInt64)
  case history(generation: UInt64)
  case replay(generation: UInt64)
  case sideHistory(generation: UInt64, refreshID: UInt64)
}

public struct SignalboxSynchronizationDiagnostic: Equatable, Sendable {
  public enum Kind: String, Equatable, Sendable {
    case decoding
    case deadline
    case protocolViolation
    case retryExhausted
    case staleCompletion
    case staleSnapshot
    case terminalFailure
    case transport
  }

  public let kind: Kind
  public let stage: SignalboxSynchronizationStage
  public let message: String

  public init(kind: Kind, stage: SignalboxSynchronizationStage, message: String) {
    self.kind = kind
    self.stage = stage
    self.message = message
  }
}

public struct SignalboxSynchronizationSnapshot: Equatable, Sendable {
  public enum Record: Equatable, Sendable {
    case turn(SignalboxTranscriptTurn)
    case entry(SignalboxTranscriptEntryMessage)
    case textEntry(SignalboxTranscriptTextEntryMessage)
    case content(SignalboxTranscriptContent)
  }

  public let sessionID: SignalboxCanonicalUUID
  public let cursor: SignalboxCanonicalUInt64
  public let records: [Record]

  fileprivate init(
    sessionID: SignalboxCanonicalUUID,
    cursor: SignalboxCanonicalUInt64,
    records: [Record]
  ) {
    self.sessionID = sessionID
    self.cursor = cursor
    self.records = records
  }
}

public enum SignalboxSessionSynchronizationPhase: Equatable, Sendable {
  case stopped
  case connect(generation: UInt64, reconnectAttempt: Int)
  case hello(generation: UInt64, reconnectAttempt: Int)
  case history(
    generation: UInt64,
    reconnectAttempt: Int,
    cursor: SignalboxCanonicalUInt64
  )
  case replay(generation: UInt64, cursor: SignalboxCanonicalUInt64)
  case steady(
    generation: UInt64,
    cursor: SignalboxCanonicalUInt64,
    refreshID: UInt64?
  )
  case recovery(
    failedStage: SignalboxSynchronizationStage,
    failureCount: Int,
    nextGeneration: UInt64?
  )
}

public enum SignalboxSessionSynchronizationInput: Sendable {
  case start
  case connected(generation: UInt64)
  case frame(generation: UInt64, message: SignalboxProcessServerMessage)
  case replayCompleted(generation: UInt64)
  case transportEnded(generation: UInt64, message: String)
  case deadlineExpired(SignalboxSynchronizationDeadlineToken)
  case retryReady(generation: UInt64)
  case sideFrame(
    generation: UInt64,
    refreshID: UInt64,
    message: SignalboxProcessServerMessage
  )
  case sideTransportEnded(generation: UInt64, refreshID: UInt64, message: String)
  case stop
}

public enum SignalboxSessionSynchronizationEffect: Equatable, Sendable {
  case openFollow(
    sessionID: SignalboxCanonicalUUID,
    generation: UInt64
  )
  case closeFollow(generation: UInt64)
  case armDeadline(
    token: SignalboxSynchronizationDeadlineToken,
    duration: Duration
  )
  case cancelDeadline(SignalboxSynchronizationDeadlineToken)
  case publishSnapshot(SignalboxSynchronizationSnapshot)
  case publishEvent(SignalboxFollowedSessionEvent)
  case requestSideSnapshot(
    sessionID: SignalboxCanonicalUUID,
    generation: UInt64,
    refreshID: UInt64
  )
  case cancelSideSnapshot(generation: UInt64, refreshID: UInt64)
  /// Offers only side-read material for the named trigger. This is never a
  /// replacement snapshot: consumers may source-qualified-upsert immutable
  /// semantic entries attributable to `trigger`, but must not replace turn
  /// projections or suppress buffered transition events with this snapshot.
  case mergeSideSnapshot(
    snapshot: SignalboxSynchronizationSnapshot,
    trigger: SignalboxFollowedSessionEvent
  )
  case scheduleReconnect(generation: UInt64, after: Duration)
  case cancelReconnect(generation: UInt64)
  case reportDiagnostic(SignalboxSynchronizationDiagnostic)
  case retryLimitReached
  case terminalFailure
}

/// A transport-independent reducer for one followed session.
///
/// The caller executes returned effects and feeds transport/deadline results
/// back as inputs. Generation and refresh identities make every completion
/// race explicit and allow stale work to be ignored without mutating state.
public struct SignalboxSessionSynchronizationMachine: Sendable {
  static let maximumRetainedDiagnostics = 128
  // docs/decisions.md records the 4 KiB retained-message choice.
  static let maximumRetainedDiagnosticMessageUTF8Bytes = 4 * 1_024

  public private(set) var phase: SignalboxSessionSynchronizationPhase = .stopped
  public private(set) var diagnostics: [SignalboxSynchronizationDiagnostic] = []

  private let sessionID: SignalboxCanonicalUUID
  private let policy: SignalboxSessionSynchronizationPolicy
  private var generation: UInt64 = 0
  private var failureCount = 0
  private var accumulator: SignalboxSnapshotAccumulator?
  private var replayBuffer: [UInt64: SignalboxBufferedFollowedEvent] = [:]
  private var replayBufferNextInsertionID: UInt64 = 0
  private var replayBufferNextRemovalID: UInt64 = 0
  private var replayBufferLastCursor: SignalboxCanonicalUInt64?
  private var replayBufferUTF8Bytes: UInt = 0
  private var publishedCursor: UInt64 = 0
  private var activeRefresh: SignalboxSideSnapshotRefresh?
  private var nextRefreshID: UInt64 = 1

  public init(
    sessionID: SignalboxCanonicalUUID,
    policy: SignalboxSessionSynchronizationPolicy
  ) {
    self.sessionID = sessionID
    self.policy = policy
  }

  public mutating func receive(
    _ input: SignalboxSessionSynchronizationInput
  ) -> [SignalboxSessionSynchronizationEffect] {
    switch input {
    case .start:
      return start()
    case .connected(let receivedGeneration):
      return connected(generation: receivedGeneration)
    case .frame(let receivedGeneration, let message):
      return frame(message, generation: receivedGeneration)
    case .replayCompleted(let receivedGeneration):
      return replayCompleted(generation: receivedGeneration)
    case .transportEnded(let receivedGeneration, let message):
      return transportEnded(generation: receivedGeneration, message: message)
    case .deadlineExpired(let token):
      return deadlineExpired(token)
    case .retryReady(let receivedGeneration):
      return retryReady(generation: receivedGeneration)
    case .sideFrame(let receivedGeneration, let refreshID, let message):
      return sideFrame(message, generation: receivedGeneration, refreshID: refreshID)
    case .sideTransportEnded(let receivedGeneration, let refreshID, let message):
      return sideTransportEnded(
        generation: receivedGeneration,
        refreshID: refreshID,
        message: message
      )
    case .stop:
      return stop()
    }
  }

  private mutating func start() -> [SignalboxSessionSynchronizationEffect] {
    guard case .stopped = phase else {
      return []
    }
    failureCount = 0
    generation = nextIdentity(after: generation)
    phase = .connect(generation: generation, reconnectAttempt: 0)
    return [
      .openFollow(sessionID: sessionID, generation: generation),
      .armDeadline(
        token: .connect(generation: generation),
        duration: policy.deadlines.connect
      ),
    ]
  }

  private mutating func connected(
    generation receivedGeneration: UInt64
  ) -> [SignalboxSessionSynchronizationEffect] {
    guard
      case .connect(let currentGeneration, let attempt) = phase,
      currentGeneration == receivedGeneration
    else {
      return staleCompletion(stage: .connect)
    }
    phase = .hello(generation: currentGeneration, reconnectAttempt: attempt)
    return [
      .cancelDeadline(.connect(generation: currentGeneration)),
      .armDeadline(
        token: .hello(generation: currentGeneration),
        duration: policy.deadlines.hello
      ),
    ]
  }

  private mutating func frame(
    _ message: SignalboxProcessServerMessage,
    generation receivedGeneration: UInt64
  ) -> [SignalboxSessionSynchronizationEffect] {
    guard receivedGeneration == generation else {
      return staleCompletion(stage: stage)
    }
    switch phase {
    case .hello(let currentGeneration, let attempt):
      return helloFrame(message, generation: currentGeneration, reconnectAttempt: attempt)
    case .history(let currentGeneration, let attempt, _):
      return historyFrame(message, generation: currentGeneration, reconnectAttempt: attempt)
    case .replay(let currentGeneration, _):
      return replayFrame(message, generation: currentGeneration)
    case .steady(let currentGeneration, _, _):
      return steadyFrame(message, generation: currentGeneration)
    case .stopped:
      return []
    case .recovery:
      return staleCompletion(stage: stage)
    case .connect:
      return protocolFailure(
        stage: stage,
        message: "Received a process frame outside a readable synchronization phase."
      )
    }
  }

  private mutating func helloFrame(
    _ message: SignalboxProcessServerMessage,
    generation currentGeneration: UInt64,
    reconnectAttempt: Int
  ) -> [SignalboxSessionSynchronizationEffect] {
    switch message {
    case .transcriptSnapshotStart(let boundary) where boundary.sessionID == sessionID:
      accumulator = SignalboxSnapshotAccumulator(
        boundary: boundary,
        capacity: policy.snapshotCapacity
      )
      clearReplayBuffer()
      phase = .history(
        generation: currentGeneration,
        reconnectAttempt: reconnectAttempt,
        cursor: boundary.cursor
      )
      return [
        .cancelDeadline(.hello(generation: currentGeneration)),
        .armDeadline(
          token: .history(generation: currentGeneration),
          duration: policy.deadlines.history
        ),
      ]
    case .protocolError(let remote):
      return remoteFailure(remote, stage: .hello)
    case .unknown(let kind, _, let decodingDiagnostic):
      return unknownFrame(
        kind: kind,
        decodingDiagnostic: decodingDiagnostic,
        stage: .hello
      )
    default:
      return protocolFailure(
        stage: .hello,
        message: "The follow stream did not begin with its matching snapshot start."
      )
    }
  }

  private mutating func historyFrame(
    _ message: SignalboxProcessServerMessage,
    generation currentGeneration: UInt64,
    reconnectAttempt: Int
  ) -> [SignalboxSessionSynchronizationEffect] {
    guard var currentAccumulator = accumulator else {
      return protocolFailure(stage: .history, message: "Snapshot history state was missing.")
    }
    accumulator = nil
    switch currentAccumulator.ingest(message, expectedSessionID: sessionID) {
    case .accepted:
      accumulator = currentAccumulator
      return []
    case .diagnostic(let kind, let decodingDiagnostic):
      accumulator = currentAccumulator
      if let decodingDiagnostic {
        return protocolFailure(stage: .history, message: decodingDiagnostic.message)
      }
      return reportUnknown(
        kind: kind,
        decodingDiagnostic: nil,
        stage: .history
      )
    case .completed(let snapshot):
      accumulator = nil
      publishedCursor = snapshot.cursor.rawValue
      phase = .replay(generation: currentGeneration, cursor: snapshot.cursor)
      return [
        .cancelDeadline(.history(generation: currentGeneration)),
        .armDeadline(
          token: .replay(generation: currentGeneration),
          duration: policy.deadlines.replay
        ),
        .publishSnapshot(snapshot),
      ]
    case .remoteFailure(let remote):
      return remoteFailure(remote, stage: .history)
    case .invalid(let message):
      return protocolFailure(stage: .history, message: message)
    }
  }

  private mutating func replayFrame(
    _ message: SignalboxProcessServerMessage,
    generation _: UInt64
  ) -> [SignalboxSessionSynchronizationEffect] {
    switch message {
    case .sessionEvent(let followed):
      guard
        case .replay(_, let snapshotCursor) = phase,
        followed.sessionID == sessionID
      else {
        return protocolFailure(
          stage: .replay,
          message: "A replayed event named a different session."
        )
      }
      guard followed.event.decodingDiagnostic == nil else {
        return protocolFailure(
          stage: .replay,
          message: followed.event.decodingDiagnostic?.message
            ?? "A malformed followed event could not be decoded."
        )
      }
      guard !followed.event.hasUnknownNestedState else {
        return protocolFailure(
          stage: .replay,
          message: "A known followed event contained an unknown closed state."
        )
      }
      let observedCursor = replayBufferLastCursor ?? snapshotCursor
      guard followed.cursor > observedCursor else {
        return diagnosticEffects(for: followed, stage: .replay)
      }
      return buffer(
        followed,
        stage: .replay,
        reportDiagnostics: true
      )
    case .protocolError(let remote):
      return remoteFailure(remote, stage: .replay)
    case .unknown(let kind, _, let decodingDiagnostic):
      return unknownFrame(
        kind: kind,
        decodingDiagnostic: decodingDiagnostic,
        stage: .replay
      )
    default:
      return protocolFailure(
        stage: .replay,
        message: "A non-event process frame arrived while the snapshot was replaying."
      )
    }
  }

  private mutating func replayCompleted(
    generation receivedGeneration: UInt64
  ) -> [SignalboxSessionSynchronizationEffect] {
    guard
      case .replay(let currentGeneration, let cursor) = phase,
      receivedGeneration == currentGeneration
    else {
      return staleCompletion(stage: .replay)
    }
    failureCount = 0
    phase = .steady(
      generation: currentGeneration,
      cursor: replayBufferLastCursor ?? cursor,
      refreshID: nil
    )
    var effects: [SignalboxSessionSynchronizationEffect] = [
      .cancelDeadline(.replay(generation: currentGeneration))
    ]
    effects.append(contentsOf: drainReplayBuffer(generation: currentGeneration))
    return effects
  }

  private mutating func steadyFrame(
    _ message: SignalboxProcessServerMessage,
    generation currentGeneration: UInt64
  ) -> [SignalboxSessionSynchronizationEffect] {
    switch message {
    case .sessionEvent(let followed):
      return receiveFollowedEvent(followed, generation: currentGeneration)
    case .protocolError(let remote):
      return remoteFailure(remote, stage: .steady)
    case .unknown(let kind, _, let decodingDiagnostic):
      return unknownFrame(
        kind: kind,
        decodingDiagnostic: decodingDiagnostic,
        stage: .steady
      )
    default:
      return protocolFailure(
        stage: .steady,
        message: "A non-event process frame arrived after synchronization."
      )
    }
  }

  private mutating func receiveFollowedEvent(
    _ followed: SignalboxFollowedSessionEvent,
    generation currentGeneration: UInt64,
    reportDiagnostics: Bool = true
  ) -> [SignalboxSessionSynchronizationEffect] {
    guard
      case .steady(_, let cursor, let refreshID) = phase,
      followed.sessionID == sessionID
    else {
      return protocolFailure(
        stage: .steady,
        message: "A followed event named a different session."
      )
    }
    guard followed.event.decodingDiagnostic == nil else {
      return protocolFailure(
        stage: .steady,
        message: followed.event.decodingDiagnostic?.message
          ?? "A malformed followed event could not be decoded."
      )
    }
    guard !followed.event.hasUnknownNestedState else {
      return protocolFailure(
        stage: .steady,
        message: "A known followed event contained an unknown closed state."
      )
    }
    guard followed.cursor > cursor else {
      return reportDiagnostics ? diagnosticEffects(for: followed, stage: .steady) : []
    }
    if activeRefresh != nil {
      let effects = buffer(
        followed,
        stage: .steady,
        reportDiagnostics: reportDiagnostics
      )
      if case .recovery = phase {
        return effects
      }
      phase = .steady(
        generation: currentGeneration,
        cursor: followed.cursor,
        refreshID: refreshID
      )
      return effects
    }
    phase = .steady(
      generation: currentGeneration,
      cursor: followed.cursor,
      refreshID: refreshID
    )
    publishedCursor = followed.cursor.rawValue
    var effects =
      reportDiagnostics
      ? diagnosticEffects(for: followed, stage: .steady)
      : []
    effects.append(.publishEvent(followed))
    if eventRequiresSideSnapshot(followed.event) {
      effects.append(contentsOf: beginSideRefresh(trigger: followed, generation: currentGeneration))
    }
    return effects
  }

  private mutating func beginSideRefresh(
    trigger: SignalboxFollowedSessionEvent,
    generation currentGeneration: UInt64
  ) -> [SignalboxSessionSynchronizationEffect] {
    guard policy.eventBufferCapacity.maximumEvents > 0,
      trigger.event.retainedUTF8Bytes <= policy.eventBufferCapacity.maximumUTF8Bytes
    else {
      return protocolFailure(
        stage: .steady,
        message: "A side-snapshot trigger exceeded the configured native-client capacity."
      )
    }
    let refreshID = nextRefreshID
    nextRefreshID = nextIdentity(after: nextRefreshID)
    activeRefresh = SignalboxSideSnapshotRefresh(
      id: refreshID,
      trigger: trigger,
      accumulator: nil
    )
    if case .steady(_, let cursor, _) = phase {
      phase = .steady(
        generation: currentGeneration,
        cursor: cursor,
        refreshID: refreshID
      )
    }
    return [
      .requestSideSnapshot(
        sessionID: sessionID,
        generation: currentGeneration,
        refreshID: refreshID
      ),
      .armDeadline(
        token: .sideHistory(generation: currentGeneration, refreshID: refreshID),
        duration: policy.deadlines.sideHistory
      ),
    ]
  }

  private mutating func sideFrame(
    _ message: SignalboxProcessServerMessage,
    generation receivedGeneration: UInt64,
    refreshID: UInt64
  ) -> [SignalboxSessionSynchronizationEffect] {
    guard
      receivedGeneration == generation,
      var refresh = activeRefresh,
      refresh.id == refreshID
    else {
      return staleCompletion(stage: .sideHistory)
    }
    if refresh.accumulator == nil {
      switch message {
      case .transcriptSnapshotStart(let boundary) where boundary.sessionID == sessionID:
        refresh.accumulator = SignalboxSnapshotAccumulator(
          boundary: boundary,
          capacity: policy.snapshotCapacity
        )
        activeRefresh = refresh
        return []
      case .protocolError(let remote):
        return remoteFailure(remote, stage: .sideHistory)
      case .unknown(let kind, _, let decodingDiagnostic):
        return unknownFrame(
          kind: kind,
          decodingDiagnostic: decodingDiagnostic,
          stage: .sideHistory
        )
      default:
        return protocolFailure(
          stage: .sideHistory,
          message: "A side history read did not begin with its matching snapshot start."
        )
      }
    }
    guard var currentAccumulator = refresh.accumulator else {
      return protocolFailure(stage: .sideHistory, message: "Side snapshot history was missing.")
    }
    activeRefresh = nil
    refresh.accumulator = nil
    switch currentAccumulator.ingest(message, expectedSessionID: sessionID) {
    case .accepted:
      refresh.accumulator = currentAccumulator
      activeRefresh = refresh
      return []
    case .diagnostic(let kind, let decodingDiagnostic):
      refresh.accumulator = currentAccumulator
      activeRefresh = refresh
      if let decodingDiagnostic {
        return protocolFailure(stage: .sideHistory, message: decodingDiagnostic.message)
      }
      return reportUnknown(
        kind: kind,
        decodingDiagnostic: nil,
        stage: .sideHistory
      )
    case .completed(let snapshot):
      activeRefresh = refresh
      guard snapshot.cursor >= refresh.trigger.cursor else {
        return staleSideSnapshot(refresh: refresh, snapshot: snapshot)
      }
      activeRefresh = nil
      if case .steady(_, let cursor, _) = phase {
        phase = .steady(
          generation: receivedGeneration,
          cursor: cursor,
          refreshID: nil
        )
      }
      var effects: [SignalboxSessionSynchronizationEffect] = [
        .cancelDeadline(
          .sideHistory(generation: receivedGeneration, refreshID: refreshID)
        ),
        .mergeSideSnapshot(snapshot: snapshot, trigger: refresh.trigger),
      ]
      effects.append(contentsOf: drainReplayBuffer(generation: receivedGeneration))
      return effects
    case .remoteFailure(let remote):
      activeRefresh = refresh
      return remoteFailure(remote, stage: .sideHistory)
    case .invalid(let message):
      activeRefresh = refresh
      return protocolFailure(stage: .sideHistory, message: message)
    }
  }

  private mutating func buffer(
    _ followed: SignalboxFollowedSessionEvent,
    stage currentStage: SignalboxSynchronizationStage,
    reportDiagnostics: Bool
  ) -> [SignalboxSessionSynchronizationEffect] {
    let retainedBytes = followed.event.retainedUTF8Bytes
    let (nextBytes, overflowed) = replayBufferUTF8Bytes.addingReportingOverflow(
      retainedBytes
    )
    let triggerBytes = activeRefresh?.trigger.event.retainedUTF8Bytes ?? 0
    let (totalRetainedBytes, totalBytesOverflowed) = nextBytes.addingReportingOverflow(
      triggerBytes
    )
    let retainedTriggerCount: UInt = activeRefresh == nil ? 0 : 1
    let (nextInsertionID, insertionIDOverflowed) =
      replayBufferNextInsertionID.addingReportingOverflow(1)
    guard
      UInt(replayBuffer.count) + retainedTriggerCount
        < policy.eventBufferCapacity.maximumEvents,
      !overflowed,
      !totalBytesOverflowed,
      !insertionIDOverflowed,
      totalRetainedBytes <= policy.eventBufferCapacity.maximumUTF8Bytes
    else {
      return protocolFailure(
        stage: currentStage,
        message: "Buffered followed events exceeded the configured native-client capacity."
      )
    }
    replayBuffer[replayBufferNextInsertionID] = SignalboxBufferedFollowedEvent(
      followed: followed
    )
    replayBufferNextInsertionID = nextInsertionID
    replayBufferLastCursor = followed.cursor
    replayBufferUTF8Bytes = nextBytes
    return reportDiagnostics ? diagnosticEffects(for: followed, stage: currentStage) : []
  }

  private mutating func drainReplayBuffer(
    generation currentGeneration: UInt64
  ) -> [SignalboxSessionSynchronizationEffect] {
    var effects: [SignalboxSessionSynchronizationEffect] = []
    while activeRefresh == nil,
      replayBufferNextRemovalID < replayBufferNextInsertionID
    {
      guard
        let buffered = replayBuffer.removeValue(
          forKey: replayBufferNextRemovalID
        )
      else {
        return protocolFailure(
          stage: .steady,
          message: "Buffered followed-event queue state was inconsistent."
        )
      }
      replayBufferNextRemovalID += 1
      replayBufferUTF8Bytes -= buffered.followed.event.retainedUTF8Bytes
      let nextEffects = publishBufferedEvent(
        buffered.followed,
        generation: currentGeneration
      )
      effects.append(contentsOf: nextEffects)
    }
    if replayBuffer.isEmpty {
      clearReplayBuffer()
    }
    return effects
  }

  private mutating func publishBufferedEvent(
    _ followed: SignalboxFollowedSessionEvent,
    generation currentGeneration: UInt64
  ) -> [SignalboxSessionSynchronizationEffect] {
    guard followed.cursor.rawValue > publishedCursor else {
      return []
    }
    publishedCursor = followed.cursor.rawValue
    var effects: [SignalboxSessionSynchronizationEffect] = [.publishEvent(followed)]
    if eventRequiresSideSnapshot(followed.event) {
      effects.append(
        contentsOf: beginSideRefresh(
          trigger: followed,
          generation: currentGeneration
        )
      )
    }
    return effects
  }

  private mutating func transportEnded(
    generation receivedGeneration: UInt64,
    message: String
  ) -> [SignalboxSessionSynchronizationEffect] {
    guard
      receivedGeneration == generation,
      acceptsTransportCompletion(generation: receivedGeneration)
    else {
      return staleCompletion(stage: stage)
    }
    return enterRecovery(stage: primaryTransportStage, message: message, kind: .transport)
  }

  private mutating func sideTransportEnded(
    generation receivedGeneration: UInt64,
    refreshID: UInt64,
    message: String
  ) -> [SignalboxSessionSynchronizationEffect] {
    guard
      receivedGeneration == generation,
      activeRefresh?.id == refreshID
    else {
      return staleCompletion(stage: .sideHistory)
    }
    return enterRecovery(stage: .sideHistory, message: message, kind: .transport)
  }

  private mutating func deadlineExpired(
    _ token: SignalboxSynchronizationDeadlineToken
  ) -> [SignalboxSessionSynchronizationEffect] {
    guard deadlineIsCurrent(token) else {
      return staleCompletion(stage: deadlineStage(token))
    }
    let expiredStage = deadlineStage(token)
    return enterRecovery(
      stage: expiredStage,
      message: "The \(expiredStage.rawValue) synchronization deadline expired.",
      kind: .deadline
    )
  }

  private mutating func retryReady(
    generation receivedGeneration: UInt64
  ) -> [SignalboxSessionSynchronizationEffect] {
    guard
      case .recovery(_, let currentFailureCount, let nextGeneration) = phase,
      nextGeneration == receivedGeneration
    else {
      return staleCompletion(stage: .connect)
    }
    generation = receivedGeneration
    accumulator = nil
    clearReplayBuffer()
    activeRefresh = nil
    phase = .connect(
      generation: receivedGeneration,
      reconnectAttempt: currentFailureCount
    )
    return [
      .openFollow(sessionID: sessionID, generation: receivedGeneration),
      .armDeadline(
        token: .connect(generation: receivedGeneration),
        duration: policy.deadlines.connect
      ),
    ]
  }

  private mutating func stop() -> [SignalboxSessionSynchronizationEffect] {
    guard phase != .stopped else {
      return []
    }
    let oldGeneration = generation
    let cancellation = currentDeadlineToken.map {
      SignalboxSessionSynchronizationEffect.cancelDeadline($0)
    }
    let sideCancellation = activeRefresh.map {
      SignalboxSessionSynchronizationEffect.cancelSideSnapshot(
        generation: oldGeneration,
        refreshID: $0.id
      )
    }
    let reconnectCancellation: SignalboxSessionSynchronizationEffect? =
      if case .recovery(_, _, let nextGeneration) = phase {
        nextGeneration.map(SignalboxSessionSynchronizationEffect.cancelReconnect)
      } else {
        nil
      }
    phase = .stopped
    accumulator = nil
    clearReplayBuffer()
    activeRefresh = nil
    return [
      cancellation,
      sideCancellation,
      reconnectCancellation,
      .closeFollow(generation: oldGeneration),
    ].compactMap { $0 }
  }

  private mutating func remoteFailure(
    _ remote: SignalboxProcessError,
    stage failedStage: SignalboxSynchronizationStage
  ) -> [SignalboxSessionSynchronizationEffect] {
    let message = "\(remote.code.rawValue): \(remote.message)"
    switch remote.code {
    case .resyncRequired, .unavailable, .internal:
      return enterRecovery(stage: failedStage, message: message, kind: .transport)
    case .malformedFrame, .unsupportedVersion, .invalidRequest, .notFound, .conflictingReuse,
      .rejected, .commitAmbiguous:
      return enterRecovery(
        stage: failedStage,
        message: message,
        kind: .protocolViolation,
        permitsRetry: false
      )
    }
  }

  private mutating func protocolFailure(
    stage failedStage: SignalboxSynchronizationStage,
    message: String
  ) -> [SignalboxSessionSynchronizationEffect] {
    enterRecovery(
      stage: failedStage,
      message: message,
      kind: .protocolViolation
    )
  }

  private mutating func staleSideSnapshot(
    refresh: SignalboxSideSnapshotRefresh,
    snapshot: SignalboxSynchronizationSnapshot
  ) -> [SignalboxSessionSynchronizationEffect] {
    enterRecovery(
      stage: .sideHistory,
      message:
        "Side snapshot cursor \(snapshot.cursor.rawValue) preceded trigger cursor "
        + "\(refresh.trigger.cursor.rawValue).",
      kind: .staleSnapshot
    )
  }

  private mutating func enterRecovery(
    stage failedStage: SignalboxSynchronizationStage,
    message: String,
    kind: SignalboxSynchronizationDiagnostic.Kind,
    permitsRetry: Bool = true
  ) -> [SignalboxSessionSynchronizationEffect] {
    failureCount += 1
    let oldGeneration = generation
    let deadline = currentDeadlineToken
    let abandonedRefresh = activeRefresh
    let diagnostic = SignalboxSynchronizationDiagnostic(
      kind: kind,
      stage: failedStage,
      message: message
    )
    retainDiagnostic(diagnostic)
    accumulator = nil
    clearReplayBuffer()
    activeRefresh = nil

    let retryDelay = permitsRetry ? policy.retry.delay(afterFailure: failureCount) : nil
    let retryGeneration = retryDelay.map { _ in nextIdentity(after: oldGeneration) }
    phase = .recovery(
      failedStage: failedStage,
      failureCount: failureCount,
      nextGeneration: retryGeneration
    )

    var effects: [SignalboxSessionSynchronizationEffect] = []
    if let deadline {
      effects.append(.cancelDeadline(deadline))
    }
    if let abandonedRefresh {
      effects.append(
        .cancelSideSnapshot(
          generation: oldGeneration,
          refreshID: abandonedRefresh.id
        )
      )
    }
    effects.append(.closeFollow(generation: oldGeneration))
    effects.append(.reportDiagnostic(diagnostic))
    guard permitsRetry else {
      let terminal = SignalboxSynchronizationDiagnostic(
        kind: .terminalFailure,
        stage: failedStage,
        message: "Synchronization stopped after a non-retriable protocol failure."
      )
      retainDiagnostic(terminal)
      effects.append(.reportDiagnostic(terminal))
      effects.append(.terminalFailure)
      return effects
    }
    if let retryDelay, let retryGeneration {
      effects.append(.scheduleReconnect(generation: retryGeneration, after: retryDelay))
    } else {
      let exhausted = SignalboxSynchronizationDiagnostic(
        kind: .retryExhausted,
        stage: failedStage,
        message: "The bounded synchronization retry policy was exhausted."
      )
      retainDiagnostic(exhausted)
      effects.append(.reportDiagnostic(exhausted))
      effects.append(.retryLimitReached)
    }
    return effects
  }

  private mutating func staleCompletion(
    stage staleStage: SignalboxSynchronizationStage
  ) -> [SignalboxSessionSynchronizationEffect] {
    let diagnostic = SignalboxSynchronizationDiagnostic(
      kind: .staleCompletion,
      stage: staleStage,
      message: "Ignored a completion from superseded synchronization work."
    )
    retainDiagnostic(diagnostic)
    return [.reportDiagnostic(diagnostic)]
  }

  private mutating func unknownFrame(
    kind: String,
    decodingDiagnostic: SignalboxDecodingDiagnostic?,
    stage currentStage: SignalboxSynchronizationStage
  ) -> [SignalboxSessionSynchronizationEffect] {
    guard let decodingDiagnostic else {
      return reportUnknown(
        kind: kind,
        decodingDiagnostic: nil,
        stage: currentStage
      )
    }
    return protocolFailure(stage: currentStage, message: decodingDiagnostic.message)
  }

  private mutating func reportUnknown(
    kind: String,
    decodingDiagnostic: SignalboxDecodingDiagnostic?,
    stage currentStage: SignalboxSynchronizationStage
  ) -> [SignalboxSessionSynchronizationEffect] {
    let message =
      decodingDiagnostic?.message
      ?? "Ignored an unrecognized process-protocol frame kind: \(kind)."
    let diagnostic = SignalboxSynchronizationDiagnostic(
      kind: .decoding,
      stage: currentStage,
      message: message
    )
    retainDiagnostic(diagnostic)
    return [.reportDiagnostic(diagnostic)]
  }

  private mutating func diagnosticEffects(
    for followed: SignalboxFollowedSessionEvent,
    stage currentStage: SignalboxSynchronizationStage
  ) -> [SignalboxSessionSynchronizationEffect] {
    guard
      case .unknown(let kind, _, let decodingDiagnostic) = followed.event
    else {
      return []
    }
    let message =
      decodingDiagnostic?.message
      ?? "Preserved an unrecognized session-event kind: \(kind)."
    let diagnostic = SignalboxSynchronizationDiagnostic(
      kind: .decoding,
      stage: currentStage,
      message: message
    )
    retainDiagnostic(diagnostic)
    return [.reportDiagnostic(diagnostic)]
  }

  private mutating func retainDiagnostic(
    _ diagnostic: SignalboxSynchronizationDiagnostic
  ) {
    diagnostics.append(
      SignalboxSynchronizationDiagnostic(
        kind: diagnostic.kind,
        stage: diagnostic.stage,
        message: retainedDiagnosticMessage(diagnostic.message)
      )
    )
    let overflow = diagnostics.count - Self.maximumRetainedDiagnostics
    if overflow > 0 {
      diagnostics.removeFirst(overflow)
    }
  }

  private func retainedDiagnosticMessage(_ message: String) -> String {
    let scalars = message.unicodeScalars
    var retainedEnd = scalars.startIndex
    var retainedBytes = 0
    while retainedEnd != scalars.endIndex {
      let scalarBytes = scalars[retainedEnd].utf8.count
      guard retainedBytes + scalarBytes <= Self.maximumRetainedDiagnosticMessageUTF8Bytes else {
        break
      }
      retainedBytes += scalarBytes
      scalars.formIndex(after: &retainedEnd)
    }
    return String(scalars[..<retainedEnd])
  }

  private mutating func clearReplayBuffer() {
    replayBuffer.removeAll(keepingCapacity: false)
    replayBufferNextInsertionID = 0
    replayBufferNextRemovalID = 0
    replayBufferLastCursor = nil
    replayBufferUTF8Bytes = 0
  }

  private func deadlineIsCurrent(
    _ token: SignalboxSynchronizationDeadlineToken
  ) -> Bool {
    currentDeadlineToken == token
  }

  private func acceptsTransportCompletion(generation receivedGeneration: UInt64) -> Bool {
    switch phase {
    case .connect(let currentGeneration, _),
      .hello(let currentGeneration, _),
      .history(let currentGeneration, _, _),
      .replay(let currentGeneration, _),
      .steady(let currentGeneration, _, _):
      return currentGeneration == receivedGeneration
    case .stopped, .recovery:
      return false
    }
  }

  private var currentDeadlineToken: SignalboxSynchronizationDeadlineToken? {
    switch phase {
    case .connect(let currentGeneration, _):
      return .connect(generation: currentGeneration)
    case .hello(let currentGeneration, _):
      return .hello(generation: currentGeneration)
    case .history(let currentGeneration, _, _):
      return .history(generation: currentGeneration)
    case .replay(let currentGeneration, _):
      return .replay(generation: currentGeneration)
    case .steady(let currentGeneration, _, let refreshID):
      return refreshID.map {
        .sideHistory(generation: currentGeneration, refreshID: $0)
      }
    case .stopped, .recovery:
      return nil
    }
  }

  private var stage: SignalboxSynchronizationStage {
    switch phase {
    case .stopped, .connect:
      return .connect
    case .hello:
      return .hello
    case .history:
      return .history
    case .replay:
      return .replay
    case .steady:
      return activeRefresh == nil ? .steady : .sideHistory
    case .recovery(let failedStage, _, _):
      return failedStage
    }
  }

  private var primaryTransportStage: SignalboxSynchronizationStage {
    switch phase {
    case .steady:
      return .steady
    default:
      return stage
    }
  }

  private func deadlineStage(
    _ token: SignalboxSynchronizationDeadlineToken
  ) -> SignalboxSynchronizationStage {
    switch token {
    case .connect:
      return .connect
    case .hello:
      return .hello
    case .history:
      return .history
    case .replay:
      return .replay
    case .sideHistory:
      return .sideHistory
    }
  }

  private func eventRequiresSideSnapshot(
    _ event: SignalboxProcessSessionEvent
  ) -> Bool {
    switch event {
    case .toolBatchTransition(_, _, let state):
      switch state {
      case .proposed, .resultsProjected:
        return true
      case .recoveryRequired, .unknown:
        return false
      }
    case .turnCompleted, .turnFailed, .turnCancelled, .turnToolReconciliationRequired,
      .unknown:
      return true
    case .sessionCreated, .inputAccepted, .turnActivated, .modelCallTransition, .turnRefused,
      .turnReconciliationRequired:
      return false
    }
  }

  private func nextIdentity(after value: UInt64) -> UInt64 {
    value == UInt64.max ? 1 : value + 1
  }
}

private struct SignalboxSideSnapshotRefresh: Sendable {
  let id: UInt64
  let trigger: SignalboxFollowedSessionEvent
  var accumulator: SignalboxSnapshotAccumulator?
}

private struct SignalboxBufferedFollowedEvent: Sendable {
  let followed: SignalboxFollowedSessionEvent
}

private enum SignalboxSnapshotAccumulatorOutcome {
  case accepted
  case diagnostic(kind: String, decodingDiagnostic: SignalboxDecodingDiagnostic?)
  case completed(SignalboxSynchronizationSnapshot)
  case remoteFailure(SignalboxProcessError)
  case invalid(String)
}

private struct SignalboxSnapshotEntryIdentity: Hashable {
  let sourceSessionID: SignalboxCanonicalUUID
  let entryID: SignalboxCanonicalUUID
}

private struct SignalboxSnapshotAccumulator: Sendable {
  let boundary: SignalboxTranscriptSnapshotBoundary
  let capacity: SignalboxSynchronizationSnapshotCapacity
  private var records: [SignalboxSynchronizationSnapshot.Record] = []
  private var turnIDs: Set<SignalboxCanonicalUUID> = []
  private var entryIDs: Set<SignalboxSnapshotEntryIdentity> = []
  private var priorAcceptancePosition: UInt64?
  private var turnCount: UInt64 = 0
  private var entryCount: UInt64 = 0
  private var entriesStarted = false
  private var contentEntryIndex: UInt64?
  private var expectedFragmentIndex: UInt64 = 0
  private var retainedUTF8Bytes: UInt = 0

  init(
    boundary: SignalboxTranscriptSnapshotBoundary,
    capacity: SignalboxSynchronizationSnapshotCapacity
  ) {
    self.boundary = boundary
    self.capacity = capacity
  }

  mutating func ingest(
    _ message: SignalboxProcessServerMessage,
    expectedSessionID: SignalboxCanonicalUUID
  ) -> SignalboxSnapshotAccumulatorOutcome {
    if let contentEntryIndex {
      return ingestContent(
        message,
        contentEntryIndex: contentEntryIndex
      )
    }
    switch message {
    case .transcriptTurn(let turn):
      guard
        !turn.state.isInvalidStoredProjection,
        !entriesStarted,
        turn.acceptancePosition.rawValue != 0,
        priorAcceptancePosition.map({ $0 < turn.acceptancePosition.rawValue }) ?? true,
        turnIDs.insert(turn.turnID).inserted
      else {
        return .invalid("Snapshot turns were not unique acceptance-order projections.")
      }
      priorAcceptancePosition = turn.acceptancePosition.rawValue
      turnCount = turnCount.addingReportingOverflow(1).partialValue
      guard append(.turn(turn)) else {
        return .invalid("Snapshot exceeded the configured native-client capacity.")
      }
      return .accepted
    case .transcriptEntry(let entry):
      entriesStarted = true
      guard
        !entry.entry.hasUnknownStoredVariant,
        entry.entryIndex.rawValue == entryCount,
        entryIDs.insert(
          SignalboxSnapshotEntryIdentity(
            sourceSessionID: entry.sourceSessionID,
            entryID: entry.entryID
          )
        ).inserted
      else {
        return .invalid("Snapshot entry indices or source-qualified identities were invalid.")
      }
      entryCount = entryCount.addingReportingOverflow(1).partialValue
      guard append(.entry(entry)) else {
        return .invalid("Snapshot exceeded the configured native-client capacity.")
      }
      return .accepted
    case .transcriptTextEntry(let entry):
      entriesStarted = true
      guard
        !entry.entry.hasUnknownStoredVariant,
        entry.entryIndex.rawValue == entryCount,
        entryIDs.insert(
          SignalboxSnapshotEntryIdentity(
            sourceSessionID: entry.sourceSessionID,
            entryID: entry.entryID
          )
        ).inserted
      else {
        return .invalid("Snapshot entry indices or source-qualified identities were invalid.")
      }
      contentEntryIndex = entry.entryIndex.rawValue
      expectedFragmentIndex = 0
      guard append(.textEntry(entry)) else {
        return .invalid("Snapshot exceeded the configured native-client capacity.")
      }
      return .accepted
    case .transcriptSnapshotEnd(let end):
      guard
        end.sessionID == expectedSessionID,
        end.sessionID == boundary.sessionID,
        end.cursor == boundary.cursor,
        end.turnCount.rawValue == turnCount,
        end.entryCount.rawValue == entryCount
      else {
        return .invalid("Snapshot terminal identity, cursor, or counts were invalid.")
      }
      return .completed(
        SignalboxSynchronizationSnapshot(
          sessionID: boundary.sessionID,
          cursor: boundary.cursor,
          records: records
        )
      )
    case .sessionEvent:
      return .invalid("A followed event arrived before the snapshot ended.")
    case .protocolError(let remote):
      return .remoteFailure(remote)
    case .unknown(let kind, _, let decodingDiagnostic):
      return .diagnostic(kind: kind, decodingDiagnostic: decodingDiagnostic)
    default:
      return .invalid("Snapshot frame order was invalid.")
    }
  }

  private mutating func ingestContent(
    _ message: SignalboxProcessServerMessage,
    contentEntryIndex: UInt64
  ) -> SignalboxSnapshotAccumulatorOutcome {
    switch message {
    case .transcriptContent(let content)
    where content.entryIndex.rawValue == contentEntryIndex
      && content.fragmentIndex.rawValue == expectedFragmentIndex
      && content.contentFragment.utf8.count
        <= SignalboxProcessProtocol.maximumContentFragmentUTF8Bytes:
      guard append(.content(content)) else {
        return .invalid("Snapshot exceeded the configured native-client capacity.")
      }
      if content.finalFragment {
        self.contentEntryIndex = nil
        entryCount = entryCount.addingReportingOverflow(1).partialValue
      } else {
        expectedFragmentIndex = expectedFragmentIndex.addingReportingOverflow(1).partialValue
      }
      return .accepted
    case .protocolError(let remote):
      return .remoteFailure(remote)
    case .unknown(let kind, _, let decodingDiagnostic):
      return .diagnostic(kind: kind, decodingDiagnostic: decodingDiagnostic)
    default:
      return .invalid("Snapshot text content fragments were invalid.")
    }
  }

  private mutating func append(
    _ record: SignalboxSynchronizationSnapshot.Record
  ) -> Bool {
    let recordBytes = record.retainedUTF8Bytes
    let (nextBytes, overflowed) = retainedUTF8Bytes.addingReportingOverflow(recordBytes)
    guard
      UInt(records.count) < capacity.maximumRecords,
      !overflowed,
      nextBytes <= capacity.maximumUTF8Bytes
    else {
      return false
    }
    records.append(record)
    retainedUTF8Bytes = nextBytes
    return true
  }
}

extension SignalboxSynchronizationSnapshot.Record {
  fileprivate var retainedUTF8Bytes: UInt {
    switch self {
    case .turn(let turn):
      return turn.state.retainedUTF8Bytes
    case .entry(let message):
      return message.entry.retainedUTF8Bytes
    case .textEntry(let message):
      return message.entry.retainedUTF8Bytes
    case .content(let content):
      return UInt(content.contentFragment.utf8.count)
    }
  }
}

extension SignalboxTranscriptTurnState {
  fileprivate var isInvalidStoredProjection: Bool {
    switch self {
    case .activeRunning(_, let currentModelCall):
      return currentModelCall?.state.hasUnknownStoredVariant ?? false
    case .failed(_, let terminalAttemptID, let terminalModelCall):
      return terminalModelCall != nil && terminalAttemptID == nil
    case .unknown:
      return true
    case .queued, .activeAwaitingModelCallRecovery, .activeAwaitingToolApproval,
      .activeAwaitingToolRecovery, .completed, .refused, .cancelled,
      .reconciliationRequired, .toolReconciliationRequired:
      return false
    }
  }

  fileprivate var retainedUTF8Bytes: UInt {
    switch self {
    case .queued(_, let content):
      return UInt(content.utf8.count)
    case .activeRunning(_, let currentModelCall):
      return currentModelCall?.state.retainedUTF8Bytes ?? 0
    case .unknown(_, let payload, let diagnostic):
      return payload.encodedUTF8Bytes
        .saturatedAdding(UInt(diagnostic?.message.utf8.count ?? 0))
    case .activeAwaitingModelCallRecovery, .activeAwaitingToolApproval,
      .activeAwaitingToolRecovery, .failed, .completed, .refused, .cancelled,
      .reconciliationRequired, .toolReconciliationRequired:
      return 0
    }
  }
}

extension SignalboxCurrentModelCallState {
  fileprivate var hasUnknownStoredVariant: Bool {
    if case .unknown = self {
      return true
    }
    return false
  }

  fileprivate var retainedUTF8Bytes: UInt {
    switch self {
    case .unknown(_, let payload):
      return payload.encodedUTF8Bytes
    case .prepared, .inFlight, .cancellationRequested:
      return 0
    }
  }
}

extension SignalboxTranscriptEntry {
  fileprivate var hasUnknownStoredVariant: Bool {
    switch self {
    case .imported(_, _, let sourceSpeaker, _):
      return sourceSpeaker.hasUnknownStoredVariant
    case .unknown:
      return true
    case .assistantToolUse, .toolExecutionResult, .toolDenied, .toolClosed,
      .turnCompleted, .turnFailed, .turnCancelled:
      return false
    }
  }

  fileprivate var retainedUTF8Bytes: UInt {
    switch self {
    case .assistantToolUse(_, _, _, let toolName, let arguments):
      return UInt(toolName.utf8.count).saturatedAdding(UInt(arguments.utf8.count))
    case .toolExecutionResult(_, _, let content),
      .toolDenied(_, let content),
      .toolClosed(_, let content):
      return UInt(content.utf8.count)
    case .imported(_, _, let sourceSpeaker, _):
      return sourceSpeaker.retainedUTF8Bytes
    case .unknown(_, let payload, let diagnostic):
      return payload.encodedUTF8Bytes
        .saturatedAdding(UInt(diagnostic?.message.utf8.count ?? 0))
    case .turnCompleted, .turnFailed, .turnCancelled:
      return 0
    }
  }
}

extension SignalboxTranscriptTextEntry {
  fileprivate var hasUnknownStoredVariant: Bool {
    switch self {
    case .imported(_, _, let sourceSpeaker):
      return sourceSpeaker.hasUnknownStoredVariant
    case .unknown:
      return true
    case .user, .assistant:
      return false
    }
  }

  fileprivate var retainedUTF8Bytes: UInt {
    switch self {
    case .imported(_, _, let sourceSpeaker):
      return sourceSpeaker.retainedUTF8Bytes
    case .unknown(_, let payload, let diagnostic):
      return payload.encodedUTF8Bytes
        .saturatedAdding(UInt(diagnostic?.message.utf8.count ?? 0))
    case .user, .assistant:
      return 0
    }
  }
}

extension SignalboxImportedSourceSpeaker {
  fileprivate var hasUnknownStoredVariant: Bool {
    if case .unknown = self {
      return true
    }
    return false
  }

  fileprivate var retainedUTF8Bytes: UInt {
    switch self {
    case .unknown(_, let payload):
      return payload.encodedUTF8Bytes
    case .notAttested, .attestedAbsent, .attested:
      return 0
    }
  }
}

extension SignalboxProcessSessionEvent {
  fileprivate var hasUnknownNestedState: Bool {
    switch self {
    case .modelCallTransition(_, _, let state):
      return state.isUnknown
    case .toolBatchTransition(_, _, let state):
      return state.isUnknown
    case .sessionCreated, .inputAccepted, .turnActivated, .turnCompleted, .turnFailed,
      .turnRefused, .turnCancelled, .turnReconciliationRequired,
      .turnToolReconciliationRequired, .unknown:
      return false
    }
  }

  fileprivate var decodingDiagnostic: SignalboxDecodingDiagnostic? {
    if case .unknown(_, _, let diagnostic) = self {
      return diagnostic
    }
    return nil
  }

  fileprivate var retainedUTF8Bytes: UInt {
    switch self {
    case .inputAccepted(_, _, _, let content):
      return UInt(content.utf8.count)
    case .modelCallTransition(_, _, let state):
      return state.retainedUTF8Bytes
    case .toolBatchTransition(_, _, let state):
      return state.retainedUTF8Bytes
    case .unknown(_, let payload, let diagnostic):
      return payload.encodedUTF8Bytes
        .saturatedAdding(UInt(diagnostic?.message.utf8.count ?? 0))
    case .sessionCreated, .turnActivated, .turnCompleted, .turnFailed, .turnRefused,
      .turnCancelled, .turnReconciliationRequired, .turnToolReconciliationRequired:
      return 0
    }
  }
}

extension SignalboxModelCallState {
  fileprivate var isUnknown: Bool {
    if case .unknown = self {
      return true
    }
    return false
  }

  fileprivate var retainedUTF8Bytes: UInt {
    if case .unknown(_, let payload) = self {
      return payload.encodedUTF8Bytes
    }
    return 0
  }
}

extension SignalboxToolBatchState {
  fileprivate var isUnknown: Bool {
    if case .unknown = self {
      return true
    }
    return false
  }

  fileprivate var retainedUTF8Bytes: UInt {
    if case .unknown(_, let payload) = self {
      return payload.encodedUTF8Bytes
    }
    return 0
  }
}

extension Dictionary where Key == String, Value == SignalboxJSONValue {
  fileprivate var encodedUTF8Bytes: UInt {
    guard let encoded = try? SignalboxJSONCoding.encoder().encode(self) else {
      return .max
    }
    return UInt(encoded.count)
  }
}

extension UInt {
  fileprivate func saturatedAdding(_ other: UInt) -> UInt {
    let (sum, overflowed) = addingReportingOverflow(other)
    return overflowed ? .max : sum
  }
}
