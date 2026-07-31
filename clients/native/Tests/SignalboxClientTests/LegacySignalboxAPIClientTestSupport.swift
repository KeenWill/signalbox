// Test-only retention of the retired REST client. Production composition and
// product targets expose no HTTP transport.
import Foundation
@testable import SignalboxNative
#if canImport(SignalboxModels)
import SignalboxModels
#endif

public struct SignalboxClientConfiguration: Equatable, Sendable {
    public let baseURL: URL
    public let apiKey: String

    public init(baseURL: URL, apiKey: String) throws {
        guard ["http", "https"].contains(baseURL.scheme?.lowercased()) else {
            throw SignalboxClientError.invalidConfiguration("Server URL must start with http:// or https://")
        }
        guard baseURL.host?.isEmpty == false else {
            throw SignalboxClientError.invalidConfiguration("Server URL must include a host")
        }
        guard !apiKey.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            throw SignalboxClientError.invalidConfiguration("API key is required")
        }
        self.baseURL = baseURL
        self.apiKey = apiKey
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
        case .notFound(let message):
            return message
        case .conflict(let message):
            return message
        case .serviceUnavailable(let message):
            return message
        case .requestFailed(let message):
            return message
        case .decodingFailed(let message):
            return message
        }
    }
}

public protocol SignalboxHTTPTransport: Sendable {
    func response(for request: URLRequest) async throws -> (Data, HTTPURLResponse)
}

public struct URLSessionSignalboxHTTPTransport: SignalboxHTTPTransport {
    private let session: URLSession

    public init(session: URLSession = .shared) {
        self.session = session
    }

    public func response(for request: URLRequest) async throws -> (Data, HTTPURLResponse) {
        let (data, response) = try await session.data(for: request)
        guard let httpResponse = response as? HTTPURLResponse else {
            throw SignalboxClientError.invalidResponse
        }
        return (data, httpResponse)
    }
}

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

public final class SignalboxAPIClient: SignalboxClientProtocol, Sendable {
    private static let collectionPageLimit = 500
    private static let eventPageLimit = 500

    private let configuration: SignalboxClientConfiguration
    private let transport: SignalboxHTTPTransport

    public init(
        configuration: SignalboxClientConfiguration,
        transport: SignalboxHTTPTransport = URLSessionSignalboxHTTPTransport()
    ) {
        self.configuration = configuration
        self.transport = transport
    }

    public func testConnection() async throws {
        let _: SignalboxTemplateListResponse = try await request(method: "GET", path: "/api/v1/templates")
    }

    public func listTemplates() async throws -> [SignalboxTemplate] {
        let response: SignalboxTemplateListResponse = try await request(method: "GET", path: "/api/v1/templates")
        return response.templates
    }

    public func listRunners() async throws -> [SignalboxRunner] {
        let response: SignalboxRunnerListResponse = try await request(method: "GET", path: "/api/v1/runners")
        return response.runners
    }

    public func listSessions(archived: Bool) async throws -> [SignalboxSessionMetadata] {
        var sessions: [SignalboxSessionMetadata] = []
        var offset = 0
        repeat {
            let response: SignalboxSessionListResponse = try await request(
                method: "GET",
                path: "/api/v1/sessions",
                queryItems: [
                    URLQueryItem(name: "archived", value: archived ? "true" : "false"),
                    URLQueryItem(name: "limit", value: "\(Self.collectionPageLimit)"),
                    URLQueryItem(name: "offset", value: "\(offset)"),
                    URLQueryItem(name: "include_total", value: "true")
                ]
            )
            sessions.append(contentsOf: response.sessions)
            guard shouldFetchNextOffsetPage(
                loadedCount: sessions.count,
                pageCount: response.sessions.count,
                responseLimit: response.limit,
                total: response.total
            ) else {
                break
            }
            offset = response.offset + response.limit
        } while true
        return sessions
    }

    public func createSession(request: SignalboxCreateSessionRequest) async throws -> SignalboxSessionView {
        try await self.request(method: "POST", path: "/api/v1/sessions", body: request)
    }

    public func patchSessionArchive(sessionID: SignalboxSessionID, isArchived: Bool) async throws -> SignalboxSessionMetadata {
        try await request(
            method: "PATCH",
            path: "/api/v1/sessions/\(sessionID.rawValue)",
            body: SignalboxPatchSessionArchiveRequest(isArchived: isArchived)
        )
    }

