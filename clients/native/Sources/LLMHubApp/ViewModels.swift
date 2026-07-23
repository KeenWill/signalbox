#if canImport(LLMHubClient)
import LLMHubClient
#endif
#if canImport(LLMHubModels)
import LLMHubModels
#endif
import Combine
import Foundation
import SwiftUI

@MainActor
final class AppCoordinator: ObservableObject {
    @Published var settings: HubSettingsViewModel
    @Published var service: (any HubClientProtocol)?
    @Published var selectedSection: AppSection = .sessions
    @Published var selectedSessionID: HubSessionID?

    let isMockMode: Bool
    let screenshotScenario: ScreenshotScenario?

    init(isMockMode: Bool, screenshotScenario: ScreenshotScenario? = nil, resetPersistedSettings: Bool = false) {
        self.screenshotScenario = screenshotScenario
        let shouldInstallMockService = isMockMode || screenshotScenario?.requiresMockService == true
        self.isMockMode = shouldInstallMockService
        if resetPersistedSettings {
            UserDefaults.standard.removeObject(forKey: NativeAppConstants.hubURLDefaultsKey)
            KeychainSecretStore().deleteSecret(
                service: NativeAppConstants.serviceName,
                account: NativeAppConstants.apiKeyAccount
            )
        }
        self.settings = HubSettingsViewModel()
        if resetPersistedSettings {
            self.settings.hubURLText = ""
            self.settings.apiKey = ""
        }
        if let screenshotScenario {
            selectedSection = screenshotScenario.selectedSection
            selectedSessionID = screenshotScenario.selectedSessionID
        }
        if shouldInstallMockService {
            self.service = MockHubService()
            self.settings.hubURLText = NativeAppConstants.defaultHubURL
            self.settings.apiKey = "mock-key"
        } else if screenshotScenario == .setup {
            self.service = nil
            self.settings.apiKey = ""
        } else if case .success(let configuration) = settings.configurationResult() {
            self.service = HubClient(configuration: configuration)
        }
    }

