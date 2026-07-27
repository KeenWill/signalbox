import Combine
import SwiftUI

#if canImport(SignalboxClient)
  import SignalboxClient
#endif
#if canImport(SignalboxModels)
  import SignalboxModels
#endif

@MainActor
final class ProcessSessionListViewModel: ObservableObject {
  @Published private(set) var sessions: [SignalboxProcessSession] = []
  @Published var showArchived = false
  @Published var searchText = ""
  @Published var errorMessage: String?
  @Published private(set) var isLoading = false

  private var serviceProvider: () -> (any SignalboxProcessServiceProtocol)?
  private var activeRefreshID = UUID()
  private var serviceGeneration: UInt64 = 0
  private var publicationGeneration: UInt64 = 0

  init(serviceProvider: @escaping () -> (any SignalboxProcessServiceProtocol)?) {
    self.serviceProvider = serviceProvider
  }

  func replaceServiceProvider(
    _ provider: @escaping () -> (any SignalboxProcessServiceProtocol)?
  ) {
    serviceProvider = provider
    serviceGeneration &+= 1
    publicationGeneration &+= 1
    activeRefreshID = UUID()
    sessions = []
    errorMessage = nil
    isLoading = false
  }

  var visibleSessions: [SignalboxProcessSession] {
    let matchingArchive = sessions.filter { $0.archived == showArchived }
    let query = searchText.trimmingCharacters(in: .whitespacesAndNewlines)
    guard !query.isEmpty else {
      return matchingArchive
    }
    return matchingArchive.filter {
      $0.displayTitle.localizedCaseInsensitiveContains(query)
        || $0.tags.contains { $0.localizedCaseInsensitiveContains(query) }
        || $0.id.rawValue.localizedCaseInsensitiveContains(query)
    }
  }

  func session(id: SignalboxCanonicalUUID) -> SignalboxProcessSession? {
    sessions.first { $0.id == id }
  }

  func refresh() async {
    let refreshID = UUID()
    publicationGeneration &+= 1
    let publication = publicationGeneration
    let generation = serviceGeneration
    activeRefreshID = refreshID
    guard let service = serviceProvider() else {
      isLoading = false
      errorMessage = remoteTransportGateMessage
      return
    }
    isLoading = true
    defer {
      if activeRefreshID == refreshID {
        isLoading = false
      }
    }
    do {
      let refreshedSessions = try await service.listSessions(includeArchived: true)
      guard activeRefreshID == refreshID, serviceGeneration == generation,
        publicationGeneration == publication
      else {
        return
      }
      sessions = refreshedSessions
      errorMessage = nil
    } catch {
      guard activeRefreshID == refreshID, serviceGeneration == generation,
        publicationGeneration == publication
      else {
        return
      }
      errorMessage = error.localizedDescription
    }
  }

  func toggleArchive(_ session: SignalboxProcessSession) async {
    let generation = serviceGeneration
    publicationGeneration &+= 1
    activeRefreshID = UUID()
    isLoading = false
    guard let service = serviceProvider() else {
      errorMessage = remoteTransportGateMessage
      return
    }
    do {
      let replacement = try await service.setArchived(!session.archived, session: session)
      guard serviceGeneration == generation else {
        return
      }
      publicationGeneration &+= 1
      activeRefreshID = UUID()
      isLoading = false
      guard let index = sessions.firstIndex(where: { $0.id == session.id }) else {
        return
      }
      sessions[index] = replacement
      errorMessage = nil
    } catch {
      guard serviceGeneration == generation else {
        return
      }
      publicationGeneration &+= 1
      activeRefreshID = UUID()
      isLoading = false
      errorMessage = error.localizedDescription
    }
  }
}

struct ProcessSessionsScreen: View {
  @EnvironmentObject private var coordinator: AppCoordinator
  @StateObject private var viewModel = ProcessSessionListViewModel { nil }
  @State private var selectedSessionID: SignalboxCanonicalUUID?
  @State private var showCreationGate = false

