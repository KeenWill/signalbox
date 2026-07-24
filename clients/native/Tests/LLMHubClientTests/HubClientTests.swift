import LLMHubClient
import LLMHubModels
import XCTest

final class HubClientTests: XCTestCase {
    func testConfigurationRejectsInvalidURL() {
        XCTAssertThrowsError(
            try HubClientConfiguration(baseURL: URL(string: "ftp://localhost")!, apiKey: "secret")
        )
    }

    func testListTemplatesUsesBearerAuthorizationWithoutPrintingSecret() async throws {
        let transport = MockHubHTTPTransport()
        await transport.setJSONResponse(
            path: "/api/v1/templates",
            json: #"{"templates":[]}"#
        )
        let configuration = try HubClientConfiguration(
            baseURL: URL(string: "http://127.0.0.1:8000")!,
            apiKey: "secret-value"
        )
        let client = HubClient(configuration: configuration, transport: transport)

        let templates = try await client.listTemplates()

        XCTAssertEqual(templates, [])
        let request = await transport.requests().first
        XCTAssertEqual(request?.method, "GET")
        XCTAssertEqual(request?.path, "/api/v1/templates")
        XCTAssertEqual(request?.authorizationHeader, "Bearer secret-value")
    }

    func testCreateSessionEncodesLinkedWorkspacePath() async throws {
        let transport = MockHubHTTPTransport()
        await transport.setJSONResponse(
            path: "/api/v1/sessions",
            statusCode: 201,
            json: """
            {
              "metadata":\(Self.sessionJSON(index: 0)),
              "status":{"state":"idle"}
            }
            """
        )
        let configuration = try HubClientConfiguration(
            baseURL: URL(string: "http://127.0.0.1:8000")!,
            apiKey: "secret-value"
        )
        let client = HubClient(configuration: configuration, transport: transport)

        let created = try await client.createSession(
            request: HubCreateSessionRequest(
                templateID: HubTemplateID(rawValue: "coder"),
                systemPrompt: nil,
                title: "LLM Hub work",
                modelAlias: nil,
                enabledTools: nil,
                runnerID: HubRunnerID(rawValue: "runner-1"),
                linkedWorkspacePath: "projects/llm_hub"
            )
        )

        XCTAssertEqual(created.metadata.title, "Session 0")
        let requests = await transport.requests()
        let request = try XCTUnwrap(requests.first)
        XCTAssertEqual(request.method, "POST")
        XCTAssertEqual(request.path, "/api/v1/sessions")
        let bodyData = try XCTUnwrap(request.body)
        let body = try XCTUnwrap(
            JSONSerialization.jsonObject(with: bodyData) as? [String: Any]
        )
        XCTAssertEqual(body["template_id"] as? String, "coder")
        XCTAssertEqual(body["title"] as? String, "LLM Hub work")
        XCTAssertEqual(body["runner_id"] as? String, "runner-1")
        XCTAssertEqual(body["linked_workspace_path"] as? String, "projects/llm_hub")
        XCTAssertNil(body["system_prompt"])
    }

    func testUnauthorizedMapsToClientError() async throws {
        let transport = MockHubHTTPTransport()
        await transport.setJSONResponse(
            path: "/api/v1/templates",
            statusCode: 401,
            json: #"{"detail":"unauthorized"}"#
        )
        let configuration = try HubClientConfiguration(
            baseURL: URL(string: "http://127.0.0.1:8000")!,
            apiKey: "bad-key"
        )
        let client = HubClient(configuration: configuration, transport: transport)

        do {
            _ = try await client.listTemplates()
            XCTFail("Expected unauthorized")
        } catch let error as HubClientError {
            XCTAssertEqual(error, .unauthorized)
        }
    }

    func testListSessionsFollowsOffsetPagination() async throws {
        let transport = MockHubHTTPTransport()
        await transport.setJSONResponse(
            path: "/api/v1/sessions?archived=false&limit=500&offset=0&include_total=true",
            json: Self.sessionListJSON(
                sessions: (0..<500).map { Self.sessionJSON(index: $0) },
                limit: 500,
                offset: 0,
                total: 501
            )
        )
        await transport.setJSONResponse(
            path: "/api/v1/sessions?archived=false&limit=500&offset=500&include_total=true",
            json: Self.sessionListJSON(
                sessions: [Self.sessionJSON(index: 500)],
                limit: 500,
                offset: 500,
                total: 501
            )
        )
        let configuration = try HubClientConfiguration(
            baseURL: URL(string: "http://127.0.0.1:8000")!,
            apiKey: "secret-value"
        )
        let client = HubClient(configuration: configuration, transport: transport)

        let sessions = try await client.listSessions(archived: false)

        XCTAssertEqual(sessions.count, 501)
        XCTAssertEqual(sessions.first?.title, "Session 0")
        XCTAssertEqual(sessions.last?.title, "Session 500")
        let requests = await transport.requests().map(\.pathAndQuery)
        XCTAssertEqual(
            requests,
            [
                "/api/v1/sessions?archived=false&limit=500&offset=0&include_total=true",
                "/api/v1/sessions?archived=false&limit=500&offset=500&include_total=true"
            ]
        )
    }

