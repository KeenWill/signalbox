import Foundation
#if canImport(LLMHubModels)
import LLMHubModels
#endif

public struct HubClientConfiguration: Equatable, Sendable {
    public let baseURL: URL
    public let apiKey: String

    public init(baseURL: URL, apiKey: String) throws {
        guard ["http", "https"].contains(baseURL.scheme?.lowercased()) else {
            throw HubClientError.invalidConfiguration("Hub URL must start with http:// or https://")
        }
        guard baseURL.host?.isEmpty == false else {
            throw HubClientError.invalidConfiguration("Hub URL must include a host")
        }
        guard !apiKey.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            throw HubClientError.invalidConfiguration("API key is required")
        }
        self.baseURL = baseURL
        self.apiKey = apiKey
    }
}

public enum HubClientError: Error, Equatable, LocalizedError {
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
            return "The hub returned an invalid response."
        case .unauthorized:
            return "The hub rejected the API key."
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

public protocol HubHTTPTransport: Sendable {
    func response(for request: URLRequest) async throws -> (Data, HTTPURLResponse)
}

public struct URLSessionHubHTTPTransport: HubHTTPTransport {
    private let session: URLSession

    public init(session: URLSession = .shared) {
        self.session = session
    }

    public func response(for request: URLRequest) async throws -> (Data, HTTPURLResponse) {
        let (data, response) = try await session.data(for: request)
        guard let httpResponse = response as? HTTPURLResponse else {
            throw HubClientError.invalidResponse
        }
        return (data, httpResponse)
    }
}

public protocol HubClientProtocol: Sendable {
    func testConnection() async throws
    func listTemplates() async throws -> [HubTemplate]
    func listRunners() async throws -> [HubRunner]
    func listSessions(archived: Bool) async throws -> [HubSessionMetadata]
    func createSession(request: HubCreateSessionRequest) async throws -> HubSessionView
    func patchSessionArchive(sessionID: HubSessionID, isArchived: Bool) async throws -> HubSessionMetadata
    func listEvents(sessionID: HubSessionID) async throws -> [HubStoredEvent]
    func appendUserMessage(sessionID: HubSessionID, text: String) async throws -> HubAppendUserMessageResponse
    func confirmInvocation(sessionID: HubSessionID, invocationID: HubToolInvocationID) async throws
    func denyInvocation(sessionID: HubSessionID, invocationID: HubToolInvocationID, reason: String?) async throws
    func listArtifacts(sessionID: HubSessionID) async throws -> [HubArtifact]
    func listArtifacts(sessionID: HubSessionID, kind: String?) async throws -> [HubArtifact]
    func getArtifact(sessionID: HubSessionID, artifactID: HubArtifactID) async throws -> HubArtifact
    func listMonitorSessions() async throws -> [HubMonitorSessionSummary]
    func streamMessages(sessionID: HubSessionID) -> AsyncThrowingStream<HubServerMessage, Error>
}

public extension HubClientProtocol {
    func listArtifacts(sessionID: HubSessionID, kind: String?) async throws -> [HubArtifact] {
        let artifacts = try await listArtifacts(sessionID: sessionID)
        guard let kind else {
            return artifacts
        }
        return artifacts.filter { $0.kind == kind }
    }

    func getArtifact(sessionID: HubSessionID, artifactID: HubArtifactID) async throws -> HubArtifact {
        let artifacts = try await listArtifacts(sessionID: sessionID)
        guard let artifact = artifacts.first(where: { $0.id == artifactID }) else {
            throw HubClientError.notFound("artifact not found")
        }
        return artifact
    }
}

public struct HubCreateSessionRequest: Encodable, Equatable, Sendable {
    public let templateID: HubTemplateID?
    public let systemPrompt: String?
    public let title: String?
    public let modelAlias: String?
    public let enabledTools: [String]?
    public let runnerID: HubRunnerID?
    public let guidanceTargetPath: String?
    public let linkedWorkspacePath: String?
    public let createdFrom: String
    public let sourceApp: String?