  var body: some View {
    NavigationStack {
      content
        .navigationTitle("Sessions")
        .toolbar {
          ToolbarItem(placement: .primaryAction) {
            Button {
              showCreationGate = true
            } label: {
              Label("Create Session", systemImage: "plus")
            }
            .accessibilityIdentifier("create-session-button")
          }
          ToolbarItem(placement: .automatic) {
            Button {
              Task { await viewModel.refresh() }
            } label: {
              Label("Refresh", systemImage: "arrow.clockwise")
            }
          }
        }
        .searchable(text: $viewModel.searchText, prompt: "Search sessions")
        .alert("Creation unavailable", isPresented: $showCreationGate) {
          Button("OK", role: .cancel) {}
        } message: {
          Text(
            "The process protocol requires a model-selection UUID but exposes no model-discovery operation."
          )
        }
        .navigationDestination(item: $selectedSessionID) { sessionID in
          if let session = viewModel.session(id: sessionID) {
            ProcessSessionDetailScreen(session: session)
          } else {
            EmptyStateView(
              systemImage: "questionmark.folder",
              title: "Session unavailable",
              message: "Refresh the session list and try again."
            )
          }
        }
        .task {
          viewModel.replaceServiceProvider { coordinator.processService }
          await viewModel.refresh()
          applyRequestedSelection()
          if coordinator.screenshotScenario == .newSession {
            showCreationGate = true
          }
        }
        .onReceive(NotificationCenter.default.publisher(for: .refreshRequested)) { _ in
          Task {
            await viewModel.refresh()
            applyRequestedSelection()
          }
        }
        .onReceive(NotificationCenter.default.publisher(for: .processServiceChanged)) { _ in
          selectedSessionID = nil
          coordinator.selectedProcessSessionID = nil
          viewModel.replaceServiceProvider { coordinator.processService }
          Task {
            await viewModel.refresh()
            applyRequestedSelection()
          }
        }
    }
  }

  @ViewBuilder
  private var content: some View {
    if coordinator.processService == nil {
      ProcessTransportGateView()
    } else {
      VStack(spacing: 0) {
        Picker("Session state", selection: $viewModel.showArchived) {
          Text("Active").tag(false)
          Text("Archived").tag(true)
        }
        .pickerStyle(.segmented)
        .padding([.horizontal, .top])
        .accessibilityIdentifier("session-state-picker")

        if let errorMessage = viewModel.errorMessage {
          ErrorBanner(message: errorMessage)
            .padding(.horizontal)
            .padding(.top, 8)
        }

        if viewModel.visibleSessions.isEmpty && !viewModel.isLoading {
          EmptyStateView(
            systemImage: viewModel.showArchived ? "archivebox" : "bubble.left.and.bubble.right",
            title: viewModel.showArchived ? "No archived sessions" : "No active sessions",
            message: "Refresh after connecting to a local signalboxd Unix socket."
          )
        } else {
          List(viewModel.visibleSessions) { session in
            Button {
              coordinator.selectedProcessSessionID = session.id
              selectedSessionID = session.id
            } label: {
              ProcessSessionRow(session: session)
            }
            .buttonStyle(.plain)
            .swipeActions(edge: .trailing) {
              Button {
                Task { await viewModel.toggleArchive(session) }
              } label: {
                Label(
                  session.archived ? "Unarchive" : "Archive",
                  systemImage: session.archived ? "tray.and.arrow.up" : "archivebox"
                )
              }
            }
            .accessibilityIdentifier("session-row-\(session.id.rawValue)")
          }
          .listStyle(.plain)
          .accessibilityIdentifier("session-list")
        }
      }
    }
  }

  private func applyRequestedSelection() {
    guard selectedSessionID == nil,
      let requested = coordinator.selectedProcessSessionID,
      viewModel.session(id: requested) != nil
    else {
      return
    }
    selectedSessionID = requested
  }
}

private struct ProcessSessionRow: View {
  let session: SignalboxProcessSession

  var body: some View {
    HStack(alignment: .top, spacing: 12) {
      Image(systemName: "point.3.connected.trianglepath.dotted")
        .font(.title3.weight(.semibold))
        .foregroundStyle(.accent)
        .frame(width: 30)
      VStack(alignment: .leading, spacing: 6) {
        Text(session.displayTitle)
          .font(.headline)
          .lineLimit(2)
        Label(session.modelSelectionLabel, systemImage: "cpu")
          .font(.caption)
          .foregroundStyle(.secondary)
        if !session.tags.isEmpty {
          Text(session.tags.prefix(4).joined(separator: "  "))
            .font(.caption2.weight(.semibold))
            .foregroundStyle(.secondary)
        }
      }
      Spacer()
      Text("v\(session.defaultsVersion.rawValue)")
        .font(.caption.monospacedDigit())
        .foregroundStyle(.secondary)
    }
    .padding(.vertical, 6)
  }
}

