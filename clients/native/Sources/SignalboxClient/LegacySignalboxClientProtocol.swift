#if canImport(SignalboxModels)
import SignalboxModels
#endif
import Foundation

/// The transport-free seam retained by presentation fixtures that predate the
/// process client. No production composition installs an implementation.
public protocol SignalboxClientProtocol: Sendable {
    func testConnection() async throws
    func listTemplates() async throws -> [SignalboxTemplate]
    func listRunners() async throws -> [SignalboxRunner]
    func listSessions(archived: Bool) async throws -> [SignalboxSessionMetadata]
    func createSession(request: SignalboxCreateSessionRequest) async throws -> SignalboxSessionView
    func patchSessionArchive(sessionID: SignalboxSessionID, isArchived: Bool) async throws -> SignalboxSessionMetadata
    func listEvents(sessionID: SignalboxSessionID) async throws -> [SignalboxStoredEvent]
    func appendUserMessage(sessionID: SignalboxSessionID, text: String) async throws -> SignalboxAppendUserMessageResponse
    func confirmInvocation(sessionID: SignalboxSessionID, invocationID: SignalboxToolInvocationID) async throws
    func denyInvocation(sessionID: SignalboxSessionID, invocationID: SignalboxToolInvocationID, reason: String?) async throws
    func listArtifacts(sessionID: SignalboxSessionID) async throws -> [SignalboxArtifact]
    func listArtifacts(sessionID: SignalboxSessionID, kind: String?) async throws -> [SignalboxArtifact]
    func getArtifact(sessionID: SignalboxSessionID, artifactID: SignalboxArtifactID) async throws -> SignalboxArtifact
    func listMonitorSessions() async throws -> [SignalboxMonitorSessionSummary]
    func streamMessages(sessionID: SignalboxSessionID) -> AsyncThrowingStream<SignalboxServerMessage, Error>
}

public extension SignalboxClientProtocol {
    func listArtifacts(sessionID: SignalboxSessionID, kind: String?) async throws -> [SignalboxArtifact] {
        let artifacts = try await listArtifacts(sessionID: sessionID)
        guard let kind else {
            return artifacts
        }
        return artifacts.filter { $0.kind == kind }
    }

    func getArtifact(sessionID: SignalboxSessionID, artifactID: SignalboxArtifactID) async throws -> SignalboxArtifact {
        let artifacts = try await listArtifacts(sessionID: sessionID)
        guard let artifact = artifacts.first(where: { $0.id == artifactID }) else {
            throw SignalboxClientError.notFound("artifact not found")
        }
        return artifact
    }
}

public enum SignalboxClientError: Error, Equatable, LocalizedError {
    case invalidConfiguration(String)
    case invalidResponse
    case unauthorized
    case notFound(String)
    case conflict(String)
    case serviceUnavailable(String)
    case requestFailed(String)
    case decodingFailed(String)

    public var errorDescription: String? {
        switch self {
        case .invalidConfiguration(let message):
            return message
        case .invalidResponse:
            return "The server returned an invalid response."
        case .unauthorized:
            return "The server rejected the API key."
        case .notFound(let message), .conflict(let message), .serviceUnavailable(let message),
             .requestFailed(let message), .decodingFailed(let message):
            return message
        }
    }
}

public struct SignalboxCreateSessionRequest: Encodable, Equatable, Sendable {
    public let templateID: SignalboxTemplateID?
    public let systemPrompt: String?
    public let title: String?
    public let modelAlias: String?
    public let enabledTools: [String]?
    public let runnerID: SignalboxRunnerID?
    public let guidanceTargetPath: String?
    public let linkedWorkspacePath: String?
    public let createdFrom: String
    public let sourceApp: String?

    public init(
        templateID: SignalboxTemplateID?,
        systemPrompt: String?,
        title: String?,
        modelAlias: String?,
        enabledTools: [String]?,
        runnerID: SignalboxRunnerID?,
        guidanceTargetPath: String? = nil,
        linkedWorkspacePath: String? = nil,
        createdFrom: String = "app:apple-native",
        sourceApp: String? = "apple-native"
    ) {
        self.templateID = templateID
        self.systemPrompt = systemPrompt
        self.title = title
        self.modelAlias = modelAlias
        self.enabledTools = enabledTools
        self.runnerID = runnerID
        self.guidanceTargetPath = guidanceTargetPath
        self.linkedWorkspacePath = linkedWorkspacePath
        self.createdFrom = createdFrom
        self.sourceApp = sourceApp
    }

    private enum CodingKeys: String, CodingKey {
        case templateID = "template_id"
        case systemPrompt = "system_prompt"
        case title
        case modelAlias = "model_alias"
        case enabledTools = "enabled_tools"
        case runnerID = "runner_id"
        case guidanceTargetPath = "guidance_target_path"
        case linkedWorkspacePath = "linked_workspace_path"
        case createdFrom = "created_from"
        case sourceApp = "source_app"
    }
}
