import Combine
import Foundation
@testable import SignalboxApp
import SignalboxClient
import SignalboxModels
import XCTest

@MainActor
final class ViewModelTests: XCTestCase {
    func testSessionListLoadsMockSessionsAndNeedsApprovalStatus() async {
        let service = MockSignalboxService()
        let viewModel = SessionListViewModel { service }

        await viewModel.refresh()

        XCTAssertEqual(viewModel.activeSessions.count, 7)
        XCTAssertEqual(
            viewModel.statusesBySessionID[SignalboxSessionID(rawValue: MockSignalboxFixtures.approvalSessionID)]?.state,
            .waitingForConfirmation
        )
        XCTAssertEqual(
            viewModel.statusesBySessionID[SignalboxSessionID(rawValue: MockSignalboxFixtures.failedSessionID)]?.state,
            .failed
        )
    }

    func testMonitorSummaryDecodesTaskSummary() throws {
        var monitorPayload = try XCTUnwrap(
            JSONSerialization.jsonObject(with: Data(MockSignalboxFixtures.monitor.utf8)) as? [String: Any]
        )
        var sessions = try XCTUnwrap(monitorPayload["sessions"] as? [[String: Any]])
        let approvalSessionIndex = try XCTUnwrap(sessions.firstIndex { session in
            guard let metadata = session["metadata"] as? [String: Any] else {
                return false
            }
            return metadata["id"] as? String == MockSignalboxFixtures.approvalSessionID
        })
        sessions[approvalSessionIndex]["task_summary"] = [
            "total": 3,
            "todo": 1,
            "in_progress": 1,
            "blocked": 1,
            "done": 0,
            "cancelled": 0,
        ]
        monitorPayload["sessions"] = sessions
        let monitorWithTaskSummary = try JSONSerialization.data(withJSONObject: monitorPayload)

        let response = try SignalboxJSONCoding.decoder().decode(
            SignalboxMonitorSessionListResponse.self,
            from: monitorWithTaskSummary
        )

        let summary = response.sessions.first { $0.id == SignalboxSessionID(rawValue: MockSignalboxFixtures.approvalSessionID) }
        XCTAssertEqual(summary?.taskSummary.total, 3)
        XCTAssertEqual(summary?.taskSummary.inProgress, 1)
        XCTAssertEqual(summary?.taskSummary.blocked, 1)

        let summaryWithoutTaskSummary = response.sessions.first { $0.id == SignalboxSessionID(rawValue: MockSignalboxFixtures.activeSessionID) }
        XCTAssertEqual(summaryWithoutTaskSummary?.taskSummary.total, 0)
    }

    func testDetailViewModelApprovesPendingTool() async {
        let service = MockSignalboxService()
        let sessions = try! await service.listSessions(archived: false)
        let session = sessions.first { $0.id == SignalboxSessionID(rawValue: MockSignalboxFixtures.approvalSessionID) }!
        let viewModel = SessionDetailViewModel(session: session) { service }

        await viewModel.load()
        let before = viewModel.timelineItems.compactMap { item -> SignalboxToolCard? in
            if case .tool(let tool) = item {
                return tool
            }
            return nil
        }
        XCTAssertEqual(before.first?.status, .waitingForApproval)

        await viewModel.approve(invocationID: SignalboxToolInvocationID(rawValue: MockSignalboxFixtures.invocationID))

        let after = viewModel.timelineItems.compactMap { item -> SignalboxToolCard? in
            if case .tool(let tool) = item {
                return tool
            }
            return nil
        }
        XCTAssertEqual(after.first?.status, .succeeded)
    }

    func testDetailStreamCanReconnectAfterCompletion() async {
        let fixtureService = MockSignalboxService()
        let sessions = try! await fixtureService.listSessions(archived: false)
        let session = sessions.first!
        let finishingService = FinishingStreamSignalboxService()
        let viewModel = SessionDetailViewModel(session: session) { finishingService }

        viewModel.connectStream()
        await waitForStreamInvocationCount(1, viewModel: viewModel, service: finishingService)
        viewModel.connectStream()
        await waitForStreamInvocationCount(2, viewModel: viewModel, service: finishingService)

        XCTAssertEqual(finishingService.streamInvocationCount, 2)
        XCTAssertFalse(viewModel.isStreaming)
    }