    func testCreateSessionSendsAndDecodesGuidanceTargetPath() async throws {
        let transport = MockHubHTTPTransport()
        await transport.setJSONResponse(
            path: "/api/v1/sessions",
            json: """
            {
              "metadata": \(Self.sessionJSON(index: 0, guidanceTargetPath: "projects/demo")),
              "status": {"state": "idle"}
            }
            """
        )
        let configuration = try HubClientConfiguration(
            baseURL: URL(string: "http://127.0.0.1:8000")!,
            apiKey: "secret-value"
        )
        let client = HubClient(configuration: configuration, transport: transport)

        let session = try await client.createSession(
            request: HubCreateSessionRequest(
                templateID: HubTemplateID(rawValue: "coder"),
                systemPrompt: nil,
                title: "Native coder",
                modelAlias: nil,
                enabledTools: nil,
                runnerID: HubRunnerID(rawValue: "local-runner"),
                guidanceTargetPath: "projects/demo"
            )
        )

        XCTAssertEqual(session.metadata.guidanceTargetPath, "projects/demo")
        let requests = await transport.requests()
        let request = try XCTUnwrap(requests.first)
        XCTAssertEqual(request.method, "POST")
        XCTAssertEqual(request.path, "/api/v1/sessions")
        let body = try XCTUnwrap(request.body)
        let payload = try XCTUnwrap(JSONSerialization.jsonObject(with: body) as? [String: Any])
        XCTAssertEqual(payload["guidance_target_path"] as? String, "projects/demo")
    }

    func testListEventsFollowsNextAfterCursor() async throws {
        let transport = MockHubHTTPTransport()
        let sessionID = HubSessionID(rawValue: "11111111-1111-4111-8111-111111111111")
        await transport.setJSONResponse(
            path: "/api/v1/sessions/\(sessionID.rawValue)/events?limit=500",
            json: """
            {
              "events":[{"event_id":1,"event":{"kind":"future_event","field":"first"}}],
              "limit":500,
              "next_after":1
            }
            """
        )
        await transport.setJSONResponse(
            path: "/api/v1/sessions/\(sessionID.rawValue)/events?limit=500&after=1",
            json: """
            {
              "events":[{"event_id":2,"event":{"kind":"future_event","field":"second"}}],
              "limit":500,
              "next_after":null
            }
            """
        )
        let configuration = try HubClientConfiguration(
            baseURL: URL(string: "http://127.0.0.1:8000")!,
            apiKey: "secret-value"
        )
        let client = HubClient(configuration: configuration, transport: transport)

        let events = try await client.listEvents(sessionID: sessionID)

        XCTAssertEqual(events.map(\.eventID.rawValue), [1, 2])
        let requests = await transport.requests().map(\.pathAndQuery)
        XCTAssertEqual(
            requests,
            [
                "/api/v1/sessions/\(sessionID.rawValue)/events?limit=500",
                "/api/v1/sessions/\(sessionID.rawValue)/events?limit=500&after=1"
            ]
        )
    }


    func testListArtifactsCanFilterByKind() async throws {
        let transport = MockHubHTTPTransport()
        let sessionID = HubSessionID(rawValue: "11111111-1111-4111-8111-111111111111")
        await transport.setJSONResponse(
            path: "/api/v1/sessions/\(sessionID.rawValue)/artifacts?limit=500&offset=0&include_total=true&kind=prompt_context",
            json: Self.artifactListJSON(
                artifacts: [Self.artifactJSON(sessionID: sessionID, kind: "prompt_context")],
                limit: 500,
                offset: 0,
                total: 1
            )
        )
        let configuration = try HubClientConfiguration(
            baseURL: URL(string: "http://127.0.0.1:8000")!,
            apiKey: "secret-value"
        )
        let client = HubClient(configuration: configuration, transport: transport)

        let artifacts = try await client.listArtifacts(sessionID: sessionID, kind: "prompt_context")

        XCTAssertEqual(artifacts.map(\.kind), ["prompt_context"])
        let requests = await transport.requests().map(\.pathAndQuery)
        XCTAssertEqual(
            requests,
            [
                "/api/v1/sessions/\(sessionID.rawValue)/artifacts?limit=500&offset=0&include_total=true&kind=prompt_context"
            ]
        )
    }