@MainActor
final class ProcessSessionDetailViewModel: ObservableObject {
  enum TranscriptRow: Identifiable {
    case timeline(SignalboxTimelineItem)
    case accepted(SignalboxProcessPendingInput)

    var id: String {
      switch self {
      case .timeline(let item):
        "timeline-\(item.id)"
      case .accepted(let input):
        "accepted-\(input.id.rawValue)"
      }
    }
  }

  @Published private(set) var timeline: [SignalboxTimelineItem] = []
  @Published private(set) var pendingInputs: [SignalboxProcessPendingInput] = []
  @Published private(set) var acceptedInputsAwaitingTranscript: [SignalboxProcessPendingInput] = []
  @Published private(set) var activity = SignalboxProcessActivity.unavailable
  @Published private(set) var phase: SignalboxSessionSynchronizationPhase = .stopped
  @Published private(set) var latestDiagnostic: String?
  @Published private(set) var isSubmitting = false
  @Published var composerText = ""
  @Published var errorMessage: String?

  let session: SignalboxProcessSession
  private var serviceProvider: () -> (any SignalboxProcessServiceProtocol)?
  private var connectedService: (any SignalboxProcessServiceProtocol)?
  private var synchronization: (any SignalboxSessionSynchronizing)?
  private var synchronizationGeneration: UInt64 = 0
  private var serviceGeneration: UInt64 = 0
  private var unresolvedSubmission: SignalboxPreparedInputSubmission?
  private var materializedAcceptedInputIDs: Set<SignalboxCanonicalUUID> = []
  private var terminalTurnIDs: Set<SignalboxCanonicalUUID> = []
  private var acceptedInputTimelineOffsets: [SignalboxCanonicalUUID: Int] = [:]
  private var projector = SignalboxProcessTranscriptProjector()
  private var normalizer = SignalboxIncrementalEventNormalizer()

  init(
    session: SignalboxProcessSession,
    serviceProvider: @escaping () -> (any SignalboxProcessServiceProtocol)?
  ) {
    self.session = session
    self.serviceProvider = serviceProvider
  }

  func replaceServiceProvider(
    _ provider: @escaping () -> (any SignalboxProcessServiceProtocol)?
  ) {
    serviceProvider = provider
  }

  func connect(replacingService: Bool = false) async {
    synchronizationGeneration &+= 1
    let generation = synchronizationGeneration
    let prior = synchronization
    synchronization = nil
    connectedService = nil
    if replacingService {
      resetServiceOwnedPresentation()
    }
    await prior?.stop()
    guard synchronizationGeneration == generation, !Task.isCancelled else {
      return
    }
    guard let service = serviceProvider() else {
      errorMessage = remoteTransportGateMessage
      return
    }
    let synchronization = await service.makeSynchronization(
      sessionID: session.id
    ) { [weak self] update in
      await self?.apply(update, generation: generation)
    }
    guard synchronizationGeneration == generation, !Task.isCancelled else {
      await synchronization.stop()
      return
    }
    connectedService = service
    self.synchronization = synchronization
    await synchronization.start()
  }

  func disconnect() {
    synchronizationGeneration &+= 1
    let current = synchronization
    synchronization = nil
    connectedService = nil
    Task {
      await current?.stop()
    }
  }