    func testDetailStreamIgnoresCancelledStreamCompletionAfterReconnect() async {
        let fixtureService = MockSignalboxService()
        let sessions = try! await fixtureService.listSessions(archived: false)
        let session = sessions.first!
        let controlledService = ControlledStreamSignalboxService()
        let viewModel = SessionDetailViewModel(session: session) { controlledService }

        viewModel.connectStream()
        await waitForControlledStreamInvocationCount(1, viewModel: viewModel, service: controlledService)
        let staleCompletionRejected = expectation(description: "stale completion rejected")
        let staleCompletionObservation = viewModel.$ignoredStaleStreamCompletionCount
            .filter { $0 == 1 }
            .first()
            .sink { _ in staleCompletionRejected.fulfill() }
        viewModel.disconnectStream()
        viewModel.connectStream()
        await waitForControlledStreamInvocationCount(2, viewModel: viewModel, service: controlledService)

        await fulfillment(of: [staleCompletionRejected], timeout: 1)
        XCTAssertTrue(viewModel.isStreaming)

        controlledService.finishStream(at: 1)
        await waitForControlledStreamToStop(viewModel)
        XCTAssertFalse(viewModel.isStreaming)
        withExtendedLifetime(staleCompletionObservation) {}
    }

    func testDetailStreamHelloRefreshesArtifactsAfterInitialSnapshot() async {
        let fixtureService = MockSignalboxService()
        let sessions = try! await fixtureService.listSessions(archived: false)
        let session = sessions.first { $0.id == SignalboxSessionID(rawValue: MockSignalboxFixtures.activeSessionID) }!
        let artifact = try! await fixtureService.listArtifacts(sessionID: session.id).first!
        let refreshService = ArtifactRefreshStreamSignalboxService(session: session, artifactAfterStreamHello: artifact)
        let viewModel = SessionDetailViewModel(session: session) { refreshService }

        await viewModel.load()
        XCTAssertTrue(viewModel.artifacts.isEmpty)

        viewModel.connectStream()

        await waitForArtifactCount(1, viewModel: viewModel)
        XCTAssertEqual(viewModel.artifacts.first?.id, artifact.id)
    }

    func testCreateSessionForwardsGuidanceTargetPath() async {
        let service = RecordingCreateSessionSignalboxService()
        let viewModel = SessionListViewModel { service }

        let session = await viewModel.createSession(
            templateID: SignalboxTemplateID(rawValue: "coder"),
            runnerID: SignalboxRunnerID(rawValue: "local-runner"),
            title: "Native coder",
            guidanceTargetPath: " projects/demo "
        )

        XCTAssertEqual(session?.id, SignalboxSessionID(rawValue: MockSignalboxFixtures.createdSessionID))
        let request = await service.lastCreateRequest
        XCTAssertEqual(request?.guidanceTargetPath, "projects/demo")
    }

    func testCreateSessionOmitsBlankGuidanceTargetPath() async {
        let service = RecordingCreateSessionSignalboxService()
        let viewModel = SessionListViewModel { service }

        _ = await viewModel.createSession(
            templateID: SignalboxTemplateID(rawValue: "coder"),
            runnerID: nil,
            title: "Native coder",
            guidanceTargetPath: " \n\t "
        )

        let request = await service.lastCreateRequest
        XCTAssertNil(request?.guidanceTargetPath)
    }

