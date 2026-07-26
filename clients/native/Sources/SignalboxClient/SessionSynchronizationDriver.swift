import Foundation

#if canImport(SignalboxModels)
  import SignalboxModels
#endif

public enum SignalboxSessionSynchronizationDriverUpdate: Equatable, Sendable {
  case phase(SignalboxSessionSynchronizationPhase)
  case authoritativeSnapshot(SignalboxSynchronizationSnapshot)
  case sideSnapshot(
    snapshot: SignalboxSynchronizationSnapshot,
    trigger: SignalboxFollowedSessionEvent
  )
  case event(SignalboxFollowedSessionEvent)
  case diagnostic(SignalboxSynchronizationDiagnostic)
  case retryLimitReached
  case terminalFailure
}

public protocol SignalboxSessionSynchronizing: Sendable {
  func start() async
  func stop() async
}

public actor SignalboxSessionSynchronizationDriver: SignalboxSessionSynchronizing {
  private let requester: any SignalboxProcessRequesting
  private let sessionID: SignalboxCanonicalUUID
  private let updates: @Sendable (SignalboxSessionSynchronizationDriverUpdate) async -> Void
  private var machine: SignalboxSessionSynchronizationMachine

  private var primaryTask: Task<Void, Never>?
  private var primaryExchange: (any SignalboxProcessExchange)?
  private var sideTask: Task<Void, Never>?
  private var sideExchange: (any SignalboxProcessExchange)?
  private var deadlineTask: Task<Void, Never>?
  private var deadlineToken: SignalboxSynchronizationDeadlineToken?
  private var reconnectTask: Task<Void, Never>?
  private var isStarted = false

  public init(
    requester: any SignalboxProcessRequesting,
    sessionID: SignalboxCanonicalUUID,
    policy: SignalboxSessionSynchronizationPolicy,
    updates: @escaping @Sendable (SignalboxSessionSynchronizationDriverUpdate) async -> Void
  ) {
    self.requester = requester
    self.sessionID = sessionID
    self.machine = SignalboxSessionSynchronizationMachine(
      sessionID: sessionID,
      policy: policy
    )
    self.updates = updates
  }

  public func start() async {
    guard !isStarted else {
      return
    }
    isStarted = true
    await process(.start)
  }

  public func stop() async {
    guard isStarted else {
      return
    }
    isStarted = false
    await process(.stop)
    await cancelAllWork()
  }

  private func process(
    _ input: SignalboxSessionSynchronizationInput
  ) async {
    let effects = machine.receive(input)
    await updates(.phase(machine.phase))
    var replayGeneration: UInt64?
    if case .replay(let generation, _) = machine.phase,
      effects.contains(where: { effect in
        if case .publishSnapshot = effect {
          return true
        }
        return false
      })
    {
      replayGeneration = generation
    }
    for effect in effects {
      await execute(effect)
    }
    if let replayGeneration, isStarted {
      await process(.replayCompleted(generation: replayGeneration))
    }
  }

  private func execute(
    _ effect: SignalboxSessionSynchronizationEffect
  ) async {
    switch effect {
    case .openFollow(_, let generation):
      openFollow(generation: generation)
    case .closeFollow:
      await closeTransports()
    case .armDeadline(let token, let duration):
      armDeadline(token, duration: duration)
    case .cancelDeadline(let token):
      cancelDeadline(token)
    case .publishSnapshot(let snapshot):
      await updates(.authoritativeSnapshot(snapshot))
    case .publishEvent(let event):
      await updates(.event(event))
    case .requestSideSnapshot(_, let generation, let refreshID):
      openSideSnapshot(generation: generation, refreshID: refreshID)
    case .mergeSideSnapshot(let snapshot, let trigger):
      await closeSideTransport()
      await updates(.sideSnapshot(snapshot: snapshot, trigger: trigger))
    case .scheduleReconnect(let generation, let delay):
      scheduleReconnect(generation: generation, delay: delay)
    case .reportDiagnostic(let diagnostic):
      await updates(.diagnostic(diagnostic))
    case .retryLimitReached:
      await updates(.retryLimitReached)
    case .terminalFailure:
      await updates(.terminalFailure)
    }
  }

  private func openFollow(generation: UInt64) {
    primaryTask?.cancel()
    primaryTask = Task { [weak self] in
      await self?.runFollow(generation: generation)
    }
  }

  private func runFollow(generation: UInt64) async {
    do {
      let exchange = try await requester.open(.followSession(sessionID: sessionID))
      guard isStarted, !Task.isCancelled else {
        await exchange.close()
        return
      }
      primaryExchange = exchange
      await process(.connected(generation: generation))
      while !Task.isCancelled, let frame = try await exchange.next() {
        await process(.frame(generation: generation, message: frame.message))
      }
      guard !Task.isCancelled, isStarted else {
        return
      }
      await process(
        .transportEnded(
          generation: generation,
          message: "The follow connection closed."
        )
      )
    } catch is CancellationError {
      return
    } catch {
      guard !Task.isCancelled, isStarted else {
        return
      }
      await process(
        .transportEnded(
          generation: generation,
          message: error.localizedDescription
        )
      )
    }
  }

  private func openSideSnapshot(
    generation: UInt64,
    refreshID: UInt64
  ) {
    sideTask?.cancel()
    sideTask = Task { [weak self] in
      await self?.runSideSnapshot(
        generation: generation,
        refreshID: refreshID
      )
    }
  }

  private func runSideSnapshot(
    generation: UInt64,
    refreshID: UInt64
  ) async {
    do {
      let exchange = try await requester.open(.readTranscript(sessionID: sessionID))
      guard isStarted, !Task.isCancelled else {
        await exchange.close()
        return
      }
      sideExchange = exchange
      while !Task.isCancelled, let frame = try await exchange.next() {
        let isTerminalBoundary: Bool
        if case .transcriptSnapshotEnd = frame.message {
          isTerminalBoundary = true
        } else {
          isTerminalBoundary = false
        }
        await process(
          .sideFrame(
            generation: generation,
            refreshID: refreshID,
            message: frame.message
          )
        )
        if isTerminalBoundary {
          return
        }
      }
      guard !Task.isCancelled, isStarted else {
        return
      }
      await process(
        .sideTransportEnded(
          generation: generation,
          refreshID: refreshID,
          message: "The side transcript connection closed."
        )
      )
    } catch is CancellationError {
      return
    } catch {
      guard !Task.isCancelled, isStarted else {
        return
      }
      await process(
        .sideTransportEnded(
          generation: generation,
          refreshID: refreshID,
          message: error.localizedDescription
        )
      )
    }
  }

  private func armDeadline(
    _ token: SignalboxSynchronizationDeadlineToken,
    duration: Duration
  ) {
    deadlineTask?.cancel()
    deadlineToken = token
    deadlineTask = Task { [weak self] in
      do {
        try await Task.sleep(for: duration)
      } catch {
        return
      }
      guard !Task.isCancelled else {
        return
      }
      await self?.deadlineExpired(token)
    }
  }

  private func deadlineExpired(
    _ token: SignalboxSynchronizationDeadlineToken
  ) async {
    guard deadlineToken == token, isStarted else {
      return
    }
    deadlineTask = nil
    deadlineToken = nil
    await process(.deadlineExpired(token))
  }

  private func cancelDeadline(
    _ token: SignalboxSynchronizationDeadlineToken
  ) {
    guard deadlineToken == token else {
      return
    }
    deadlineTask?.cancel()
    deadlineTask = nil
    deadlineToken = nil
  }

  private func scheduleReconnect(
    generation: UInt64,
    delay: Duration
  ) {
    reconnectTask?.cancel()
    reconnectTask = Task { [weak self] in
      do {
        try await Task.sleep(for: delay)
      } catch {
        return
      }
      guard !Task.isCancelled else {
        return
      }
      await self?.reconnectReady(generation: generation)
    }
  }

  private func reconnectReady(generation: UInt64) async {
    guard isStarted else {
      return
    }
    reconnectTask = nil
    await process(.retryReady(generation: generation))
  }

  private func closeTransports() async {
    primaryTask?.cancel()
    primaryTask = nil
    if let primaryExchange {
      await primaryExchange.close()
    }
    primaryExchange = nil
    await closeSideTransport()
  }

  private func closeSideTransport() async {
    sideTask?.cancel()
    sideTask = nil
    if let sideExchange {
      await sideExchange.close()
    }
    sideExchange = nil
  }

  private func cancelAllWork() async {
    deadlineTask?.cancel()
    deadlineTask = nil
    deadlineToken = nil
    reconnectTask?.cancel()
    reconnectTask = nil
    await closeTransports()
  }
}