    func testConnectionAndInstallClient() async {
        if case .failure(let error) = settings.configurationResult() {
            await settings.testConnection(using: FailingHubClient(error: error))
            return
        }
        if isMockMode {
            await settings.testConnection(using: service)
            return
        }
        switch settings.configurationResult() {
        case .success(let configuration):
            let client = HubClient(configuration: configuration)
            await settings.testConnection(using: client)
            if settings.connectionStatus == .connected {
                service = client
                NotificationCenter.default.post(name: .hubRefreshRequested, object: nil)
            }
        case .failure(let error):
            await settings.testConnection(using: FailingHubClient(error: error))
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

    var selectedSessionID: HubSessionID? {
        switch self {
        case .activeChat, .completedTool, .artifactPreview:
            return HubSessionID(rawValue: MockHubFixtures.activeSessionID)
        case .markdownBasics:
            return HubSessionID(rawValue: MockHubFixtures.markdownBasicsSessionID)
        case .markdownTable:
            return HubSessionID(rawValue: MockHubFixtures.markdownTableSessionID)
        case .markdownCode:
            return HubSessionID(rawValue: MockHubFixtures.markdownCodeSessionID)
        case .markdownMessage:
            return HubSessionID(rawValue: MockHubFixtures.markdownSessionID)
        case .pendingApproval:
            return HubSessionID(rawValue: MockHubFixtures.approvalSessionID)
        case .failedTool:
            return HubSessionID(rawValue: MockHubFixtures.failedSessionID)
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

private struct FailingHubClient: HubClientProtocol {
    let error: HubClientError

    func testConnection() async throws { throw error }
    func listTemplates() async throws -> [HubTemplate] { throw error }
    func listRunners() async throws -> [HubRunner] { throw error }
    func listSessions(archived: Bool) async throws -> [HubSessionMetadata] { throw error }
    func createSession(request: HubCreateSessionRequest) async throws -> HubSessionView { throw error }
    func patchSessionArchive(sessionID: HubSessionID, isArchived: Bool) async throws -> HubSessionMetadata { throw error }
    func listEvents(sessionID: HubSessionID) async throws -> [HubStoredEvent] { throw error }
    func appendUserMessage(sessionID: HubSessionID, text: String) async throws -> HubAppendUserMessageResponse { throw error }
    func confirmInvocation(sessionID: HubSessionID, invocationID: HubToolInvocationID) async throws { throw error }
    func denyInvocation(sessionID: HubSessionID, invocationID: HubToolInvocationID, reason: String?) async throws { throw error }
    func listArtifacts(sessionID: HubSessionID) async throws -> [HubArtifact] { throw error }
    func listMonitorSessions() async throws -> [HubMonitorSessionSummary] { throw error }
    func streamMessages(sessionID: HubSessionID) -> AsyncThrowingStream<HubServerMessage, Error> {
        AsyncThrowingStream { continuation in continuation.finish(throwing: error) }
    }
}

@MainActor
final class SessionListViewModel: ObservableObject {
    @Published private(set) var activeSessions: [HubSessionMetadata] = []
    @Published private(set) var archivedSessions: [HubSessionMetadata] = []
    @Published private(set) var templates: [HubTemplate] = []
    @Published private(set) var runners: [HubRunner] = []
    @Published private(set) var statusesBySessionID: [HubSessionID: HubSessionStatus] = [:]
    @Published var searchText: String = ""
    @Published var showArchived: Bool = false
    @Published var errorMessage: String?
    @Published var isLoading = false

    private var serviceProvider: () -> (any HubClientProtocol)?

    init(serviceProvider: @escaping () -> (any HubClientProtocol)?) {
        self.serviceProvider = serviceProvider
    }

    func replaceServiceProvider(_ provider: @escaping () -> (any HubClientProtocol)?) {
        self.serviceProvider = provider
    }

    var visibleSessions: [HubSessionMetadata] {
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

    func session(with sessionID: HubSessionID) -> HubSessionMetadata? {
        (activeSessions + archivedSessions).first { $0.id == sessionID }
    }

    func refresh() async {
        guard let service = serviceProvider() else {
            errorMessage = "Configure a hub connection in Settings."
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
            self.statusesBySessionID = Dictionary(uniqueKeysWithValues: monitorSessions.map { ($0.metadata.id, $0.status) })
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func createSession(
        templateID: HubTemplateID?,
        runnerID: HubRunnerID?,
        title: String,
        guidanceTargetPath: String? = nil,
        linkedWorkspacePath: String? = nil
    ) async -> HubSessionMetadata? {
        guard let service = serviceProvider() else {
            errorMessage = "Configure a hub connection in Settings."
            return nil
        }
        do {
            let request = HubCreateSessionRequest(
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

    func setArchived(_ isArchived: Bool, sessionID: HubSessionID) async {
        guard let service = serviceProvider() else {
            errorMessage = "Configure a hub connection in Settings."
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
    @Published private(set) var session: HubSessionMetadata
    @Published private(set) var status: HubSessionStatus = HubSessionStatus(state: .idle)
    @Published private(set) var events: [HubStoredEvent] = []
    @Published private(set) var artifacts: [HubArtifact] = []
    @Published var composerText: String = ""
    @Published var errorMessage: String?
    @Published var isStreaming = false

    private var serviceProvider: () -> (any HubClientProtocol)?
    private var streamTask: Task<Void, Never>?
    private var activeStreamID: UUID?

    init(session: HubSessionMetadata, serviceProvider: @escaping () -> (any HubClientProtocol)?) {
        self.session = session
        self.serviceProvider = serviceProvider
    }

    func setServiceProvider(_ provider: @escaping () -> (any HubClientProtocol)?) {
        self.serviceProvider = provider
    }

    var timelineItems: [HubTimelineItem] {
        HubEventNormalizer.normalize(events)
    }

    var latestPromptContextArtifact: HubArtifact? {
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
            errorMessage = "Configure a hub connection in Settings."
            return
        }
        do {
            async let events = service.listEvents(sessionID: session.id)
            async let artifacts = service.listArtifacts(sessionID: session.id)
            self.events = try await events
            self.artifacts = try await artifacts
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func connectStream() {
        guard streamTask == nil, let service = serviceProvider() else {
            return
        }
        let sessionID = session.id
        let streamID = UUID()
        activeStreamID = streamID
        isStreaming = true
        streamTask = Task { [weak self] in
            do {
                for try await message in service.streamMessages(sessionID: sessionID) {
                    await MainActor.run {
                        self?.apply(message, streamID: streamID)
                    }
                    if case .streamHello = message {
                        await self?.refreshArtifactsAfterStreamHello(
                            service: service,
                            sessionID: sessionID,
                            streamID: streamID
                        )
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
    }

    func disconnectStream() {
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
            upsert(event: HubStoredEvent(eventID: response.eventID, event: response.event))
            status = response.sessionStatus
            connectStream()
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func approve(invocationID: HubToolInvocationID) async {
        guard let service = serviceProvider() else { return }
        do {
            try await service.confirmInvocation(sessionID: session.id, invocationID: invocationID)
            events = try await service.listEvents(sessionID: session.id)
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func deny(invocationID: HubToolInvocationID) async {
        guard let service = serviceProvider() else { return }
        do {
            try await service.denyInvocation(
                sessionID: session.id,
                invocationID: invocationID,
                reason: "Denied from native client."
            )
            events = try await service.listEvents(sessionID: session.id)
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    private func apply(_ message: HubServerMessage, streamID: UUID) {
        guard activeStreamID == streamID else {
            return
        }
        apply(message)
    }

    private func apply(_ message: HubServerMessage) {
        switch message {
        case .streamHello(let hello):
            session = hello.session
            status = hello.status
            events = hello.events
        case .eventAppended(let mutation), .eventUpdated(let mutation):
            upsert(event: HubStoredEvent(eventID: mutation.eventID, event: mutation.event))
        case .eventDeleted(let eventID):
            events.removeAll { $0.eventID == eventID }
        case .statusChanged(let status):
            self.status = status
        case .metadataChanged(let metadata):
            session = metadata
        case .artifactCreated(let artifact):
            upsert(artifact: artifact)
        case .heartbeat:
            break
        case .unknown(let kind, let payload):
            events.append(
                HubStoredEvent(
                    eventID: HubEventID(rawValue: (events.last?.eventID.rawValue ?? 0) + 1),
                    event: .unknown(HubUnknownEvent(kind: kind, payload: payload))
                )
            )
        }
    }

    private func refreshArtifactsAfterStreamHello(
        service: any HubClientProtocol,
        sessionID: HubSessionID,
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

    private func finishStream(streamID: UUID, error: Error?) {
        guard activeStreamID == streamID else {
            return
        }
        activeStreamID = nil
        streamTask = nil
        isStreaming = false
        if let error {
            errorMessage = error.localizedDescription
        }
    }

    private func upsert(artifact: HubArtifact) {
        if let index = artifacts.firstIndex(where: { $0.id == artifact.id }) {
            artifacts[index] = artifact
        } else {
            artifacts.insert(artifact, at: 0)
        }
    }

    private func upsert(event: HubStoredEvent) {
        if let index = events.firstIndex(where: { $0.eventID == event.eventID }) {
            events[index] = event
        } else {
            events.append(event)
        }
        events.sort { $0.eventID < $1.eventID }
    }
}

@MainActor
final class OperationsViewModel: ObservableObject {
    @Published private(set) var monitorSessions: [HubMonitorSessionSummary] = []
    @Published private(set) var runners: [HubRunner] = []
    @Published private(set) var templates: [HubTemplate] = []
    @Published var errorMessage: String?

    private var serviceProvider: () -> (any HubClientProtocol)?

    init(serviceProvider: @escaping () -> (any HubClientProtocol)?) {
        self.serviceProvider = serviceProvider
    }

    func setServiceProvider(_ provider: @escaping () -> (any HubClientProtocol)?) {
        self.serviceProvider = provider
    }

    var needsAttention: [HubMonitorSessionSummary] {
        monitorSessions.filter { $0.status.state == .waitingForConfirmation || $0.status.state == .failed }
    }

    func refresh() async {
        guard let service = serviceProvider() else {
            errorMessage = "Configure a hub connection in Settings."
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
