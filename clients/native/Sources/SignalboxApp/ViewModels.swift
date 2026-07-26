#if canImport(SignalboxClient)
import SignalboxClient
#endif
#if canImport(SignalboxModels)
import SignalboxModels
#endif
import Combine
import Foundation
import SwiftUI

@MainActor
final class AppCoordinator: ObservableObject {
    @Published var settings: SignalboxSettingsViewModel
    @Published var service: (any SignalboxClientProtocol)?
    @Published var selectedSection: AppSection = .sessions
    @Published var selectedSessionID: SignalboxSessionID?

    let isMockMode: Bool
    let screenshotScenario: ScreenshotScenario?

    init(isMockMode: Bool, screenshotScenario: ScreenshotScenario? = nil, resetPersistedSettings: Bool = false) {
        self.screenshotScenario = screenshotScenario
        let shouldInstallMockService = isMockMode || screenshotScenario?.requiresMockService == true
        self.isMockMode = shouldInstallMockService
        if resetPersistedSettings {
            UserDefaults.standard.removeObject(forKey: NativeAppConstants.serverURLDefaultsKey)
            KeychainSecretStore().deleteSecret()
        }
        self.settings = SignalboxSettingsViewModel()
        if resetPersistedSettings {
            self.settings.serverURLText = ""
            self.settings.apiKey = ""
        }
        if let screenshotScenario {
            selectedSection = screenshotScenario.selectedSection
            selectedSessionID = screenshotScenario.selectedSessionID
        }
        if shouldInstallMockService {
            self.service = MockSignalboxService()
            self.settings.serverURLText = NativeAppConstants.defaultServerURL
            self.settings.apiKey = "mock-key"
        } else if screenshotScenario == .setup {
            self.service = nil
            self.settings.apiKey = ""
        } else if case .success(let configuration) = settings.configurationResult() {
            self.service = SignalboxAPIClient(configuration: configuration)
        }
    }

    func testConnectionAndInstallClient() async {
        if case .failure(let error) = settings.configurationResult() {
            await settings.testConnection(using: FailingSignalboxClient(error: error))
            return
        }
        if isMockMode {
            await settings.testConnection(using: service)
            return
        }
        switch settings.configurationResult() {
        case .success(let configuration):
            let client = SignalboxAPIClient(configuration: configuration)
            await settings.testConnection(using: client)
            if settings.connectionStatus == .connected {
                service = client
                NotificationCenter.default.post(name: .refreshRequested, object: nil)
            }
        case .failure(let error):
            await settings.testConnection(using: FailingSignalboxClient(error: error))
        }
    }
}

enum ScreenshotScenario: String, CaseIterable, Sendable {
    case setup
    case sessions
    case newSession = "new-session"
    case activeChat = "active-chat"
    case markdownBasics = "markdown-basics"
    case markdownTable = "markdown-table"
    case markdownCode = "markdown-code"
    case markdownMessage = "markdown-message"
    case pendingApproval = "pending-approval"
    case completedTool = "completed-tool"
    case failedTool = "failed-tool"
    case artifactPreview = "artifact-preview"
    case runners
    case monitor
    case settings

    var requiresMockService: Bool {
        self != .setup
    }

    var selectedSection: AppSection {
        switch self {
        case .runners:
            return .runners
        case .monitor:
            return .monitor
        case .settings:
            return .settings
        default:
            return .sessions
        }
    }

    var selectedSessionID: SignalboxSessionID? {
        switch self {
        case .activeChat, .completedTool, .artifactPreview:
            return SignalboxSessionID(rawValue: MockSignalboxFixtures.activeSessionID)
        case .markdownBasics:
            return SignalboxSessionID(rawValue: MockSignalboxFixtures.markdownBasicsSessionID)
        case .markdownTable:
            return SignalboxSessionID(rawValue: MockSignalboxFixtures.markdownTableSessionID)
        case .markdownCode:
            return SignalboxSessionID(rawValue: MockSignalboxFixtures.markdownCodeSessionID)
        case .markdownMessage:
            return SignalboxSessionID(rawValue: MockSignalboxFixtures.markdownSessionID)
        case .pendingApproval:
            return SignalboxSessionID(rawValue: MockSignalboxFixtures.approvalSessionID)
        case .failedTool:
            return SignalboxSessionID(rawValue: MockSignalboxFixtures.failedSessionID)
        default:
            return nil
        }
    }