  func send() async {
    let content = composerText
    guard
      !content.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
      !isSubmitting,
      let service = connectedService
    else {
      return
    }
    let generation = serviceGeneration
    isSubmitting = true
    defer {
      if serviceGeneration == generation {
        isSubmitting = false
      }
    }
    var preparedForAttempt: SignalboxPreparedInputSubmission?
    var reusedUnresolvedSubmission = false
    do {
      let prepared: SignalboxPreparedInputSubmission
      if let unresolvedSubmission,
        hasExactUTF8(unresolvedSubmission.content, content)
      {
        prepared = unresolvedSubmission
        reusedUnresolvedSubmission = true
      } else {
        unresolvedSubmission = nil
        prepared = try await service.prepareInputSubmission(
          session: session,
          content: content
        )
      }
      preparedForAttempt = prepared
      guard serviceGeneration == generation else {
        return
      }
      let submitted = try await service.submit(prepared)
      guard serviceGeneration == generation else {
        return
      }
      let acceptedInput = SignalboxProcessPendingInput(
        id: submitted.acceptedInputID,
        turnID: submitted.turnID,
        acceptancePosition: submitted.acceptancePosition,
        content: prepared.content
      )
      pendingInputs.removeAll { $0.id == submitted.acceptedInputID }
      acceptedInputsAwaitingTranscript.removeAll { $0.id == submitted.acceptedInputID }
      if !materializedAcceptedInputIDs.contains(submitted.acceptedInputID) {
        if terminalTurnIDs.contains(submitted.turnID) {
          retainAcceptedInputAwaitingTranscript(acceptedInput)
        } else {
          pendingInputs.append(acceptedInput)
          pendingInputs.sort { $0.acceptancePosition.rawValue < $1.acceptancePosition.rawValue }
        }
      }
      unresolvedSubmission = nil
      if hasExactUTF8(composerText, prepared.content) {
        composerText = ""
      }
      errorMessage = nil
    } catch {
      guard serviceGeneration == generation else {
        return
      }
      if error is CancellationError {
        unresolvedSubmission = preparedForAttempt
      } else if let serviceError = error as? SignalboxProcessServiceError,
        case .mutationRetryExhausted = serviceError
      {
        unresolvedSubmission = preparedForAttempt
      } else if let openError = error as? SignalboxProcessRequestOpenError,
        case .definitelyUnsent = openError,
        reusedUnresolvedSubmission
      {
        unresolvedSubmission = preparedForAttempt
      } else {
        unresolvedSubmission = nil
      }
      errorMessage = error.localizedDescription
    }
  }

  func apply(_ update: SignalboxSessionSynchronizationDriverUpdate) {
    apply(update, generation: synchronizationGeneration)
  }

  private func apply(
    _ update: SignalboxSessionSynchronizationDriverUpdate,
    generation: UInt64
  ) {
    guard synchronizationGeneration == generation else {
      return
    }
    do {
      switch update {
      case .phase(let phase):
        self.phase = phase
      case .authoritativeSnapshot(let snapshot):
        let projection = try projector.projectAuthoritativeSnapshot(snapshot)
        try normalizer.replaceAll(with: projection.records)
        timeline = normalizer.timelineItems
        pendingInputs = projection.pendingInputs
        acceptedInputsAwaitingTranscript.removeAll {
          projection.materializedAcceptedInputIDs.contains($0.id)
        }
        for id in projection.materializedAcceptedInputIDs {
          acceptedInputTimelineOffsets[id] = nil
        }
        materializedAcceptedInputIDs = projection.materializedAcceptedInputIDs
        terminalTurnIDs = terminalTurnIDs(in: snapshot)
        activity = projection.activity
        errorMessage = nil
      case .sideSnapshot(let snapshot, let trigger):
        let projection = try projector.projectSideSnapshot(snapshot, attributableTo: trigger)
        normalizer.upsert(contentsOf: projection.records)
        timeline = normalizer.timelineItems
        materializedAcceptedInputIDs.formUnion(projection.materializedAcceptedInputIDs)
        if projection.activity.state == .waitingForToolDecision,
          sideSnapshotApprovalMatchesTrigger(snapshot, trigger: trigger)
        {
          activity = projection.activity
        }
        pendingInputs.removeAll {
          projection.materializedAcceptedInputIDs.contains($0.id)
        }
        acceptedInputsAwaitingTranscript.removeAll {
          projection.materializedAcceptedInputIDs.contains($0.id)
        }
        for id in projection.materializedAcceptedInputIDs {
          acceptedInputTimelineOffsets[id] = nil
        }
      case .event(let followed):
        applyLiveEvent(followed.event)
      case .diagnostic(let diagnostic):
        latestDiagnostic = diagnostic.message
      case .retryLimitReached:
        errorMessage = "Synchronization reached its bounded reconnect limit."
      case .terminalFailure:
        errorMessage = "Synchronization stopped after a terminal protocol failure."
      }
    } catch {
      errorMessage = error.localizedDescription
      let synchronization = synchronization
      Task {
        await synchronization?.recoverFromProjectionFailure(error.localizedDescription)
      }
    }
  }