    func testDetailViewModelExposesLatestPromptContextSummary() async throws {
        let fixtureService = MockSignalboxService()
        let sessions = try await fixtureService.listSessions(archived: false)
        let session = try XCTUnwrap(
            sessions.first { $0.id == SignalboxSessionID(rawValue: MockSignalboxFixtures.activeSessionID) }
        )
        let olderArtifact = try makePromptContextArtifact(
            id: "11111111-2222-4333-8444-555555555555",
            sessionID: session.id,
            eventID: 999,
            modelAlias: "claude-haiku-latest",
            runnerID: "old-runner",
            linkedWorkspacePath: nil,
            enabledTools: ["read_file"],
            createdAt: "2026-05-10T12:01:00Z"
        )
        let latestArtifact = try makePromptContextArtifact(
            id: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
            sessionID: session.id,
            eventID: nil,
            modelAlias: "claude-sonnet-latest",
            runnerID: "local-runner",
            linkedWorkspacePath: "projects/signalbox",
            enabledTools: ["read_file", "inspect_image", "bash"],
            createdAt: "2026-05-10T12:04:00Z",
            workspaceDir: "/workspaces/signalbox",
            projectWorkspaceDir: "/workspaces/mono"
        )
        let service = StaticArtifactSignalboxService(artifacts: [olderArtifact, latestArtifact])
        let viewModel = SessionDetailViewModel(session: session) { service }

        await viewModel.load()

        let promptContext = try XCTUnwrap(viewModel.latestPromptContextArtifact)
        XCTAssertEqual(promptContext.id, latestArtifact.id)
        let summary = try XCTUnwrap(promptContext.promptContextSummary)
        XCTAssertTrue(summary.projectContextConsidered)
        XCTAssertTrue(summary.truncated)
        XCTAssertEqual(summary.guidanceDocumentCount, 2)
        XCTAssertEqual(summary.selectedSkillCardCount, 3)
        XCTAssertEqual(summary.selectedSkillDocumentCount, 1)
        XCTAssertEqual(summary.selectedAgentProfileCardCount, 1)
        XCTAssertEqual(summary.modelAlias, "claude-sonnet-latest")
        XCTAssertEqual(summary.runnerID, "local-runner")
        XCTAssertEqual(summary.workspaceDir, "/workspaces/signalbox")
        XCTAssertEqual(summary.projectWorkspaceDir, "/workspaces/mono")
        XCTAssertEqual(summary.linkedWorkspacePath, "projects/signalbox")
        XCTAssertEqual(summary.enabledToolCount, 3)
    }

    func testPromptContextSummaryIsUnavailableForIncompleteMetadata() throws {
        let artifact = try makePromptContextArtifact(
            id: "bbbbbbbb-cccc-4ddd-8eee-ffffffffffff",
            sessionID: SignalboxSessionID(rawValue: MockSignalboxFixtures.activeSessionID),
            eventID: nil,
            metadata: ["truncated": false]
        )

        XCTAssertTrue(artifact.isPromptContextArtifact)
        XCTAssertNil(artifact.promptContextSummary)
    }

    func testPromptContextSummaryRejectsFractionalCountMetadata() throws {
        let artifact = try makePromptContextArtifact(
            id: "cccccccc-dddd-4eee-8fff-aaaaaaaaaaaa",
            sessionID: SignalboxSessionID(rawValue: MockSignalboxFixtures.activeSessionID),
            eventID: nil,
            metadata: [
                "guidance_documents": [],
                "selected_skill_card_count": 1.5,
                "selected_skill_document_count": 1,
                "selected_agent_profile_card_count": 0,
                "truncated": false,
                "project_context_considered": true,
                "runtime_context": [
                    "enabled_tool_names": ["read_file"],
                ],
            ]
        )

        XCTAssertTrue(artifact.isPromptContextArtifact)
        XCTAssertNil(artifact.promptContextSummary)
    }

    func testPromptContextSummaryAllowsAbsentWorkspaceMetadata() throws {
        let sessionID = SignalboxSessionID(rawValue: MockSignalboxFixtures.activeSessionID)
        let artifact = try makePromptContextArtifact(
            id: "dddddddd-eeee-4fff-8000-111111111111",
            sessionID: sessionID,
            eventID: nil,
            modelAlias: "claude-sonnet-latest",
            runnerID: "local-runner",
            linkedWorkspacePath: nil,
            enabledTools: [],
            createdAt: "2026-05-10T12:05:00Z",
            workspaceDir: nil,
            projectWorkspaceDir: nil
        )

        let summary = try XCTUnwrap(artifact.promptContextSummary)
        XCTAssertNil(summary.workspaceDir)
        XCTAssertNil(summary.projectWorkspaceDir)
        XCTAssertNil(summary.linkedWorkspacePath)
    }

