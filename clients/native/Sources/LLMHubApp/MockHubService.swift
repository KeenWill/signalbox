#if canImport(LLMHubClient)
import LLMHubClient
#endif
#if canImport(LLMHubModels)
import LLMHubModels
#endif
import Foundation

actor MockHubService: HubClientProtocol {
    private var sessions: [HubSessionMetadata]
    private var activeEvents: [HubSessionID: [HubStoredEvent]]
    private var artifactsBySession: [HubSessionID: [HubArtifact]]
    private let decoder = HubJSONCoding.decoder()

    init() {
        let fixture = try! decoder.decode(MockHubFixture.self, from: Data(MockHubFixtures.initial.utf8))
        self.sessions = fixture.sessions
        self.activeEvents = Dictionary(uniqueKeysWithValues: fixture.eventsBySession.map {
            (HubSessionID(rawValue: $0.key), $0.value)
        })
        self.artifactsBySession = Dictionary(uniqueKeysWithValues: fixture.artifactsBySession.map {
            (HubSessionID(rawValue: $0.key), $0.value)
        })
    }

    func testConnection() async throws {}

    func listTemplates() async throws -> [HubTemplate] {
        try decoder.decode(HubTemplateListResponse.self, from: Data(MockHubFixtures.templates.utf8)).templates
    }

    func listRunners() async throws -> [HubRunner] {
        try decoder.decode(HubRunnerListResponse.self, from: Data(MockHubFixtures.runners.utf8)).runners
    }

    func listSessions(archived: Bool) async throws -> [HubSessionMetadata] {
        sessions.filter { $0.isArchived == archived }
    }

    func createSession(request: HubCreateSessionRequest) async throws -> HubSessionView {
        let created = try decoder.decode(HubSessionView.self, from: Data(MockHubFixtures.createdSession.utf8))
        sessions.insert(created.metadata, at: 0)
        activeEvents[created.metadata.id] = []
        artifactsBySession[created.metadata.id] = []
        return created
    }

    func patchSessionArchive(sessionID: HubSessionID, isArchived: Bool) async throws -> HubSessionMetadata {
        guard let index = sessions.firstIndex(where: { $0.id == sessionID }) else {
            throw HubClientError.notFound("session not found")
        }
        sessions[index].isArchived = isArchived
        return sessions[index]
    }

    func listEvents(sessionID: HubSessionID) async throws -> [HubStoredEvent] {
        activeEvents[sessionID] ?? []
    }

    func appendUserMessage(
        sessionID: HubSessionID,
        text: String
    ) async throws -> HubAppendUserMessageResponse {
        let response = try decoder.decode(
            HubAppendUserMessageResponse.self,
            from: Data(MockHubFixtures.appendedUserMessage(text: text).utf8)
        )
        activeEvents[sessionID, default: []].append(HubStoredEvent(eventID: response.eventID, event: response.event))
        return response
    }

    func confirmInvocation(sessionID: HubSessionID, invocationID: HubToolInvocationID) async throws {
        activeEvents[sessionID] = try decoder.decode(
            [HubStoredEvent].self,
            from: Data(MockHubFixtures.approvedToolEvents.utf8)
        )
    }

    func denyInvocation(sessionID: HubSessionID, invocationID: HubToolInvocationID, reason: String?) async throws {
        activeEvents[sessionID] = try decoder.decode(
            [HubStoredEvent].self,
            from: Data(MockHubFixtures.deniedToolEvents.utf8)
        )
    }

    func listArtifacts(sessionID: HubSessionID) async throws -> [HubArtifact] {
        artifactsBySession[sessionID] ?? []
    }

    func listMonitorSessions() async throws -> [HubMonitorSessionSummary] {
        try decoder.decode(HubMonitorSessionListResponse.self, from: Data(MockHubFixtures.monitor.utf8)).sessions
    }

    nonisolated func streamMessages(sessionID: HubSessionID) -> AsyncThrowingStream<HubServerMessage, Error> {
        AsyncThrowingStream { continuation in
            Task {
                guard sessionID.rawValue == MockHubFixtures.activeSessionID else {
                    continuation.finish()
                    return
                }
                let decoder = HubJSONCoding.decoder()
                for rawMessage in MockHubFixtures.streamMessages {
                    try await Task.sleep(nanoseconds: 180_000_000)
                    let message = try decoder.decode(HubServerMessage.self, from: Data(rawMessage.utf8))
                    continuation.yield(message)
                }
                continuation.finish()
            }
        }
    }
}

private struct MockHubFixture: Decodable {
    let sessions: [HubSessionMetadata]
    let eventsBySession: [String: [HubStoredEvent]]
    let artifactsBySession: [String: [HubArtifact]]

    private enum CodingKeys: String, CodingKey {
        case sessions
        case eventsBySession = "events_by_session"
        case artifactsBySession = "artifacts_by_session"
    }
}