  var transcriptRows: [TranscriptRow] {
    var rows = timeline.map(TranscriptRow.timeline)
    for input in acceptedInputsAwaitingTranscript.reversed() {
      let offset = min(acceptedInputTimelineOffsets[input.id] ?? rows.count, rows.count)
      rows.insert(.accepted(input), at: offset)
    }
    return rows
  }

  private func hasExactUTF8(_ lhs: String, _ rhs: String) -> Bool {
    lhs.utf8.elementsEqual(rhs.utf8)
  }

  private func sideSnapshotApprovalMatchesTrigger(
    _ snapshot: SignalboxSynchronizationSnapshot,
    trigger: SignalboxFollowedSessionEvent
  ) -> Bool {
    guard
      case .toolBatchTransition(let triggerTurnID, let triggerModelCallID, .proposed) =
        trigger.event,
      let requestID = snapshot.records.compactMap({ record -> SignalboxCanonicalUUID? in
        guard case .turn(let turn) = record,
          turn.turnID == triggerTurnID,
          case .activeAwaitingToolApproval(let requestID) = turn.state
        else {
          return nil
        }
        return requestID
      }).first
    else {
      return false
    }
    return snapshot.records.contains { record in
      guard case .entry(let entry) = record,
        case .assistantToolUse(
          let turnID,
          let modelCallID,
          let entryRequestID,
          _,
          _
        ) = entry.entry
      else {
        return false
      }
      return turnID == triggerTurnID
        && modelCallID == triggerModelCallID
        && entryRequestID == requestID
    }
  }

  private func resetServiceOwnedPresentation() {
    serviceGeneration &+= 1
    timeline = []
    pendingInputs = []
    acceptedInputsAwaitingTranscript = []
    acceptedInputTimelineOffsets = [:]
    activity = .unavailable
    phase = .stopped
    latestDiagnostic = nil
    isSubmitting = false
    errorMessage = nil
    unresolvedSubmission = nil
    materializedAcceptedInputIDs = []
    terminalTurnIDs = []
    projector = SignalboxProcessTranscriptProjector()
    normalizer = SignalboxIncrementalEventNormalizer()
  }

  private func applyLiveEvent(_ event: SignalboxProcessSessionEvent) {
    switch event {
    case .inputAccepted(let acceptedInputID, let turnID, let acceptancePosition, let content):
      if !materializedAcceptedInputIDs.contains(acceptedInputID) {
        let acceptedInput = SignalboxProcessPendingInput(
          id: acceptedInputID,
          turnID: turnID,
          acceptancePosition: acceptancePosition,
          content: content
        )
        if terminalTurnIDs.contains(turnID) {
          pendingInputs.removeAll { $0.id == acceptedInputID }
          retainAcceptedInputAwaitingTranscript(acceptedInput)
          return
        }
        if let index = pendingInputs.firstIndex(where: { $0.id == acceptedInputID }) {
          pendingInputs[index] = acceptedInput
        } else {
          pendingInputs.append(acceptedInput)
        }
        pendingInputs.sort { $0.acceptancePosition.rawValue < $1.acceptancePosition.rawValue }
        if !activityRepresentsActiveTurn {
          activity = .init(state: .queued, label: "Queued")
        }
      }
    case .turnActivated:
      activity = .init(state: .running, label: "Running")
    case .modelCallTransition(_, _, let state):
      applyModelCallState(state)
    case .toolBatchTransition(_, _, let state):
      switch state {
      case .proposed:
        activity = .init(state: .running, label: "Running")
      case .resultsProjected:
        activity = .init(state: .running, label: "Running")
      case .recoveryRequired:
        activity = .init(state: .recoveryRequired, label: "Recovery required")
      case .unknown:
        break
      }
    case .turnCompleted(let turnID, _, _, _):
      applyTerminalTurn(
        turnID: turnID,
        terminalActivity: .init(state: .completed, label: "Completed")
      )
    case .turnFailed(let turnID, _, _):
      applyTerminalTurn(
        turnID: turnID,
        terminalActivity: .init(state: .failed, label: "Failed")
      )
    case .turnRefused(let turnID, _, _):
      applyTerminalTurn(
        turnID: turnID,
        terminalActivity: .init(state: .refused, label: "Refused")
      )
    case .turnCancelled(let turnID, _, _):
      applyTerminalTurn(
        turnID: turnID,
        terminalActivity: .init(state: .cancelled, label: "Cancelled")
      )
    case .turnReconciliationRequired, .turnToolReconciliationRequired:
      activity = .init(state: .recoveryRequired, label: "Recovery required")
    case .sessionCreated, .unknown:
      break
    }
  }