    func testPromptContextSummaryRejectsMalformedCountMetadata() throws {
        let sessionID = SignalboxSessionID(rawValue: MockSignalboxFixtures.activeSessionID)
        let rawArtifact = """
        {
          "id": "cccccccc-dddd-4eee-8fff-000000000000",
          "session_id": "\(sessionID.rawValue)",
          "event_id": null,
          "kind": "prompt_context",
          "title": "Prompt Context",
          "mime_type": "text/markdown",
          "path": null,
          "content_text": "# Prompt Context",
          "metadata": {
            "guidance_documents": [],
            "selected_skill_card_count": 1.5,
            "selected_skill_document_count": 0,
            "selected_agent_profile_card_count": 0,
            "truncated": false,
            "project_context_considered": true,
            "runtime_context": {
              "model_alias": "claude-sonnet-latest",
              "runner_id": "local-runner",
              "workspace_dir": "/workspaces/signalbox",
              "project_workspace_dir": "/workspaces/mono",
              "linked_workspace_path": null,
              "enabled_tool_names": []
            }
          },
          "created_at": "2026-05-10T12:04:00Z"
        }
        """
        let artifact = try SignalboxJSONCoding.decoder().decode(
            SignalboxArtifact.self,
            from: Data(rawArtifact.utf8)
        )

        XCTAssertTrue(artifact.isPromptContextArtifact)
        XCTAssertNil(artifact.promptContextSummary)
    }

    private func waitForStreamInvocationCount(
        _ expectedCount: Int,
        viewModel: SessionDetailViewModel,
        service: FinishingStreamSignalboxService,
        file: StaticString = #filePath,
        line: UInt = #line
    ) async {
        for _ in 0..<50 where service.streamInvocationCount < expectedCount || viewModel.isStreaming {
            try? await Task.sleep(nanoseconds: 10_000_000)
        }
        XCTAssertEqual(service.streamInvocationCount, expectedCount, file: file, line: line)
        XCTAssertFalse(viewModel.isStreaming, file: file, line: line)
    }

    private func waitForControlledStreamInvocationCount(
        _ expectedCount: Int,
        viewModel: SessionDetailViewModel,
        service: ControlledStreamSignalboxService,
        file: StaticString = #filePath,
        line: UInt = #line
    ) async {
        for _ in 0..<50 where service.streamInvocationCount < expectedCount {
            try? await Task.sleep(nanoseconds: 10_000_000)
        }
        XCTAssertEqual(service.streamInvocationCount, expectedCount, file: file, line: line)
        XCTAssertTrue(viewModel.isStreaming, file: file, line: line)
    }

    private func waitForControlledStreamToStop(
        _ viewModel: SessionDetailViewModel,
        file: StaticString = #filePath,
        line: UInt = #line
    ) async {
        for _ in 0..<50 where viewModel.isStreaming {
            try? await Task.sleep(nanoseconds: 10_000_000)
        }
        XCTAssertFalse(viewModel.isStreaming, file: file, line: line)
    }

    private func waitForArtifactCount(
        _ expectedCount: Int,
        viewModel: SessionDetailViewModel,
        file: StaticString = #filePath,
        line: UInt = #line
    ) async {
        for _ in 0..<50 where viewModel.artifacts.count != expectedCount {
            try? await Task.sleep(nanoseconds: 10_000_000)
        }
        XCTAssertEqual(viewModel.artifacts.count, expectedCount, file: file, line: line)
    }

