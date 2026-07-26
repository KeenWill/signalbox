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

  init(serviceProvider: @escaping () -> (any SignalboxProcessServiceProtocol)?) {
    self.serviceProvider = serviceProvider
  }

  func replaceServiceProvider(
    _ provider: @escaping () -> (any SignalboxProcessServiceProtocol)?
  ) {
    serviceProvider = provider
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
    guard let service = serviceProvider() else {
      errorMessage = remoteTransportGateMessage
      return
    }
    isLoading = true
    defer { isLoading = false }
    do {
      sessions = try await service.listSessions(includeArchived: true)
      errorMessage = nil
    } catch {
      errorMessage = error.localizedDescription
    }
  }

  func toggleArchive(_ session: SignalboxProcessSession) async {
    guard let service = serviceProvider() else {
      errorMessage = remoteTransportGateMessage
      return
    }
    do {
      let replacement = try await service.setArchived(!session.archived, session: session)
      guard let index = sessions.firstIndex(where: { $0.id == session.id }) else {
        return
      }
      sessions[index] = replacement
      errorMessage = nil
    } catch {
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
  @Published private(set) var timeline: [SignalboxTimelineItem] = []
  @Published private(set) var pendingInputs: [SignalboxProcessPendingInput] = []
  @Published private(set) var activity = SignalboxProcessActivity.unavailable
  @Published private(set) var phase: SignalboxSessionSynchronizationPhase = .stopped
  @Published private(set) var latestDiagnostic: String?
  @Published private(set) var isSubmitting = false
  @Published var composerText = ""
  @Published var errorMessage: String?

  let session: SignalboxProcessSession
  private var serviceProvider: () -> (any SignalboxProcessServiceProtocol)?
  private var synchronization: (any SignalboxSessionSynchronizing)?
  private var unresolvedSubmission: SignalboxPreparedInputSubmission?
  private var materializedAcceptedInputIDs: Set<SignalboxCanonicalUUID> = []
  private var projector = SignalboxProcessTranscriptProjector()
  private let normalizer = SignalboxIncrementalEventNormalizer()

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

  func connect() async {
    guard let service = serviceProvider() else {
      errorMessage = remoteTransportGateMessage
      return
    }
    let synchronization = await service.makeSynchronization(
      sessionID: session.id
    ) { [weak self] update in
      await self?.apply(update)
    }
    self.synchronization = synchronization
    await synchronization.start()
  }

  func disconnect() {
    let current = synchronization
    synchronization = nil
    Task {
      await current?.stop()
    }
  }

  func send() async {
    let content = composerText
    guard
      !content.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
      !isSubmitting,
      let service = serviceProvider()
    else {
      return
    }
    isSubmitting = true
    defer { isSubmitting = false }
    var preparedForAttempt: SignalboxPreparedInputSubmission?
    do {
      let prepared: SignalboxPreparedInputSubmission
      if let unresolvedSubmission,
        hasExactUTF8(unresolvedSubmission.content, content)
      {
        prepared = unresolvedSubmission
      } else {
        unresolvedSubmission = nil
        prepared = try await service.prepareInputSubmission(
          session: session,
          content: content
        )
      }
      preparedForAttempt = prepared
      let submitted = try await service.submit(prepared)
      pendingInputs.removeAll { $0.id == submitted.acceptedInputID }
      if !materializedAcceptedInputIDs.contains(submitted.acceptedInputID) {
        pendingInputs.append(
          SignalboxProcessPendingInput(
            id: submitted.acceptedInputID,
            turnID: submitted.turnID,
            content: prepared.content
          )
        )
      }
      unresolvedSubmission = nil
      if hasExactUTF8(content, prepared.content) {
        composerText = ""
      }
      errorMessage = nil
    } catch {
      if let serviceError = error as? SignalboxProcessServiceError,
        case .mutationRetryExhausted = serviceError
      {
        unresolvedSubmission = preparedForAttempt
      } else {
        unresolvedSubmission = nil
      }
      errorMessage = error.localizedDescription
    }
  }

  func apply(_ update: SignalboxSessionSynchronizationDriverUpdate) {
    do {
      switch update {
      case .phase(let phase):
        self.phase = phase
      case .authoritativeSnapshot(let snapshot):
        let projection = try projector.projectAuthoritativeSnapshot(snapshot)
        try normalizer.replaceAll(with: projection.records)
        timeline = normalizer.timelineItems
        pendingInputs = projection.pendingInputs
        materializedAcceptedInputIDs = projection.materializedAcceptedInputIDs
        activity = projection.activity
        errorMessage = nil
      case .sideSnapshot(let snapshot, let trigger):
        let projection = try projector.projectSideSnapshot(snapshot, attributableTo: trigger)
        normalizer.upsert(contentsOf: projection.records)
        timeline = normalizer.timelineItems
        materializedAcceptedInputIDs.formUnion(projection.materializedAcceptedInputIDs)
        pendingInputs.removeAll {
          projection.materializedAcceptedInputIDs.contains($0.id)
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
    }
  }

  private func hasExactUTF8(_ lhs: String, _ rhs: String) -> Bool {
    lhs.utf8.elementsEqual(rhs.utf8)
  }

  private func applyLiveEvent(_ event: SignalboxProcessSessionEvent) {
    switch event {
    case .inputAccepted(let acceptedInputID, let turnID, _, let content):
      pendingInputs.removeAll { $0.id == acceptedInputID }
      if !materializedAcceptedInputIDs.contains(acceptedInputID) {
        pendingInputs.append(
          SignalboxProcessPendingInput(
            id: acceptedInputID,
            turnID: turnID,
            content: content
          )
        )
        activity = .init(state: .queued, label: "Queued")
      }
    case .turnActivated:
      activity = .init(state: .running, label: "Running")
    case .modelCallTransition(_, _, let state):
      applyModelCallState(state)
    case .toolBatchTransition(_, _, let state):
      switch state {
      case .proposed:
        activity = .init(
          state: .waitingForToolDecision,
          label: "Tool decision unavailable"
        )
      case .resultsProjected:
        activity = .init(state: .running, label: "Running")
      case .recoveryRequired:
        activity = .init(state: .recoveryRequired, label: "Recovery required")
      case .unknown:
        break
      }
    case .turnCompleted:
      activity = .init(state: .completed, label: "Completed")
    case .turnFailed:
      activity = .init(state: .failed, label: "Failed")
    case .turnRefused(let turnID, _, _):
      pendingInputs.removeAll { $0.turnID == turnID }
      activity = .init(state: .refused, label: "Refused")
    case .turnCancelled:
      activity = .init(state: .cancelled, label: "Cancelled")
    case .turnReconciliationRequired, .turnToolReconciliationRequired:
      activity = .init(state: .recoveryRequired, label: "Recovery required")
    case .sessionCreated, .unknown:
      break
    }
  }

  private func applyModelCallState(_ state: SignalboxModelCallState) {
    switch state {
    case .prepared, .inFlight, .cancellationRequested:
      activity = .init(state: .running, label: "Running")
    case .terminal(let disposition):
      switch disposition {
      case .completed:
        activity = .init(state: .completed, label: "Completed")
      case .knownFailed, .ambiguous:
        activity = .init(state: .failed, label: "Failed")
      case .refused:
        activity = .init(state: .refused, label: "Refused")
      case .cancelled:
        activity = .init(state: .cancelled, label: "Cancelled")
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
      ScrollView {
        LazyVStack(alignment: .leading, spacing: 12) {
          ForEach(viewModel.timeline) { item in
            processTimelineView(item)
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
          if viewModel.timeline.isEmpty && viewModel.pendingInputs.isEmpty {
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