    static func parse(arguments: [String]) -> ScreenshotScenario? {
        guard let optionIndex = arguments.firstIndex(of: "--screenshot-state") else {
            return nil
        }
        let valueIndex = arguments.index(after: optionIndex)
        guard arguments.indices.contains(valueIndex) else {
            return nil
        }
        return ScreenshotScenario(rawValue: arguments[valueIndex])
    }
}

enum AppSection: String, CaseIterable, Identifiable {
    case sessions
    case monitor
    case runners
    case templates
    case settings

    var id: String { rawValue }

    var title: String {
        switch self {
        case .sessions:
            return "Sessions"
        case .monitor:
            return "Monitor"
        case .runners:
            return "Runners"
        case .templates:
            return "Templates"
        case .settings:
            return "Settings"
        }
    }

    var systemImage: String {
        switch self {
        case .sessions:
            return "bubble.left.and.bubble.right"
        case .monitor:
            return "waveform.path.ecg"
        case .runners:
            return "server.rack"
        case .templates:
            return "doc.on.doc"
        case .settings:
            return "gearshape"
        }
    }
}

private struct FailingSignalboxClient: SignalboxClientProtocol {
    let error: SignalboxClientError

    func testConnection() async throws { throw error }
    func listTemplates() async throws -> [SignalboxTemplate] { throw error }
    func listRunners() async throws -> [SignalboxRunner] { throw error }
    func listSessions(archived: Bool) async throws -> [SignalboxSessionMetadata] { throw error }
    func createSession(request: SignalboxCreateSessionRequest) async throws -> SignalboxSessionView { throw error }
    func patchSessionArchive(sessionID: SignalboxSessionID, isArchived: Bool) async throws -> SignalboxSessionMetadata { throw error }
    func listEvents(sessionID: SignalboxSessionID) async throws -> [SignalboxStoredEvent] { throw error }
    func appendUserMessage(sessionID: SignalboxSessionID, text: String) async throws -> SignalboxAppendUserMessageResponse { throw error }
    func confirmInvocation(sessionID: SignalboxSessionID, invocationID: SignalboxToolInvocationID) async throws { throw error }
    func denyInvocation(sessionID: SignalboxSessionID, invocationID: SignalboxToolInvocationID, reason: String?) async throws { throw error }
    func listArtifacts(sessionID: SignalboxSessionID) async throws -> [SignalboxArtifact] { throw error }
    func listMonitorSessions() async throws -> [SignalboxMonitorSessionSummary] { throw error }
    func streamMessages(sessionID: SignalboxSessionID) -> AsyncThrowingStream<SignalboxServerMessage, Error> {
        AsyncThrowingStream { continuation in continuation.finish(throwing: error) }
    }
}

@MainActor
final class SessionListViewModel: ObservableObject {
    @Published private(set) var activeSessions: [SignalboxSessionMetadata] = []
    @Published private(set) var archivedSessions: [SignalboxSessionMetadata] = []
    @Published private(set) var templates: [SignalboxTemplate] = []
    @Published private(set) var runners: [SignalboxRunner] = []
    @Published private(set) var statusesBySessionID: [SignalboxSessionID: SignalboxSessionStatus] = [:]
    @Published var searchText: String = ""
    @Published var showArchived: Bool = false
    @Published var errorMessage: String?
    @Published var isLoading = false

    private var serviceProvider: () -> (any SignalboxClientProtocol)?

    init(serviceProvider: @escaping () -> (any SignalboxClientProtocol)?) {
        self.serviceProvider = serviceProvider
    }

    func replaceServiceProvider(_ provider: @escaping () -> (any SignalboxClientProtocol)?) {
        self.serviceProvider = provider
    }

    var visibleSessions: [SignalboxSessionMetadata] {
        let source = showArchived ? archivedSessions : activeSessions
        let trimmedSearch = searchText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmedSearch.isEmpty else {
            return source
        }
        return source.filter { session in
            session.displayTitle.localizedCaseInsensitiveContains(trimmedSearch)
                || session.tags.contains { $0.localizedCaseInsensitiveContains(trimmedSearch) }
                || session.runnerID?.rawValue.localizedCaseInsensitiveContains(trimmedSearch) == true
            }
    }