    private func makePromptContextArtifact(
        id: String,
        sessionID: SignalboxSessionID,
        eventID: Int?,
        modelAlias: String = "claude-sonnet-latest",
        runnerID: String = "local-runner",
        linkedWorkspacePath: String? = "projects/signalbox",
        enabledTools: [String] = ["read_file", "bash"],
        createdAt: String = "2026-05-10T12:04:00Z",
        workspaceDir: String? = "/workspaces/signalbox",
        projectWorkspaceDir: String? = "/workspaces/mono",
        metadata: [String: Any]? = nil
    ) throws -> SignalboxArtifact {
        let linkedWorkspacePathValue: Any = linkedWorkspacePath.map { $0 as Any } ?? NSNull()
        let eventIDValue: Any = eventID.map { $0 as Any } ?? NSNull()
        let workspaceDirValue: Any = workspaceDir.map { $0 as Any } ?? NSNull()
        let projectWorkspaceDirValue: Any = projectWorkspaceDir.map { $0 as Any } ?? NSNull()
        let artifactMetadata: [String: Any] = metadata ?? [
            "guidance_documents": [
                ["path": "AGENTS.md", "kind": "agents"],
                ["path": ".agents/style-guide.md", "kind": "style_guide"],
            ],
            "selected_skill_card_count": 3,
            "selected_skill_document_count": 1,
            "selected_agent_profile_card_count": 1,
            "truncated": true,
            "project_context_considered": true,
            "runtime_context": [
                "model_alias": modelAlias,
                "runner_id": runnerID,
                "workspace_dir": workspaceDirValue,
                "project_workspace_dir": projectWorkspaceDirValue,
                "linked_workspace_path": linkedWorkspacePathValue,
                "enabled_tool_names": enabledTools,
            ],
        ]
        let payload: [String: Any] = [
            "id": id,
            "session_id": sessionID.rawValue,
            "event_id": eventIDValue,
            "kind": "prompt_context",
            "title": "Prompt Context",
            "mime_type": "text/markdown",
            "path": NSNull(),
            "content_text": "# Prompt Context\n\nLoaded project context.",
            "metadata": artifactMetadata,
            "created_at": createdAt,
        ]
        let data = try JSONSerialization.data(withJSONObject: payload)
        return try SignalboxJSONCoding.decoder().decode(SignalboxArtifact.self, from: data)
    }
}

private actor RecordingCreateSessionSignalboxService: SignalboxClientProtocol {
    private(set) var lastCreateRequest: SignalboxCreateSessionRequest?

    func testConnection() async throws {}
    func listTemplates() async throws -> [SignalboxTemplate] { [] }
    func listRunners() async throws -> [SignalboxRunner] { [] }
    func listSessions(archived: Bool) async throws -> [SignalboxSessionMetadata] { [] }
    func createSession(request: SignalboxCreateSessionRequest) async throws -> SignalboxSessionView {
        lastCreateRequest = request
        return try SignalboxJSONCoding.decoder().decode(
            SignalboxSessionView.self,
            from: Data(MockSignalboxFixtures.createdSession.utf8)
        )
    }
    func patchSessionArchive(sessionID: SignalboxSessionID, isArchived: Bool) async throws -> SignalboxSessionMetadata {
        throw SignalboxClientError.requestFailed("not implemented")
    }
    func listEvents(sessionID: SignalboxSessionID) async throws -> [SignalboxStoredEvent] { [] }
    func appendUserMessage(sessionID: SignalboxSessionID, text: String) async throws -> SignalboxAppendUserMessageResponse {
        throw SignalboxClientError.requestFailed("not implemented")
    }
    func confirmInvocation(sessionID: SignalboxSessionID, invocationID: SignalboxToolInvocationID) async throws {}
    func denyInvocation(sessionID: SignalboxSessionID, invocationID: SignalboxToolInvocationID, reason: String?) async throws {}
    func listArtifacts(sessionID: SignalboxSessionID) async throws -> [SignalboxArtifact] { [] }
    func listMonitorSessions() async throws -> [SignalboxMonitorSessionSummary] { [] }

    nonisolated func streamMessages(sessionID: SignalboxSessionID) -> AsyncThrowingStream<SignalboxServerMessage, Error> {
        AsyncThrowingStream { continuation in
            continuation.finish()
        }
    }
}

private final class FinishingStreamSignalboxService: SignalboxClientProtocol, @unchecked Sendable {
    private let lock = NSLock()
    private var lockedStreamInvocationCount = 0

    var streamInvocationCount: Int {
        lock.lock()
        defer { lock.unlock() }
        return lockedStreamInvocationCount
    }