    public init(
        templateID: HubTemplateID?,
        systemPrompt: String?,
        title: String?,
        modelAlias: String?,
        enabledTools: [String]?,
        runnerID: HubRunnerID?,
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

public final class HubClient: HubClientProtocol, Sendable {
    private static let collectionPageLimit = 500
    private static let eventPageLimit = 500

    private let configuration: HubClientConfiguration
    private let transport: HubHTTPTransport

    public init(
        configuration: HubClientConfiguration,
        transport: HubHTTPTransport = URLSessionHubHTTPTransport()
    ) {
        self.configuration = configuration
        self.transport = transport
    }

    public func testConnection() async throws {
        let _: HubTemplateListResponse = try await request(method: "GET", path: "/api/v1/templates")
    }

    public func listTemplates() async throws -> [HubTemplate] {
        let response: HubTemplateListResponse = try await request(method: "GET", path: "/api/v1/templates")
        return response.templates
    }

    public func listRunners() async throws -> [HubRunner] {
        let response: HubRunnerListResponse = try await request(method: "GET", path: "/api/v1/runners")
        return response.runners
    }

    public func listSessions(archived: Bool) async throws -> [HubSessionMetadata] {
        var sessions: [HubSessionMetadata] = []
        var offset = 0
        repeat {
            let response: HubSessionListResponse = try await request(
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

    public func createSession(request: HubCreateSessionRequest) async throws -> HubSessionView {
        try await self.request(method: "POST", path: "/api/v1/sessions", body: request)
    }

    public func patchSessionArchive(sessionID: HubSessionID, isArchived: Bool) async throws -> HubSessionMetadata {
        try await request(
            method: "PATCH",
            path: "/api/v1/sessions/\(sessionID.rawValue)",
            body: HubPatchSessionArchiveRequest(isArchived: isArchived)
        )
    }

    public func listEvents(sessionID: HubSessionID) async throws -> [HubStoredEvent] {
        var events: [HubStoredEvent] = []
        var after: HubEventID?
        repeat {
            var queryItems = [URLQueryItem(name: "limit", value: "\(Self.eventPageLimit)")]
            if let after {
                queryItems.append(URLQueryItem(name: "after", value: "\(after.rawValue)"))
            }
            let response: HubEventPage = try await request(
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
        sessionID: HubSessionID,
        text: String
    ) async throws -> HubAppendUserMessageResponse {
        try await request(
            method: "POST",
            path: "/api/v1/sessions/\(sessionID.rawValue)/messages",
            body: HubAppendUserMessageRequest(text: text, createdFrom: "app:apple-native")
        )
    }

    public func confirmInvocation(sessionID: HubSessionID, invocationID: HubToolInvocationID) async throws {
        try await postConfirmation(
            sessionID: sessionID,
            invocationID: invocationID,
            decision: "approved",
            reason: nil
        )
    }

    public func denyInvocation(
        sessionID: HubSessionID,
        invocationID: HubToolInvocationID,
        reason: String?
    ) async throws {
        try await postConfirmation(
            sessionID: sessionID,
            invocationID: invocationID,
            decision: "denied",
            reason: reason
        )
    }

    public func listArtifacts(sessionID: HubSessionID) async throws -> [HubArtifact] {
        try await listArtifacts(sessionID: sessionID, kind: nil)
    }

    public func listArtifacts(sessionID: HubSessionID, kind: String?) async throws -> [HubArtifact] {
        var artifacts: [HubArtifact] = []
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
            let response: HubArtifactListResponse = try await request(
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

    public func getArtifact(sessionID: HubSessionID, artifactID: HubArtifactID) async throws -> HubArtifact {
        try await request(
            method: "GET",
            path: "/api/v1/sessions/\(sessionID.rawValue)/artifacts/\(artifactID.rawValue)"
        )
    }

    public func listMonitorSessions() async throws -> [HubMonitorSessionSummary] {
        var sessions: [HubMonitorSessionSummary] = []
        var offset = 0
        repeat {
            let response: HubMonitorSessionListResponse = try await request(
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

    public func streamMessages(sessionID: HubSessionID) -> AsyncThrowingStream<HubServerMessage, Error> {
        let stream = HubWebSocketStream(
            url: webSocketURL(path: "/api/v1/sessions/\(sessionID.rawValue)/stream")
        )
        return stream.messages()
    }

    private func postConfirmation(
        sessionID: HubSessionID,
        invocationID: HubToolInvocationID,
        decision: String,
        reason: String?
    ) async throws {
        let body = HubConfirmationRequest(decision: decision, reason: reason)
        let _: HubForwardedResponse = try await request(
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
        let bodyData = try HubJSONCoding.encoder().encode(body)
        let request = try makeRequest(method: method, path: path, queryItems: queryItems, bodyData: bodyData)
        return try await decodeResponse(request)
    }

    private func decodeResponse<Response: Decodable>(_ request: URLRequest) async throws -> Response {
        do {
            let (data, response) = try await transport.response(for: request)
            try validate(response: response, data: data)
            do {
                return try HubJSONCoding.decoder().decode(Response.self, from: data)
            } catch {
                throw HubClientError.decodingFailed("Could not decode hub response: \(error.localizedDescription)")
            }
        } catch let error as HubClientError {
            throw error
        } catch {
            throw HubClientError.requestFailed("Hub request failed: \(error.localizedDescription)")
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
            throw HubClientError.invalidConfiguration("Hub URL could not be combined with \(path)")
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
            throw HubClientError.unauthorized
        case 404:
            throw HubClientError.notFound(detail(from: data))
        case 409:
            throw HubClientError.conflict(detail(from: data))
        case 503:
            throw HubClientError.serviceUnavailable(detail(from: data))
        default:
            throw HubClientError.requestFailed("Hub returned HTTP \(response.statusCode): \(detail(from: data))")
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
        components.scheme = configuration.baseURL.scheme == "https" ? "wss" : "ws"
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

private struct HubPatchSessionArchiveRequest: Encodable {
    let isArchived: Bool

    private enum CodingKeys: String, CodingKey {
        case isArchived = "is_archived"
    }
}

private struct HubAppendUserMessageRequest: Encodable {
    let text: String
    let createdFrom: String

    private enum CodingKeys: String, CodingKey {
        case text
        case createdFrom = "created_from"
    }
}

private struct HubConfirmationRequest: Encodable {
    let decision: String
    let reason: String?
}

private struct HubForwardedResponse: Decodable {
    let status: String
}
