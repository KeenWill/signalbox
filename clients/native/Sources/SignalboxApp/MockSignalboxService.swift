#if canImport(SignalboxClient)
import SignalboxClient
#endif
#if canImport(SignalboxModels)
import SignalboxModels
#endif
import Foundation

actor MockSignalboxService: SignalboxClientProtocol {
    private var sessions: [SignalboxSessionMetadata]
    private var activeEvents: [SignalboxSessionID: [SignalboxStoredEvent]]
    private var artifactsBySession: [SignalboxSessionID: [SignalboxArtifact]]
    private let decoder = SignalboxJSONCoding.decoder()

    init() {
        let fixture = try! decoder.decode(MockSignalboxFixture.self, from: Data(MockSignalboxFixtures.initial.utf8))
        self.sessions = fixture.sessions
        self.activeEvents = Dictionary(uniqueKeysWithValues: fixture.eventsBySession.map {
            (SignalboxSessionID(rawValue: $0.key), $0.value)
        })
        self.artifactsBySession = Dictionary(uniqueKeysWithValues: fixture.artifactsBySession.map {
            (SignalboxSessionID(rawValue: $0.key), $0.value)
        })
    }

    func testConnection() async throws {}

    func listTemplates() async throws -> [SignalboxTemplate] {
        try decoder.decode(SignalboxTemplateListResponse.self, from: Data(MockSignalboxFixtures.templates.utf8)).templates
    }

    func listRunners() async throws -> [SignalboxRunner] {
        try decoder.decode(SignalboxRunnerListResponse.self, from: Data(MockSignalboxFixtures.runners.utf8)).runners
    }

    func listSessions(archived: Bool) async throws -> [SignalboxSessionMetadata] {
        sessions.filter { $0.isArchived == archived }
    }

    func createSession(request: SignalboxCreateSessionRequest) async throws -> SignalboxSessionView {
        let created = try decoder.decode(SignalboxSessionView.self, from: Data(MockSignalboxFixtures.createdSession.utf8))
        sessions.insert(created.metadata, at: 0)
        activeEvents[created.metadata.id] = []
        artifactsBySession[created.metadata.id] = []
        return created
    }

    func patchSessionArchive(sessionID: SignalboxSessionID, isArchived: Bool) async throws -> SignalboxSessionMetadata {
        guard let index = sessions.firstIndex(where: { $0.id == sessionID }) else {
            throw SignalboxClientError.notFound("session not found")
        }
        sessions[index].isArchived = isArchived
        return sessions[index]
    }

    func listEvents(sessionID: SignalboxSessionID) async throws -> [SignalboxStoredEvent] {
        activeEvents[sessionID] ?? []
    }

    func appendUserMessage(
        sessionID: SignalboxSessionID,
        text: String
    ) async throws -> SignalboxAppendUserMessageResponse {
        let response = try decoder.decode(
            SignalboxAppendUserMessageResponse.self,
            from: Data(MockSignalboxFixtures.appendedUserMessage(text: text).utf8)
        )
        activeEvents[sessionID, default: []].append(SignalboxStoredEvent(eventID: response.eventID, event: response.event))
        return response
    }

    func confirmInvocation(sessionID: SignalboxSessionID, invocationID: SignalboxToolInvocationID) async throws {
        activeEvents[sessionID] = try decoder.decode(
            [SignalboxStoredEvent].self,
            from: Data(MockSignalboxFixtures.approvedToolEvents.utf8)
        )
    }

    func denyInvocation(sessionID: SignalboxSessionID, invocationID: SignalboxToolInvocationID, reason: String?) async throws {
        activeEvents[sessionID] = try decoder.decode(
            [SignalboxStoredEvent].self,
            from: Data(MockSignalboxFixtures.deniedToolEvents.utf8)
        )
    }

    func listArtifacts(sessionID: SignalboxSessionID) async throws -> [SignalboxArtifact] {
        artifactsBySession[sessionID] ?? []
    }

    func listMonitorSessions() async throws -> [SignalboxMonitorSessionSummary] {
        try decoder.decode(SignalboxMonitorSessionListResponse.self, from: Data(MockSignalboxFixtures.monitor.utf8)).sessions
    }

    nonisolated func streamMessages(sessionID: SignalboxSessionID) -> AsyncThrowingStream<SignalboxServerMessage, Error> {
        AsyncThrowingStream { continuation in
            Task {
                guard sessionID.rawValue == MockSignalboxFixtures.activeSessionID else {
                    continuation.finish()
                    return
                }
                let decoder = SignalboxJSONCoding.decoder()
                for rawMessage in MockSignalboxFixtures.streamMessages {
                    try await Task.sleep(
                        nanoseconds: MockSignalboxFixtures.streamMessageDelayNanoseconds
                    )
                    let message = try decoder.decode(SignalboxServerMessage.self, from: Data(rawMessage.utf8))
                    continuation.yield(message)
                }
                continuation.finish()
            }
        }
    }
}

private struct MockSignalboxFixture: Decodable {
    let sessions: [SignalboxSessionMetadata]
    let eventsBySession: [String: [SignalboxStoredEvent]]
    let artifactsBySession: [String: [SignalboxArtifact]]

    private enum CodingKeys: String, CodingKey {
        case sessions
        case eventsBySession = "events_by_session"
        case artifactsBySession = "artifacts_by_session"
    }
}
