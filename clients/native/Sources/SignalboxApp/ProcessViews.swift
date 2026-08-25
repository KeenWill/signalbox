import Combine
import SwiftUI

#if canImport(SignalboxClient)
  import SignalboxClient
#endif
#if canImport(SignalboxModels)
  import SignalboxModels
#endif

let importedContinuationInProgressMessage =
  "An imported conversation continuation is already in progress."
let importedContinuationEndpointChangedMessage =
  "The process service changed during continuation. Try again."

@MainActor
/// Refresh and mutation publications are gated by both service generation and
/// operation identity, so a replaced socket cannot update the new endpoint's UI.
final class ProcessSessionListViewModel: ObservableObject {
  @Published private(set) var conversations: [SignalboxProcessConversation] = []
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
    conversations = []
    errorMessage = nil
    isLoading = false
  }

  var visibleConversations: [SignalboxProcessConversation] {
    let matchingArchive = conversations.filter { $0.archived == showArchived }
    let query = searchText.trimmingCharacters(in: .whitespacesAndNewlines)
    guard !query.isEmpty else {
      return matchingArchive
    }
    return matchingArchive.filter {
      $0.displayTitle.localizedCaseInsensitiveContains(query)
        || $0.conversationID.rawValue.localizedCaseInsensitiveContains(query)
        || $0.origin.rawValue.localizedCaseInsensitiveContains(query)
    }
  }

  func conversation(id: String) -> SignalboxProcessConversation? {
    conversations.first { $0.id == id }
  }

  func conversation(conversationID: SignalboxCanonicalUUID) -> SignalboxProcessConversation? {
    conversations.first { $0.conversationID == conversationID }
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
      let refreshedConversations = try await service.listConversations(includeArchived: true)
      guard activeRefreshID == refreshID, serviceGeneration == generation,
        publicationGeneration == publication
      else {
        return
      }
      conversations = refreshedConversations
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

  func toggleArchive(_ conversation: SignalboxProcessConversation) async {
    guard conversation.origin == .native else {
      return
    }
    let generation = serviceGeneration
    publicationGeneration &+= 1
    activeRefreshID = UUID()
    isLoading = false
    guard let service = serviceProvider() else {
      errorMessage = remoteTransportGateMessage
      return
    }
    do {
      let replacement = try await service.setConversationArchived(
        !conversation.archived,
        conversation: conversation
      )
      guard serviceGeneration == generation else {
        return
      }
      publicationGeneration &+= 1
      activeRefreshID = UUID()
      isLoading = false
      guard let index = conversations.firstIndex(where: { $0.id == conversation.id }) else {
        return
      }
      conversations[index] = replacement
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
  @State private var selectedConversationID: String?
  @State private var requestedLocalSessionID: SignalboxCanonicalUUID?
  @State private var showCreationSheet = false

  var body: some View {
    NavigationStack {
      content
        .navigationTitle("Sessions")
        .toolbar {
          ToolbarItem(placement: .primaryAction) {
            Button {
              showCreationSheet = true
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
        .sheet(isPresented: $showCreationSheet) {
          ProcessSessionCreationSheet {
            await viewModel.refresh()
            applyRequestedSelection()
          }
          .environmentObject(coordinator)
        }
        .navigationDestination(item: $selectedConversationID) { conversationID in
          if let conversation = viewModel.conversation(id: conversationID) {
            ProcessConversationDetailScreen(conversation: conversation) { sessionID in
              requestedLocalSessionID = sessionID
            }
          } else {
            EmptyStateView(
              systemImage: "questionmark.folder",
              title: "Session unavailable",
              message: "Refresh the session list and try again."
            )
          }
        }
        .onChange(of: selectedConversationID) { _, selection in
          if selection == nil {
            if requestedLocalSessionID != nil {
              Task {
                await viewModel.refresh()
                applyRequestedSelection()
              }
            }
          }
        }
        .task {
          viewModel.replaceServiceProvider { coordinator.processService }
          await viewModel.refresh()
          applyRequestedSelection()
          if coordinator.screenshotScenario == .newSession {
            showCreationSheet = true
          }
        }
        .onReceive(NotificationCenter.default.publisher(for: .refreshRequested)) { _ in
          Task {
            await viewModel.refresh()
            applyRequestedSelection()
          }
        }
        .onReceive(NotificationCenter.default.publisher(for: .processServiceChanged)) { _ in
          selectedConversationID = nil
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

        if viewModel.visibleConversations.isEmpty && !viewModel.isLoading {
          EmptyStateView(
            systemImage: viewModel.showArchived ? "archivebox" : "bubble.left.and.bubble.right",
            title: viewModel.showArchived ? "No archived sessions" : "No active sessions",
            message: "Refresh after connecting to a local signalboxd Unix socket."
          )
        } else {
          List(viewModel.visibleConversations) { conversation in
            Button {
              selectedConversationID = conversation.id
            } label: {
              ProcessConversationRow(conversation: conversation)
            }
            .buttonStyle(.plain)
            .swipeActions(edge: .trailing) {
              if conversation.origin == .native {
                Button {
                  Task { await viewModel.toggleArchive(conversation) }
                } label: {
                  Label(
                    conversation.archived ? "Unarchive" : "Archive",
                    systemImage: conversation.archived ? "tray.and.arrow.up" : "archivebox"
                  )
                }
              }
            }
            .accessibilityIdentifier("session-row-\(conversation.conversationID.rawValue)")
          }
          .listStyle(.plain)
          .accessibilityIdentifier("session-list")
        }
      }
    }
  }

  private func applyRequestedSelection() {
    guard selectedConversationID == nil,
      let requested = requestedLocalSessionID ?? coordinator.selectedProcessSessionID,
      let conversation = viewModel.conversation(conversationID: requested)
    else {
      return
    }
    if requestedLocalSessionID == requested {
      requestedLocalSessionID = nil
    } else if coordinator.selectedProcessSessionID == requested {
      coordinator.selectedProcessSessionID = nil
    }
    selectedConversationID = conversation.id
  }
}

struct ProcessSessionCreationRetryState {
  private var unresolvedCreation: SignalboxPreparedSessionCreation?

  func reusableCreation(
    modelSelection: SignalboxModelSelection,
    systemPrompt: String?
  ) -> SignalboxPreparedSessionCreation? {
    guard
      let unresolvedCreation,
      unresolvedCreation.modelSelection == modelSelection,
      Self.optionalStringsHaveExactUTF8(unresolvedCreation.systemPrompt, systemPrompt)
    else {
      return nil
    }
    return unresolvedCreation
  }

  mutating func prepareForNewIntent() {
    unresolvedCreation = nil
  }

  mutating func recordSuccess() {
    unresolvedCreation = nil
  }

  mutating func recordFailure(
    _ error: Error,
    prepared: SignalboxPreparedSessionCreation?,
    reusedUnresolvedCreation: Bool
  ) {
    if error is CancellationError {
      unresolvedCreation = prepared
    } else if let serviceError = error as? SignalboxProcessServiceError,
      serviceError.retainsPreparedMutationIdentity
    {
      unresolvedCreation = prepared
    } else if let openError = error as? SignalboxProcessRequestOpenError,
      case .definitelyUnsent = openError,
      reusedUnresolvedCreation
    {
      unresolvedCreation = prepared
    } else {
      unresolvedCreation = nil
    }
  }

  private static func optionalStringsHaveExactUTF8(_ lhs: String?, _ rhs: String?) -> Bool {
    switch (lhs, rhs) {
    case (.none, .none):
      return true
    case (.some(let lhs), .some(let rhs)):
      return lhs.utf8.elementsEqual(rhs.utf8)
    case (.none, .some), (.some, .none):
      return false
    }
  }
}

/// Internal rather than private so the snapshot suite can render the sheet's
/// content as a standalone screen: nothing in process presents a sheet.
struct ProcessSessionCreationSheet: View {
  @EnvironmentObject private var coordinator: AppCoordinator
  @Environment(\.dismiss) private var dismiss
  let didCreate: () async -> Void
  @State private var aliases: [SignalboxModelAliasSummary] = []
  @State private var selectedAliasID: SignalboxCanonicalUUID?
  @State private var systemPrompt = ""
  @State private var isLoading = true
  @State private var isCreating = false
  @State private var errorMessage: String?
  @State private var creationRetryState = ProcessSessionCreationRetryState()

  var body: some View {
    NavigationStack {
      Form {
        if isLoading {
          ProgressView("Reading model aliases")
        } else {
          Picker("Model alias", selection: $selectedAliasID) {
            ForEach(aliases) { alias in
              Text(
                "\(alias.aliasID.rawValue) → \(alias.selectionID.rawValue.prefix(8))"
              )
              .tag(Optional(alias.aliasID))
            }
          }
          .accessibilityIdentifier("model-alias-picker")
          TextField("Optional system prompt", text: $systemPrompt, axis: .vertical)
            .lineLimit(3...10)
            .accessibilityIdentifier("system-prompt-field")
        }
        if aliases.isEmpty && !isLoading {
          Text("The daemon configuration contains no model aliases.")
            .foregroundStyle(.secondary)
        }
        if let errorMessage {
          Text(errorMessage)
            .foregroundStyle(.red)
            .textSelection(.enabled)
        }
      }
      .navigationTitle("New Session")
      .toolbar {
        ToolbarItem(placement: .cancellationAction) {
          Button("Cancel") {
            dismiss()
          }
          .disabled(isCreating)
        }
        ToolbarItem(placement: .confirmationAction) {
          Button("Create") {
            Task { await create() }
          }
          .disabled(selectedAliasID == nil || isCreating)
          .accessibilityIdentifier("confirm-create-session-button")
        }
      }
      .task {
        await loadAliases()
      }
    }
    .frame(minWidth: 520, minHeight: 320)
    .interactiveDismissDisabled(isCreating)
  }

  private func loadAliases() async {
    guard let service = coordinator.processService else {
      errorMessage = remoteTransportGateMessage
      isLoading = false
      return
    }
    do {
      aliases = try await service.listModelAliases()
      selectedAliasID = aliases.first?.aliasID
      errorMessage = nil
    } catch {
      errorMessage = error.localizedDescription
    }
    isLoading = false
  }

  private func create() async {
    guard
      let service = coordinator.processService,
      let selectedAliasID,
      !isCreating
    else {
      return
    }
    isCreating = true
    defer {
      isCreating = false
    }
    var preparedForAttempt: SignalboxPreparedSessionCreation?
    var reusedUnresolvedCreation = false
    do {
      let prompt = systemPrompt.isEmpty ? nil : systemPrompt
      let modelSelection = SignalboxModelSelection.alias(aliasID: selectedAliasID)
      let prepared: SignalboxPreparedSessionCreation
      if let unresolvedCreation = creationRetryState.reusableCreation(
        modelSelection: modelSelection,
        systemPrompt: prompt
      ) {
        prepared = unresolvedCreation
        reusedUnresolvedCreation = true
      } else {
        creationRetryState.prepareForNewIntent()
        prepared = try await service.prepareSessionCreation(
          modelSelection: modelSelection,
          systemPrompt: prompt
        )
      }
      preparedForAttempt = prepared
      let sessionID = try await service.createSession(prepared)
      creationRetryState.recordSuccess()
      coordinator.selectedProcessSessionID = sessionID
      await didCreate()
      dismiss()
    } catch {
      creationRetryState.recordFailure(
        error,
        prepared: preparedForAttempt,
        reusedUnresolvedCreation: reusedUnresolvedCreation
      )
      errorMessage = error.localizedDescription
    }
  }
}

private struct ProcessConversationRow: View {
  let conversation: SignalboxProcessConversation

  var body: some View {
    HStack(alignment: .top, spacing: 12) {
      Image(systemName: "point.3.connected.trianglepath.dotted")
        .font(.title3.weight(.semibold))
        .foregroundStyle(.accent)
        .frame(width: 30)
      VStack(alignment: .leading, spacing: 6) {
        Text(conversation.displayTitle)
          .font(.headline)
          .lineLimit(2)
        Label(
          conversation.origin == .native ? "Native session" : "Imported conversation",
          systemImage: conversation.origin == .native ? "cpu" : "square.and.arrow.down"
        )
          .font(.caption)
          .foregroundStyle(.secondary)
        if let entryCount = conversation.importedEntryCount {
          Text("\(entryCount.rawValue) imported entries")
            .font(.caption2)
            .foregroundStyle(.secondary)
        }
      }
      Spacer()
      if let defaultsVersion = conversation.defaultsVersion {
        Text("v\(defaultsVersion.rawValue)")
          .font(.caption.monospacedDigit())
          .foregroundStyle(.secondary)
      }
    }
    .padding(.vertical, 6)
  }
}

private struct ProcessConversationDetailScreen: View {
  @EnvironmentObject private var coordinator: AppCoordinator
  @Environment(\.dismiss) private var dismiss
  let conversation: SignalboxProcessConversation
  let didCreateSession: (SignalboxCanonicalUUID) -> Void
  @State private var session: SignalboxProcessSession?
  @State private var errorMessage: String?

  var body: some View {
    Group {
      switch conversation.origin {
      case .native:
        if let session {
          ProcessSessionDetailScreen(session: session)
        } else if let errorMessage {
          EmptyStateView(
            systemImage: "exclamationmark.triangle",
            title: "Session unavailable",
            message: errorMessage
          )
        } else {
          ProgressView("Loading session")
        }
      case .imported:
        ProcessImportedConversationScreen(
          conversation: conversation,
          continuationRetryStore: coordinator.importedContinuationRetryStore
        ) { sessionID in
          didCreateSession(sessionID)
          NotificationCenter.default.post(name: .refreshRequested, object: nil)
          dismiss()
        }
      }
    }
    .task(id: conversation.id) {
      guard conversation.origin == .native, let service = coordinator.processService else {
        return
      }
      do {
        session = try await service.readSession(conversation: conversation)
        errorMessage = nil
      } catch {
        errorMessage = error.localizedDescription
      }
    }
  }
}

final class ProcessImportedContinuationRetryConsumer {}

private final class ProcessImportedContinuationWeakConsumer {
  weak var value: ProcessImportedContinuationRetryConsumer?

  init(_ value: ProcessImportedContinuationRetryConsumer) {
    self.value = value
  }

  func matches(_ consumer: ProcessImportedContinuationRetryConsumer) -> Bool {
    value === consumer
  }
}

struct ProcessImportedContinuationRetryState {
  struct Recovery: Equatable {
    let prepared: SignalboxPreparedImportedSessionCreation
    let resolvedSessionID: SignalboxCanonicalUUID?
  }

  private struct PendingRecovery {
    let prepared: SignalboxPreparedImportedSessionCreation
    var consumers: [ProcessImportedContinuationWeakConsumer]
  }

  private struct ResolvedReceipt {
    let prepared: SignalboxPreparedImportedSessionCreation
    let sessionID: SignalboxCanonicalUUID
    let consumer: ProcessImportedContinuationWeakConsumer
  }

  private var pendingRecoveries: [PendingRecovery] = []
  private var resolvedReceipts: [ResolvedReceipt] = []

  mutating func recovery(
    importedConversationID: SignalboxCanonicalUUID,
    throughPosition: SignalboxCanonicalUInt64,
    relationship: SignalboxImportedSessionRelationship,
    modelSelection: SignalboxModelSelection,
    consumer: ProcessImportedContinuationRetryConsumer
  ) -> Recovery? {
    pruneDeadConsumers()
    if let receiptIndex = resolvedReceipts.firstIndex(where: {
      Self.matchesIntent(
        $0.prepared,
        importedConversationID: importedConversationID,
        throughPosition: throughPosition,
        relationship: relationship,
        modelSelection: modelSelection
      ) && $0.consumer.matches(consumer)
    }) {
      let receipt = resolvedReceipts.remove(at: receiptIndex)
      return Recovery(
        prepared: receipt.prepared,
        resolvedSessionID: receipt.sessionID
      )
    }
    guard
      let pendingIndex = pendingRecoveries.firstIndex(where: {
        Self.matchesIntent(
          $0.prepared,
          importedConversationID: importedConversationID,
          throughPosition: throughPosition,
          relationship: relationship,
          modelSelection: modelSelection
        )
      })
    else {
      return nil
    }
    if !pendingRecoveries[pendingIndex].consumers.contains(where: {
      $0.matches(consumer)
    }) {
      pendingRecoveries[pendingIndex].consumers.append(
        ProcessImportedContinuationWeakConsumer(consumer)
      )
    }
    return Recovery(
      prepared: pendingRecoveries[pendingIndex].prepared,
      resolvedSessionID: nil
    )
  }

  mutating func removeAll() {
    pendingRecoveries.removeAll()
    resolvedReceipts.removeAll()
  }

  mutating func recordSuccess(
    _ prepared: SignalboxPreparedImportedSessionCreation,
    sessionID: SignalboxCanonicalUUID,
    consumer: ProcessImportedContinuationRetryConsumer
  ) {
    pruneDeadConsumers()
    guard
      let pendingIndex = pendingRecoveries.firstIndex(where: {
        Self.matchesIntent($0.prepared, prepared)
      })
    else {
      return
    }
    let pending = pendingRecoveries.remove(at: pendingIndex)
    for waitingConsumer in pending.consumers {
      guard let waitingValue = waitingConsumer.value, waitingValue !== consumer else {
        continue
      }
      resolvedReceipts.removeAll {
        Self.matchesIntent($0.prepared, prepared)
          && $0.consumer.matches(waitingValue)
      }
      resolvedReceipts.append(
        ResolvedReceipt(
          prepared: prepared,
          sessionID: sessionID,
          consumer: waitingConsumer
        )
      )
    }
  }

  mutating func recordFailure(
    _ error: Error,
    prepared: SignalboxPreparedImportedSessionCreation?,
    reusedUnresolvedCreation: Bool,
    consumer: ProcessImportedContinuationRetryConsumer
  ) {
    let retainsPreparedCreation: Bool
    if error is CancellationError {
      retainsPreparedCreation = true
    } else if let serviceError = error as? SignalboxProcessServiceError {
      retainsPreparedCreation = serviceError.retainsPreparedImportedSessionCreation
    } else if let openError = error as? SignalboxProcessRequestOpenError {
      retainsPreparedCreation = openError.retainsPreparedImportedSessionCreation(
        whenReused: reusedUnresolvedCreation
      )
    } else {
      retainsPreparedCreation = false
    }
    guard let prepared else {
      return
    }
    pruneDeadConsumers()
    let pendingIndex = pendingRecoveries.firstIndex {
      Self.matchesIntent($0.prepared, prepared)
    }
    guard retainsPreparedCreation else {
      if let pendingIndex {
        pendingRecoveries.remove(at: pendingIndex)
      }
      return
    }
    if let pendingIndex {
      if !pendingRecoveries[pendingIndex].consumers.contains(where: {
        $0.matches(consumer)
      }) {
        pendingRecoveries[pendingIndex].consumers.append(
          ProcessImportedContinuationWeakConsumer(consumer)
        )
      }
    } else {
      pendingRecoveries.append(
        PendingRecovery(
          prepared: prepared,
          consumers: [ProcessImportedContinuationWeakConsumer(consumer)]
        )
      )
    }
  }

  private mutating func pruneDeadConsumers() {
    for index in pendingRecoveries.indices {
      pendingRecoveries[index].consumers.removeAll { $0.value == nil }
    }
    resolvedReceipts.removeAll { $0.consumer.value == nil }
  }

  private static func matchesIntent(
    _ candidate: SignalboxPreparedImportedSessionCreation,
    _ requested: SignalboxPreparedImportedSessionCreation
  ) -> Bool {
    matchesIntent(
      candidate,
      importedConversationID: requested.importedConversationID,
      throughPosition: requested.throughPosition,
      relationship: requested.relationship,
      modelSelection: requested.modelSelection
    )
  }

  private static func matchesIntent(
    _ candidate: SignalboxPreparedImportedSessionCreation,
    importedConversationID: SignalboxCanonicalUUID,
    throughPosition: SignalboxCanonicalUInt64,
    relationship: SignalboxImportedSessionRelationship,
    modelSelection: SignalboxModelSelection
  ) -> Bool {
    candidate.importedConversationID == importedConversationID
      && candidate.throughPosition == throughPosition
      && candidate.relationship == relationship
      && candidate.modelSelection == modelSelection
  }
}

@MainActor
/// Retains outcome-unknown continuation commands across view replacement while
/// keeping resolution and in-flight ownership scoped to one endpoint generation.
final class ProcessImportedContinuationRetryStore {
  private var state = ProcessImportedContinuationRetryState()
  private var endpoint: String?
  private var endpointGeneration: UInt64 = 0
  private var activeAttemptGeneration: UInt64?

  func activateEndpoint(_ endpoint: String?) {
    guard self.endpoint != endpoint else {
      return
    }
    self.endpoint = endpoint
    endpointGeneration &+= 1
    state.removeAll()
  }

  func beginAttempt() -> UInt64? {
    guard activeAttemptGeneration != endpointGeneration else {
      return nil
    }
    activeAttemptGeneration = endpointGeneration
    return endpointGeneration
  }

  func endAttempt(endpointGeneration: UInt64) {
    guard activeAttemptGeneration == endpointGeneration else {
      return
    }
    activeAttemptGeneration = nil
  }

  func recovery(
    importedConversationID: SignalboxCanonicalUUID,
    throughPosition: SignalboxCanonicalUInt64,
    relationship: SignalboxImportedSessionRelationship,
    modelSelection: SignalboxModelSelection,
    consumer: ProcessImportedContinuationRetryConsumer
  ) -> ProcessImportedContinuationRetryState.Recovery? {
    state.recovery(
      importedConversationID: importedConversationID,
      throughPosition: throughPosition,
      relationship: relationship,
      modelSelection: modelSelection,
      consumer: consumer
    )
  }

  func recordSuccess(
    _ prepared: SignalboxPreparedImportedSessionCreation,
    sessionID: SignalboxCanonicalUUID,
    consumer: ProcessImportedContinuationRetryConsumer,
    endpointGeneration: UInt64
  ) -> Bool {
    guard endpointGeneration == self.endpointGeneration else {
      return false
    }
    state.recordSuccess(prepared, sessionID: sessionID, consumer: consumer)
    return true
  }

  func recordFailure(
    _ error: Error,
    prepared: SignalboxPreparedImportedSessionCreation?,
    reusedUnresolvedCreation: Bool,
    consumer: ProcessImportedContinuationRetryConsumer,
    endpointGeneration: UInt64
  ) {
    guard endpointGeneration == self.endpointGeneration else {
      return
    }
    state.recordFailure(
      error,
      prepared: prepared,
      reusedUnresolvedCreation: reusedUnresolvedCreation,
      consumer: consumer
    )
  }
}

private extension SignalboxProcessServiceError {
  var retainsPreparedMutationIdentity: Bool {
    switch self {
    case .mutationRetryExhausted, .remote(code: .unknown, message: _, detail: _):
      return true
    case .unexpectedMessage, .invalidPage, .deadlineExceeded, .remote:
      return false
    }
  }

  var retainsPreparedImportedSessionCreation: Bool {
    retainsPreparedMutationIdentity
  }
}

private extension SignalboxProcessRequestOpenError {
  func retainsPreparedImportedSessionCreation(whenReused: Bool) -> Bool {
    switch self {
    case .definitelyUnsent:
      return whenReused
    case .sendOutcomeUnknown:
      return true
    }
  }
}

@MainActor
final class ProcessImportedConversationViewModel: ObservableObject {
  @Published private(set) var transcript: SignalboxImportedConversationTranscript?
  @Published private(set) var aliases: [SignalboxModelAliasSummary] = []
  @Published private(set) var isLoading = false
  @Published private(set) var isContinuing = false
  @Published var errorMessage: String?

  private var serviceProvider: () -> (any SignalboxProcessServiceProtocol)?
  private var generation: UInt64 = 0
  private let continuationRetryStore: ProcessImportedContinuationRetryStore
  private let continuationConsumer: ProcessImportedContinuationRetryConsumer

  init(serviceProvider: @escaping () -> (any SignalboxProcessServiceProtocol)?) {
    self.serviceProvider = serviceProvider
    continuationRetryStore = ProcessImportedContinuationRetryStore()
    continuationConsumer = ProcessImportedContinuationRetryConsumer()
  }

  init(
    serviceProvider: @escaping () -> (any SignalboxProcessServiceProtocol)?,
    continuationRetryStore: ProcessImportedContinuationRetryStore,
    continuationConsumer: ProcessImportedContinuationRetryConsumer =
      ProcessImportedContinuationRetryConsumer()
  ) {
    self.serviceProvider = serviceProvider
    self.continuationRetryStore = continuationRetryStore
    self.continuationConsumer = continuationConsumer
  }

  func replaceServiceProvider(
    _ provider: @escaping () -> (any SignalboxProcessServiceProtocol)?
  ) {
    serviceProvider = provider
    generation &+= 1
    transcript = nil
    aliases = []
    isLoading = false
    isContinuing = false
    errorMessage = nil
  }

  func load(conversation: SignalboxProcessConversation) async {
    let activeGeneration = generation
    guard let service = serviceProvider() else {
      errorMessage = remoteTransportGateMessage
      return
    }
    isLoading = true
    defer {
      if generation == activeGeneration {
        isLoading = false
      }
    }
    do {
      let transcript = try await service.readImportedConversation(conversation: conversation)
      guard generation == activeGeneration else {
        return
      }
      self.transcript = transcript
      errorMessage = nil
    } catch {
      guard generation == activeGeneration else {
        return
      }
      errorMessage = error.localizedDescription
      return
    }
    do {
      let aliases = try await service.listModelAliases()
      guard generation == activeGeneration else {
        return
      }
      self.aliases = aliases
      errorMessage = nil
    } catch {
      guard generation == activeGeneration else {
        return
      }
      errorMessage = error.localizedDescription
    }
  }

  func continueConversation(
    conversation: SignalboxProcessConversation,
    throughPosition: SignalboxCanonicalUInt64,
    relationship: SignalboxImportedSessionRelationship,
    aliasID: SignalboxCanonicalUUID
  ) async throws -> SignalboxCanonicalUUID {
    guard !isContinuing else {
      let error = SignalboxProcessServiceError.unexpectedMessage(
        importedContinuationInProgressMessage
      )
      errorMessage = error.localizedDescription
      throw error
    }
    guard let retryEndpointGeneration = continuationRetryStore.beginAttempt() else {
      let error = SignalboxProcessServiceError.unexpectedMessage(
        importedContinuationInProgressMessage
      )
      errorMessage = error.localizedDescription
      throw error
    }
    defer {
      continuationRetryStore.endAttempt(endpointGeneration: retryEndpointGeneration)
    }
    isContinuing = true
    defer {
      isContinuing = false
    }
    let selection = SignalboxModelSelection.alias(aliasID: aliasID)
    let recovery = continuationRetryStore.recovery(
      importedConversationID: conversation.conversationID,
      throughPosition: throughPosition,
      relationship: relationship,
      modelSelection: selection,
      consumer: continuationConsumer
    )
    if let resolvedSessionID = recovery?.resolvedSessionID {
      errorMessage = nil
      return resolvedSessionID
    }
    guard let service = serviceProvider() else {
      errorMessage = remoteTransportGateMessage
      throw SignalboxProcessServiceError.unexpectedMessage(remoteTransportGateMessage)
    }
    var preparedForAttempt: SignalboxPreparedImportedSessionCreation?
    var reusedUnresolvedCreation = false
    do {
      let prepared: SignalboxPreparedImportedSessionCreation
      if let recovery {
        prepared = recovery.prepared
        reusedUnresolvedCreation = true
      } else {
        prepared = try await service.prepareImportedSessionCreation(
          conversation: conversation,
          throughPosition: throughPosition,
          relationship: relationship,
          modelSelection: selection
        )
      }
      preparedForAttempt = prepared
      let sessionID = try await service.createSessionFromImportedFrontier(prepared)
      guard
        continuationRetryStore.recordSuccess(
          prepared,
          sessionID: sessionID,
          consumer: continuationConsumer,
          endpointGeneration: retryEndpointGeneration
        )
      else {
        throw SignalboxProcessServiceError.unexpectedMessage(
          importedContinuationEndpointChangedMessage
        )
      }
      errorMessage = nil
      return sessionID
    } catch {
      continuationRetryStore.recordFailure(
        error,
        prepared: preparedForAttempt,
        reusedUnresolvedCreation: reusedUnresolvedCreation,
        consumer: continuationConsumer,
        endpointGeneration: retryEndpointGeneration
      )
      errorMessage = error.localizedDescription
      throw error
    }
  }
}

private struct ProcessImportedConversationScreen: View {
  @EnvironmentObject private var coordinator: AppCoordinator
  let conversation: SignalboxProcessConversation
  let didContinue: (SignalboxCanonicalUUID) -> Void
  @StateObject private var viewModel = ProcessImportedConversationViewModel { nil }
  @State private var selectedPosition: SignalboxCanonicalUInt64?
  @State private var showContinuationSheet = false

  init(
    conversation: SignalboxProcessConversation,
    continuationRetryStore: ProcessImportedContinuationRetryStore,
    didContinue: @escaping (SignalboxCanonicalUUID) -> Void
  ) {
    self.conversation = conversation
    self.didContinue = didContinue
    _viewModel = StateObject(
      wrappedValue: ProcessImportedConversationViewModel(
        serviceProvider: { nil },
        continuationRetryStore: continuationRetryStore
      )
    )
  }

  var body: some View {
    Group {
      if let transcript = viewModel.transcript {
        List {
          Section {
            LabeledContent("Source", value: sourceFormatLabel)
            LabeledContent("Entries", value: "\(transcript.entryCount.rawValue)")
          }
          if let errorMessage = viewModel.errorMessage {
            Section {
              Text(errorMessage)
                .foregroundStyle(.red)
                .textSelection(.enabled)
            }
          } else if viewModel.aliases.isEmpty, !viewModel.isLoading {
            Section {
              Text("The daemon configuration contains no model aliases for continuation.")
                .foregroundStyle(.secondary)
            }
          }
          Section("Read-only transcript") {
            ForEach(transcript.entries) { entry in
              Button {
                selectedPosition = entry.position
              } label: {
                importedEntryRow(entry)
              }
              .buttonStyle(.plain)
              .accessibilityIdentifier("imported-entry-\(entry.position.rawValue)")
            }
          }
        }
        .accessibilityIdentifier("imported-transcript-list")
      } else if let errorMessage = viewModel.errorMessage, !viewModel.isLoading {
        EmptyStateView(
          systemImage: "exclamationmark.triangle",
          title: "Imported transcript unavailable",
          message: errorMessage
        )
      } else {
        ProgressView("Loading imported transcript")
      }
    }
    .navigationTitle(conversation.displayTitle)
    .toolbar {
      ToolbarItem(placement: .primaryAction) {
        Button("Continue") {
          showContinuationSheet = true
        }
        .disabled(
          selectedPosition == nil || viewModel.aliases.isEmpty || viewModel.isContinuing
        )
        .accessibilityIdentifier("continue-imported-conversation-button")
      }
    }
    .sheet(isPresented: $showContinuationSheet) {
      if let selectedPosition {
        ProcessImportedContinuationSheet(
          conversation: conversation,
          throughPosition: selectedPosition,
          viewModel: viewModel
        ) { sessionID in
          showContinuationSheet = false
          didContinue(sessionID)
        }
      }
    }
    .task(id: conversation.id) {
      viewModel.replaceServiceProvider { coordinator.processService }
      await viewModel.load(conversation: conversation)
      selectedPosition = viewModel.transcript?.entries.last?.position
    }
  }

  private var sourceFormatLabel: String {
    switch conversation.importedSourceFormat {
    case .claudeCodeSessionJSONLV1:
      "Claude Code JSONL v1"
    case .claudeCodeSessionJSONLV2:
      "Claude Code JSONL v2"
    case .codexRolloutJSONLV1:
      "Codex rollout JSONL v1"
    case .unknown(let value):
      SignalboxProcessPresentation.retainedLabel(
        "Unrecognized format (\(value))"
      )
    case nil:
      "Unknown"
    }
  }

  @ViewBuilder
  private func importedEntryRow(_ entry: SignalboxImportedConversationEntry) -> some View {
    HStack(alignment: .top, spacing: 12) {
      Image(
        systemName: selectedPosition == entry.position
          ? "checkmark.circle.fill" : "circle"
      )
      .foregroundStyle(selectedPosition == entry.position ? Color.accentColor : Color.secondary)
      VStack(alignment: .leading, spacing: 5) {
        HStack {
          Text("#\(entry.position.rawValue)")
            .font(.caption.monospacedDigit().weight(.semibold))
          Text(entry.sourceSpeakerLabel)
            .font(.caption)
          Spacer()
          Text(entry.contentKindLabel)
            .font(.caption)
            .foregroundStyle(.secondary)
        }
        if let preview = entry.textPreview {
          Text(preview.preview)
            .textSelection(.enabled)
          if preview.truncated {
            Text("Preview truncated")
              .font(.caption2)
              .foregroundStyle(.secondary)
          }
        } else {
          Text("No text preview")
            .foregroundStyle(.secondary)
        }
      }
    }
    .contentShape(Rectangle())
  }
}

private struct ProcessImportedContinuationSheet: View {
  @Environment(\.dismiss) private var dismiss
  let conversation: SignalboxProcessConversation
  let throughPosition: SignalboxCanonicalUInt64
  @ObservedObject var viewModel: ProcessImportedConversationViewModel
  let didContinue: (SignalboxCanonicalUUID) -> Void
  @State private var relationship = SignalboxImportedSessionRelationship.resume
  @State private var selectedAliasID: SignalboxCanonicalUUID?

  var body: some View {
    NavigationStack {
      Form {
        LabeledContent("Through entry", value: "\(throughPosition.rawValue)")
        Picker("Relationship", selection: $relationship) {
          Text("Resume").tag(SignalboxImportedSessionRelationship.resume)
          Text("Fork").tag(SignalboxImportedSessionRelationship.fork)
        }
        .pickerStyle(.segmented)
        .accessibilityIdentifier("imported-relationship-picker")
        Picker("Model alias", selection: $selectedAliasID) {
          ForEach(viewModel.aliases) { alias in
            Text("\(alias.aliasID.rawValue) → \(alias.selectionID.rawValue.prefix(8))")
              .tag(Optional(alias.aliasID))
          }
        }
        .accessibilityIdentifier("imported-model-alias-picker")
        if let errorMessage = viewModel.errorMessage {
          Text(errorMessage)
            .foregroundStyle(.red)
            .textSelection(.enabled)
        }
      }
      .navigationTitle("Continue Import")
      .toolbar {
        ToolbarItem(placement: .cancellationAction) {
          Button("Cancel") {
            dismiss()
          }
          .disabled(viewModel.isContinuing)
        }
        ToolbarItem(placement: .confirmationAction) {
          Button("Continue") {
            Task { await continueConversation() }
          }
          .disabled(selectedAliasID == nil || viewModel.isContinuing)
          .accessibilityIdentifier("confirm-imported-continuation-button")
        }
      }
      .task {
        selectedAliasID = viewModel.aliases.first?.aliasID
      }
    }
    .frame(minWidth: 520, minHeight: 320)
    .interactiveDismissDisabled(viewModel.isContinuing)
  }

  private func continueConversation() async {
    guard let selectedAliasID else {
      return
    }
    do {
      let sessionID = try await viewModel.continueConversation(
        conversation: conversation,
        throughPosition: throughPosition,
        relationship: relationship,
        aliasID: selectedAliasID
      )
      didContinue(sessionID)
    } catch {
      // The view model publishes the actionable failure inside the sheet.
    }
  }
}

struct ProcessRunnerTransition: Equatable {
  let runnerID: SignalboxCanonicalUUID
  let placementRevision: SignalboxCanonicalUInt64
  let sandboxProfile: SignalboxRunnerSandboxProfile
  let workingDirectory: SignalboxRunnerWorkingDirectory?
  let state: SignalboxRunnerStateTransitionState

  var statusLabel: String {
    let directoryLabel = workingDirectory.map {
      "selected directory \(String(reflecting: $0.rawValue))"
    } ?? "runner-default directory"
    return SignalboxProcessPresentation.retainedLabel(
      "Runner \(runnerID.rawValue) · \(state.rawValue) · revision \(placementRevision.rawValue)"
        + " · sandbox \(sandboxProfile.rawValue) · \(directoryLabel)"
    )
  }
}

private enum ProcessSessionPresentationError: LocalizedError {
  case streamedTextCapacityExceeded

  var errorDescription: String? {
    switch self {
    case .streamedTextCapacityExceeded:
      "The live provider-text overlay exceeded its retained UTF-8 byte limit."
    }
  }
}

@MainActor
/// The synchronization driver is the only authority for transcript ordering.
/// This view model projects its updates and separately retains retry identities
/// for mutations whose send outcome may be unknown.
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
  @Published private(set) var streamedText: SignalboxProcessStreamedText?
  @Published private(set) var activeTurnID: SignalboxCanonicalUUID?
  @Published private(set) var runner: SignalboxRunnerProjection?
  @Published private(set) var runnerTransition: ProcessRunnerTransition?
  @Published private(set) var isSubmitting = false
  @Published private(set) var isDecidingTool = false
  @Published var composerText = ""
  @Published var errorMessage: String?

  @Published private(set) var session: SignalboxProcessSession
  private var serviceProvider: () -> (any SignalboxProcessServiceProtocol)?
  private var connectedService: (any SignalboxProcessServiceProtocol)?
  private var synchronization: (any SignalboxSessionSynchronizing)?
  private var synchronizationGeneration: UInt64 = 0
  private var serviceGeneration: UInt64 = 0
  private var unresolvedSubmission: SignalboxPreparedInputSubmission?
  private var unresolvedToolDecision: SignalboxPreparedToolRequestDecision?
  private var unresolvedTurnStop: SignalboxPreparedTurnStop?
  private var materializedAcceptedInputIDs: Set<SignalboxCanonicalUUID> = []
  private var terminalTurnIDs: Set<SignalboxCanonicalUUID> = []
  private var acceptedInputTimelineOffsets: [SignalboxCanonicalUUID: Int] = [:]
  private enum MutationBlockReason: Equatable {
    case unknownTurnState
    case unknownNestedState
  }

  private enum TimelinePresentationKey: Hashable {
    case normalized(String)
    case unrecognized(UInt64)
    case unrecognizedHistoryBoundary
  }

  private struct RetainedUnrecognizedTimelineItem {
    let item: SignalboxTimelineItem
    let utf8Bytes: UInt
  }

  private struct RetainedToolApprovalDecision {
    let decision: SignalboxToolApprovalEventDecision
    let decider: SignalboxToolApprovalEventDecider
    let rationale: String?
  }

  private var mutationBlocksByTurnID: [SignalboxCanonicalUUID: MutationBlockReason] = [:]
  private var sideSnapshotCursorsByTurnID: [SignalboxCanonicalUUID: UInt64] = [:]
  private var normalizedTimelineItemIDs: Set<String> = []
  private var timelinePresentationOrder: [TimelinePresentationKey] = []
  private var unrecognizedLiveTimelineItemsByCursor:
    [UInt64: RetainedUnrecognizedTimelineItem] = [:]
  private var unrecognizedLiveTimelineUTF8Bytes: UInt = 0
  private var toolApprovalDecisionsByRequestID:
    [String: RetainedToolApprovalDecision] = [:]
  private var hasUnrecognizedLiveTimelineHistoryBoundary = false
  private var nextUnrecognizedLiveEventID = -1
  private var projector = SignalboxProcessTranscriptProjector()
  private var normalizer = SignalboxIncrementalEventNormalizer()

  var canDecideToolRequest: Bool {
    connectedService != nil && !isDecidingTool && mutationBlocksByTurnID.isEmpty
  }

  var runnerStatusLabel: String? {
    if let runnerTransition {
      return runnerTransition.statusLabel
    }
    guard let runner else {
      return nil
    }
    let runnerID = runner.runnerID?.rawValue ?? "unassigned"
    let directoryLabel = runner.workingDirectory.map {
      "selected directory \(String(reflecting: $0.rawValue))"
    } ?? "runner-default directory"
    let healthLabel = runner.connectionHealth.map { " · health \($0.rawValue)" } ?? ""
    return SignalboxProcessPresentation.retainedLabel(
      "Runner \(runnerID) · \(runner.state.rawValue)\(healthLabel)"
        + " · revision \(runner.placementRevision.rawValue)"
        + " · sandbox \(runner.sandboxProfile.rawValue) · \(directoryLabel)"
    )
  }

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
      mutationBlocksByTurnID.isEmpty,
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
      await refreshSessionDefaultsIfNeeded(
        after: error,
        using: service,
        generation: generation
      )
      guard serviceGeneration == generation else {
        return
      }
      if error is CancellationError {
        unresolvedSubmission = preparedForAttempt
      } else if let serviceError = error as? SignalboxProcessServiceError,
        serviceError.retainsPreparedMutationIdentity
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

  func decideToolRequest(
    _ invocationID: SignalboxToolInvocationID,
    decision: SignalboxProcessToolDecision
  ) async {
    guard
      !isDecidingTool,
      mutationBlocksByTurnID.isEmpty,
      let service = connectedService
    else {
      return
    }
    let generation = serviceGeneration
    isDecidingTool = true
    defer {
      if serviceGeneration == generation {
        isDecidingTool = false
      }
    }
    var preparedForAttempt: SignalboxPreparedToolRequestDecision?
    var reusedUnresolvedDecision = false
    do {
      let requestID = try SignalboxCanonicalUUID(validating: invocationID.rawValue)
      let prepared: SignalboxPreparedToolRequestDecision
      if let unresolvedToolDecision,
        unresolvedToolDecision.sessionID == session.id,
        unresolvedToolDecision.toolRequestID == requestID,
        unresolvedToolDecision.decision == decision
      {
        prepared = unresolvedToolDecision
        reusedUnresolvedDecision = true
      } else {
        unresolvedToolDecision = nil
        prepared = try await service.prepareToolRequestDecision(
          sessionID: session.id,
          toolRequestID: requestID,
          decision: decision
        )
      }
      preparedForAttempt = prepared
      guard serviceGeneration == generation else {
        return
      }
      _ = try await service.decideToolRequest(prepared)
      guard serviceGeneration == generation else {
        return
      }
      unresolvedToolDecision = nil
      errorMessage = nil
    } catch {
      guard serviceGeneration == generation else {
        return
      }
      if error is CancellationError {
        unresolvedToolDecision = preparedForAttempt
      } else if let serviceError = error as? SignalboxProcessServiceError,
        serviceError.retainsPreparedMutationIdentity
      {
        unresolvedToolDecision = preparedForAttempt
      } else if let openError = error as? SignalboxProcessRequestOpenError,
        case .definitelyUnsent = openError,
        reusedUnresolvedDecision
      {
        unresolvedToolDecision = preparedForAttempt
      } else {
        unresolvedToolDecision = nil
      }
      errorMessage = error.localizedDescription
    }
  }

  func stopAndSendSuccessor() async {
    let content = composerText
    guard
      !content.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
      !isSubmitting,
      mutationBlocksByTurnID.isEmpty,
      let activeTurnID,
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
    var preparedForAttempt: SignalboxPreparedTurnStop?
    var reusedUnresolvedTurnStop = false
    do {
      let prepared: SignalboxPreparedTurnStop
      if let unresolvedTurnStop,
        unresolvedTurnStop.sessionID == session.id,
        hasExactUTF8(unresolvedTurnStop.content, content)
      {
        prepared = unresolvedTurnStop
        reusedUnresolvedTurnStop = true
      } else {
        unresolvedTurnStop = nil
        prepared = try await service.prepareTurnStop(
          session: session,
          activeTurnID: activeTurnID,
          content: content
        )
      }
      preparedForAttempt = prepared
      guard serviceGeneration == generation else {
        return
      }
      let submitted = try await service.stopTurn(prepared)
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
      unresolvedTurnStop = nil
      if hasExactUTF8(composerText, prepared.content) {
        composerText = ""
      }
      errorMessage = nil
    } catch {
      guard serviceGeneration == generation else {
        return
      }
      await refreshSessionDefaultsIfNeeded(
        after: error,
        using: service,
        generation: generation
      )
      guard serviceGeneration == generation else {
        return
      }
      if error is CancellationError {
        unresolvedTurnStop = preparedForAttempt
      } else if let serviceError = error as? SignalboxProcessServiceError,
        serviceError.retainsPreparedMutationIdentity
      {
        unresolvedTurnStop = preparedForAttempt
      } else if let openError = error as? SignalboxProcessRequestOpenError,
        case .definitelyUnsent = openError,
        reusedUnresolvedTurnStop
      {
        unresolvedTurnStop = preparedForAttempt
      } else {
        unresolvedTurnStop = nil
      }
      errorMessage = error.localizedDescription
    }
  }

  var canSubmit: Bool {
    guard connectedService != nil, mutationBlocksByTurnID.isEmpty else {
      return false
    }
    if case .steady = phase {
      return true
    }
    return false
  }

  var canSend: Bool {
    canSubmit && activeTurnID == nil
  }

  var canStopAndSend: Bool {
    canSubmit
      && activeTurnID != nil
      && activity.state != .waitingForToolDecision
      && activity.state != .recoveryRequired
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
        runner = snapshot.runner
        runnerTransition = nil
        try normalizer.replaceAll(with: projection.records)
        toolApprovalDecisionsByRequestID = retainedToolApprovalDecisions(
          projection.toolApprovalDecisionsByRequestID
        )
        refreshTimeline()
        pendingInputs = projection.pendingInputs
        acceptedInputsAwaitingTranscript.removeAll {
          projection.materializedAcceptedInputIDs.contains($0.id)
        }
        for id in projection.materializedAcceptedInputIDs {
          acceptedInputTimelineOffsets[id] = nil
        }
        materializedAcceptedInputIDs = projection.materializedAcceptedInputIDs
        terminalTurnIDs = terminalTurnIDs(in: snapshot)
        activeTurnID = activeTurnID(in: snapshot)
        mutationBlocksByTurnID = mutationBlocksByTurnID(in: snapshot)
        sideSnapshotCursorsByTurnID = [:]
        activity = projection.activity
        streamedText = nil
        errorMessage = nil
      case .sideSnapshot(let snapshot, let trigger):
        let projection = try projector.projectSideSnapshot(
          snapshot,
          attributableTo: trigger
        )
        runner = snapshot.runner
        runnerTransition = nil
        normalizer.upsert(contentsOf: projection.records)
        toolApprovalDecisionsByRequestID.merge(
          retainedToolApprovalDecisions(projection.toolApprovalDecisionsByRequestID),
          uniquingKeysWith: { _, latest in latest }
        )
        refreshTimeline()
        let snapshotTerminalTurnIDs = terminalTurnIDs(in: snapshot)
        let snapshotActiveTurnID = activeTurnID(in: snapshot)
        let snapshotTerminalizedActiveTurn = activeTurnID.map {
          snapshotTerminalTurnIDs.contains($0)
        } ?? false
        terminalTurnIDs.formUnion(snapshotTerminalTurnIDs)
        if let activeTurnID, snapshotTerminalTurnIDs.contains(activeTurnID) {
          self.activeTurnID = nil
        }
        let wasMutationBlocked = !mutationBlocksByTurnID.isEmpty
        mutationBlocksByTurnID = mutationBlocksByTurnID(in: snapshot)
        sideSnapshotCursorsByTurnID = Dictionary(
          snapshot.records.compactMap { record in
            guard case .turn(let turn) = record else {
              return nil
            }
            return (turn.turnID, snapshot.cursor.rawValue)
          }, uniquingKeysWith: { _, latest in latest })
        materializedAcceptedInputIDs.formUnion(projection.materializedAcceptedInputIDs)
        if let snapshotActiveTurnID {
          activeTurnID = snapshotActiveTurnID
          if projection.activity.state != .waitingForToolDecision
            || sideSnapshotApprovalMatchesTrigger(snapshot, trigger: trigger)
          {
            activity = projection.activity
          }
        } else if snapshotTerminalizedActiveTurn || isTerminalActivity(projection.activity) {
          activity = projection.activity
        } else if case .turnActivated(let turnID, _) = trigger.event,
          snapshotTerminalTurnIDs.contains(turnID)
        {
          activeTurnID = snapshotActiveTurnID
          activity = projection.activity
        } else if !mutationBlocksByTurnID.isEmpty
          || projection.activity.state == .recoveryRequired
          || (wasMutationBlocked && projection.activity.state == .running)
        {
          activity = projection.activity
        } else if projection.activity.state == .waitingForToolDecision,
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
        streamedText = nil
      case .event(let followed):
        applyLiveEvent(followed)
      case .providerTextDelta(let delta):
        if var current = streamedText,
          current.turnID == delta.turnID,
          current.modelCallID == delta.modelCallID
        {
          guard current.append(delta) else {
            streamedText = nil
            throw ProcessSessionPresentationError.streamedTextCapacityExceeded
          }
          streamedText = current
        } else {
          streamedText = SignalboxProcessStreamedText(delta: delta)
        }
      case .diagnostic(let diagnostic):
        latestDiagnostic = diagnostic.message
      case .retryLimitReached:
        connectedService = nil
        errorMessage = "Synchronization reached its bounded reconnect limit."
      case .terminalFailure:
        connectedService = nil
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

  private func refreshSessionDefaultsIfNeeded(
    after error: Error,
    using service: any SignalboxProcessServiceProtocol,
    generation: UInt64
  ) async {
    guard
      let serviceError = error as? SignalboxProcessServiceError,
      case .remote(
        code: .rejected,
        message: _,
        detail: .some(
          .defaultsVersionMismatch(let sessionID, expected: _, current: _)
        )
      ) = serviceError,
      sessionID == session.id,
      let refreshed = try? await service.listSessions(includeArchived: true).first(where: {
        $0.id == session.id
      }),
      serviceGeneration == generation
    else {
      return
    }
    session = refreshed
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

  private func mutationBlocksByTurnID(
    in snapshot: SignalboxSynchronizationSnapshot
  ) -> [SignalboxCanonicalUUID: MutationBlockReason] {
    var blocks: [SignalboxCanonicalUUID: MutationBlockReason] = [:]
    var unresolvedUnknownTurnID: SignalboxCanonicalUUID?
    for record in snapshot.records {
      guard case .turn(let turn) = record else {
        continue
      }
      switch turn.state {
      case .unknown:
        unresolvedUnknownTurnID = turn.turnID
      case .activeRunning(_, let currentModelCall):
        unresolvedUnknownTurnID = nil
        if let currentModelCall, case .unknown = currentModelCall.state {
          blocks[turn.turnID] = .unknownNestedState
        }
      case .queued, .queuedDelegated, .queuedDelegationWake:
        break
      case .activeAwaitingChild, .activeAwaitingModelCallRecovery,
        .activeAwaitingToolApproval, .activeAwaitingToolRecovery, .failed, .completed, .refused,
        .cancelled, .delegationTerminated,
        .reconciliationRequired, .toolReconciliationRequired:
        unresolvedUnknownTurnID = nil
      }
    }
    if let unresolvedUnknownTurnID {
      blocks[unresolvedUnknownTurnID] = .unknownTurnState
    }
    return blocks
  }

  private func resetServiceOwnedPresentation() {
    serviceGeneration &+= 1
    timeline = []
    pendingInputs = []
    acceptedInputsAwaitingTranscript = []
    acceptedInputTimelineOffsets = [:]
    activity = .unavailable
    activeTurnID = nil
    runner = nil
    runnerTransition = nil
    mutationBlocksByTurnID = [:]
    sideSnapshotCursorsByTurnID = [:]
    normalizedTimelineItemIDs = []
    timelinePresentationOrder = []
    unrecognizedLiveTimelineItemsByCursor = [:]
    unrecognizedLiveTimelineUTF8Bytes = 0
    toolApprovalDecisionsByRequestID = [:]
    hasUnrecognizedLiveTimelineHistoryBoundary = false
    nextUnrecognizedLiveEventID = -1
    phase = .stopped
    latestDiagnostic = nil
    isSubmitting = false
    isDecidingTool = false
    errorMessage = nil
    unresolvedSubmission = nil
    unresolvedToolDecision = nil
    streamedText = nil
    materializedAcceptedInputIDs = []
    terminalTurnIDs = []
    projector = SignalboxProcessTranscriptProjector()
    normalizer = SignalboxIncrementalEventNormalizer()
  }

  private func applyLiveEvent(_ followed: SignalboxFollowedSessionEvent) {
    switch followed.event {
    case .inputAccepted(let acceptedInputID, let turnID, let acceptancePosition, let content):
      if !materializedAcceptedInputIDs.contains(acceptedInputID) {
        let acceptedInput = SignalboxProcessPendingInput(
          id: acceptedInputID,
          turnID: turnID,
          acceptancePosition: acceptancePosition,
          content: content.displayText
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
        if mutationBlocksByTurnID.isEmpty, !activityRepresentsActiveTurn {
          activity = .init(state: .queued, label: "Queued")
        }
      }
    case .turnActivated(let turnID, _):
      let admitsActivation = admitsStateTransition(for: turnID, at: followed.cursor)
      if admitsActivation || !terminalTurnIDs.contains(turnID) {
        activeTurnID = turnID
      }
      guard admitsActivation else {
        return
      }
      mutationBlocksByTurnID.removeValue(forKey: turnID)
      guard mutationBlocksByTurnID.isEmpty else {
        return
      }
      activity = .init(state: .running, label: "Running")
    case .modelCallTransition(let turnID, _, let state):
      retainUnrecognizedNestedLiveEvent(followed)
      guard admitsStateTransition(for: turnID, at: followed.cursor) else {
        return
      }
      if modelCallStateBlocksMutation(state) {
        if mutationBlocksByTurnID[turnID] != .unknownTurnState {
          mutationBlocksByTurnID[turnID] = .unknownNestedState
        }
      } else {
        if mutationBlocksByTurnID[turnID] == .unknownNestedState {
          mutationBlocksByTurnID.removeValue(forKey: turnID)
        }
      }
      applyModelCallState(state, for: turnID)
    case .toolBatchTransition(let turnID, _, let state):
      retainUnrecognizedNestedLiveEvent(followed)
      guard admitsStateTransition(for: turnID, at: followed.cursor) else {
        return
      }
      switch state {
      case .proposed:
        if mutationBlocksByTurnID[turnID] == .unknownNestedState {
          mutationBlocksByTurnID.removeValue(forKey: turnID)
        }
        applyNestedActivity(.init(state: .running, label: "Running"), for: turnID)
      case .resultsProjected:
        if mutationBlocksByTurnID[turnID] == .unknownNestedState {
          mutationBlocksByTurnID.removeValue(forKey: turnID)
        }
        applyNestedActivity(.init(state: .running, label: "Running"), for: turnID)
      case .recoveryRequired:
        if mutationBlocksByTurnID[turnID] == .unknownNestedState {
          mutationBlocksByTurnID.removeValue(forKey: turnID)
        }
        applyNestedActivity(
          .init(state: .recoveryRequired, label: "Recovery required"),
          for: turnID
        )
      case .unknown(let kind, _):
        if mutationBlocksByTurnID[turnID] != .unknownTurnState {
          mutationBlocksByTurnID[turnID] = .unknownNestedState
        }
        applyNestedActivity(
          .init(state: .recoveryRequired, label: "Recovery required"),
          for: turnID
        )
        presentDiagnostic("Preserved an unrecognized tool-batch state: \(kind).")
      }
    case .toolApprovalDecided(_, let toolRequestID, let decision, let decider, let rationale):
      toolApprovalDecisionsByRequestID[toolRequestID.rawValue] = RetainedToolApprovalDecision(
        decision: decision,
        decider: decider,
        rationale: rationale
      )
      refreshTimeline()
    case .turnCompleted(let turnID, _, _, _):
      applyTerminalTurn(
        turnID: turnID,
        at: followed.cursor,
        terminalActivity: .init(state: .completed, label: "Completed")
      )
    case .turnFailed(let turnID, _, _):
      applyTerminalTurn(
        turnID: turnID,
        at: followed.cursor,
        terminalActivity: .init(state: .failed, label: "Failed")
      )
    case .turnRefused(let turnID, _, _):
      applyTerminalTurn(
        turnID: turnID,
        at: followed.cursor,
        terminalActivity: .init(state: .refused, label: "Refused")
      )
    case .turnCancelled(let turnID, _, _):
      applyTerminalTurn(
        turnID: turnID,
        at: followed.cursor,
        terminalActivity: .init(state: .cancelled, label: "Cancelled")
      )
    case .turnReconciliationRequired(let turnID, _, _),
      .turnToolReconciliationRequired(let turnID, _, _):
      applyTerminalTurn(
        turnID: turnID,
        at: followed.cursor,
        terminalActivity: .init(state: .recoveryRequired, label: "Recovery required")
      )
    case .runnerStateTransition(
      let runnerID,
      let placementRevision,
      let sandboxProfile,
      let workingDirectory,
      let state
    ):
      runnerTransition = ProcessRunnerTransition(
        runnerID: runnerID,
        placementRevision: placementRevision,
        sandboxProfile: sandboxProfile,
        workingDirectory: workingDirectory,
        state: state
      )
    case .unknown(let kind, _, let decodingDiagnostic):
      retainUnrecognizedLiveEvent(
        kind: kind,
        diagnostic: decodingDiagnostic?.message,
        cursor: followed.cursor
      )
    case .sessionCreated, .sessionModelSettingsChanged, .turnModelSettingsResolved,
      .contextCompacted:
      break
    }
  }

  private func retainUnrecognizedNestedLiveEvent(
    _ followed: SignalboxFollowedSessionEvent
  ) {
    guard let event = projector.projectUnrecognizedFollowedEvent(followed) else {
      return
    }
    retainUnrecognizedLiveEvent(
      kind: event.kind,
      diagnostic: event.diagnostic,
      cursor: followed.cursor
    )
  }

  private func retainUnrecognizedLiveEvent(
    kind: String,
    diagnostic: String?,
    cursor: SignalboxCanonicalUInt64
  ) {
    guard unrecognizedLiveTimelineItemsByCursor[cursor.rawValue] == nil else {
      return
    }
    let retainedKind = SignalboxProcessPresentation.retainedLabel(kind)
    let retainedDiagnostic = SignalboxProcessPresentation.retainedLabel(
      diagnostic
        ?? "The session event kind is not rendered by this client."
    )
    let retainedBytes = UInt(retainedKind.utf8.count + retainedDiagnostic.utf8.count)
    guard makeRoomForUnrecognizedLiveTimelineItem(utf8Bytes: retainedBytes) else {
      return
    }
    let retained = RetainedUnrecognizedTimelineItem(
      item: .unknown(
        SignalboxUnknownEventCard(
          eventID: SignalboxEventID(rawValue: nextUnrecognizedLiveEventID),
          kind: retainedKind,
          diagnostic: retainedDiagnostic
        )
      ),
      utf8Bytes: retainedBytes
    )
    nextUnrecognizedLiveEventID -= 1
    unrecognizedLiveTimelineItemsByCursor[cursor.rawValue] = retained
    unrecognizedLiveTimelineUTF8Bytes += retainedBytes
    timelinePresentationOrder.append(.unrecognized(cursor.rawValue))
    refreshTimeline()
  }

  private func retainedToolApprovalDecisions(
    _ decisions: [String: SignalboxTranscriptToolApproval]
  ) -> [String: RetainedToolApprovalDecision] {
    decisions.mapValues { approval in
      RetainedToolApprovalDecision(
        decision: approval.decision,
        decider: approval.decider,
        rationale: approval.rationale
      )
    }
  }

  /// Evicts the oldest unknown-event cards so a future-event stream cannot exhaust
  /// presentation memory, while retaining a visible truncation boundary.
  private func makeRoomForUnrecognizedLiveTimelineItem(utf8Bytes: UInt) -> Bool {
    let capacity = SignalboxProcessApplicationPolicy.nativeDefault.synchronization
      .eventBufferCapacity
    guard
      capacity.maximumEvents > 1,
      utf8Bytes <= capacity.maximumUTF8Bytes
    else {
      return false
    }
    while retainedUnrecognizedLiveTimelineCount >= capacity.maximumEvents
      || unrecognizedLiveTimelineUTF8Bytes > capacity.maximumUTF8Bytes - utf8Bytes
    {
      guard evictOldestUnrecognizedLiveTimelineItem() else {
        return false
      }
    }
    return true
  }

  private var retainedUnrecognizedLiveTimelineCount: UInt {
    UInt(unrecognizedLiveTimelineItemsByCursor.count)
      + (hasUnrecognizedLiveTimelineHistoryBoundary ? 1 : 0)
  }

  /// Discards the oldest unknown-event card so a future-event stream cannot exhaust
  /// retained presentation memory, installing a visible boundary on first eviction.
  private func evictOldestUnrecognizedLiveTimelineItem() -> Bool {
    guard
      let index = timelinePresentationOrder.firstIndex(where: { key in
        if case .unrecognized = key {
          return true
        }
        return false
      }),
      case .unrecognized(let cursor) = timelinePresentationOrder[index],
      let evicted = unrecognizedLiveTimelineItemsByCursor.removeValue(forKey: cursor)
    else {
      return false
    }
    unrecognizedLiveTimelineUTF8Bytes -= evicted.utf8Bytes
    if hasUnrecognizedLiveTimelineHistoryBoundary {
      timelinePresentationOrder.remove(at: index)
    } else {
      timelinePresentationOrder[index] = .unrecognizedHistoryBoundary
      hasUnrecognizedLiveTimelineHistoryBoundary = true
      unrecognizedLiveTimelineUTF8Bytes += Self.unrecognizedLiveTimelineHistoryBoundaryBytes
    }
    return true
  }

  private static let unrecognizedLiveTimelineHistoryBoundaryKind =
    "unrecognized_session_event_history_truncated"
  private static let unrecognizedLiveTimelineHistoryBoundaryDiagnostic =
    "Earlier unrecognized session events were removed to keep retained history bounded."
  private static let unrecognizedLiveTimelineHistoryBoundary: SignalboxTimelineItem =
    .unknown(
      SignalboxUnknownEventCard(
        eventID: SignalboxEventID(rawValue: .min),
        kind: unrecognizedLiveTimelineHistoryBoundaryKind,
        diagnostic: unrecognizedLiveTimelineHistoryBoundaryDiagnostic
      )
    )

  private static let unrecognizedLiveTimelineHistoryBoundaryBytes = UInt(
    unrecognizedLiveTimelineHistoryBoundaryKind.utf8.count
      + unrecognizedLiveTimelineHistoryBoundaryDiagnostic.utf8.count
  )

  private func refreshTimeline() {
    let normalizedItems = normalizer.timelineItems.map(applyingToolApprovalDecision)
    let normalizedItemsByID = Dictionary(
      normalizedItems.map { ($0.id, $0) },
      uniquingKeysWith: { first, _ in first }
    )
    let normalizedKeys = normalizedItems.map { TimelinePresentationKey.normalized($0.id) }
    let currentNormalizedIDs = Set(normalizedItems.map(\.id))
    let retainedNormalizedIDs = normalizedTimelineItemIDs.intersection(currentNormalizedIDs)
    var additionsBeforeRetainedID: [String: [TimelinePresentationKey]] = [:]
    var pendingAdditions: [TimelinePresentationKey] = []
    for key in normalizedKeys {
      guard case .normalized(let id) = key else {
        continue
      }
      if retainedNormalizedIDs.contains(id) {
        additionsBeforeRetainedID[id] = pendingAdditions
        pendingAdditions = []
      } else {
        pendingAdditions.append(key)
      }
    }
    var refreshedPresentationOrder: [TimelinePresentationKey] = []
    for key in timelinePresentationOrder {
      switch key {
      case .normalized(let id):
        guard retainedNormalizedIDs.contains(id) else {
          continue
        }
        refreshedPresentationOrder.append(contentsOf: additionsBeforeRetainedID[id] ?? [])
        refreshedPresentationOrder.append(key)
      case .unrecognized, .unrecognizedHistoryBoundary:
        refreshedPresentationOrder.append(key)
      }
    }
    refreshedPresentationOrder.append(contentsOf: pendingAdditions)
    let normalizedRanks = Dictionary(
      uniqueKeysWithValues: normalizedKeys.enumerated().map { ($0.element, $0.offset) }
    )
    var reorderedPresentationOrder: [TimelinePresentationKey] = []
    var normalizedSegment: [TimelinePresentationKey] = []
    for key in refreshedPresentationOrder {
      switch key {
      case .normalized:
        normalizedSegment.append(key)
      case .unrecognized, .unrecognizedHistoryBoundary:
        reorderedPresentationOrder.append(
          contentsOf: normalizedSegment.sorted {
            normalizedRanks[$0, default: .max] < normalizedRanks[$1, default: .max]
          }
        )
        normalizedSegment = []
        reorderedPresentationOrder.append(key)
      }
    }
    reorderedPresentationOrder.append(
      contentsOf: normalizedSegment.sorted {
        normalizedRanks[$0, default: .max] < normalizedRanks[$1, default: .max]
      }
    )
    timelinePresentationOrder = reorderedPresentationOrder
    normalizedTimelineItemIDs = currentNormalizedIDs
    timeline = timelinePresentationOrder.compactMap { key in
      switch key {
      case .normalized(let id):
        return normalizedItemsByID[id]
      case .unrecognized(let cursor):
        return unrecognizedLiveTimelineItemsByCursor[cursor]?.item
      case .unrecognizedHistoryBoundary:
        return Self.unrecognizedLiveTimelineHistoryBoundary
      }
    }
  }

  private func applyingToolApprovalDecision(
    to item: SignalboxTimelineItem
  ) -> SignalboxTimelineItem {
    guard case .tool(let tool) = item,
      let approval = toolApprovalDecisionsByRequestID[tool.invocationID.rawValue]
    else {
      return item
    }
    let status: SignalboxToolCardStatus
    let reason: String?
    let decisionLabel: String
    switch approval.decision {
    case .approve:
      status = tool.status == .waitingForApproval || tool.status == .proposed
        ? .approved : tool.status
      reason = tool.decisionReason
      decisionLabel = "Approved"
    case .deny(let denialReason):
      status = .denied
      reason = denialReason ?? tool.decisionReason
      decisionLabel = "Denied"
    }
    let deciderLabel: String
    switch approval.decider {
    case .user(let commandID):
      deciderLabel = "\(decisionLabel) by user; command \(commandID.rawValue)"
    case .delegate(let modelSelectionID, let modelCallID):
      deciderLabel =
        "\(decisionLabel) by delegate; model selection \(modelSelectionID.rawValue); "
        + "call \(modelCallID.rawValue)"
    case .userOverride(let commandID, let overriddenToolRequestID):
      deciderLabel =
        "\(decisionLabel) by user override; command \(commandID.rawValue); "
        + "overrides denial of \(overriddenToolRequestID.rawValue)"
    }
    return .tool(
      SignalboxToolCard(
        eventID: tool.eventID,
        invocationID: tool.invocationID,
        toolName: tool.toolName,
        status: status,
        arguments: tool.arguments,
        output: tool.output,
        statusUpdates: tool.statusUpdates,
        decisionReason: reason,
        approvalDecider: deciderLabel,
        approvalRationale: approval.rationale,
        childSessionID: tool.childSessionID,
        decisionAvailable: false
      )
    )
  }

  private func admitsStateTransition(
    for turnID: SignalboxCanonicalUUID,
    at cursor: SignalboxCanonicalUInt64
  ) -> Bool {
    guard let snapshotCursor = sideSnapshotCursorsByTurnID[turnID] else {
      return true
    }
    guard cursor.rawValue > snapshotCursor else {
      return false
    }
    sideSnapshotCursorsByTurnID.removeValue(forKey: turnID)
    return true
  }

  private func applyTerminalTurn(
    turnID: SignalboxCanonicalUUID,
    at cursor: SignalboxCanonicalUInt64,
    terminalActivity: SignalboxProcessActivity
  ) {
    let admitsTerminalState = admitsStateTransition(for: turnID, at: cursor)
    terminalTurnIDs.insert(turnID)
    if admitsTerminalState {
      mutationBlocksByTurnID.removeValue(forKey: turnID)
    }
    if activeTurnID == turnID {
      activeTurnID = nil
    }
    if streamedText?.turnID == turnID {
      streamedText = nil
    }
    let acceptedInputs = pendingInputs.filter { $0.turnID == turnID }
    pendingInputs.removeAll { $0.turnID == turnID }
    for acceptedInput in acceptedInputs {
      retainAcceptedInputAwaitingTranscript(acceptedInput)
    }
    guard admitsTerminalState, mutationBlocksByTurnID.isEmpty else {
      return
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
        case .failed, .completed, .refused, .cancelled, .delegationTerminated,
          .reconciliationRequired, .toolReconciliationRequired:
          return turn.turnID
        case .queued, .queuedDelegated, .queuedDelegationWake, .activeRunning,
          .activeAwaitingChild,
          .activeAwaitingToolApproval, .activeAwaitingModelCallRecovery,
          .activeAwaitingToolRecovery, .unknown:
          return nil
        }
      })
  }

  private func activeTurnID(
    in snapshot: SignalboxSynchronizationSnapshot
  ) -> SignalboxCanonicalUUID? {
    snapshot.records.compactMap { record -> SignalboxCanonicalUUID? in
      guard case .turn(let turn) = record else {
        return nil
      }
      switch turn.state {
      case .activeRunning, .activeAwaitingChild, .activeAwaitingToolApproval,
        .activeAwaitingModelCallRecovery, .activeAwaitingToolRecovery:
        return turn.turnID
      case .queued, .queuedDelegated, .queuedDelegationWake, .failed, .completed, .refused,
        .cancelled, .delegationTerminated,
        .reconciliationRequired, .toolReconciliationRequired, .unknown:
        return nil
      }
    }.first
  }

  private var activityRepresentsActiveTurn: Bool {
    switch activity.state {
    case .running, .waitingForToolDecision, .recoveryRequired:
      return true
    case .unavailable, .queued, .failed, .completed, .refused, .cancelled:
      return false
    }
  }

  private func isTerminalActivity(_ activity: SignalboxProcessActivity) -> Bool {
    switch activity.state {
    case .failed, .completed, .refused, .cancelled:
      return true
    case .unavailable, .queued, .running, .waitingForToolDecision, .recoveryRequired:
      return false
    }
  }

  private func modelCallStateBlocksMutation(_ state: SignalboxModelCallState) -> Bool {
    switch state {
    case .terminal(.unknown), .unknown:
      return true
    case .prepared, .inFlight, .cancellationRequested, .terminal:
      return false
    }
  }

  private func applyModelCallState(
    _ state: SignalboxModelCallState,
    for turnID: SignalboxCanonicalUUID
  ) {
    switch state {
    case .prepared, .inFlight, .cancellationRequested:
      applyNestedActivity(.init(state: .running, label: "Running"), for: turnID)
    case .terminal(let disposition):
      switch disposition {
      case .ambiguous:
        applyNestedActivity(
          .init(state: .recoveryRequired, label: "Recovery required"),
          for: turnID
        )
      case .completed, .knownFailed, .refused, .cancelled:
        applyNestedActivity(.init(state: .running, label: "Running"), for: turnID)
      case .unknown(let value):
        applyNestedActivity(
          .init(state: .recoveryRequired, label: "Recovery required"),
          for: turnID
        )
        presentDiagnostic("Preserved an unrecognized model-call disposition: \(value).")
      }
    case .unknown(let kind, _):
      applyNestedActivity(
        .init(state: .recoveryRequired, label: "Recovery required"),
        for: turnID
      )
      presentDiagnostic("Preserved an unrecognized model-call state: \(kind).")
    }
  }

  private func applyNestedActivity(
    _ nestedActivity: SignalboxProcessActivity,
    for turnID: SignalboxCanonicalUUID
  ) {
    let preservesBlockedActivity = mutationBlocksByTurnID.contains { blockedTurnID, reason in
      blockedTurnID != turnID || reason == .unknownTurnState
    }
    guard !preservesBlockedActivity else {
      return
    }
    activity = nestedActivity
  }

  private func presentDiagnostic(_ message: String) {
    latestDiagnostic = SignalboxSessionSynchronizationMachine.retainedDiagnosticMessage(message)
  }
}

struct ProcessSessionDetailScreen: View {
  @EnvironmentObject private var coordinator: AppCoordinator
  @StateObject private var viewModel: ProcessSessionDetailViewModel
  @State private var showArtifactGate = false
  @State private var deniedToolRequest: SignalboxToolInvocationID?
  @State private var denialReason = ""

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
          if let streamedText = viewModel.streamedText {
            VStack(alignment: .leading, spacing: 6) {
              HStack(spacing: 8) {
                Text("Assistant")
                  .font(.caption.weight(.semibold))
                  .foregroundStyle(.secondary)
                ProgressView()
                  .controlSize(.small)
              }
              Text(streamedText.text)
                .textSelection(.enabled)
            }
            .padding(12)
            .background(.accent.opacity(0.08), in: RoundedRectangle(cornerRadius: 8))
            .accessibilityIdentifier("provider-streamed-text")
          }
          if viewModel.timeline.isEmpty && viewModel.pendingInputs.isEmpty
            && viewModel.acceptedInputsAwaitingTranscript.isEmpty
          {
            EmptyStateView(
              systemImage: "text.bubble",
              title: "No transcript entries",
              message: "The durable snapshot contains no presentable entries."
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
      Text("The process protocol exposes no artifact operation.")
    }
    .alert(
      "Deny tool request",
      isPresented: Binding(
        get: { deniedToolRequest != nil },
        set: {
          if !$0 {
            deniedToolRequest = nil
            denialReason = ""
          }
        }
      )
    ) {
      TextField("Reason", text: $denialReason)
      Button("Cancel", role: .cancel) {
        deniedToolRequest = nil
        denialReason = ""
      }
      Button("Deny", role: .destructive) {
        guard let request = deniedToolRequest else {
          return
        }
        let reason = denialReason.trimmingCharacters(in: .whitespacesAndNewlines)
        deniedToolRequest = nil
        denialReason = ""
        Task {
          await viewModel.decideToolRequest(request, decision: .deny(reason: reason))
        }
      }
      .disabled(denialReason.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
    } message: {
      Text("The reason is recorded with the durable tool decision.")
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
        if let runnerStatusLabel = viewModel.runnerStatusLabel {
          Text(runnerStatusLabel)
            .font(.caption)
            .foregroundStyle(.secondary)
        }
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
          || !viewModel.canSend
      )
      .accessibilityLabel("Send")
      .accessibilityIdentifier("send-message-button")
      if viewModel.activeTurnID != nil {
        Button(role: .destructive) {
          Task { await viewModel.stopAndSendSuccessor() }
        } label: {
          Label("Stop & Send", systemImage: "stop.circle")
        }
        .buttonStyle(.bordered)
        .disabled(
          viewModel.composerText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            || viewModel.isSubmitting
            || !viewModel.canStopAndSend
        )
        .help("Stop the active turn and submit the composer text as its successor.")
        .accessibilityIdentifier("stop-turn-button")
      }
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
      ToolInvocationCard(
        tool: tool,
        decisionAvailable: tool.decisionAvailable && viewModel.canDecideToolRequest,
        onApprove: {
          Task {
            await viewModel.decideToolRequest(tool.invocationID, decision: .approve)
          }
        },
        onDeny: {
          deniedToolRequest = tool.invocationID
        }
      )
    case .processEvidence(let notice):
      ProcessNoticeCard(notice: notice)
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