    func session(with sessionID: SignalboxSessionID) -> SignalboxSessionMetadata? {
        (activeSessions + archivedSessions).first { $0.id == sessionID }
    }

    func refresh() async {
        guard let service = serviceProvider() else {
            errorMessage = "Configure a server connection in Settings."
            return
        }
        isLoading = true
        defer { isLoading = false }
        do {
            async let active = service.listSessions(archived: false)
            async let archived = service.listSessions(archived: true)
            async let templates = service.listTemplates()
            async let runners = service.listRunners()
            async let monitor = service.listMonitorSessions()
            self.activeSessions = try await active
            self.archivedSessions = try await archived
            self.templates = try await templates
            self.runners = try await runners
            let monitorSessions = try await monitor
            self.statusesBySessionID = Dictionary(
                monitorSessions.map { ($0.metadata.id, $0.status) },
                uniquingKeysWith: { first, _ in first }
            )
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func createSession(
        templateID: SignalboxTemplateID?,
        runnerID: SignalboxRunnerID?,
        title: String,
        guidanceTargetPath: String? = nil,
        linkedWorkspacePath: String? = nil
    ) async -> SignalboxSessionMetadata? {
        guard let service = serviceProvider() else {
            errorMessage = "Configure a server connection in Settings."
            return nil
        }
        do {
            let request = SignalboxCreateSessionRequest(
                templateID: templateID,
                systemPrompt: templateID == nil ? "You are an agent operations assistant." : nil,
                title: title.isEmpty ? "Native session" : title,
                modelAlias: nil,
                enabledTools: nil,
                runnerID: runnerID,
                guidanceTargetPath: normalizedGuidanceTargetPath(guidanceTargetPath),
                linkedWorkspacePath: linkedWorkspacePath
            )
            let created = try await service.createSession(request: request)
            activeSessions.insert(created.metadata, at: 0)
            errorMessage = nil
            return created.metadata
        } catch {
            errorMessage = error.localizedDescription
            return nil
        }
    }

    private func normalizedGuidanceTargetPath(_ path: String?) -> String? {
        let trimmedPath = path?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        return trimmedPath.isEmpty ? nil : trimmedPath
    }

    func setArchived(_ isArchived: Bool, sessionID: SignalboxSessionID) async {
        guard let service = serviceProvider() else {
            errorMessage = "Configure a server connection in Settings."
            return
        }
        do {
            let updated = try await service.patchSessionArchive(sessionID: sessionID, isArchived: isArchived)
            activeSessions.removeAll { $0.id == sessionID }
            archivedSessions.removeAll { $0.id == sessionID }
            if updated.isArchived {
                archivedSessions.insert(updated, at: 0)
            } else {
                activeSessions.insert(updated, at: 0)
            }
        } catch {
            errorMessage = error.localizedDescription
        }
    }
}

@MainActor
final class SessionDetailViewModel: ObservableObject {
    private enum StreamMessageDisposition: Sendable {
        case applied
        case bufferedForHistorySynchronization
        case ignored
    }

    private enum HistorySynchronizationResult: Equatable, Sendable {
        case synchronized
        case recoverableFailure
        case cancelled
    }

    private struct InitialHistoryLoad: Sendable {
        let events: Task<[SignalboxStoredEvent], Error>
        let artifacts: Task<[SignalboxArtifact], Error>
    }

    private struct StreamHistorySynchronization {
        let streamID: UUID
        var bufferedMessages: [SignalboxServerMessage]
        let initialHistoryLoad: InitialHistoryLoad?
        let completion: CheckedContinuation<HistorySynchronizationResult, Never>?
    }

    @Published private(set) var session: SignalboxSessionMetadata
    @Published private(set) var status: SignalboxSessionStatus = SignalboxSessionStatus(state: .idle)
    @Published private(set) var artifacts: [SignalboxArtifact] = []
    @Published private(set) var unhandledFrameKinds: [String: Int] = [:]
    @Published private(set) var latestStreamDiagnostic: String?
    @Published private(set) var ignoredStaleStreamCompletionCount = 0
    @Published var composerText: String = ""
    @Published var errorMessage: String?
    @Published var isStreaming = false

    private var serviceProvider: () -> (any SignalboxClientProtocol)?
    private var streamTask: Task<Void, Never>?
    private var activeStreamID: UUID?
    private let eventNormalizer = SignalboxIncrementalEventNormalizer()
    private var hasLoadedHistorySnapshot = false
    private var streamHistorySynchronization: StreamHistorySynchronization?
    private var streamHelloTimeoutTask: Task<Void, Never>?
    private let streamHelloTimeout: Duration
    private let waitForStreamHello: @Sendable (Duration) async throws -> Void

    init(
        session: SignalboxSessionMetadata,
        streamHelloTimeout: Duration = SignalboxWebSocketStream.defaultHeartbeatTimeout,
        waitForStreamHello: @escaping @Sendable (Duration) async throws -> Void = {
            try await Task.sleep(for: $0)
        },
        serviceProvider: @escaping () -> (any SignalboxClientProtocol)?
    ) {
        self.session = session
        self.streamHelloTimeout = streamHelloTimeout
        self.waitForStreamHello = waitForStreamHello
        self.serviceProvider = serviceProvider
    }

    func setServiceProvider(_ provider: @escaping () -> (any SignalboxClientProtocol)?) {
        self.serviceProvider = provider
    }

    var events: [SignalboxStoredEvent] {
        eventNormalizer.records
    }

    var timelineItems: [SignalboxTimelineItem] {
        eventNormalizer.timelineItems
    }

    var timeline: SignalboxTimelineCollection {
        eventNormalizer.timeline
    }

    var latestPromptContextArtifact: SignalboxArtifact? {
        artifacts.filter(\.isPromptContextArtifact).max { lhs, rhs in
            if lhs.createdAt != rhs.createdAt {
                return lhs.createdAt < rhs.createdAt
            }
            let lhsEventID = lhs.eventID?.rawValue ?? -1
            let rhsEventID = rhs.eventID?.rawValue ?? -1
            return lhsEventID < rhsEventID
        }
    }

    func load() async {
        guard let service = serviceProvider() else {
            errorMessage = "Configure a server connection in Settings."
            return
        }
        do {
            async let events = service.listEvents(sessionID: session.id)
            async let artifacts = service.listArtifacts(sessionID: session.id)
            replaceEvents(with: try await events)
            self.artifacts = try await artifacts
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func loadAndConnect() async {
        guard let service = serviceProvider() else {
            errorMessage = "Configure a server connection in Settings."
            return
        }
        let sessionID = session.id
        let initialHistoryLoad = InitialHistoryLoad(
            events: Task {
                try await service.listEvents(sessionID: sessionID)
            },
            artifacts: Task {
                try await service.listArtifacts(sessionID: sessionID)
            }
        )
        let result: HistorySynchronizationResult = await withCheckedContinuation { continuation in
            let started = startStream(
                service: service,
                synchronizeHistory: true,
                initialHistoryLoad: initialHistoryLoad,
                completion: continuation
            )
            if !started {
                continuation.resume(returning: .recoverableFailure)
            }
        }
        if result == .recoverableFailure, !Task.isCancelled {
            do {
                replaceEvents(with: try await initialHistoryLoad.events.value)
                artifacts = try await initialHistoryLoad.artifacts.value
                errorMessage = nil
            } catch {
                await load()
            }
            connectStream(synchronizeHistory: true)
        }
    }

    func connectStream() {
        connectStream(
            synchronizeHistory: hasLoadedHistorySnapshot || !eventNormalizer.records.isEmpty
        )
    }

    private func connectStream(synchronizeHistory: Bool) {
        guard streamTask == nil, let service = serviceProvider() else {
            return
        }
        _ = startStream(
            service: service,
            synchronizeHistory: synchronizeHistory,
            initialHistoryLoad: nil,
            completion: nil
        )
    }

    @discardableResult
    private func startStream(
        service: any SignalboxClientProtocol,
        synchronizeHistory: Bool,
        initialHistoryLoad: InitialHistoryLoad?,
        completion: CheckedContinuation<HistorySynchronizationResult, Never>?
    ) -> Bool {
        guard streamTask == nil else {
            return false
        }
        let sessionID = session.id
        let streamID = UUID()
        if synchronizeHistory {
            streamHistorySynchronization = StreamHistorySynchronization(
                streamID: streamID,
                bufferedMessages: [],
                initialHistoryLoad: initialHistoryLoad,
                completion: completion
            )
        }
        activeStreamID = streamID
        isStreaming = true
        streamTask = Task { [weak self] in
            do {
                for try await message in service.streamMessages(sessionID: sessionID) {
                    let disposition = await MainActor.run {
                        self?.receive(message, streamID: streamID) ?? .ignored
                    }
                    if case .streamHello = message {
                        switch disposition {
                        case .applied:
                            await self?.refreshArtifactsAfterStreamHello(
                                service: service,
                                sessionID: sessionID,
                                streamID: streamID
                            )
                        case .bufferedForHistorySynchronization:
                            await self?.synchronizeHistoryAfterStreamHello(
                                service: service,
                                sessionID: sessionID,
                                streamID: streamID
                            )
                        case .ignored:
                            break
                        }
                    }
                }
                await MainActor.run {
                    self?.finishStream(streamID: streamID, error: nil)
                }
            } catch is CancellationError {
                await MainActor.run {
                    self?.finishStream(streamID: streamID, error: nil)
                }
            } catch {
                await MainActor.run {
                    self?.finishStream(streamID: streamID, error: error)
                }
            }
        }
        if synchronizeHistory {
            let helloTimeout = streamHelloTimeout
            let waitForStreamHello = waitForStreamHello
            streamHelloTimeoutTask = Task { [weak self] in
                do {
                    try await waitForStreamHello(helloTimeout)
                } catch {
                    return
                }
                guard !Task.isCancelled else {
                    return
                }
                self?.expireHistorySynchronization(streamID: streamID)
            }
        }
        if let initialHistoryLoad {
            Task { [weak self] in
                await self?.displayInitialHistory(
                    from: initialHistoryLoad,
                    streamID: streamID
                )
            }
        }
        return true
    }

    func disconnectStream() {
        cancelHistorySynchronization(
            streamID: activeStreamID,
            result: .cancelled
        )
        activeStreamID = nil
        streamTask?.cancel()
        streamTask = nil
        isStreaming = false
    }

    func sendMessage() async {
        let text = composerText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty, let service = serviceProvider() else {
            return
        }
        composerText = ""
        do {
            let response = try await service.appendUserMessage(sessionID: session.id, text: text)
            upsert(event: SignalboxStoredEvent(eventID: response.eventID, event: response.event))
            status = response.sessionStatus
            connectStream()
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func approve(invocationID: SignalboxToolInvocationID) async {
        guard let service = serviceProvider() else { return }
        do {
            try await service.confirmInvocation(sessionID: session.id, invocationID: invocationID)
            replaceEvents(with: try await service.listEvents(sessionID: session.id))
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func deny(invocationID: SignalboxToolInvocationID) async {
        guard let service = serviceProvider() else { return }
        do {
            try await service.denyInvocation(
                sessionID: session.id,
                invocationID: invocationID,
                reason: "Denied from native client."
            )
            replaceEvents(with: try await service.listEvents(sessionID: session.id))
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    private func receive(
        _ message: SignalboxServerMessage,
        streamID: UUID
    ) -> StreamMessageDisposition {
        guard activeStreamID == streamID else {
            return .ignored
        }
        if var synchronization = streamHistorySynchronization,
           synchronization.streamID == streamID
        {
            synchronization.bufferedMessages.append(message)
            streamHistorySynchronization = synchronization
            return .bufferedForHistorySynchronization
        }
        apply(message)
        return .applied
    }

    private func apply(_ message: SignalboxServerMessage) {
        switch message {
        case .streamHello(let hello):
            session = hello.session
            status = hello.status
            mergeEvents(with: hello.events)
        case .eventAppended(let mutation), .eventUpdated(let mutation):
            upsert(event: SignalboxStoredEvent(eventID: mutation.eventID, event: mutation.event))
        case .eventDeleted(let eventID):
            removeEvent(withID: eventID)
        case .statusChanged(let status):
            self.status = status
        case .metadataChanged(let metadata):
            session = metadata
        case .artifactCreated(let artifact):
            upsert(artifact: artifact)
        case .heartbeat:
            break
        case .diagnostic(let diagnostic):
            latestStreamDiagnostic = diagnostic.message
            errorMessage = diagnostic.message
        case .unknown(let kind, _, let decodingDiagnostic):
            unhandledFrameKinds[kind, default: 0] += 1
            if let decodingDiagnostic {
                latestStreamDiagnostic = decodingDiagnostic.message
                errorMessage = decodingDiagnostic.message
            }
        }
    }

    private func refreshArtifactsAfterStreamHello(
        service: any SignalboxClientProtocol,
        sessionID: SignalboxSessionID,
        streamID: UUID
    ) async {
        do {
            let artifacts = try await service.listArtifacts(sessionID: sessionID)
            guard activeStreamID == streamID else {
                return
            }
            self.artifacts = artifacts
        } catch {
            guard activeStreamID == streamID else {
                return
            }
            errorMessage = error.localizedDescription
        }
    }

    private func synchronizeHistoryAfterStreamHello(
        service: any SignalboxClientProtocol,
        sessionID: SignalboxSessionID,
        streamID: UUID
    ) async {
        streamHelloTimeoutTask?.cancel()
        streamHelloTimeoutTask = nil
        do {
            // The hello marks the subscription boundary: only a history read
            // started after it can observe mutations, such as deletions, that
            // landed before the stream existed. The pre-stream initial load is
            // for early rendering only and is never authoritative.
            let synchronizedEvents = try await service.listEvents(sessionID: sessionID)
            guard let synchronization = takeHistorySynchronization(streamID: streamID) else {
                return
            }
            let preservedStreamError = errorMessage == latestStreamDiagnostic
                ? errorMessage
                : nil
            replaceEvents(with: synchronizedEvents)
            errorMessage = nil
            synchronization.bufferedMessages.forEach(apply)
            if errorMessage == nil {
                errorMessage = preservedStreamError
            }
            synchronization.completion?.resume(returning: .synchronized)
            // The artifact refresh is reported, never fatal: only an event
            // history failure may enter the recovery path below, and neither
            // artifact failure nor artifact latency may tear down a healthy
            // stream or suspend its consumer.
            Task { [weak self] in
                await self?.refreshArtifactsAfterStreamHello(
                    service: service,
                    sessionID: sessionID,
                    streamID: streamID
                )
            }
        } catch {
            guard let synchronization = takeHistorySynchronization(streamID: streamID) else {
                return
            }
            synchronization.bufferedMessages.forEach(apply)
            errorMessage = lastStreamDiagnostic(
                in: synchronization.bufferedMessages
            ) ?? error.localizedDescription
            disconnectStream()
            guard let completion = synchronization.completion else {
                connectStream(synchronizeHistory: true)
                return
            }
            completion.resume(returning: .recoverableFailure)
        }
    }

    private func displayInitialHistory(
        from historyLoad: InitialHistoryLoad,
        streamID: UUID
    ) async {
        do {
            let events = try await historyLoad.events.value
            guard streamHistorySynchronization?.streamID == streamID else {
                return
            }
            replaceEvents(with: events)
            errorMessage = nil
        } catch {
            guard streamHistorySynchronization?.streamID == streamID else {
                return
            }
            errorMessage = error.localizedDescription
            return
        }
        do {
            let artifacts = try await historyLoad.artifacts.value
            guard streamHistorySynchronization?.streamID == streamID else {
                return
            }
            self.artifacts = artifacts
            errorMessage = nil
        } catch {
            guard streamHistorySynchronization?.streamID == streamID else {
                return
            }
            errorMessage = error.localizedDescription
        }
    }

    private func takeHistorySynchronization(
        streamID: UUID
    ) -> StreamHistorySynchronization? {
        guard let synchronization = streamHistorySynchronization,
              synchronization.streamID == streamID
        else {
            return nil
        }
        streamHelloTimeoutTask?.cancel()
        streamHelloTimeoutTask = nil
        streamHistorySynchronization = nil
        return synchronization
    }

    private func expireHistorySynchronization(streamID: UUID) {
        guard let synchronization = takeHistorySynchronization(streamID: streamID) else {
            return
        }
        applySynchronizationDiagnostics(in: synchronization.bufferedMessages)
        guard let completion = synchronization.completion else {
            disconnectStream()
            connectStream(synchronizeHistory: true)
            return
        }
        disconnectStream()
        completion.resume(returning: .recoverableFailure)
    }

    private func cancelHistorySynchronization(
        streamID: UUID?,
        result: HistorySynchronizationResult
    ) {
        guard let streamID,
              let synchronization = takeHistorySynchronization(streamID: streamID)
        else {
            return
        }
        if result == .cancelled {
            synchronization.initialHistoryLoad?.events.cancel()
            synchronization.initialHistoryLoad?.artifacts.cancel()
        }
        synchronization.completion?.resume(returning: result)
    }

    private func finishStream(streamID: UUID, error: Error?) {
        guard activeStreamID == streamID else {
            ignoredStaleStreamCompletionCount += 1
            return
        }
        cancelHistorySynchronization(
            streamID: streamID,
            result: .recoverableFailure
        )
        activeStreamID = nil
        streamTask = nil
        isStreaming = false
        if let error {
            errorMessage = error.localizedDescription
        }
    }

    private func applySynchronizationDiagnostics(
        in messages: [SignalboxServerMessage]
    ) {
        for message in messages {
            switch message {
            case .diagnostic, .unknown:
                apply(message)
            default:
                continue
            }
        }
    }

    private func lastStreamDiagnostic(
        in messages: [SignalboxServerMessage]
    ) -> String? {
        for message in messages.reversed() {
            switch message {
            case .diagnostic(let diagnostic):
                return diagnostic.message
            case .unknown(_, _, let diagnostic):
                if let diagnostic {
                    return diagnostic.message
                }
            default:
                continue
            }
        }
        return nil
    }

    private func upsert(artifact: SignalboxArtifact) {
        if let index = artifacts.firstIndex(where: { $0.id == artifact.id }) {
            artifacts[index] = artifact
        } else {
            artifacts.insert(artifact, at: 0)
        }
    }

    private func upsert(event: SignalboxStoredEvent) {
        objectWillChange.send()
        eventNormalizer.upsert(event)
    }

    private func removeEvent(withID eventID: SignalboxEventID) {
        objectWillChange.send()
        eventNormalizer.remove(eventID: eventID)
    }

    private func replaceEvents(with events: [SignalboxStoredEvent]) {
        objectWillChange.send()
        eventNormalizer.replaceAll(with: events)
        hasLoadedHistorySnapshot = true
    }

    private func mergeEvents(with events: [SignalboxStoredEvent]) {
        objectWillChange.send()
        eventNormalizer.upsert(contentsOf: events)
    }
}

@MainActor
final class OperationsViewModel: ObservableObject {
    @Published private(set) var monitorSessions: [SignalboxMonitorSessionSummary] = []
    @Published private(set) var runners: [SignalboxRunner] = []
    @Published private(set) var templates: [SignalboxTemplate] = []
    @Published var errorMessage: String?

    private var serviceProvider: () -> (any SignalboxClientProtocol)?

    init(serviceProvider: @escaping () -> (any SignalboxClientProtocol)?) {
        self.serviceProvider = serviceProvider
    }

    func setServiceProvider(_ provider: @escaping () -> (any SignalboxClientProtocol)?) {
        self.serviceProvider = provider
    }

    var needsAttention: [SignalboxMonitorSessionSummary] {
        monitorSessions.filter { $0.status.state == .waitingForConfirmation || $0.status.state == .failed }
    }

    func refresh() async {
        guard let service = serviceProvider() else {
            errorMessage = "Configure a server connection in Settings."
            return
        }
        do {
            async let monitor = service.listMonitorSessions()
            async let runners = service.listRunners()
            async let templates = service.listTemplates()
            self.monitorSessions = try await monitor
            self.runners = try await runners
            self.templates = try await templates
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }
}
