import Foundation

public struct RecordedHubRequest: Equatable, Sendable {
    public let method: String
    public let path: String
    public let query: String?
    public let authorizationHeader: String?
    public let body: Data?

    public var pathAndQuery: String {
        guard let query, !query.isEmpty else {
            return path
        }
        return "\(path)?\(query)"
    }
}

public actor MockHubHTTPTransport: HubHTTPTransport {
    private var responses: [String: (Int, Data)] = [:]
    private var recordedRequests: [RecordedHubRequest] = []

    public init() {}

    public func setJSONResponse(path: String, statusCode: Int = 200, json: String) {
        responses[path] = (statusCode, Data(json.utf8))
    }

    public func requests() -> [RecordedHubRequest] {
        recordedRequests
    }

    public func response(for request: URLRequest) async throws -> (Data, HTTPURLResponse) {
        let path = request.url?.path ?? ""
        let query = request.url?.query
        recordedRequests.append(
            RecordedHubRequest(
                method: request.httpMethod ?? "GET",
                path: path,
                query: query,
                authorizationHeader: request.value(forHTTPHeaderField: "Authorization"),
                body: request.httpBody
            )
        )
        let pathAndQuery = RecordedHubRequest(
            method: request.httpMethod ?? "GET",
            path: path,
            query: query,
            authorizationHeader: nil,
            body: nil
        ).pathAndQuery
        let response = responses[pathAndQuery] ?? responses[path] ?? (404, Data(#"{"detail":"not found"}"#.utf8))
        let httpResponse = HTTPURLResponse(
            url: request.url!,
            statusCode: response.0,
            httpVersion: "HTTP/1.1",
            headerFields: ["Content-Type": "application/json"]
        )!
        return (response.1, httpResponse)
    }
}