  private func applyTerminalTurn(
    turnID: SignalboxCanonicalUUID,
    terminalActivity: SignalboxProcessActivity
  ) {
    terminalTurnIDs.insert(turnID)
    let acceptedInputs = pendingInputs.filter { $0.turnID == turnID }
    pendingInputs.removeAll { $0.turnID == turnID }
    for acceptedInput in acceptedInputs {
      retainAcceptedInputAwaitingTranscript(acceptedInput)
    }
    activity =
      if pendingInputs.isEmpty {
        terminalActivity
      } else {
        .init(state: .queued, label: "Queued")
      }
  }

  private func retainAcceptedInputAwaitingTranscript(
    _ acceptedInput: SignalboxProcessPendingInput
  ) {
    if let index = acceptedInputsAwaitingTranscript.firstIndex(where: {
      $0.id == acceptedInput.id
    }) {
      acceptedInputsAwaitingTranscript[index] = acceptedInput
    } else {
      acceptedInputTimelineOffsets[acceptedInput.id] = timeline.count
      acceptedInputsAwaitingTranscript.append(acceptedInput)
    }
    acceptedInputsAwaitingTranscript.sort {
      $0.acceptancePosition.rawValue < $1.acceptancePosition.rawValue
    }
  }

  private func terminalTurnIDs(
    in snapshot: SignalboxSynchronizationSnapshot
  ) -> Set<SignalboxCanonicalUUID> {
    Set(
      snapshot.records.compactMap { record in
        guard case .turn(let turn) = record else {
          return nil
        }
        switch turn.state {
        case .failed, .completed, .refused, .cancelled:
          return turn.turnID
        case .queued, .activeRunning, .activeAwaitingToolApproval,
          .activeAwaitingModelCallRecovery, .activeAwaitingToolRecovery,
          .reconciliationRequired, .toolReconciliationRequired, .unknown:
          return nil
        }
      })
  }

  private var activityRepresentsActiveTurn: Bool {
    switch activity.state {
    case .running, .waitingForToolDecision, .recoveryRequired:
      return true
    case .unavailable, .queued, .failed, .completed, .refused, .cancelled:
      return false
    }
  }

  private func applyModelCallState(_ state: SignalboxModelCallState) {
    switch state {
    case .prepared, .inFlight, .cancellationRequested:
      activity = .init(state: .running, label: "Running")
    case .terminal(let disposition):
      switch disposition {
      case .ambiguous:
        activity = .init(state: .recoveryRequired, label: "Recovery required")
      case .completed, .knownFailed, .refused, .cancelled:
        activity = .init(state: .running, label: "Running")
      }
    case .unknown:
      break
    }
  }
}

struct ProcessSessionDetailScreen: View {
  @EnvironmentObject private var coordinator: AppCoordinator
  @StateObject private var viewModel: ProcessSessionDetailViewModel
  @State private var showArtifactGate = false

  init(session: SignalboxProcessSession) {
    _viewModel = StateObject(
      wrappedValue: ProcessSessionDetailViewModel(session: session) { nil }
    )
  }