    func testGetArtifactFetchesArtifactByID() async throws {
        let transport = MockHubHTTPTransport()
        let sessionID = HubSessionID(rawValue: "11111111-1111-4111-8111-111111111111")
        let artifactID = HubArtifactID(rawValue: "22222222-2222-4222-8222-222222222222")
        await transport.setJSONResponse(
            path: "/api/v1/sessions/\(sessionID.rawValue)/artifacts/\(artifactID.rawValue)",
            json: Self.artifactJSON(sessionID: sessionID, artifactID: artifactID, kind: "prompt_context")
        )
        let configuration = try HubClientConfiguration(
            baseURL: URL(string: "http://127.0.0.1:8000")!,
            apiKey: "secret-value"
        )
        let client = HubClient(configuration: configuration, transport: transport)

        let artifact = try await client.getArtifact(sessionID: sessionID, artifactID: artifactID)

        XCTAssertEqual(artifact.id, artifactID)
        XCTAssertEqual(artifact.kind, "prompt_context")
        let request = await transport.requests().first
        XCTAssertEqual(
            request?.path,
            "/api/v1/sessions/\(sessionID.rawValue)/artifacts/\(artifactID.rawValue)"
        )
    }

    func testArtifactDetailRoutesDispatchThroughProtocolExistential() async throws {
        let transport = MockHubHTTPTransport()
        let sessionID = HubSessionID(rawValue: "11111111-1111-4111-8111-111111111111")
        let artifactID = HubArtifactID(rawValue: "22222222-2222-4222-8222-222222222222")
        await transport.setJSONResponse(
            path: "/api/v1/sessions/\(sessionID.rawValue)/artifacts?limit=500&offset=0&include_total=true&kind=prompt_context",
            json: Self.artifactListJSON(
                artifacts: [Self.artifactJSON(sessionID: sessionID, artifactID: artifactID, kind: "prompt_context")],
                limit: 500,
                offset: 0,
                total: 1
            )
        )
        await transport.setJSONResponse(
            path: "/api/v1/sessions/\(sessionID.rawValue)/artifacts/\(artifactID.rawValue)",
            json: Self.artifactJSON(sessionID: sessionID, artifactID: artifactID, kind: "prompt_context")
        )
        let configuration = try HubClientConfiguration(
            baseURL: URL(string: "http://127.0.0.1:8000")!,
            apiKey: "secret-value"
        )
        let client: any HubClientProtocol = HubClient(configuration: configuration, transport: transport)

        let artifacts = try await client.listArtifacts(sessionID: sessionID, kind: "prompt_context")
        let artifact = try await client.getArtifact(sessionID: sessionID, artifactID: artifactID)

        XCTAssertEqual(artifacts.map(\.id), [artifactID])
        XCTAssertEqual(artifact.id, artifactID)
        let requests = await transport.requests().map(\.pathAndQuery)
        XCTAssertEqual(
            requests,
            [
                "/api/v1/sessions/\(sessionID.rawValue)/artifacts?limit=500&offset=0&include_total=true&kind=prompt_context",
                "/api/v1/sessions/\(sessionID.rawValue)/artifacts/\(artifactID.rawValue)"
            ]
        )
    }

    private static func sessionListJSON(sessions: [String], limit: Int, offset: Int, total: Int) -> String {
        """
        {
          "sessions":[\(sessions.joined(separator: ","))],
          "limit":\(limit),
          "offset":\(offset),
          "total":\(total)
        }
        """
    }

    private static func sessionJSON(index: Int, guidanceTargetPath: String? = nil) -> String {
        let guidanceTargetValue = guidanceTargetPath.map { "\"\($0)\"" } ?? "null"
        return """
        {
          "id":"\(sessionID(index: index))",
          "created_from_template_id":null,
          "title":"Session \(index)",
          "description":null,
          "system_prompt":"You are testing pagination.",
          "model_alias":"claude-sonnet-latest",
          "enabled_tools":null,
          "tool_permission_overrides":{},
          "dangerous_auto_approve_all_tools":false,
          "runner_id":null,
          "tags":[],
          "is_archived":false,
          "parent_session_id":null,
          "linked_workspace_path":null,
          "guidance_target_path":\(guidanceTargetValue),
          "created_at":"2026-05-10T12:00:00Z",
          "last_modified_at":"2026-05-10T12:00:00Z",
          "last_turn_ended_at":null,
          "created_from":"test",
          "last_prompted_from":null,
          "source_app":null
        }
        """
    }


    private static func artifactListJSON(artifacts: [String], limit: Int, offset: Int, total: Int) -> String {
        """
        {
          "artifacts":[\(artifacts.joined(separator: ","))],
          "limit":\(limit),
          "offset":\(offset),
          "total":\(total)
        }
        """
    }

    private static func artifactJSON(
        sessionID: HubSessionID,
        artifactID: HubArtifactID = HubArtifactID(rawValue: "22222222-2222-4222-8222-222222222222"),
        kind: String = "report"
    ) -> String {
        """
        {
          "id":"\(artifactID.rawValue)",
          "session_id":"\(sessionID.rawValue)",
          "event_id":7,
          "kind":"\(kind)",
          "title":"Prompt Context",
          "mime_type":"text/markdown",
          "path":null,
          "content_text":"# Prompt Context",
          "metadata":{},
          "created_at":"2026-05-10T12:00:00Z"
        }
        """
    }

    private static func sessionID(index: Int) -> String {
        String(format: "00000000-0000-4000-8000-%012d", index)
    }
}
