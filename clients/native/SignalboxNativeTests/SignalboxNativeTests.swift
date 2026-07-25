import Combine
import XCTest
@testable import SignalboxNative

@MainActor
final class SignalboxNativeTests: XCTestCase {
    func testMockServiceLoadsMainOperationsState() async throws {
        let service = MockSignalboxService()
        let sessions = try await service.listSessions(archived: false)
        let runners = try await service.listRunners()
        let monitor = try await service.listMonitorSessions()

        XCTAssertEqual(sessions.count, 7)
        XCTAssertTrue(runners.contains { $0.status == .online })
        XCTAssertTrue(monitor.contains { $0.status.state == .waitingForConfirmation })
        XCTAssertTrue(monitor.contains { $0.status.state == .failed })
    }

    func testSettingsRejectsInvalidServerURL() {
        let settings = SignalboxSettingsViewModel(
            keychain: KeychainSecretStore(),
            userDefaults: UserDefaults(suiteName: "SignalboxNativeTests")!
        )
        settings.serverURLText = "not a url"
        settings.apiKey = "key"

        guard case .failure = settings.configurationResult() else {
            return XCTFail("Expected invalid URL failure")
        }
    }

    func testApprovalCardsShowTheirMatchedConcurrentToolCallActions() throws {
        let callEventID = SignalboxEventID(rawValue: 1)
        let firstCallID = SignalboxToolCallID(rawValue: "call-A")
        let secondCallID = SignalboxToolCallID(rawValue: "call-B")
        let firstInvocationID = SignalboxToolInvocationID(rawValue: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
        let secondInvocationID = SignalboxToolInvocationID(rawValue: "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb")
        let timestamp = try XCTUnwrap(
            SignalboxJSONCoding.decoder().decode(Date.self, from: Data(#""2026-05-10T12:00:00Z""#.utf8))
        )
        let callEvent = SignalboxStoredEvent(
            eventID: callEventID,
            event: .message(
                SignalboxMessageEvent(
                    kind: "message",
                    message: SignalboxMessage(
                        role: .assistant,
                        parts: [
                            .functionCall(
                                SignalboxFunctionCallContent(
                                    kind: "function_call",
                                    name: "bash",
                                    arguments: #"{"cmd":"ls"}"#,
                                    callID: firstCallID
                                )
                            ),
                            .functionCall(
                                SignalboxFunctionCallContent(
                                    kind: "function_call",
                                    name: "bash",
                                    arguments: #"{"cmd":"delete important"}"#,
                                    callID: secondCallID
                                )
                            ),
                        ]
                    ),
                    visibleToLLM: true,
                    visibleToUser: true,
                    isStreaming: false,
                    parentToolInvocation: nil,
                    createdAt: timestamp,
                    lastModifiedAt: timestamp,
                    createdFrom: "test"
                )
            )
        )
        let firstInvocation = SignalboxStoredEvent(
            eventID: SignalboxEventID(rawValue: 2),
            event: .toolInvocation(
                SignalboxToolInvocationEvent(
                    kind: "tool_invocation",
                    invocationID: firstInvocationID,
                    toolName: "bash",
                    toolCallID: firstCallID,
                    functionCallEventID: callEventID,
                    functionResponseEventID: nil,
                    result: nil,
                    statusUpdates: [],
                    pendingConfirmation: true,
                    decision: nil,
                    decisionAt: nil,
                    decisionReason: nil,
                    isCollapsedByOwner: false,
                    childSessionID: nil,
                    lastModifiedAt: timestamp
                )
            )
        )
        let secondInvocation = SignalboxStoredEvent(
            eventID: SignalboxEventID(rawValue: 3),
            event: .toolInvocation(
                SignalboxToolInvocationEvent(
                    kind: "tool_invocation",
                    invocationID: secondInvocationID,
                    toolName: "bash",
                    toolCallID: secondCallID,
                    functionCallEventID: callEventID,
                    functionResponseEventID: nil,
                    result: nil,
                    statusUpdates: [],
                    pendingConfirmation: true,
                    decision: nil,
                    decisionAt: nil,
                    decisionReason: nil,
                    isCollapsedByOwner: false,
                    childSessionID: nil,
                    lastModifiedAt: timestamp
                )
            )
        )

        let timeline = SignalboxEventNormalizer.normalize([callEvent, firstInvocation, secondInvocation])

        XCTAssertEqual(timeline.count, 2)
        guard case .tool(let firstCard) = timeline[0],
              case .tool(let secondCard) = timeline[1]
        else {
            return XCTFail("Expected two approval cards")
        }
        XCTAssertEqual(firstCard.invocationID, firstInvocationID)
        XCTAssertEqual(firstCard.status, .waitingForApproval)
        XCTAssertEqual(firstCard.arguments, #"{"cmd":"ls"}"#)
        XCTAssertEqual(secondCard.invocationID, secondInvocationID)
        XCTAssertEqual(secondCard.status, .waitingForApproval)
        XCTAssertEqual(secondCard.arguments, #"{"cmd":"delete important"}"#)
    }

    func testWebSocketStreamAcknowledgesHeartbeatBeforeYieldingNextFrame() async throws {
        let heartbeat = """
        {"kind":"heartbeat","sent_at":"2026-05-10T12:00:00Z"}
        """
        let nextFrame = """
        {"kind":"turn_started","turn_id":"turn-1"}
        """
        let transport = StubSignalboxWebSocketTransport(
            incoming: [.string(heartbeat), .string(nextFrame)]
        )
        let stream = SignalboxWebSocketStream(transport: transport)

        var iterator = stream.messages().makeAsyncIterator()
        let yieldedMessage = try await iterator.next()
        let message = try XCTUnwrap(yieldedMessage)

        guard case .unknown(let kind, _, _) = message else {
            return XCTFail("Expected the frame after the heartbeat")
        }
        XCTAssertEqual(kind, "turn_started")
        let sentMessages = await transport.sentMessages
        XCTAssertEqual(sentMessages.count, 1)
        guard case .string(let acknowledgment) = sentMessages[0] else {
            return XCTFail("Expected a string heartbeat acknowledgment")
        }
        let decoded = try SignalboxJSONCoding.decoder().decode(
            TestHeartbeatAcknowledgment.self,
            from: Data(acknowledgment.utf8)
        )
        XCTAssertEqual(decoded.kind, "heartbeat_ack")
        XCTAssertEqual(
            decoded.sentAt,
            try SignalboxJSONCoding.decoder().decode(Date.self, from: Data(#""2026-05-10T12:00:00Z""#.utf8))
        )
    }

    func testWebSocketStreamContinuesAfterUndecodableFrame() async throws {
        let transport = StubSignalboxWebSocketTransport(
            incoming: [
                .string(#"{"kind":"event_appended","event_id":"not-an-integer","event":{}}"#),
                .string(#"{"kind":"turn_started","turn_id":"turn-1"}"#),
            ]
        )
        let stream = SignalboxWebSocketStream(transport: transport)

        var iterator = stream.messages().makeAsyncIterator()
        let firstYield = try await iterator.next()
        let firstMessage = try XCTUnwrap(firstYield)
        let secondYield = try await iterator.next()
        let secondMessage = try XCTUnwrap(secondYield)

        guard case .unknown(let firstKind, _, let diagnostic) = firstMessage else {
            return XCTFail("Expected the evolved known frame to use the unknown-frame path")
        }
        XCTAssertEqual(firstKind, "event_appended")
        XCTAssertEqual(diagnostic?.message, "Unexpected field type at event_id.")
        guard case .unknown(let secondKind, _, let secondDiagnostic) = secondMessage else {
            return XCTFail("Expected the frame after the decode failure")
        }
        XCTAssertEqual(secondKind, "turn_started")
        XCTAssertNil(secondDiagnostic)
    }

    func testWebSocketStreamSurfacesMalformedPayloadAndContinues() async throws {
        let transport = StubSignalboxWebSocketTransport(
            incoming: [
                .string("not-json"),
                .string(#"{"kind":"turn_started","turn_id":"turn-1"}"#),
            ]
        )
        let stream = SignalboxWebSocketStream(transport: transport)

        var iterator = stream.messages().makeAsyncIterator()
        let firstYield = try await iterator.next()
        let firstMessage = try XCTUnwrap(firstYield)
        let secondYield = try await iterator.next()
        let secondMessage = try XCTUnwrap(secondYield)

        guard case .diagnostic(let diagnostic) = firstMessage else {
            return XCTFail("Expected a surfaced payload diagnostic")
        }
        XCTAssertEqual(diagnostic.message, "Invalid field value at the payload.")
        guard case .unknown(let kind, _, let secondDiagnostic) = secondMessage else {
            return XCTFail("Expected the frame after the malformed payload")
        }
        XCTAssertEqual(kind, "turn_started")
        XCTAssertNil(secondDiagnostic)
    }

    func testWebSocketStreamFailsWhenHeartbeatsStop() async throws {
        let heartbeat = """
        {"kind":"heartbeat","sent_at":"2026-05-10T12:00:00Z"}
        """
        let transport = QuietSignalboxWebSocketTransport(heartbeat: .string(heartbeat))
        let stream = SignalboxWebSocketStream(
            transport: transport,
            heartbeatTimeout: .milliseconds(25)
        )

        var iterator = stream.messages().makeAsyncIterator()

        do {
            _ = try await iterator.next()
            XCTFail("Expected a quiet-connection failure")
        } catch let error as SignalboxWebSocketStreamError {
            XCTAssertEqual(error, .connectionWentQuiet)
            XCTAssertEqual(error.errorDescription, "The server connection stopped receiving heartbeats.")
        }
        let sentMessages = await transport.sentMessages
        XCTAssertEqual(sentMessages.count, 1)
    }

    func testListEventsPreservesPageWhenKnownEventFieldsEvolve() async throws {
        let transport = MockSignalboxHTTPTransport()
        await transport.setJSONResponse(
            path: "/api/v1/sessions/session-1/events",
            json: """
            {
              "events": [
                {
                  "event_id": 1,
                  "event": {"kind": "future_event", "field": "preserved"}
                },
                {
                  "event_id": 2,
                  "event": {
                    "kind": "message",
                    "message": {
                      "role": "assistant",
                      "parts": [{"kind": "text", "text": "still loading"}]
                    },
                    "visible_to_llm": true,
                    "visible_to_user": true,
                    "is_streaming": false,
                    "parent_tool_invocation": null,
                    "created_at": "2026-05-10T12:00:00Z",
                    "last_modified_at": "2026-05-10T12:00:00Z"
                  }
                }
              ],
              "limit": 500,
              "next_after": null
            }
            """
        )
        let configuration = try SignalboxClientConfiguration(
            baseURL: try XCTUnwrap(URL(string: "http://127.0.0.1:8000")),
            apiKey: "synthetic-api-key"
        )
        let client = SignalboxAPIClient(configuration: configuration, transport: transport)

        let events = try await client.listEvents(sessionID: SignalboxSessionID(rawValue: "session-1"))

        XCTAssertEqual(events.count, 2)
        XCTAssertEqual(events[0].eventID, SignalboxEventID(rawValue: 1))
        guard case .unknown(let preservedUnknown) = events[0].event else {
            return XCTFail("Expected the unknown event to remain available")
        }
        XCTAssertEqual(preservedUnknown.kind, "future_event")
        XCTAssertNil(preservedUnknown.decodingDiagnostic)
        XCTAssertEqual(events[1].eventID, SignalboxEventID(rawValue: 2))
        guard case .unknown(let evolvedKnownEvent) = events[1].event else {
            return XCTFail("Expected the evolved event to degrade to an unknown event")
        }
        XCTAssertEqual(evolvedKnownEvent.kind, "message")
        XCTAssertEqual(
            evolvedKnownEvent.decodingDiagnostic?.message,
            "Missing required field at events[1].event.created_from."
        )
    }

    func testUnknownStreamFramesDoNotCreateSyntheticEventIDs() async throws {
        let fixtureService = MockSignalboxService()
        let sessions = try await fixtureService.listSessions(archived: false)
        let session = try XCTUnwrap(sessions.first)
        let service = UnknownFrameSignalboxService()
        let viewModel = SessionDetailViewModel(session: session) { service }
        let frameHandled = expectation(description: "unknown frame recorded")
        let observation = viewModel.$unhandledFrameKinds
            .filter { $0["turn_started"] == 1 }
            .first()
            .sink { _ in frameHandled.fulfill() }

        viewModel.connectStream()
        await fulfillment(of: [frameHandled], timeout: 1)

        XCTAssertTrue(viewModel.events.isEmpty)
        XCTAssertTrue(viewModel.timelineItems.isEmpty)
        XCTAssertEqual(viewModel.unhandledFrameKinds, ["turn_started": 1])
        withExtendedLifetime(observation) {}
    }
}

private actor StubSignalboxWebSocketTransport: SignalboxWebSocketTransport {
    private var incoming: [SignalboxWebSocketMessage]
    private(set) var sentMessages: [SignalboxWebSocketMessage] = []

    init(incoming: [SignalboxWebSocketMessage]) {
        self.incoming = incoming
    }

    func receive() async throws -> SignalboxWebSocketMessage {
        guard !incoming.isEmpty else {
            throw StubSignalboxWebSocketTransportError.endOfStream
        }
        return incoming.removeFirst()
    }

    func send(_ message: SignalboxWebSocketMessage) async throws {
        sentMessages.append(message)
    }

    func cancel() async {}
}

private enum StubSignalboxWebSocketTransportError: Error {
    case endOfStream
}

private final class UnknownFrameSignalboxService: SignalboxClientProtocol, @unchecked Sendable {
    func testConnection() async throws {}
    func listTemplates() async throws -> [SignalboxTemplate] { [] }
    func listRunners() async throws -> [SignalboxRunner] { [] }
    func listSessions(archived: Bool) async throws -> [SignalboxSessionMetadata] { [] }
    func createSession(request: SignalboxCreateSessionRequest) async throws -> SignalboxSessionView {
        throw SignalboxClientError.requestFailed("not implemented")
    }
    func patchSessionArchive(
        sessionID: SignalboxSessionID,
        isArchived: Bool
    ) async throws -> SignalboxSessionMetadata {
        throw SignalboxClientError.requestFailed("not implemented")
    }
    func listEvents(sessionID: SignalboxSessionID) async throws -> [SignalboxStoredEvent] { [] }
    func appendUserMessage(
        sessionID: SignalboxSessionID,
        text: String
    ) async throws -> SignalboxAppendUserMessageResponse {
        throw SignalboxClientError.requestFailed("not implemented")
    }
    func confirmInvocation(
        sessionID: SignalboxSessionID,
        invocationID: SignalboxToolInvocationID
    ) async throws {}
    func denyInvocation(
        sessionID: SignalboxSessionID,
        invocationID: SignalboxToolInvocationID,
        reason: String?
    ) async throws {}
    func listArtifacts(sessionID: SignalboxSessionID) async throws -> [SignalboxArtifact] { [] }
    func listMonitorSessions() async throws -> [SignalboxMonitorSessionSummary] { [] }

    func streamMessages(
        sessionID: SignalboxSessionID
    ) -> AsyncThrowingStream<SignalboxServerMessage, Error> {
        AsyncThrowingStream { continuation in
            continuation.yield(
                .unknown(
                    kind: "turn_started",
                    payload: ["turn_id": .string("turn-1")],
                    decodingDiagnostic: nil
                )
            )
            continuation.finish()
        }
    }
}

private actor QuietSignalboxWebSocketTransport: SignalboxWebSocketTransport {
    private var heartbeat: SignalboxWebSocketMessage?
    private(set) var sentMessages: [SignalboxWebSocketMessage] = []

    init(heartbeat: SignalboxWebSocketMessage) {
        self.heartbeat = heartbeat
    }

    func receive() async throws -> SignalboxWebSocketMessage {
        if let heartbeat {
            self.heartbeat = nil
            return heartbeat
        }
        try await Task.sleep(for: .milliseconds(200))
        throw StubSignalboxWebSocketTransportError.endOfStream
    }

    func send(_ message: SignalboxWebSocketMessage) async throws {
        sentMessages.append(message)
    }

    func cancel() async {}
}

private struct TestHeartbeatAcknowledgment: Decodable {
    let kind: String
    let sentAt: Date

    private enum CodingKeys: String, CodingKey {
        case kind
        case sentAt = "sent_at"
    }
}