    func testConnection() async throws {}
    func listTemplates() async throws -> [SignalboxTemplate] { [] }
    func listRunners() async throws -> [SignalboxRunner] { [] }
    func listSessions(archived: Bool) async throws -> [SignalboxSessionMetadata] { [] }
    func createSession(request: SignalboxCreateSessionRequest) async throws -> SignalboxSessionView {
        throw SignalboxClientError.requestFailed("not implemented")
    }
    func patchSessionArchive(sessionID: SignalboxSessionID, isArchived: Bool) async throws -> SignalboxSessionMetadata {
        throw SignalboxClientError.requestFailed("not implemented")
    }
    func listEvents(sessionID: SignalboxSessionID) async throws -> [SignalboxStoredEvent] { [] }
    func appendUserMessage(sessionID: SignalboxSessionID, text: String) async throws -> SignalboxAppendUserMessageResponse {
        throw SignalboxClientError.requestFailed("not implemented")
    }
    func confirmInvocation(sessionID: SignalboxSessionID, invocationID: SignalboxToolInvocationID) async throws {}
    func denyInvocation(sessionID: SignalboxSessionID, invocationID: SignalboxToolInvocationID, reason: String?) async throws {}
    func listArtifacts(sessionID: SignalboxSessionID) async throws -> [SignalboxArtifact] { [] }
    func listMonitorSessions() async throws -> [SignalboxMonitorSessionSummary] { [] }

    func streamMessages(sessionID: SignalboxSessionID) -> AsyncThrowingStream<SignalboxServerMessage, Error> {
        lock.lock()
        lockedStreamInvocationCount += 1
        lock.unlock()
        return AsyncThrowingStream { continuation in
            continuation.finish()
        }
    }
}

private final class ControlledStreamSignalboxService: SignalboxClientProtocol, @unchecked Sendable {
    private let lock = NSLock()
    private var lockedStreamContinuations: [AsyncThrowingStream<SignalboxServerMessage, Error>.Continuation] = []

    var streamInvocationCount: Int {
        lock.lock()
        defer { lock.unlock() }
        return lockedStreamContinuations.count
    }

    func finishStream(at index: Int) {
        lock.lock()
        let continuation = lockedStreamContinuations[index]
        lock.unlock()
        continuation.finish()
    }

    func testConnection() async throws {}
    func listTemplates() async throws -> [SignalboxTemplate] { [] }
    func listRunners() async throws -> [SignalboxRunner] { [] }
    func listSessions(archived: Bool) async throws -> [SignalboxSessionMetadata] { [] }
    func createSession(request: SignalboxCreateSessionRequest) async throws -> SignalboxSessionView {
        throw SignalboxClientError.requestFailed("not implemented")
    }
    func patchSessionArchive(sessionID: SignalboxSessionID, isArchived: Bool) async throws -> SignalboxSessionMetadata {
        throw SignalboxClientError.requestFailed("not implemented")
    }
    func listEvents(sessionID: SignalboxSessionID) async throws -> [SignalboxStoredEvent] { [] }
    func appendUserMessage(sessionID: SignalboxSessionID, text: String) async throws -> SignalboxAppendUserMessageResponse {
        throw SignalboxClientError.requestFailed("not implemented")
    }
    func confirmInvocation(sessionID: SignalboxSessionID, invocationID: SignalboxToolInvocationID) async throws {}
    func denyInvocation(sessionID: SignalboxSessionID, invocationID: SignalboxToolInvocationID, reason: String?) async throws {}
    func listArtifacts(sessionID: SignalboxSessionID) async throws -> [SignalboxArtifact] { [] }
    func listMonitorSessions() async throws -> [SignalboxMonitorSessionSummary] { [] }

    func streamMessages(sessionID: SignalboxSessionID) -> AsyncThrowingStream<SignalboxServerMessage, Error> {
        AsyncThrowingStream { continuation in
            lock.lock()
            lockedStreamContinuations.append(continuation)
            lock.unlock()
        }
    }
}

private final class StaticArtifactSignalboxService: SignalboxClientProtocol, @unchecked Sendable {
    private let artifacts: [SignalboxArtifact]

    init(artifacts: [SignalboxArtifact]) {
        self.artifacts = artifacts
    }