  var body: some View {
    VStack(spacing: 0) {
      header
        .padding()
        .background(.bar)
      if let error = viewModel.errorMessage {
        ErrorBanner(message: error)
          .padding([.horizontal, .top])
      }
      if let diagnostic = viewModel.latestDiagnostic {
        Text(diagnostic)
          .font(.caption)
          .foregroundStyle(.secondary)
          .padding(.horizontal)
          .padding(.top, 6)
          .accessibilityIdentifier("synchronization-diagnostic")
      }
      if viewModel.session.dangerousToolAutoApproval {
        Label("Dangerous tools are auto-approved", systemImage: "exclamationmark.triangle.fill")
          .font(.callout.weight(.semibold))
          .foregroundStyle(.orange)
          .padding(.horizontal)
          .padding(.top, 8)
          .accessibilityIdentifier("dangerous-tool-auto-approval-warning")
      }
      ScrollView {
        LazyVStack(alignment: .leading, spacing: 12) {
          ForEach(viewModel.transcriptRows) { row in
            switch row {
            case .timeline(let item):
              processTimelineView(item)
            case .accepted(let acceptedInput):
              VStack(alignment: .leading, spacing: 4) {
                Text("Accepted input")
                  .font(.caption.weight(.semibold))
                  .foregroundStyle(.secondary)
                Text(acceptedInput.content)
              }
              .padding(12)
              .background(.secondary.opacity(0.08), in: RoundedRectangle(cornerRadius: 8))
            }
          }
          ForEach(viewModel.pendingInputs) { pending in
            VStack(alignment: .leading, spacing: 4) {
              Text("Pending input")
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
              Text(pending.content)
            }
            .padding(12)
            .background(.secondary.opacity(0.08), in: RoundedRectangle(cornerRadius: 8))
          }
          if viewModel.timeline.isEmpty && viewModel.pendingInputs.isEmpty
            && viewModel.acceptedInputsAwaitingTranscript.isEmpty
          {
            EmptyStateView(
              systemImage: "text.bubble",
              title: "No transcript entries",
              message: "The version 5 snapshot contains no presentable entries."
            )
          }
        }
        .padding()
        .frame(maxWidth: 960)
        .frame(maxWidth: .infinity)
      }
      composer
        .padding()
        .background(.bar)
    }
    .navigationTitle(viewModel.session.displayTitle)
    #if os(iOS)
      .navigationBarTitleDisplayMode(.inline)
    #endif
    .task {
      viewModel.replaceServiceProvider { coordinator.processService }
      await viewModel.connect()
      if coordinator.screenshotScenario == .artifactPreview {
        showArtifactGate = true
      }
    }
    .onReceive(NotificationCenter.default.publisher(for: .processServiceChanged)) { _ in
      Task {
        await viewModel.connect(replacingService: true)
      }
    }
    .onDisappear {
      viewModel.disconnect()
    }
    .alert("Artifacts unavailable", isPresented: $showArtifactGate) {
      Button("OK", role: .cancel) {}
    } message: {
      Text("The version 5 process protocol exposes no artifact operation.")
    }
  }

  private var header: some View {
    HStack {
      VStack(alignment: .leading, spacing: 4) {
        Text(viewModel.session.modelSelectionLabel)
          .font(.caption)
          .foregroundStyle(.secondary)
        Text(viewModel.activity.label)
          .font(.callout.weight(.semibold))
      }
      Spacer()
      Text(phaseLabel)
        .font(.caption.weight(.semibold))
        .foregroundStyle(.secondary)
    }
  }

  private var composer: some View {
    HStack(alignment: .bottom, spacing: 10) {
      TextField("Message the agent", text: $viewModel.composerText, axis: .vertical)
        .textFieldStyle(.roundedBorder)
        .lineLimit(1...5)
        .accessibilityIdentifier("message-composer")
      Button {
        Task { await viewModel.send() }
      } label: {
        Image(systemName: "paperplane.fill")
          .frame(width: 34, height: 34)
      }
      .buttonStyle(.borderedProminent)
      .disabled(
        viewModel.composerText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
          || viewModel.isSubmitting
      )
      .accessibilityLabel("Send")
      .accessibilityIdentifier("send-message-button")
    }
  }

  private var phaseLabel: String {
    switch viewModel.phase {
    case .stopped:
      return "Stopped"
    case .connect:
      return "Connecting"
    case .hello:
      return "Hello"
    case .history:
      return "History"
    case .replay:
      return "Replay"
    case .steady:
      return "Live"
    case .recovery:
      return "Recovering"
    }
  }

  @ViewBuilder
  private func processTimelineView(_ item: SignalboxTimelineItem) -> some View {
    switch item {
    case .message(let message):
      MessageBubble(message: message)
    case .tool(let tool):
      ToolInvocationCard(tool: tool, onApprove: {}, onDeny: {})
    case .turnFailure(let failure):
      FailureCard(failure: failure)
    case .unknown(let unknown):
      UnknownEventCard(unknown: unknown)
    }
  }
}

struct ProcessTransportGateView: View {
  var body: some View {
    ContentUnavailableView {
      Label("Local socket required", systemImage: "cable.connector")
    } description: {
      Text(remoteTransportGateMessage)
    }
    .accessibilityIdentifier("setup-no-connection")
  }
}

struct UnavailableProcessCapabilityView: View {
  let title: String
  let detail: String

  var body: some View {
    ContentUnavailableView {
      Label(title, systemImage: "lock")
    } description: {
      Text(detail)
    }
  }
}
