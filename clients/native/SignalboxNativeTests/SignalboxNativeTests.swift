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

        guard case .unknown(let kind, _) = message else {
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

private struct TestHeartbeatAcknowledgment: Decodable {
    let kind: String
    let sentAt: Date

    private enum CodingKeys: String, CodingKey {
        case kind
        case sentAt = "sent_at"
    }
}