    public func listEvents(sessionID: SignalboxSessionID) async throws -> [SignalboxStoredEvent] {
        var events: [SignalboxStoredEvent] = []
        var after: SignalboxEventID?
        repeat {
            var queryItems = [URLQueryItem(name: "limit", value: "\(Self.eventPageLimit)")]
            if let after {
                queryItems.append(URLQueryItem(name: "after", value: "\(after.rawValue)"))
            }
            let response: SignalboxEventPage = try await request(
                method: "GET",
                path: "/api/v1/sessions/\(sessionID.rawValue)/events",
                queryItems: queryItems
            )
            events.append(contentsOf: response.events)
            guard !response.events.isEmpty, let nextAfter = response.nextAfter, nextAfter != after else {
                break
            }
            after = nextAfter
        } while true
        return events
    }

    public func appendUserMessage(
        sessionID: SignalboxSessionID,
        text: String
    ) async throws -> SignalboxAppendUserMessageResponse {
        try await request(
            method: "POST",
            path: "/api/v1/sessions/\(sessionID.rawValue)/messages",
            body: SignalboxAppendUserMessageRequest(text: text, createdFrom: "app:apple-native")
        )
    }

    public func confirmInvocation(sessionID: SignalboxSessionID, invocationID: SignalboxToolInvocationID) async throws {
        try await postConfirmation(
            sessionID: sessionID,
            invocationID: invocationID,
            decision: "approved",
            reason: nil
        )
    }

    public func denyInvocation(
        sessionID: SignalboxSessionID,
        invocationID: SignalboxToolInvocationID,
        reason: String?
    ) async throws {
        try await postConfirmation(
            sessionID: sessionID,
            invocationID: invocationID,
            decision: "denied",
            reason: reason
        )
    }

    public func listArtifacts(sessionID: SignalboxSessionID) async throws -> [SignalboxArtifact] {
        try await listArtifacts(sessionID: sessionID, kind: nil)
    }

    public func listArtifacts(sessionID: SignalboxSessionID, kind: String?) async throws -> [SignalboxArtifact] {
        var artifacts: [SignalboxArtifact] = []
        var offset = 0
        repeat {
            var queryItems = [
                URLQueryItem(name: "limit", value: "\(Self.collectionPageLimit)"),
                URLQueryItem(name: "offset", value: "\(offset)"),
                URLQueryItem(name: "include_total", value: "true")
            ]
            if let kind {
                queryItems.append(URLQueryItem(name: "kind", value: kind))
            }
            let response: SignalboxArtifactListResponse = try await request(
                method: "GET",
                path: "/api/v1/sessions/\(sessionID.rawValue)/artifacts",
                queryItems: queryItems
            )
            artifacts.append(contentsOf: response.artifacts)
            guard shouldFetchNextOffsetPage(
                loadedCount: artifacts.count,
                pageCount: response.artifacts.count,
                responseLimit: response.limit,
                total: response.total
            ) else {
                break
            }
            offset = response.offset + response.limit
        } while true
        return artifacts
    }

    public func getArtifact(sessionID: SignalboxSessionID, artifactID: SignalboxArtifactID) async throws -> SignalboxArtifact {
        try await request(
            method: "GET",
            path: "/api/v1/sessions/\(sessionID.rawValue)/artifacts/\(artifactID.rawValue)"
        )
    }

    public func listMonitorSessions() async throws -> [SignalboxMonitorSessionSummary] {
        var sessions: [SignalboxMonitorSessionSummary] = []
        var offset = 0
        repeat {
            let response: SignalboxMonitorSessionListResponse = try await request(
                method: "GET",
                path: "/api/v1/monitor/sessions",
                queryItems: [
                    URLQueryItem(name: "limit", value: "\(Self.collectionPageLimit)"),
                    URLQueryItem(name: "offset", value: "\(offset)"),
                    URLQueryItem(name: "include_total", value: "true")
                ]
            )
            sessions.append(contentsOf: response.sessions)
            guard shouldFetchNextOffsetPage(
                loadedCount: sessions.count,
                pageCount: response.sessions.count,
                responseLimit: response.limit,
                total: response.total
            ) else {
                break
            }
            offset = response.offset + response.limit
        } while true
        return sessions
    }

    public func streamMessages(sessionID: SignalboxSessionID) -> AsyncThrowingStream<SignalboxServerMessage, Error> {
        _ = sessionID
        return AsyncThrowingStream { continuation in
            continuation.finish(
                throwing: SignalboxClientError.requestFailed(
                    "The retired REST compatibility client has no product WebSocket transport."
                )
            )
        }
    }

    private func postConfirmation(
        sessionID: SignalboxSessionID,
        invocationID: SignalboxToolInvocationID,
        decision: String,
        reason: String?
    ) async throws {
        let body = SignalboxConfirmationRequest(decision: decision, reason: reason)
        let _: SignalboxForwardedResponse = try await request(
            method: "POST",
            path: "/api/v1/sessions/\(sessionID.rawValue)/invocations/\(invocationID.rawValue)/confirm",
            body: body
        )
    }