    func testConnection() async throws {}
    func listTemplates() async throws -> [SignalboxTemplate] { [] }
    func listRunners() async throws -> [SignalboxRunner] { [] }
    func listSessions(archived: Bool) async throws -> [SignalboxSessionMetadata] { [] }
    func createSession(request: SignalboxCreateSessionRequest) async throws -> SignalboxSessionView {
        throw SignalboxClientError.requestFailed("not implemented")
    }
    func patchSessionArchive(sessionID: SignalboxSessionID, isArchived: Bool) async throws -> SignalboxSessionMetadata {
        throw SignalboxClientError.requestFailed("not implemented")
    }
    func listEvents(sessionID: SignalboxSessionID) async throws -> [SignalboxStoredEvent] { [] }
    func appendUserMessage(sessionID: SignalboxSessionID, text: String) async throws -> SignalboxAppendUserMessageResponse {
        throw SignalboxClientError.requestFailed("not implemented")
    }
    func confirmInvocation(sessionID: SignalboxSessionID, invocationID: SignalboxToolInvocationID) async throws {}
    func denyInvocation(sessionID: SignalboxSessionID, invocationID: SignalboxToolInvocationID, reason: String?) async throws {}
    func listArtifacts(sessionID: SignalboxSessionID) async throws -> [SignalboxArtifact] { artifacts }
    func listMonitorSessions() async throws -> [SignalboxMonitorSessionSummary] { [] }

    func streamMessages(sessionID: SignalboxSessionID) -> AsyncThrowingStream<SignalboxServerMessage, Error> {
        AsyncThrowingStream { continuation in
            continuation.finish()
        }
    }
}

private final class ArtifactRefreshStreamSignalboxService: SignalboxClientProtocol, @unchecked Sendable {
    private let session: SignalboxSessionMetadata
    private let artifactAfterStreamHello: SignalboxArtifact
    private let lock = NSLock()
    private var lockedArtifactListCallCount = 0

    init(session: SignalboxSessionMetadata, artifactAfterStreamHello: SignalboxArtifact) {
        self.session = session
        self.artifactAfterStreamHello = artifactAfterStreamHello
    }

    func testConnection() async throws {}
    func listTemplates() async throws -> [SignalboxTemplate] { [] }
    func listRunners() async throws -> [SignalboxRunner] { [] }
    func listSessions(archived: Bool) async throws -> [SignalboxSessionMetadata] { [] }
    func createSession(request: SignalboxCreateSessionRequest) async throws -> SignalboxSessionView {
        throw SignalboxClientError.requestFailed("not implemented")
    }
    func patchSessionArchive(sessionID: SignalboxSessionID, isArchived: Bool) async throws -> SignalboxSessionMetadata {
        throw SignalboxClientError.requestFailed("not implemented")
    }
    func listEvents(sessionID: SignalboxSessionID) async throws -> [SignalboxStoredEvent] { [] }
    func appendUserMessage(sessionID: SignalboxSessionID, text: String) async throws -> SignalboxAppendUserMessageResponse {
        throw SignalboxClientError.requestFailed("not implemented")
    }
    func confirmInvocation(sessionID: SignalboxSessionID, invocationID: SignalboxToolInvocationID) async throws {}
    func denyInvocation(sessionID: SignalboxSessionID, invocationID: SignalboxToolInvocationID, reason: String?) async throws {}
    func listMonitorSessions() async throws -> [SignalboxMonitorSessionSummary] { [] }

    func listArtifacts(sessionID: SignalboxSessionID) async throws -> [SignalboxArtifact] {
        lock.lock()
        lockedArtifactListCallCount += 1
        let callCount = lockedArtifactListCallCount
        lock.unlock()
        return callCount == 1 ? [] : [artifactAfterStreamHello]
    }

    func streamMessages(sessionID: SignalboxSessionID) -> AsyncThrowingStream<SignalboxServerMessage, Error> {
        AsyncThrowingStream { continuation in
            Task {
                do {
                    continuation.yield(try Self.streamHelloMessage(session: session))
                    continuation.finish()
                } catch {
                    continuation.finish(throwing: error)
                }
            }
        }
    }

    private static func streamHelloMessage(session: SignalboxSessionMetadata) throws -> SignalboxServerMessage {
        let encoder = SignalboxJSONCoding.encoder()
        let sessionJSON = String(data: try encoder.encode(session), encoding: .utf8)!
        let statusJSON = String(data: try encoder.encode(SignalboxSessionStatus(state: .idle)), encoding: .utf8)!
        let rawMessage = """
        {"kind":"stream_hello","session":\(sessionJSON),"status":\(statusJSON),"events":[]}
        """
        return try SignalboxJSONCoding.decoder().decode(SignalboxServerMessage.self, from: Data(rawMessage.utf8))
    }
}
