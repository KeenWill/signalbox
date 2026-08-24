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
    case modelCallUsage(SignalboxTranscriptModelCallUsage)
    case entry(SignalboxTranscriptEntryMessage)
    case userEntry(SignalboxTranscriptUserEntryMessage)
    case textEntry(SignalboxTranscriptTextEntryMessage)
    case content(SignalboxTranscriptContent)
  }

  public let sessionID: SignalboxCanonicalUUID
  public let cursor: SignalboxCanonicalUInt64
  public let runner: SignalboxRunnerProjection?
  public let records: [Record]

  init(
    sessionID: SignalboxCanonicalUUID,
    cursor: SignalboxCanonicalUInt64,
    runner: SignalboxRunnerProjection? = nil,
    records: [Record]
  ) {
    self.sessionID = sessionID
    self.cursor = cursor
    self.runner = runner
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
  case projectionRejected(message: String)
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
  case publishProviderTextDelta(SignalboxProviderTextDelta)
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
  static let maximumRetainedDiagnosticMessageUTF8Bytes = 4 * 1_024

  public private(set) var phase: SignalboxSessionSynchronizationPhase = .stopped
  public private(set) var diagnostics: [SignalboxSynchronizationDiagnostic] = []

  private let sessionID: SignalboxCanonicalUUID
  private let policy: SignalboxSessionSynchronizationPolicy
  private var generation: UInt64 = 0
  private var failureCount = 0
  private var accumulator: SignalboxSnapshotAccumulator?
  private var replayBuffer: [UInt64: SignalboxBufferedFollowMessage] = [:]
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
    case .projectionRejected(let message):
      switch phase {
      case .stopped, .recovery:
        return []
      case .connect, .hello, .history, .replay, .steady:
        return enterRecovery(stage: stage, message: message, kind: .protocolViolation)
      }
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
        return protocolFailure(
          stage: .history,
          message: "Rejected malformed known process-protocol frame \(kind): \(decodingDiagnostic.message)"
        )
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
      // Unknown nested states are retained as visible evidence by the projector.
      // Only malformed envelopes fail replay before cursor validation.
      // This keeps snapshot catch-up tolerant of newer closed-state variants.
      let observedCursor = replayBufferLastCursor ?? snapshotCursor
      guard followed.cursor > observedCursor else {
        return diagnosticEffects(for: followed, stage: .replay)
      }
      return buffer(
        followed,
        stage: .replay,
        reportDiagnostics: true
      )
    case .providerTextDelta(let delta):
      guard delta.sessionID == sessionID else {
        return protocolFailure(
          stage: .replay,
          message: "A replayed provider delta named a different session."
        )
      }
      return buffer(delta, stage: .replay)
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
    case .providerTextDelta(let delta):
      return receiveProviderTextDelta(delta)
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

  private mutating func receiveProviderTextDelta(
    _ delta: SignalboxProviderTextDelta
  ) -> [SignalboxSessionSynchronizationEffect] {
    guard delta.sessionID == sessionID else {
      return protocolFailure(
        stage: .steady,
        message: "A provider delta named a different session."
      )
    }
    guard activeRefresh == nil else {
      return buffer(delta, stage: .steady)
    }
    return [.publishProviderTextDelta(delta)]
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
    // Unknown nested states remain visible evidence after synchronization.
    // They do not invalidate an otherwise well-formed followed event.
    // The diagnostic path below reports their unrecognized protocol content.
    // Cursor validation still governs whether the event advances the stream.
    guard followed.cursor > cursor else {
      return reportDiagnostics ? diagnosticEffects(for: followed, stage: .steady) : []
    }
    let requiresSideSnapshot = eventRequiresSideSnapshot(followed.event)
    if requiresSideSnapshot {
      guard sideRefreshTriggerFitsCapacity(followed) else {
        return sideRefreshTriggerCapacityFailure()
      }
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
    if requiresSideSnapshot {
      effects.append(contentsOf: beginSideRefresh(trigger: followed, generation: currentGeneration))
    }
    return effects
  }

  private mutating func beginSideRefresh(
    trigger: SignalboxFollowedSessionEvent,
    generation currentGeneration: UInt64
  ) -> [SignalboxSessionSynchronizationEffect] {
    guard sideRefreshTriggerFitsCapacity(trigger) else {
      return sideRefreshTriggerCapacityFailure()
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
        return protocolFailure(
          stage: .sideHistory,
          message: "Rejected malformed known process-protocol frame \(kind): \(decodingDiagnostic.message)"
        )
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
    let effects = buffer(
      .event(followed),
      stage: currentStage
    )
    if case .recovery = phase {
      return effects
    }
    return reportDiagnostics
      ? effects + diagnosticEffects(for: followed, stage: currentStage)
      : effects
  }

  private mutating func buffer(
    _ delta: SignalboxProviderTextDelta,
    stage currentStage: SignalboxSynchronizationStage
  ) -> [SignalboxSessionSynchronizationEffect] {
    buffer(.providerTextDelta(delta), stage: currentStage)
  }

  private mutating func buffer(
    _ message: SignalboxBufferedFollowMessage,
    stage currentStage: SignalboxSynchronizationStage
  ) -> [SignalboxSessionSynchronizationEffect] {
    let retainedBytes = message.retainedUTF8Bytes
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
    replayBuffer[replayBufferNextInsertionID] = message
    replayBufferNextInsertionID = nextInsertionID
    if case .event(let followed) = message {
      replayBufferLastCursor = followed.cursor
    }
    replayBufferUTF8Bytes = nextBytes
    return []
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
      replayBufferUTF8Bytes -= buffered.retainedUTF8Bytes
      let nextEffects: [SignalboxSessionSynchronizationEffect]
      switch buffered {
      case .event(let followed):
        nextEffects = publishBufferedEvent(
          followed,
          generation: currentGeneration
        )
      case .providerTextDelta(let delta):
        nextEffects = [.publishProviderTextDelta(delta)]
      }
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
    let requiresSideSnapshot = eventRequiresSideSnapshot(followed.event)
    if requiresSideSnapshot {
      guard sideRefreshTriggerFitsCapacity(followed) else {
        return sideRefreshTriggerCapacityFailure()
      }
    }
    publishedCursor = followed.cursor.rawValue
    var effects: [SignalboxSessionSynchronizationEffect] = [.publishEvent(followed)]
    if requiresSideSnapshot {
      effects.append(
        contentsOf: beginSideRefresh(
          trigger: followed,
          generation: currentGeneration
        )
      )
    }
    return effects
  }

  private func sideRefreshTriggerFitsCapacity(
    _ trigger: SignalboxFollowedSessionEvent
  ) -> Bool {
    policy.eventBufferCapacity.maximumEvents > 0
      && trigger.event.retainedUTF8Bytes <= policy.eventBufferCapacity.maximumUTF8Bytes
  }

  private mutating func sideRefreshTriggerCapacityFailure()
    -> [SignalboxSessionSynchronizationEffect]
  {
    protocolFailure(
      stage: .steady,
      message: "A side-snapshot trigger exceeded the configured native-client capacity."
    )
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
    case .unknown:
      return enterRecovery(
        stage: failedStage,
        message: message,
        kind: .protocolViolation
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
    let retainedDiagnostic = retainDiagnostic(diagnostic)
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
    effects.append(.reportDiagnostic(retainedDiagnostic))
    guard permitsRetry else {
      let terminal = SignalboxSynchronizationDiagnostic(
        kind: .terminalFailure,
        stage: failedStage,
        message: "Synchronization stopped after a non-retriable protocol failure."
      )
      let retainedTerminal = retainDiagnostic(terminal)
      effects.append(.reportDiagnostic(retainedTerminal))
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
      let retainedExhausted = retainDiagnostic(exhausted)
      effects.append(.reportDiagnostic(retainedExhausted))
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
    let retainedDiagnostic = retainDiagnostic(diagnostic)
    return [.reportDiagnostic(retainedDiagnostic)]
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
    return protocolFailure(
      stage: currentStage,
      message: "Rejected malformed known process-protocol frame \(kind): \(decodingDiagnostic.message)"
    )
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
    let retainedDiagnostic = retainDiagnostic(diagnostic)
    return [.reportDiagnostic(retainedDiagnostic)]
  }

  private mutating func diagnosticEffects(
    for followed: SignalboxFollowedSessionEvent,
    stage currentStage: SignalboxSynchronizationStage
  ) -> [SignalboxSessionSynchronizationEffect] {
    guard let unrecognized = followed.event.unrecognizedContent else {
      return []
    }
    let message =
      unrecognized.decodingDiagnostic?.message
      ?? "Preserved unrecognized session-event content: \(unrecognized.kind)."
    let diagnostic = SignalboxSynchronizationDiagnostic(
      kind: .decoding,
      stage: currentStage,
      message: message
    )
    let retainedDiagnostic = retainDiagnostic(diagnostic)
    return [.reportDiagnostic(retainedDiagnostic)]
  }

  private mutating func retainDiagnostic(
    _ diagnostic: SignalboxSynchronizationDiagnostic
  ) -> SignalboxSynchronizationDiagnostic {
    let retainedDiagnostic = SignalboxSynchronizationDiagnostic(
      kind: diagnostic.kind,
      stage: diagnostic.stage,
      message: Self.retainedDiagnosticMessage(diagnostic.message)
    )
    diagnostics.append(retainedDiagnostic)
    let overflow = diagnostics.count - Self.maximumRetainedDiagnostics
    if overflow > 0 {
      diagnostics.removeFirst(overflow)
    }
    return retainedDiagnostic
  }

  /// Bounds protocol-derived text to prevent retained-history memory exhaustion
  /// and UI layout stalls from an oversized diagnostic.
  public static func retainedDiagnosticMessage(_ message: String) -> String {
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
    case .toolApprovalDecided, .contextCompacted, .turnCompleted, .turnFailed, .turnRefused, .turnCancelled,
      .turnReconciliationRequired, .turnToolReconciliationRequired, .unknown:
      return true
    case .sessionCreated, .sessionModelSettingsChanged, .turnModelSettingsResolved,
      .inputAccepted, .modelCallTransition, .turnActivated, .runnerStateTransition:
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

private enum SignalboxBufferedFollowMessage: Sendable {
  case event(SignalboxFollowedSessionEvent)
  case providerTextDelta(SignalboxProviderTextDelta)

  var retainedUTF8Bytes: UInt {
    switch self {
    case .event(let followed):
      return followed.event.retainedUTF8Bytes
    case .providerTextDelta(let delta):
      return UInt(delta.content.utf8.count)
    }
  }
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

/// Row counts alone cannot prove usage ownership. Turn states may require a
/// terminal owner, merely permit historical calls, or expose a current call
/// identity that the terminal-only usage section must reject.
private enum SignalboxSnapshotModelCallOwnership {
  case impossible
  case permitted
  case required(SignalboxSnapshotRequiredModelCallOwnership)
  case forbidden(SignalboxCanonicalUUID)
}

private enum SignalboxSnapshotRequiredModelCallOwnership {
  case identity(SignalboxCanonicalUUID)
  case owner
}

private struct SignalboxSnapshotAccumulator: Sendable {
  let boundary: SignalboxTranscriptSnapshotBoundary
  let capacity: SignalboxSynchronizationSnapshotCapacity
  private var records: [SignalboxSynchronizationSnapshot.Record] = []
  private var turnAcceptancePositions: [SignalboxCanonicalUUID: UInt64] = [:]
  private var firstTurnID: SignalboxCanonicalUUID?
  private var queuedTurnIDs: Set<SignalboxCanonicalUUID> = []
  private var modelCallOwningTurnIDs: Set<SignalboxCanonicalUUID> = []
  private var unmatchedTerminalModelCallOwners:
    [SignalboxCanonicalUUID: SignalboxCanonicalUUID] = [:]
  private var unmatchedTerminalModelCallOwnerIDs: Set<SignalboxCanonicalUUID> = []
  private var forbiddenModelCallOwners:
    [SignalboxCanonicalUUID: SignalboxCanonicalUUID] = [:]
  private var modelCallIDs: Set<SignalboxCanonicalUUID> = []
  private var entryIDs: Set<SignalboxSnapshotEntryIdentity> = []
  private var modelIdentityTurns = SignalboxSnapshotModelIdentityTurns()
  private var pendingModelIdentityTurnID: SignalboxCanonicalUUID?
  private var priorAcceptancePosition: UInt64?
  private var priorModelCallTurnAcceptancePosition: UInt64?
  private var priorModelCallID: String?
  private var turnCount: UInt64 = 0
  private var modelCallCount: UInt64 = 0
  private var entryCount: UInt64 = 0
  private var modelCallsStarted = false
  private var modelCallsEnded = false
  private var entriesStarted = false
  private var contentEntryIndex: UInt64?
  private var expectedFragmentIndex: UInt64 = 0
  private let retainedRunnerRecordCount: UInt
  private let boundaryFitsCapacity: Bool
  private var retainedUTF8Bytes: UInt = 0
  init(
    boundary: SignalboxTranscriptSnapshotBoundary,
    capacity: SignalboxSynchronizationSnapshotCapacity
  ) {
    self.boundary = boundary
    self.capacity = capacity
    let runnerRecordCount: UInt = boundary.runner == nil ? 0 : 1
    let runnerBytes = boundary.runner?.retainedUTF8Bytes ?? 0
    retainedRunnerRecordCount = runnerRecordCount
    retainedUTF8Bytes = runnerBytes
    boundaryFitsCapacity = runnerRecordCount <= capacity.maximumRecords
      && runnerBytes <= capacity.maximumUTF8Bytes
  }
  mutating func ingest(
    _ message: SignalboxProcessServerMessage,
    expectedSessionID: SignalboxCanonicalUUID
  ) -> SignalboxSnapshotAccumulatorOutcome {
    guard boundaryFitsCapacity else {
      return .invalid("Snapshot exceeded the configured native-client capacity.")
    }
    if let contentEntryIndex {
      return ingestContent(
        message,
        contentEntryIndex: contentEntryIndex
      )
    }
    switch message {
    case .transcriptTurn(let turn):
      if let malformed = turn.state.malformedStoredProjection {
        return .diagnostic(kind: malformed.kind, decodingDiagnostic: malformed.diagnostic)
      }
      let ownership = turn.state.snapshotModelCallOwnership
      let exposedModelCallID = ownership.exposedModelCallID
      guard
        !turn.state.isInvalidStoredProjection,
        !modelCallsStarted,
        !modelCallsEnded,
        !entriesStarted,
        turn.acceptancePosition.rawValue != 0,
        priorAcceptancePosition.map({ $0 < turn.acceptancePosition.rawValue }) ?? true,
        (exposedModelCallID.map {
          unmatchedTerminalModelCallOwners[$0] == nil
            && forbiddenModelCallOwners[$0] == nil
        } ?? true),
        turnAcceptancePositions.updateValue(
          turn.acceptancePosition.rawValue, forKey: turn.turnID
        ) == nil
      else {
        return .invalid("Snapshot turns were not unique acceptance-order projections.")
      }
      switch ownership {
      case .impossible:
        break
      case .permitted:
        modelCallOwningTurnIDs.insert(turn.turnID)
      case .required(.identity(let requiredTerminalModelCallID)):
        modelCallOwningTurnIDs.insert(turn.turnID)
        unmatchedTerminalModelCallOwners[requiredTerminalModelCallID] = turn.turnID
      case .required(.owner):
        modelCallOwningTurnIDs.insert(turn.turnID)
        unmatchedTerminalModelCallOwnerIDs.insert(turn.turnID)
      case .forbidden(let forbiddenModelCallID):
        modelCallOwningTurnIDs.insert(turn.turnID)
        forbiddenModelCallOwners[forbiddenModelCallID] = turn.turnID
      }
      if firstTurnID == nil {
        firstTurnID = turn.turnID
      }
      if case .queued = turn.state {
        queuedTurnIDs.insert(turn.turnID)
      } else if case .queuedDelegated = turn.state {
        queuedTurnIDs.insert(turn.turnID)
      } else if case .queuedDelegationWake = turn.state {
        queuedTurnIDs.insert(turn.turnID)
      }
      priorAcceptancePosition = turn.acceptancePosition.rawValue
      turnCount = turnCount.addingReportingOverflow(1).partialValue
      guard append(.turn(turn)) else {
        return .invalid("Snapshot exceeded the configured native-client capacity.")
      }
      return turn.state.snapshotUnknownDiagnostic ?? .accepted
    case .transcriptModelCallUsage(let evidence):
      guard let turnAcceptancePosition = turnAcceptancePositions[evidence.turnID] else {
        return .invalid("Snapshot model-call usage order or identities were invalid.")
      }
      let followsPriorModelCall = priorModelCallTurnAcceptancePosition.map { priorPosition in
        turnAcceptancePosition > priorPosition
          || (turnAcceptancePosition == priorPosition
            && priorModelCallID.map { $0 < evidence.modelCallID.rawValue } == true)
      } ?? true
      guard
        !modelCallsEnded,
        !entriesStarted,
        evidence.modelCallIndex.rawValue == modelCallCount,
        modelCallOwningTurnIDs.contains(evidence.turnID),
        forbiddenModelCallOwners[evidence.modelCallID] == nil,
        (unmatchedTerminalModelCallOwners[evidence.modelCallID].map {
          $0 == evidence.turnID
        } ?? true),
        followsPriorModelCall,
        modelCallIDs.insert(evidence.modelCallID).inserted
      else {
        return .invalid("Snapshot model-call usage order or identities were invalid.")
      }
      modelCallsStarted = true
      unmatchedTerminalModelCallOwners.removeValue(forKey: evidence.modelCallID)
      unmatchedTerminalModelCallOwnerIDs.remove(evidence.turnID)
      priorModelCallTurnAcceptancePosition = turnAcceptancePosition
      priorModelCallID = evidence.modelCallID.rawValue
      modelCallCount = modelCallCount.addingReportingOverflow(1).partialValue
      guard append(.modelCallUsage(evidence)) else {
        return .invalid("Snapshot exceeded the configured native-client capacity.")
      }
      return .accepted
    case .transcriptModelCallsEnd(let count):
      guard
        !modelCallsEnded,
        !entriesStarted,
        count.rawValue == modelCallCount,
        unmatchedTerminalModelCallOwners.isEmpty,
        unmatchedTerminalModelCallOwnerIDs.isEmpty
      else {
        return .invalid("Snapshot model-call evidence identities or count were invalid.")
      }
      modelCallsEnded = true
      return .accepted
    case .transcriptEntry(let entry):
      entriesStarted = true
      if let malformed = entry.entry.malformedStoredProjection {
        return .diagnostic(kind: malformed.kind, decodingDiagnostic: malformed.diagnostic)
      }
      guard
        modelCallsEnded && pendingModelIdentityTurnID == nil,
        !entry.entry.hasMalformedStoredProjection && entry.entry.modelIdentityTurnIsKnown(in: turnAcceptancePositions),
        entry.entry.modelIdentityTurnID == nil || entry.sourceSessionID == boundary.sessionID,
        entry.entry.modelIdentityTurnID == nil || entry.entry.modelIdentityTurnID != firstTurnID,
        entry.entry.modelIdentityTurnID.map({ !queuedTurnIDs.contains($0) }) ?? true,
        entry.entryIndex.rawValue == entryCount,
        entryIDs.insert(
          SignalboxSnapshotEntryIdentity(
            sourceSessionID: entry.sourceSessionID,
            entryID: entry.entryID
          )
        ).inserted,
        entry.entry.admitsModelIdentityTurn(&modelIdentityTurns.markers, &pendingModelIdentityTurnID)
      else {
        return .invalid("Snapshot entry indices or source-qualified identities were invalid.")
      }
      entryCount = entryCount.addingReportingOverflow(1).partialValue
      guard append(.entry(entry)) else {
        return .invalid("Snapshot exceeded the configured native-client capacity.")
      }
      return .accepted
    case .transcriptUserEntry(let entry):
      entriesStarted = true
      guard
        modelCallsEnded,
        pendingModelIdentityTurnID == nil || entry.sourceSessionID == boundary.sessionID,
        consumesModelIdentityTurnOrigin(turnID: entry.turnID),
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
      guard append(.userEntry(entry)) else {
        return .invalid("Snapshot exceeded the configured native-client capacity.")
      }
      return .accepted
    case .transcriptTextEntry(let entry):
      entriesStarted = true
      if let malformed = entry.entry.malformedStoredProjection {
        return .diagnostic(kind: malformed.kind, decodingDiagnostic: malformed.diagnostic)
      }
      guard
        modelCallsEnded,
        pendingModelIdentityTurnID == nil || entry.sourceSessionID == boundary.sessionID,
        !entry.entry.hasMalformedStoredProjection && consumesModelIdentityTurnOrigin(entry.entry),
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
        end.entryCount.rawValue == entryCount,
        modelCallsEnded && pendingModelIdentityTurnID == nil
      else {
        return .invalid("Snapshot terminal identity, cursor, or counts were invalid.")
      }
      return .completed(
        SignalboxSynchronizationSnapshot(
          sessionID: boundary.sessionID,
          cursor: boundary.cursor,
          runner: boundary.runner,
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
      UInt(records.count).saturatedAdding(retainedRunnerRecordCount) < capacity.maximumRecords,
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

extension SignalboxRunnerProjection {
  fileprivate var retainedUTF8Bytes: UInt {
    selector.retainedUTF8Bytes
      .saturatedAdding(UInt(credentialProfile?.rawValue.utf8.count ?? 0))
      .saturatedAdding(UInt(repository?.rawValue.utf8.count ?? 0))
      .saturatedAdding(UInt(workingDirectory?.rawValue.utf8.count ?? 0))
  }
}

extension SignalboxRunnerProjectionSelector {
  fileprivate var retainedUTF8Bytes: UInt {
    switch self {
    case .runner:
      return 0
    case .capabilityClass(let name):
      return UInt(name.rawValue.utf8.count)
    }
  }
}

extension SignalboxSynchronizationSnapshot.Record {
  fileprivate var retainedUTF8Bytes: UInt {
    switch self {
    case .turn(let turn):
      return turn.state.retainedUTF8Bytes
    case .modelCallUsage(let usage):
      return UInt(usage.cost?.amountUSD.rawValue.utf8.count ?? 0)
        .saturatedAdding(UInt(usage.cost?.rateVersion.rawValue.utf8.count ?? 0))
    case .entry(let message):
      return message.entry.retainedUTF8Bytes
    case .userEntry(let message):
      return message.content.retainedUTF8Bytes
    case .textEntry(let message):
      return message.entry.retainedUTF8Bytes
    case .content(let content):
      return UInt(content.contentFragment.utf8.count)
    }
  }
}

extension SignalboxTranscriptTurnState {
  fileprivate var malformedStoredProjection:
    (kind: String, diagnostic: SignalboxDecodingDiagnostic)?
  {
    guard case .unknown(let kind, _, let decodingDiagnostic) = self,
      let decodingDiagnostic
    else {
      return nil
    }
    return ("transcript_turn.state.\(kind)", decodingDiagnostic)
  }

  fileprivate var snapshotModelCallOwnership: SignalboxSnapshotModelCallOwnership {
    switch self {
    case .queued, .queuedDelegated, .queuedDelegationWake: return .impossible
    case .delegationTerminated: return .permitted
    case .unknown: return .permitted
    case .activeAwaitingChild: return .permitted
    case .activeAwaitingModelCallRecovery(_, let recoveryModelCallID):
      return .required(.identity(recoveryModelCallID))
    case .failed(_, _, let terminalModelCall):
      guard let terminalModelCall else {
        return .permitted
      }
      return .required(.identity(terminalModelCall.modelCallID))
    case .completed(_, _, let terminalModelCallID),
      .refused(_, _, let terminalModelCallID),
      .reconciliationRequired(_, _, let terminalModelCallID):
      return .required(.identity(terminalModelCallID))
    case .cancelled(_, _, let terminalModelCallID):
      guard let terminalModelCallID else {
        return .permitted
      }
      return .required(.identity(terminalModelCallID))
    case .activeRunning(_, let currentModelCall):
      guard let currentModelCall else {
        return .permitted
      }
      return .forbidden(currentModelCall.modelCallID)
    case .activeAwaitingToolApproval, .activeAwaitingToolRecovery,
      .toolReconciliationRequired:
      return .required(.owner)
    }
  }

  fileprivate var isInvalidStoredProjection: Bool {
    switch self {
    case .failed(_, let terminalAttemptID, let terminalModelCall):
      return terminalModelCall != nil
        && terminalAttemptID == nil
    case .unknown(_, _, let decodingDiagnostic):
      return decodingDiagnostic != nil
    case .queued, .queuedDelegated, .queuedDelegationWake, .delegationTerminated,
      .activeRunning, .activeAwaitingChild, .activeAwaitingModelCallRecovery,
      .activeAwaitingToolApproval,
      .activeAwaitingToolRecovery, .completed,
      .refused, .cancelled, .reconciliationRequired,
      .toolReconciliationRequired:
      return false
    }
  }

  fileprivate var retainedUTF8Bytes: UInt {
    switch self {
    case .queued(_, let content):
      return content.retainedUTF8Bytes
    case .queuedDelegated(_, _, _, let content):
      return UInt(content.utf8.count)
    case .queuedDelegationWake:
      return 0
    case .delegationTerminated:
      return 0
    case .activeRunning(_, let currentModelCall): return currentModelCall?.state.retainedUTF8Bytes ?? 0
    case .failed(_, _, let terminalModelCall): return terminalModelCall?.retainedUTF8Bytes ?? 0
    case .unknown(let kind, let payload, let diagnostic):
      return UInt(kind.utf8.count).saturatedAdding(payload.encodedUTF8Bytes)
        .saturatedAdding(UInt(diagnostic?.message.utf8.count ?? 0))
    case .activeAwaitingChild, .activeAwaitingModelCallRecovery,
      .activeAwaitingToolApproval, .activeAwaitingToolRecovery, .completed, .refused, .cancelled,
      .reconciliationRequired, .toolReconciliationRequired:
      return 0
    }
  }
}

extension SignalboxSnapshotModelCallOwnership {
  fileprivate var exposedModelCallID: SignalboxCanonicalUUID? {
    switch self {
    case .required(.identity(let requiredTerminalModelCallID)):
      return requiredTerminalModelCallID
    case .forbidden(let forbiddenModelCallID):
      return forbiddenModelCallID
    case .impossible, .permitted, .required(.owner):
      return nil
    }
  }
}

private struct SignalboxSnapshotModelIdentityTurns {
  var markers: Set<SignalboxCanonicalUUID> = []
  var origins: Set<SignalboxCanonicalUUID> = []
}

extension SignalboxSnapshotAccumulator {
  fileprivate mutating func consumesModelIdentityTurnOrigin(
    turnID: SignalboxCanonicalUUID
  ) -> Bool {
    guard let expectedTurnID = pendingModelIdentityTurnID else {
      modelIdentityTurns.origins.insert(turnID)
      return true
    }
    guard turnID == expectedTurnID, modelIdentityTurns.origins.insert(turnID).inserted else {
      return false
    }
    pendingModelIdentityTurnID = nil
    return true
  }

  fileprivate mutating func consumesModelIdentityTurnOrigin(
    _ entry: SignalboxTranscriptTextEntry
  ) -> Bool {
    entry.consumesTurnOrigin(
      &pendingModelIdentityTurnID,
      seenTurnIDs: &modelIdentityTurns.origins
    )
  }
}

extension SignalboxCurrentModelCallState {
  fileprivate var retainedUTF8Bytes: UInt {
    switch self {
    case .unknown(let kind, let payload):
      return UInt(kind.utf8.count).saturatedAdding(payload.encodedUTF8Bytes)
    case .prepared, .inFlight, .cancellationRequested:
      return 0
    }
  }
}

extension SignalboxTranscriptEntry {
  fileprivate var malformedStoredProjection:
    (kind: String, diagnostic: SignalboxDecodingDiagnostic)?
  {
    guard case .unknown(let kind, _, let decodingDiagnostic) = self,
      let decodingDiagnostic
    else {
      return nil
    }
    return (kind, decodingDiagnostic)
  }

  fileprivate var modelIdentityTurnID: SignalboxCanonicalUUID? {
    if case .modelIdentityChanged(let turnID, _, _) = self {
      return turnID
    }
    return nil
  }

  fileprivate func modelIdentityTurnIsKnown(
    in turnAcceptancePositions: [SignalboxCanonicalUUID: UInt64]
  ) -> Bool {
    if case .modelIdentityChanged(let turnID, _, _) = self {
      return turnAcceptancePositions[turnID] != nil
    }
    return true
  }

  fileprivate func admitsModelIdentityTurn(
    _ turnIDs: inout Set<SignalboxCanonicalUUID>,
    _ pendingTurnID: inout SignalboxCanonicalUUID?
  ) -> Bool {
    guard let modelIdentityTurnID else {
      return true
    }
    guard turnIDs.insert(modelIdentityTurnID).inserted else {
      return false
    }
    pendingTurnID = modelIdentityTurnID
    return true
  }

  fileprivate var hasMalformedStoredProjection: Bool {
    if case .unknown(_, _, let decodingDiagnostic) = self {
      return decodingDiagnostic != nil
    }
    return false
  }

  fileprivate var retainedUTF8Bytes: UInt {
    switch self {
    case .assistantToolUse(_, _, _, let toolName, let arguments, let approval):
      let approvalBytes: UInt
      switch approval?.decision {
      case .deny(let reason):
        approvalBytes = UInt(reason?.utf8.count ?? 0)
          .saturatedAdding(UInt(approval?.rationale?.utf8.count ?? 0))
      case .approve:
        approvalBytes = UInt(approval?.rationale?.utf8.count ?? 0)
      case nil:
        approvalBytes = 0
      }
      return UInt(toolName.utf8.count)
        .saturatedAdding(UInt(arguments.utf8.count))
        .saturatedAdding(approvalBytes)
    case .toolExecutionResult(_, _, let content),
      .toolDenied(_, let content),
      .toolClosed(_, let content):
      return UInt(content.utf8.count)
    case .delegatedTask(_, _, _, let content),
      .delegationMessage(_, _, _, _, _, _, let content):
      return UInt(content.utf8.count)
    case .delegationResult(_, _, _, _, _, _, let content, _, _):
      return UInt(content?.utf8.count ?? 0)
    case .imported(_, _, let sourceSpeaker, let contentKind):
      return sourceSpeaker.retainedUTF8Bytes.saturatedAdding(contentKind.retainedUTF8Bytes)
    case .unknown(let kind, let payload, let diagnostic):
      return UInt(kind.utf8.count)
        .saturatedAdding(payload.encodedUTF8Bytes)
        .saturatedAdding(UInt(diagnostic?.message.utf8.count ?? 0))
    case .modelIdentityChanged, .turnCompleted, .turnFailed, .turnCancelled:
      return 0
    }
  }
}

extension SignalboxTranscriptTextEntry {
  fileprivate var malformedStoredProjection:
    (kind: String, diagnostic: SignalboxDecodingDiagnostic)?
  {
    guard case .unknown(let kind, _, let decodingDiagnostic) = self,
      let decodingDiagnostic
    else {
      return nil
    }
    return (kind, decodingDiagnostic)
  }

  fileprivate func consumesTurnOrigin(
    _ pendingTurnID: inout SignalboxCanonicalUUID?,
    seenTurnIDs: inout Set<SignalboxCanonicalUUID>
  ) -> Bool {
    guard case .user(_, let turnID) = self else {
      return pendingTurnID == nil
    }
    guard let expectedTurnID = pendingTurnID else {
      seenTurnIDs.insert(turnID)
      return true
    }
    guard turnID == expectedTurnID, seenTurnIDs.insert(turnID).inserted else {
      return false
    }
    pendingTurnID = nil
    return true
  }

  fileprivate var hasMalformedStoredProjection: Bool {
    if case .unknown(_, _, let decodingDiagnostic) = self {
      return decodingDiagnostic != nil
    }
    return false
  }

  fileprivate var retainedUTF8Bytes: UInt {
    switch self {
    case .imported(_, _, let sourceSpeaker):
      return sourceSpeaker.retainedUTF8Bytes
    case .unknown(let kind, let payload, let diagnostic):
      return UInt(kind.utf8.count)
        .saturatedAdding(payload.encodedUTF8Bytes)
        .saturatedAdding(UInt(diagnostic?.message.utf8.count ?? 0))
    case .user, .assistant, .contextSummary:
      return 0
    }
  }
}

extension SignalboxImportedSourceSpeaker {
  fileprivate var retainedUTF8Bytes: UInt {
    switch self {
    case .unknown(let kind, let payload):
      return UInt(kind.utf8.count).saturatedAdding(payload.encodedUTF8Bytes)
    case .attested(let speaker):
      return speaker.retainedUTF8Bytes
    case .notAttested, .attestedAbsent:
      return 0
    }
  }
}

extension SignalboxFailedTerminalModelCall {
  fileprivate var retainedUTF8Bytes: UInt {
    disposition.retainedUTF8Bytes.saturatedAdding(cause?.retainedUTF8Bytes ?? 0)
  }
}

extension SignalboxFailedModelCallDisposition {
  fileprivate var retainedUTF8Bytes: UInt {
    if case .unknown(let value) = self {
      return UInt(value.utf8.count)
    }
    return 0
  }
}

extension SignalboxFailedModelCallCause {
  fileprivate var retainedUTF8Bytes: UInt {
    if case .unknown(let value) = self {
      return UInt(value.utf8.count)
    }
    return 0
  }
}

extension SignalboxImportedContentKind {
  fileprivate var retainedUTF8Bytes: UInt {
    if case .unknown(let value) = self {
      return UInt(value.utf8.count)
    }
    return 0
  }
}

extension SignalboxImportedSpeaker {
  fileprivate var retainedUTF8Bytes: UInt {
    if case .unknown(let value) = self {
      return UInt(value.utf8.count)
    }
    return 0
  }
}

extension SignalboxProcessSessionEvent {
  fileprivate var unrecognizedContent: (
    kind: String, decodingDiagnostic: SignalboxDecodingDiagnostic?
  )? {
    switch self {
    case .modelCallTransition(_, _, .unknown(let kind, _)):
      return ("model_call_transition.state.\(kind)", nil)
    case .modelCallTransition(_, _, .terminal(.unknown(let disposition))):
      let kind = "model_call_transition.state.terminal.disposition.\(disposition)"
      return (kind, nil)
    case .toolBatchTransition(_, _, .unknown(let kind, _)):
      return ("tool_batch_transition.state.\(kind)", nil)
    case .unknown(let kind, _, let diagnostic):
      return (kind, diagnostic)
    case .sessionCreated, .sessionModelSettingsChanged, .turnModelSettingsResolved,
      .inputAccepted, .turnActivated, .modelCallTransition,
      .toolBatchTransition, .toolApprovalDecided, .contextCompacted, .turnCompleted, .turnFailed,
      .turnRefused, .turnCancelled, .turnReconciliationRequired,
      .turnToolReconciliationRequired, .runnerStateTransition:
      return nil
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
      return content.retainedUTF8Bytes
    case .modelCallTransition(_, _, let state):
      return state.retainedUTF8Bytes
    case .toolBatchTransition(_, _, let state):
      return state.retainedUTF8Bytes
    case .toolApprovalDecided(_, _, let decision, _, let rationale):
      return decision.retainedUTF8Bytes
        .saturatedAdding(UInt(rationale?.utf8.count ?? 0))
    case .runnerStateTransition(_, _, _, let workingDirectory, _):
      return UInt(workingDirectory?.rawValue.utf8.count ?? 0)
    case .unknown(let kind, let payload, let diagnostic):
      return UInt(kind.utf8.count)
        .saturatedAdding(payload.encodedUTF8Bytes)
        .saturatedAdding(UInt(diagnostic?.message.utf8.count ?? 0))
    case .sessionCreated, .sessionModelSettingsChanged, .turnModelSettingsResolved,
      .turnActivated, .contextCompacted, .turnCompleted, .turnFailed,
      .turnRefused, .turnCancelled, .turnReconciliationRequired,
      .turnToolReconciliationRequired:
      return 0
    }
  }
}

extension SignalboxToolApprovalEventDecision {
  fileprivate var retainedUTF8Bytes: UInt {
    if case .deny(let reason) = self {
      return UInt(reason?.utf8.count ?? 0)
    }
    return 0
  }
}

extension SignalboxModelCallState {
  fileprivate var retainedUTF8Bytes: UInt {
    switch self {
    case .unknown(let kind, let payload):
      return UInt(kind.utf8.count).saturatedAdding(payload.encodedUTF8Bytes)
    case .terminal(let disposition):
      return disposition.retainedUTF8Bytes
    case .prepared, .inFlight, .cancellationRequested:
      return 0
    }
  }
}

extension SignalboxModelCallDisposition {
  fileprivate var retainedUTF8Bytes: UInt {
    if case .unknown(let value) = self {
      return UInt(value.utf8.count)
    }
    return 0
  }
}

extension SignalboxToolBatchState {
  fileprivate var retainedUTF8Bytes: UInt {
    if case .unknown(let kind, let payload) = self {
      return UInt(kind.utf8.count).saturatedAdding(payload.encodedUTF8Bytes)
    }
    return 0
  }
}

extension SignalboxTranscriptTurnState {
  fileprivate var snapshotUnknownDiagnostic: SignalboxSnapshotAccumulatorOutcome? {
    switch self {
    case .activeRunning(_, let currentModelCall):
      guard case .unknown(let kind, _) = currentModelCall?.state else {
        return nil
      }
      return .diagnostic(
        kind: "transcript_turn.state.active_running.current_model_call.state.\(kind)",
        decodingDiagnostic: nil
      )
    case .unknown(let kind, _, _):
      return .diagnostic(
        kind: "transcript_turn.state.\(kind)",
        decodingDiagnostic: nil
      )
    case .queued, .queuedDelegated, .queuedDelegationWake, .delegationTerminated,
      .activeAwaitingChild,
      .activeAwaitingModelCallRecovery,
      .activeAwaitingToolApproval,
      .activeAwaitingToolRecovery, .completed, .failed, .refused, .cancelled,
      .reconciliationRequired, .toolReconciliationRequired:
      return nil
    }
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