    private func request<Response: Decodable>(
        method: String,
        path: String,
        queryItems: [URLQueryItem] = []
    ) async throws -> Response {
        let request = try makeRequest(method: method, path: path, queryItems: queryItems, bodyData: nil)
        return try await decodeResponse(request)
    }

    private func request<Body: Encodable, Response: Decodable>(
        method: String,
        path: String,
        queryItems: [URLQueryItem] = [],
        body: Body
    ) async throws -> Response {
        let bodyData = try SignalboxJSONCoding.encoder().encode(body)
        let request = try makeRequest(method: method, path: path, queryItems: queryItems, bodyData: bodyData)
        return try await decodeResponse(request)
    }

    private func decodeResponse<Response: Decodable>(_ request: URLRequest) async throws -> Response {
        do {
            let (data, response) = try await transport.response(for: request)
            try validate(response: response, data: data)
            do {
                return try SignalboxJSONCoding.decoder().decode(Response.self, from: data)
            } catch {
                throw SignalboxClientError.decodingFailed("Could not decode server response: \(error.localizedDescription)")
            }
        } catch let error as SignalboxClientError {
            throw error
        } catch {
            throw SignalboxClientError.requestFailed("Server request failed: \(error.localizedDescription)")
        }
    }

    private func makeRequest(
        method: String,
        path: String,
        queryItems: [URLQueryItem],
        bodyData: Data?
    ) throws -> URLRequest {
        var components = URLComponents(
            url: configuration.baseURL.appendingPathComponent(path.trimmingCharacters(in: CharacterSet(charactersIn: "/"))),
            resolvingAgainstBaseURL: false
        )
        components?.queryItems = queryItems.isEmpty ? nil : queryItems
        guard let url = components?.url else {
            throw SignalboxClientError.invalidConfiguration("Server URL could not be combined with \(path)")
        }
        var request = URLRequest(url: url)
        request.httpMethod = method
        request.setValue("Bearer \(configuration.apiKey)", forHTTPHeaderField: "Authorization")
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        if let bodyData {
            request.httpBody = bodyData
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        }
        return request
    }

    private func validate(response: HTTPURLResponse, data: Data) throws {
        switch response.statusCode {
        case 200..<300:
            return
        case 401:
            throw SignalboxClientError.unauthorized
        case 404:
            throw SignalboxClientError.notFound(detail(from: data))
        case 409:
            throw SignalboxClientError.conflict(detail(from: data))
        case 503:
            throw SignalboxClientError.serviceUnavailable(detail(from: data))
        default:
            throw SignalboxClientError.requestFailed("Server returned HTTP \(response.statusCode): \(detail(from: data))")
        }
    }

    private func detail(from data: Data) -> String {
        if
            let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
            let detail = object["detail"] as? String
        {
            return detail
        }
        return String(data: data, encoding: .utf8) ?? "No response body"
    }

    private func webSocketURL(path: String) -> URL {
        var components = URLComponents(url: configuration.baseURL, resolvingAgainstBaseURL: false)!
        components.scheme = configuration.baseURL.scheme?.lowercased() == "https" ? "wss" : "ws"
        components.path = Self.combinedPath(basePath: components.path, endpointPath: path)
        components.queryItems = [URLQueryItem(name: "token", value: configuration.apiKey)]
        return components.url!
    }

    private static func combinedPath(basePath: String, endpointPath: String) -> String {
        let trimmedBasePath = basePath.trimmingCharacters(in: CharacterSet(charactersIn: "/"))
        let trimmedEndpointPath = endpointPath.trimmingCharacters(in: CharacterSet(charactersIn: "/"))
        let path = [trimmedBasePath, trimmedEndpointPath]
            .filter { !$0.isEmpty }
            .joined(separator: "/")
        return "/\(path)"
    }

    private func shouldFetchNextOffsetPage(
        loadedCount: Int,
        pageCount: Int,
        responseLimit: Int,
        total: Int?
    ) -> Bool {
        guard pageCount > 0 else {
            return false
        }
        if let total {
            return loadedCount < total
        }
        return pageCount >= responseLimit
    }
}

private struct SignalboxPatchSessionArchiveRequest: Encodable {
    let isArchived: Bool

    private enum CodingKeys: String, CodingKey {
        case isArchived = "is_archived"
    }
}

private struct SignalboxAppendUserMessageRequest: Encodable {
    let text: String
    let createdFrom: String

    private enum CodingKeys: String, CodingKey {
        case text
        case createdFrom = "created_from"
    }
}

private struct SignalboxConfirmationRequest: Encodable {
    let decision: String
    let reason: String?
}

private struct SignalboxForwardedResponse: Decodable {
    let status: String
}
