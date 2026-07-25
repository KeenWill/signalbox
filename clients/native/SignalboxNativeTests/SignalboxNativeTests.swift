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

    func testSettingsRejectsInvalidServerURL() throws {
        let settings = SignalboxSettingsViewModel(
            keychain: KeychainSecretStore(
                service: "co.rdwd.SignalboxNativeTests.invalid-url",
                account: "synthetic-api-key"
            ),
            userDefaults: UserDefaults(suiteName: "SignalboxNativeTests")!
        )
        settings.serverURLText = "not a url"
        settings.apiKey = "key"

        try requireFailure(settings.configurationResult())
    }

    func testSettingsKeychainUsesInjectedIdentity() throws {
        let service = "co.rdwd.SignalboxNativeTests.injected-identity"
        let account = "synthetic-api-key"
        let keychain = KeychainSecretStore(service: service, account: account)
        let defaultsSuite = "SignalboxNativeTests.injected-keychain-identity"
        let userDefaults = try XCTUnwrap(UserDefaults(suiteName: defaultsSuite))
        keychain.deleteSecret()
        userDefaults.removePersistentDomain(forName: defaultsSuite)
        defer {
            keychain.deleteSecret()
            userDefaults.removePersistentDomain(forName: defaultsSuite)
        }
        try keychain.writeSecret("synthetic-existing-api-key")
        let settings = SignalboxSettingsViewModel(
            keychain: keychain,
            userDefaults: userDefaults
        )

        XCTAssertEqual(settings.apiKey, "synthetic-existing-api-key")

        settings.apiKey = "synthetic-saved-api-key"
        settings.save()

        XCTAssertEqual(keychain.readSecret(), "synthetic-saved-api-key")
    }

    func testClientErrorsDoNotExposeConfiguredCredential() async throws {
        let configuredCredential = "synthetic-credential-must-stay-private"
        let transport = MockSignalboxHTTPTransport()
        await transport.setJSONResponse(
            path: "/api/v1/templates",
            statusCode: 503,
            json: #"{"detail":"synthetic service unavailable"}"#
        )
        let configuration = try SignalboxClientConfiguration(
            baseURL: try XCTUnwrap(URL(string: "http://127.0.0.1:8000")),
            apiKey: configuredCredential
        )
        let client = SignalboxAPIClient(configuration: configuration, transport: transport)

        let error = try await requireSignalboxClientError {
            _ = try await client.listTemplates()
        }

        XCTAssertFalse(error.localizedDescription.contains(configuredCredential))
    }

    func testApprovalCardsShowTheirMatchedConcurrentToolCallActions() throws {
        let callEventID = SignalboxEventID(rawValue: 1)
        let firstCallID = SignalboxToolCallID(rawValue: "call-A")
        let secondCallID = SignalboxToolCallID(rawValue: "call-B")
        let firstInvocationID = SignalboxToolInvocationID(rawValue: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
        let secondInvocationID = SignalboxToolInvocationID(rawValue: "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb")
        let firstArguments = #"{"cmd":"ls"}"#
        let secondArguments = #"{"cmd":"delete important"}"#
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
                                    arguments: firstArguments,
                                    callID: firstCallID
                                )
                            ),
                            .functionCall(
                                SignalboxFunctionCallContent(
                                    kind: "function_call",
                                    name: "bash",
                                    arguments: secondArguments,
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

        let incremental = SignalboxIncrementalEventNormalizer()
        incremental.upsert(callEvent)
        XCTAssertEqual(
            incremental.timelineItems,
            SignalboxEventNormalizer.normalize([callEvent])
        )
        incremental.upsert(firstInvocation)
        XCTAssertEqual(
            incremental.timelineItems,
            SignalboxEventNormalizer.normalize([callEvent, firstInvocation])
        )
        incremental.upsert(secondInvocation)
        let timeline = SignalboxEventNormalizer.normalize(
            [callEvent, firstInvocation, secondInvocation]
        )

        XCTAssertEqual(incremental.timelineItems, timeline)
        XCTAssertEqual(timeline.count, 2)
        let firstCard = try requireToolCard(timeline[0])
        let secondCard = try requireToolCard(timeline[1])
        XCTAssertEqual(firstCard.invocationID, firstInvocationID)
        XCTAssertEqual(firstCard.status, .waitingForApproval)
        XCTAssertEqual(firstCard.arguments, firstArguments)
        XCTAssertEqual(secondCard.invocationID, secondInvocationID)
        XCTAssertEqual(secondCard.status, .waitingForApproval)
        XCTAssertEqual(secondCard.arguments, secondArguments)
    }

    func testIncrementalNormalizerMatchesExistingTimelineFixtures() async throws {
        let service = MockSignalboxService()
        let activeEvents = try await service.listEvents(
            sessionID: SignalboxSessionID(rawValue: MockSignalboxFixtures.activeSessionID)
        )
        let approvalEvents = try await service.listEvents(
            sessionID: SignalboxSessionID(rawValue: MockSignalboxFixtures.approvalSessionID)
        )
        let failedEvents = try await service.listEvents(
            sessionID: SignalboxSessionID(rawValue: MockSignalboxFixtures.failedSessionID)
        )

        assertIncrementalNormalizationMatchesNaive(activeEvents)
        assertIncrementalNormalizationMatchesNaive(approvalEvents)
        assertIncrementalNormalizationMatchesNaive(failedEvents)
    }

    func testIncrementalNormalizerLongSequencePreservesOutputWithinLinearEvaluationBudget() throws {
        let comparison = try longSequenceNormalizationComparison()

        XCTAssertEqual(comparison.incrementalTimeline, comparison.naiveTimeline)
        XCTAssertEqual(
            comparison.incrementalMetrics.recordEvaluationCount,
            LongSequenceFixture.eventCount
        )
        XCTAssertEqual(
            comparison.naiveMetrics.recordEvaluationCount,
            LongSequenceFixture.naiveRecordEvaluationCount
        )
        XCTAssertLessThan(
            comparison.incrementalMetrics.recordEvaluationCount,
            comparison.naiveMetrics.recordEvaluationCount
        )
    }

    func testIncrementalNormalizerKeepsStableTimelineCollectionAcrossAppends() throws {
        let timestamp = try SignalboxJSONCoding.decoder().decode(
            Date.self,
            from: Data(LongSequenceFixture.timestampJSON.utf8)
        )
        let firstRecord = longSequenceRecord(
            eventID: LongSequenceFixture.firstEventID,
            timestamp: timestamp
        )
        let secondRecord = longSequenceRecord(
            eventID: LongSequenceFixture.secondEventID,
            timestamp: timestamp
        )
        let incremental = SignalboxIncrementalEventNormalizer()
        let timeline = incremental.timeline

        incremental.upsert(firstRecord)
        incremental.upsert(secondRecord)

        XCTAssertTrue(timeline === incremental.timeline)
        XCTAssertEqual(
            Array(timeline),
            SignalboxEventNormalizer.normalize([firstRecord, secondRecord])
        )
    }

    func testStreamHelloReusesLoadedHistoryAndReplaysConcurrentDeletion() async throws {
        let fixture = try await streamHelloHistoryReuseFixture()
        let service = HistoryReuseStreamSignalboxService(fixture: fixture)
        let viewModel = SessionDetailViewModel(session: fixture.session) { service }

        let loadTask = Task {
            await viewModel.loadAndConnect()
        }
        await service.waitForStreamInvocation()
        let streamStopped = observeCurrentStreamStopped(on: viewModel)
        service.sendStreamHello()
        await service.waitForListEventsInvocation()
        service.sendDeletedEventAndFinish()
        service.resumeHistoryEvents()
        await loadTask.value
        await fulfillment(
            of: [streamStopped.expectation],
            timeout: StreamHelloHistoryReuseFixture.observationTimeout
        )

        XCTAssertEqual(viewModel.events, fixture.expectedMergedEvents)
        XCTAssertEqual(
            viewModel.timelineItems,
            SignalboxEventNormalizer.normalize(fixture.expectedMergedEvents)
        )
        XCTAssertEqual(
            service.listEventsCallCount,
            StreamHelloHistoryReuseFixture.expectedListEventsCallCount
        )
        withExtendedLifetime(streamStopped.cancellable) {}
    }

    func testLoadAndConnectFallsBackWhenStreamHelloTimesOut() async throws {
        let fixture = try await streamHelloHistoryReuseFixture()
        let service = HistoryReuseStreamSignalboxService(fixture: fixture)
        let timeoutWaiter = ImmediateFirstStreamHelloTimeoutWaiter()
        let viewModel = SessionDetailViewModel(
            session: fixture.session,
            streamHelloTimeout: StreamHelloHistoryReuseFixture.retryTimeout,
            waitForStreamHello: { timeout in
                await service.waitForStreamInvocationCount(
                    StreamHelloHistoryReuseFixture.firstStreamInvocationCount
                )
                try await timeoutWaiter.wait(for: timeout)
            }
        ) { service }

        let loadTask = Task {
            await viewModel.loadAndConnect()
        }
        await service.waitForListEventsInvocation()
        service.resumeHistoryEvents()
        await loadTask.value

        XCTAssertEqual(viewModel.events, fixture.historyEvents)
        XCTAssertEqual(
            service.listEventsCallCount,
            StreamHelloHistoryReuseFixture.expectedListEventsCallCount
        )
        viewModel.disconnectStream()
    }

    func testLoadAndConnectRecoversFromInitialHistoryFailure() async throws {
        let fixture = try await streamHelloHistoryReuseFixture()
        let service = HistoryReuseStreamSignalboxService(
            fixture: fixture,
            historyFailureCount: StreamHelloHistoryReuseFixture.initialHistoryFailureCount
        )
        let viewModel = SessionDetailViewModel(session: fixture.session) { service }

        let loadTask = Task {
            await viewModel.loadAndConnect()
        }
        await service.waitForStreamInvocation()
        service.sendStreamHello()
        await service.waitForListEventsInvocation()
        service.resumeHistoryEvents()
        await loadTask.value

        XCTAssertEqual(viewModel.events, fixture.historyEvents)
        XCTAssertEqual(
            service.listEventsCallCount,
            StreamHelloHistoryReuseFixture.expectedRecoveryListEventsCallCount
        )
        XCTAssertNil(viewModel.errorMessage)
        viewModel.disconnectStream()
    }

    func testReconnectHelloTimeoutRetriesAuthoritativeSynchronization() async throws {
        let fixture = try await streamHelloHistoryReuseFixture()
        let service = HistoryReuseStreamSignalboxService(fixture: fixture)
        let timeoutWaiter = ImmediateFirstStreamHelloTimeoutWaiter()
        let viewModel = SessionDetailViewModel(
            session: fixture.session,
            streamHelloTimeout: StreamHelloHistoryReuseFixture.retryTimeout,
            waitForStreamHello: { timeout in
                await service.waitForStreamInvocationCount(
                    StreamHelloHistoryReuseFixture.firstStreamInvocationCount
                )
                try await timeoutWaiter.wait(for: timeout)
            }
        ) { service }
        let synchronizedStatus = observeStatus(
            .waitingForConfirmation,
            on: viewModel
        )

        let initialLoad = Task {
            await viewModel.load()
        }
        await service.waitForListEventsInvocation()
        service.resumeHistoryEvents()
        await initialLoad.value
        viewModel.connectStream()
        await service.waitForStreamInvocationCount(
            StreamHelloHistoryReuseFixture.retryStreamInvocationCount
        )
        service.sendStreamHello()
        await service.waitForListEventsInvocation()
        service.resumeHistoryEvents()
        await fulfillment(
            of: [synchronizedStatus.expectation],
            timeout: StreamHelloHistoryReuseFixture.observationTimeout
        )

        XCTAssertEqual(
            service.streamInvocationCount,
            StreamHelloHistoryReuseFixture.retryStreamInvocationCount
        )
        XCTAssertEqual(
            service.listEventsCallCount,
            StreamHelloHistoryReuseFixture.expectedReconnectListEventsCallCount
        )
        XCTAssertEqual(viewModel.events, fixture.expectedSynchronizedEvents)
        withExtendedLifetime(synchronizedStatus.cancellable) {}
        viewModel.disconnectStream()
    }

    func testEmptyLoadedHistoryStillSynchronizesAfterStreamHello() async throws {
        let fixture = try await streamHelloHistoryReuseFixture()
        let service = HistoryReuseStreamSignalboxService(
            fixture: fixture,
            historyEvents: StreamHelloHistoryReuseFixture.emptyHistoryEvents
        )
        let viewModel = SessionDetailViewModel(session: fixture.session) { service }
        let synchronizedStatus = observeStatus(
            .waitingForConfirmation,
            on: viewModel
        )

        let initialLoad = Task {
            await viewModel.load()
        }
        await service.waitForListEventsInvocation()
        service.resumeHistoryEvents()
        await initialLoad.value
        viewModel.connectStream()
        await service.waitForStreamInvocation()
        service.sendStreamHello()
        await service.waitForListEventsInvocation()
        service.resumeHistoryEvents()
        await fulfillment(
            of: [synchronizedStatus.expectation],
            timeout: StreamHelloHistoryReuseFixture.observationTimeout
        )

        XCTAssertEqual(
            service.listEventsCallCount,
            StreamHelloHistoryReuseFixture.expectedReconnectListEventsCallCount
        )
        XCTAssertEqual(viewModel.events, fixture.helloEvents)
        withExtendedLifetime(synchronizedStatus.cancellable) {}
        viewModel.disconnectStream()
    }

    func testHistorySynchronizationPreservesBufferedDiagnostic() async throws {
        let fixture = try await streamHelloHistoryReuseFixture()
        let service = HistoryReuseStreamSignalboxService(fixture: fixture)
        let viewModel = SessionDetailViewModel(session: fixture.session) { service }
        let synchronizedStatus = observeStatus(
            .waitingForConfirmation,
            on: viewModel
        )

        let loadTask = Task {
            await viewModel.loadAndConnect()
        }
        await service.waitForStreamInvocation()
        service.sendDiagnostic()
        service.sendStreamHello()
        await service.waitForListEventsInvocation()
        service.resumeHistoryEvents()
        await loadTask.value
        await fulfillment(
            of: [synchronizedStatus.expectation],
            timeout: StreamHelloHistoryReuseFixture.observationTimeout
        )

        XCTAssertEqual(
            viewModel.errorMessage,
            StreamHelloHistoryReuseFixture.bufferedDiagnosticMessage
        )
        XCTAssertEqual(
            viewModel.latestStreamDiagnostic,
            StreamHelloHistoryReuseFixture.bufferedDiagnosticMessage
        )
        withExtendedLifetime(synchronizedStatus.cancellable) {}
        viewModel.disconnectStream()
    }

    func testWebSocketStreamAcknowledgesHeartbeatBeforeYieldingNextFrame() async throws {
        let heartbeatSentAt = "2026-05-10T12:00:00Z"
        let expectedSentAt = try SignalboxJSONCoding.decoder().decode(
            Date.self,
            from: Data("\"\(heartbeatSentAt)\"".utf8)
        )
        let heartbeat = """
        {"kind":"heartbeat","sent_at":"\(heartbeatSentAt)"}
        """
        let nextFrameKind = "turn_started"
        let nextFrame = """
        {"kind":"\(nextFrameKind)","turn_id":"turn-1"}
        """
        let transport = StubSignalboxWebSocketTransport(
            incoming: [.string(heartbeat), .string(nextFrame)]
        )
        let stream = SignalboxWebSocketStream(transportFactory: { transport })

        var iterator = stream.messages().makeAsyncIterator()
        let yieldedMessage = try await iterator.next()
        let message = try XCTUnwrap(yieldedMessage)

        let (kind, _) = try requireUnknownServerMessage(message)
        XCTAssertEqual(kind, nextFrameKind)
        let sentMessages = await transport.sentMessages
        XCTAssertEqual(sentMessages.count, 1)
        let acknowledgment = try requireStringWebSocketMessage(sentMessages[0])
        let decoded = try SignalboxJSONCoding.decoder().decode(
            TestHeartbeatAcknowledgment.self,
            from: Data(acknowledgment.utf8)
        )
        XCTAssertEqual(decoded.kind, "heartbeat_ack")
        XCTAssertEqual(decoded.sentAt, expectedSentAt)
    }

    func testWebSocketStreamCreatesTransportLazilyForEachMessagesCall() async throws {
        let firstFrameKind = "turn_started"
        let secondFrameKind = "turn_completed"
        let firstTransport = StubSignalboxWebSocketTransport(
            incoming: [.string(#"{"kind":"\#(firstFrameKind)"}"#)]
        )
        let secondTransport = StubSignalboxWebSocketTransport(
            incoming: [.string(#"{"kind":"\#(secondFrameKind)"}"#)]
        )
        let factory = StubSignalboxWebSocketTransportFactory(
            transports: [firstTransport, secondTransport]
        )
        let stream = SignalboxWebSocketStream(
            transportFactory: { factory.makeTransport() }
        )

        XCTAssertEqual(factory.createdTransportCount, 0)
        var firstIterator = stream.messages().makeAsyncIterator()
        XCTAssertEqual(factory.createdTransportCount, 1)
        let firstYield = try await firstIterator.next()
        let firstMessage = try XCTUnwrap(firstYield)
        let (firstKind, _) = try requireUnknownServerMessage(firstMessage)
        XCTAssertEqual(firstKind, firstFrameKind)

        var secondIterator = stream.messages().makeAsyncIterator()
        XCTAssertEqual(factory.createdTransportCount, 2)
        let secondYield = try await secondIterator.next()
        let secondMessage = try XCTUnwrap(secondYield)
        let (secondKind, _) = try requireUnknownServerMessage(secondMessage)
        XCTAssertEqual(secondKind, secondFrameKind)
    }

    func testWebSocketStreamContinuesAfterUndecodableFrame() async throws {
        let evolvedFrameKind = "event_appended"
        let followingFrameKind = "turn_started"
        let transport = StubSignalboxWebSocketTransport(
            incoming: [
                .string(
                    #"{"kind":"\#(evolvedFrameKind)","event_id":"not-an-integer","event":{}}"#
                ),
                .string(#"{"kind":"\#(followingFrameKind)","turn_id":"turn-1"}"#),
            ]
        )
        let stream = SignalboxWebSocketStream(transportFactory: { transport })

        var iterator = stream.messages().makeAsyncIterator()
        let firstYield = try await iterator.next()
        let firstMessage = try XCTUnwrap(firstYield)
        let secondYield = try await iterator.next()
        let secondMessage = try XCTUnwrap(secondYield)

        let (firstKind, diagnostic) = try requireUnknownServerMessage(firstMessage)
        XCTAssertEqual(firstKind, evolvedFrameKind)
        XCTAssertEqual(diagnostic?.message, "Unexpected field type at event_id.")
        let (secondKind, secondDiagnostic) = try requireUnknownServerMessage(secondMessage)
        XCTAssertEqual(secondKind, followingFrameKind)
        XCTAssertNil(secondDiagnostic)
    }

    func testWebSocketStreamSurfacesMalformedPayloadAndContinues() async throws {
        let followingFrameKind = "turn_started"
        let transport = StubSignalboxWebSocketTransport(
            incoming: [
                .string("not-json"),
                .string(#"{"kind":"\#(followingFrameKind)","turn_id":"turn-1"}"#),
            ]
        )
        let stream = SignalboxWebSocketStream(transportFactory: { transport })

        var iterator = stream.messages().makeAsyncIterator()
        let firstYield = try await iterator.next()
        let firstMessage = try XCTUnwrap(firstYield)
        let secondYield = try await iterator.next()
        let secondMessage = try XCTUnwrap(secondYield)

        let diagnostic = try requireDecodingDiagnostic(firstMessage)
        XCTAssertEqual(diagnostic.message, "Invalid field value at the payload.")
        let (kind, secondDiagnostic) = try requireUnknownServerMessage(secondMessage)
        XCTAssertEqual(kind, followingFrameKind)
        XCTAssertNil(secondDiagnostic)
    }

    func testWebSocketStreamFailsWhenHeartbeatsStop() async throws {
        let heartbeat = """
        {"kind":"heartbeat","sent_at":"2026-05-10T12:00:00Z"}
        """
        let transport = QuietSignalboxWebSocketTransport(heartbeat: .string(heartbeat))
        let scheduler = ControlledSignalboxWebSocketWatchdogScheduler()
        let stream = SignalboxWebSocketStream(
            transportFactory: { transport },
            heartbeatTimeout: .seconds(45),
            watchdogScheduler: scheduler
        )

        let messages = stream.messages()
        let nextMessage = Task {
            var iterator = messages.makeAsyncIterator()
            return try await iterator.next()
        }

        await transport.waitForSentMessageCount(1)
        await scheduler.waitForScheduleCount(2)
        await scheduler.fireLatest()
        let error = try await requireWebSocketStreamError(nextMessage)

        XCTAssertEqual(error, .connectionWentQuiet)
        XCTAssertEqual(error.errorDescription, "The server connection stopped receiving heartbeats.")
        let sentMessages = await transport.sentMessages
        XCTAssertEqual(sentMessages.count, 1)
    }

    func testWebSocketStreamDistinguishesInitialLivenessTimeout() async throws {
        let transport = QuietSignalboxWebSocketTransport(heartbeat: nil)
        let scheduler = ControlledSignalboxWebSocketWatchdogScheduler()
        let stream = SignalboxWebSocketStream(
            transportFactory: { transport },
            heartbeatTimeout: .seconds(45),
            watchdogScheduler: scheduler
        )

        let messages = stream.messages()
        let nextMessage = Task {
            var iterator = messages.makeAsyncIterator()
            return try await iterator.next()
        }

        await scheduler.waitForScheduleCount(1)
        await scheduler.fireLatest()
        let error = try await requireWebSocketStreamError(nextMessage)

        XCTAssertEqual(error, .connectionTimedOut)
        XCTAssertEqual(
            error.errorDescription,
            "The server connection did not receive a heartbeat in time."
        )
        let sentMessages = await transport.sentMessages
        XCTAssertTrue(sentMessages.isEmpty)
    }

    func testListEventsPreservesPageWhenKnownEventFieldsEvolve() async throws {
        let preservedEventID = SignalboxEventID(rawValue: 1)
        let evolvedEventID = SignalboxEventID(rawValue: 2)
        let preservedEventKind = "future_event"
        let evolvedEventKind = "message"
        let transport = MockSignalboxHTTPTransport()
        await transport.setJSONResponse(
            path: "/api/v1/sessions/session-1/events",
            json: """
            {
              "events": [
                {
                  "event_id": \(preservedEventID.rawValue),
                  "event": {"kind": "\(preservedEventKind)", "field": "preserved"}
                },
                {
                  "event_id": \(evolvedEventID.rawValue),
                  "event": {
                    "kind": "\(evolvedEventKind)",
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
        XCTAssertEqual(events[0].eventID, preservedEventID)
        let preservedUnknown = try requireUnknownConversationEvent(events[0].event)
        XCTAssertEqual(preservedUnknown.kind, preservedEventKind)
        XCTAssertNil(preservedUnknown.decodingDiagnostic)
        XCTAssertEqual(events[1].eventID, evolvedEventID)
        let evolvedKnownEvent = try requireUnknownConversationEvent(events[1].event)
        XCTAssertEqual(evolvedKnownEvent.kind, evolvedEventKind)
        XCTAssertEqual(
            evolvedKnownEvent.decodingDiagnostic?.message,
            "Missing required field at events[1].event.created_from."
        )
    }

    func testKnownEventInvalidTimestampDiagnosticIncludesFieldPath() throws {
        let record = try SignalboxJSONCoding.decoder().decode(
            SignalboxStoredEvent.self,
            from: Data(
                """
                {
                  "event_id": 1,
                  "event": {
                    "kind": "message",
                    "message": {
                      "role": "assistant",
                      "parts": [{"kind": "text", "text": "visible detail"}]
                    },
                    "visible_to_llm": true,
                    "visible_to_user": true,
                    "is_streaming": false,
                    "parent_tool_invocation": null,
                    "created_at": "not-a-timestamp",
                    "last_modified_at": "2026-05-10T12:00:00Z",
                    "created_from": "server"
                  }
                }
                """.utf8
            )
        )

        let degradedEvent = try requireUnknownConversationEvent(record.event)

        XCTAssertEqual(
            degradedEvent.decodingDiagnostic?.message,
            "Invalid field value at event.created_at."
        )
    }

    func testNormalizerSuppressesHiddenKnownEventThatDegradesToUnknown() throws {
        let record = try SignalboxJSONCoding.decoder().decode(
            SignalboxStoredEvent.self,
            from: Data(
                """
                {
                  "event_id": 1,
                  "event": {
                    "kind": "message",
                    "message": {
                      "role": "assistant",
                      "parts": [{"kind": "text", "text": "internal detail"}]
                    },
                    "visible_to_llm": true,
                    "visible_to_user": false,
                    "is_streaming": false,
                    "parent_tool_invocation": null,
                    "created_at": "2026-05-10T12:00:00Z",
                    "last_modified_at": "2026-05-10T12:00:00Z"
                  }
                }
                """.utf8
            )
        )

        let degradedEvent = try requireUnknownConversationEvent(record.event)
        XCTAssertEqual(
            degradedEvent.decodingDiagnostic?.message,
            "Missing required field at event.created_from."
        )
        let naiveTimeline = SignalboxEventNormalizer.normalize([record])
        let incrementalTimeline = SignalboxIncrementalEventNormalizer(records: [record]).timelineItems
        XCTAssertEqual(incrementalTimeline, naiveTimeline)
        XCTAssertTrue(naiveTimeline.isEmpty)
    }

    func testUnknownStreamFramesDoNotCreateSyntheticEventIDs() async throws {
        let fixtureService = MockSignalboxService()
        let sessions = try await fixtureService.listSessions(archived: false)
        let session = try XCTUnwrap(sessions.first)
        let service = UnknownFrameSignalboxService()
        let viewModel = SessionDetailViewModel(session: session) { service }
        let observation = observeUnhandledFrame(
            UnknownFrameSignalboxService.frameKind,
            on: viewModel
        )

        viewModel.connectStream()
        await fulfillment(of: [observation.expectation], timeout: 1)

        XCTAssertTrue(viewModel.events.isEmpty)
        XCTAssertTrue(viewModel.timelineItems.isEmpty)
        XCTAssertEqual(
            viewModel.unhandledFrameKinds,
            [UnknownFrameSignalboxService.frameKind: 1]
        )
        withExtendedLifetime(observation.cancellable) {}
    }

    func testReconnectRejectsProcessedStaleStreamCompletion() async throws {
        let fixtureService = MockSignalboxService()
        let sessions = try await fixtureService.listSessions(archived: false)
        let session = try XCTUnwrap(sessions.first)
        let service = ControlledReconnectSignalboxService()
        let viewModel = SessionDetailViewModel(session: session) { service }

        viewModel.connectStream()
        await service.waitForStreamInvocationCount(1)
        let staleCompletion = observeStaleCompletionRejection(on: viewModel)
        viewModel.disconnectStream()
        viewModel.connectStream()
        await service.waitForStreamInvocationCount(2)

        await fulfillment(of: [staleCompletion.expectation], timeout: 1)

        XCTAssertTrue(viewModel.isStreaming)
        XCTAssertEqual(viewModel.ignoredStaleStreamCompletionCount, 1)

        let currentStreamStopped = observeCurrentStreamStopped(on: viewModel)
        service.finishStream(at: 1)
        await fulfillment(of: [currentStreamStopped.expectation], timeout: 1)

        XCTAssertFalse(viewModel.isStreaming)
        withExtendedLifetime(staleCompletion.cancellable) {}
        withExtendedLifetime(currentStreamStopped.cancellable) {}
    }

    private func requireFailure<Success, Failure: Error>(
        _ result: Result<Success, Failure>
    ) throws {
        guard case .failure = result else {
            throw SignalboxNativeTestExpectationError("Expected a failure result")
        }
    }

    private func requireSignalboxClientError(
        _ operation: () async throws -> Void
    ) async throws -> SignalboxClientError {
        do {
            try await operation()
            throw SignalboxNativeTestExpectationError("Expected a Signalbox client error")
        } catch let error as SignalboxClientError {
            return error
        } catch {
            throw error
        }
    }

    private func requireToolCard(
        _ item: SignalboxTimelineItem
    ) throws -> SignalboxToolCard {
        guard case .tool(let card) = item else {
            throw SignalboxNativeTestExpectationError("Expected a tool timeline card")
        }
        return card
    }

    private func assertIncrementalNormalizationMatchesNaive(
        _ records: [SignalboxStoredEvent],
        file: StaticString = #filePath,
        line: UInt = #line
    ) {
        let incremental = SignalboxIncrementalEventNormalizer(records: records)
        let naiveTimeline = SignalboxEventNormalizer.normalize(records)
        XCTAssertEqual(incremental.timelineItems, naiveTimeline, file: file, line: line)
    }

    private func longSequenceNormalizationComparison() throws -> LongSequenceComparison {
        let timestamp = try SignalboxJSONCoding.decoder().decode(
            Date.self,
            from: Data(LongSequenceFixture.timestampJSON.utf8)
        )
        var naiveRecords: [SignalboxStoredEvent] = []
        var naiveTimeline: [SignalboxTimelineItem] = []
        var naiveMetrics = SignalboxEventNormalizationMetrics()
        let incremental = SignalboxIncrementalEventNormalizer()

        for eventID in LongSequenceFixture.eventIDRange {
            let record = longSequenceRecord(eventID: eventID, timestamp: timestamp)
            naiveRecords.append(record)
            naiveTimeline = SignalboxEventNormalizer.normalize(
                naiveRecords,
                recording: &naiveMetrics
            )
            incremental.upsert(record)
        }

        return LongSequenceComparison(
            naiveTimeline: naiveTimeline,
            incrementalTimeline: incremental.timelineItems,
            naiveMetrics: naiveMetrics,
            incrementalMetrics: incremental.metrics
        )
    }

    private func streamHelloHistoryReuseFixture() async throws -> StreamHelloHistoryReuseFixture {
        let sourceService = MockSignalboxService()
        let sessions = try await sourceService.listSessions(archived: false)
        let approvalSessionID = SignalboxSessionID(rawValue: MockSignalboxFixtures.approvalSessionID)
        let session = try XCTUnwrap(sessions.first { $0.id == approvalSessionID })
        let historyEvents = try await sourceService.listEvents(sessionID: session.id)
        let expectedSynchronizedEvents = try SignalboxJSONCoding.decoder().decode(
            [SignalboxStoredEvent].self,
            from: Data(MockSignalboxFixtures.approvedToolEvents.utf8)
        )
        let deletedEventID = try XCTUnwrap(historyEvents.first?.eventID)
        let helloEvents = Array(
            expectedSynchronizedEvents.suffix(
                StreamHelloHistoryReuseFixture.helloWindowEventCount
            )
        )
        XCTAssertEqual(
            helloEvents.count,
            StreamHelloHistoryReuseFixture.helloWindowEventCount
        )
        XCTAssertFalse(
            helloEvents.contains { $0.eventID == deletedEventID }
        )
        return StreamHelloHistoryReuseFixture(
            session: session,
            historyEvents: historyEvents,
            helloEvents: helloEvents,
            deletedEventID: deletedEventID,
            expectedSynchronizedEvents: expectedSynchronizedEvents,
            expectedMergedEvents: expectedSynchronizedEvents.filter {
                $0.eventID != deletedEventID
            }
        )
    }

    /// A visible user message whose identity and text derive from its event ID.
    private func longSequenceRecord(
        eventID: Int,
        timestamp: Date
    ) -> SignalboxStoredEvent {
        SignalboxStoredEvent(
            eventID: SignalboxEventID(rawValue: eventID),
            event: .message(
                SignalboxMessageEvent(
                    kind: LongSequenceFixture.messageKind,
                    message: SignalboxMessage(
                        role: .user,
                        parts: [
                            .text(
                                SignalboxTextContent(
                                    kind: LongSequenceFixture.textPartKind,
                                    text: "\(LongSequenceFixture.textPrefix)\(eventID)"
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
                    createdFrom: LongSequenceFixture.createdFrom
                )
            )
        )
    }

    private func requireUnknownServerMessage(
        _ message: SignalboxServerMessage
    ) throws -> (kind: String, diagnostic: SignalboxDecodingDiagnostic?) {
        guard case .unknown(let kind, _, let diagnostic) = message else {
            throw SignalboxNativeTestExpectationError("Expected an unknown server message")
        }
        return (kind, diagnostic)
    }

    private func requireStringWebSocketMessage(
        _ message: SignalboxWebSocketMessage
    ) throws -> String {
        guard case .string(let string) = message else {
            throw SignalboxNativeTestExpectationError("Expected a string WebSocket message")
        }
        return string
    }

    private func requireDecodingDiagnostic(
        _ message: SignalboxServerMessage
    ) throws -> SignalboxDecodingDiagnostic {
        guard case .diagnostic(let diagnostic) = message else {
            throw SignalboxNativeTestExpectationError("Expected a decoding diagnostic")
        }
        return diagnostic
    }

    private func requireUnknownConversationEvent(
        _ event: SignalboxConversationEvent
    ) throws -> SignalboxUnknownEvent {
        guard case .unknown(let unknownEvent) = event else {
            throw SignalboxNativeTestExpectationError("Expected an unknown conversation event")
        }
        return unknownEvent
    }

    private func requireWebSocketStreamError(
        _ nextMessage: Task<SignalboxServerMessage?, Error>
    ) async throws -> SignalboxWebSocketStreamError {
        do {
            _ = try await nextMessage.value
            throw SignalboxNativeTestExpectationError("Expected a WebSocket stream error")
        } catch let error as SignalboxWebSocketStreamError {
            return error
        } catch {
            throw error
        }
    }

    private func observeUnhandledFrame(
        _ kind: String,
        on viewModel: SessionDetailViewModel
    ) -> PublishedObservation {
        let expectation = expectation(description: "unknown frame recorded")
        let cancellable = viewModel.$unhandledFrameKinds
            .filter { $0[kind] == 1 }
            .first()
            .sink { _ in expectation.fulfill() }
        return PublishedObservation(expectation: expectation, cancellable: cancellable)
    }

    private func observeStaleCompletionRejection(
        on viewModel: SessionDetailViewModel
    ) -> PublishedObservation {
        let expectation = expectation(description: "stale completion rejected")
        let cancellable = viewModel.$ignoredStaleStreamCompletionCount
            .filter { $0 == 1 }
            .first()
            .sink { _ in expectation.fulfill() }
        return PublishedObservation(expectation: expectation, cancellable: cancellable)
    }

    private func observeStatus(
        _ expectedStatus: SignalboxSessionState,
        on viewModel: SessionDetailViewModel
    ) -> PublishedObservation {
        let expectation = expectation(description: "expected stream status applied")
        let cancellable = viewModel.$status
            .filter { $0.state == expectedStatus }
            .first()
            .sink { _ in expectation.fulfill() }
        return PublishedObservation(expectation: expectation, cancellable: cancellable)
    }

    private func observeCurrentStreamStopped(
        on viewModel: SessionDetailViewModel
    ) -> PublishedObservation {
        let expectation = expectation(description: "current stream stopped")
        let cancellable = viewModel.$isStreaming
            .filter { !$0 }
            .first()
            .sink { _ in expectation.fulfill() }
        return PublishedObservation(expectation: expectation, cancellable: cancellable)
    }
}

private enum LongSequenceFixture {
    /// Hundreds of events make a full-log normalization per append visibly
    /// quadratic while keeping the test fixture compact.
    static let eventIDRange = 1...400
    static let eventCount = eventIDRange.count
    static let firstEventID = eventIDRange.lowerBound
    static let secondEventID = firstEventID + 1

    /// One naive normalization after each append evaluates the triangular
    /// record total through `eventCount`.
    static let naiveRecordEvaluationCount = eventCount * (eventCount + 1) / 2

    /// Arbitrary shared timestamp for every generated message.
    static let timestampJSON = #""2026-05-10T12:00:00Z""#

    static let messageKind = "message"
    static let textPartKind = "text"
    static let textPrefix = "Long-sequence event "
    static let createdFrom = "test"
}

private struct LongSequenceComparison {
    let naiveTimeline: [SignalboxTimelineItem]
    let incrementalTimeline: [SignalboxTimelineItem]
    let naiveMetrics: SignalboxEventNormalizationMetrics
    let incrementalMetrics: SignalboxEventNormalizationMetrics
}

private struct StreamHelloHistoryReuseFixture {
    /// The hello contains the changed invocation and its new response, while
    /// the preceding records exist only in the paginated history.
    static let helloWindowEventCount = 2

    /// Loading history once and then applying hello must not fetch it again.
    static let expectedListEventsCallCount = 1

    /// One failed synchronized load plus one fallback load.
    static let expectedRecoveryListEventsCallCount = 2

    /// One initial snapshot plus one hello-bounded reconnect snapshot.
    static let expectedReconnectListEventsCallCount = 2

    /// The recovery fixture fails only its first history request.
    static let initialHistoryFailureCount = 1

    /// A local actor handoff should comfortably complete within this bound.
    static let observationTimeout: TimeInterval = 1

    /// A retry remains pending long enough for its hello to cancel the deadline.
    static let retryTimeout: Duration = .seconds(60)

    static let firstStreamInvocationCount = 1
    static let retryStreamInvocationCount = 2

    static let streamHelloKind = "stream_hello"
    static let bufferedDiagnosticMessage = "Controlled buffered diagnostic"
    static let emptyHistoryEvents: [SignalboxStoredEvent] = []

    let session: SignalboxSessionMetadata
    let historyEvents: [SignalboxStoredEvent]
    let helloEvents: [SignalboxStoredEvent]
    let deletedEventID: SignalboxEventID
    let expectedSynchronizedEvents: [SignalboxStoredEvent]
    let expectedMergedEvents: [SignalboxStoredEvent]
}

private struct PublishedObservation {
    let expectation: XCTestExpectation
    let cancellable: AnyCancellable
}

private struct SignalboxNativeTestExpectationError: LocalizedError {
    let errorDescription: String?

    init(_ description: String) {
        self.errorDescription = description
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

private final class StubSignalboxWebSocketTransportFactory: @unchecked Sendable {
    private let lock = NSLock()
    private var transports: [any SignalboxWebSocketTransport]
    private var creationCount = 0

    init(transports: [any SignalboxWebSocketTransport]) {
        self.transports = transports
    }

    var createdTransportCount: Int {
        lock.lock()
        defer { lock.unlock() }
        return creationCount
    }

    func makeTransport() -> any SignalboxWebSocketTransport {
        lock.lock()
        defer { lock.unlock() }
        precondition(!transports.isEmpty, "No stub WebSocket transport remains")
        creationCount += 1
        return transports.removeFirst()
    }
}

private final class ControlledSignalboxWebSocketWatchdogScheduler:
    @unchecked Sendable,
    SignalboxWebSocketWatchdogScheduler
{
    private struct ScheduledAction {
        let id: Int
        let action: @Sendable () async -> Void
        var isCancelled: Bool
    }

    private let lock = NSLock()
    private var nextID = 0
    private var scheduledActions: [ScheduledAction] = []
    private var scheduleCountWaiters: [Int: [CheckedContinuation<Void, Never>]] = [:]

    func schedule(
        after _: Duration,
        action: @escaping @Sendable () async -> Void
    ) -> any SignalboxWebSocketScheduledAction {
        lock.lock()
        let id = nextID
        nextID += 1
        scheduledActions.append(
            ScheduledAction(id: id, action: action, isCancelled: false)
        )
        let scheduleCount = scheduledActions.count
        let readyWaiters = scheduleCountWaiters
            .filter { expectedCount, _ in expectedCount <= scheduleCount }
            .flatMap(\.value)
        scheduleCountWaiters = scheduleCountWaiters.filter { expectedCount, _ in
            expectedCount > scheduleCount
        }
        lock.unlock()
        readyWaiters.forEach { $0.resume() }
        return ControlledSignalboxWebSocketScheduledAction(
            id: id,
            scheduler: self
        )
    }

    func waitForScheduleCount(_ expectedCount: Int) async {
        await withCheckedContinuation { continuation in
            lock.lock()
            if scheduledActions.count >= expectedCount {
                lock.unlock()
                continuation.resume()
            } else {
                scheduleCountWaiters[expectedCount, default: []].append(continuation)
                lock.unlock()
            }
        }
    }

    func fireLatest() async {
        let action = takeLatest()
        await action()
    }

    private func takeLatest() -> @Sendable () async -> Void {
        lock.lock()
        guard let index = scheduledActions.lastIndex(where: { !$0.isCancelled }) else {
            lock.unlock()
            preconditionFailure("No live watchdog action is scheduled")
        }
        let action = scheduledActions.remove(at: index).action
        lock.unlock()
        return action
    }

    fileprivate func cancel(id: Int) {
        lock.lock()
        let index = scheduledActions.firstIndex { $0.id == id }
        if let index {
            scheduledActions[index].isCancelled = true
        }
        lock.unlock()
    }
}

private final class ControlledSignalboxWebSocketScheduledAction:
    @unchecked Sendable,
    SignalboxWebSocketScheduledAction
{
    private let id: Int
    private weak var scheduler: ControlledSignalboxWebSocketWatchdogScheduler?

    init(
        id: Int,
        scheduler: ControlledSignalboxWebSocketWatchdogScheduler
    ) {
        self.id = id
        self.scheduler = scheduler
    }

    func cancel() {
        scheduler?.cancel(id: id)
    }
}

private enum StubSignalboxWebSocketTransportError: Error {
    case endOfStream
}

private final class ImmediateFirstStreamHelloTimeoutWaiter: @unchecked Sendable {
    private let lock = NSLock()
    private var invocationCount = 0

    func wait(for timeout: Duration) async throws {
        let expiresImmediately = lock.withLock {
            invocationCount += 1
            return invocationCount == StreamHelloHistoryReuseFixture.firstStreamInvocationCount
        }
        guard !expiresImmediately else {
            return
        }
        try await Task.sleep(for: timeout)
    }
}

private final class HistoryReuseStreamSignalboxService: SignalboxClientProtocol, @unchecked Sendable {
    /// This service intentionally models no artifacts on either load.
    private static let noArtifacts: [SignalboxArtifact] = []
    private static let historySynchronizationFailure = SignalboxClientError.requestFailed(
        "Controlled initial history failure"
    )

    private let fixture: StreamHelloHistoryReuseFixture
    private let historyEvents: [SignalboxStoredEvent]
    private let lock = NSLock()
    private var lockedListEventsCallCount = 0
    private var lockedStreamInvocationCount = 0
    private var lockedHistoryFailuresRemaining: Int
    private var listEventsContinuation: CheckedContinuation<[SignalboxStoredEvent], Never>?
    private var listEventsInvocationWaiters: [CheckedContinuation<Void, Never>] = []
    private var streamContinuation: AsyncThrowingStream<SignalboxServerMessage, Error>.Continuation?
    private var streamInvocationWaiters: [Int: [CheckedContinuation<Void, Never>]] = [:]

    init(
        fixture: StreamHelloHistoryReuseFixture,
        historyEvents: [SignalboxStoredEvent]? = nil,
        historyFailureCount: Int = 0
    ) {
        self.fixture = fixture
        self.historyEvents = historyEvents ?? fixture.historyEvents
        self.lockedHistoryFailuresRemaining = historyFailureCount
    }

    var listEventsCallCount: Int {
        lock.withLock {
            lockedListEventsCallCount
        }
    }

    var streamInvocationCount: Int {
        lock.withLock {
            lockedStreamInvocationCount
        }
    }

    func waitForStreamInvocation() async {
        await waitForStreamInvocationCount(
            StreamHelloHistoryReuseFixture.firstStreamInvocationCount
        )
    }

    func waitForStreamInvocationCount(_ expectedCount: Int) async {
        await withCheckedContinuation { continuation in
            lock.lock()
            if lockedStreamInvocationCount >= expectedCount {
                lock.unlock()
                continuation.resume()
            } else {
                streamInvocationWaiters[expectedCount, default: []].append(continuation)
                lock.unlock()
            }
        }
    }

    func waitForListEventsInvocation() async {
        await withCheckedContinuation { continuation in
            lock.lock()
            if listEventsContinuation != nil {
                lock.unlock()
                continuation.resume()
            } else {
                listEventsInvocationWaiters.append(continuation)
                lock.unlock()
            }
        }
    }

    func sendStreamHello() {
        let continuation = lock.withLock {
            guard let continuation = streamContinuation else {
                preconditionFailure("No stream invocation is ready")
            }
            return continuation
        }
        continuation.yield(
            .streamHello(
                SignalboxStreamHello(
                    kind: StreamHelloHistoryReuseFixture.streamHelloKind,
                    session: fixture.session,
                    status: SignalboxSessionStatus(state: .waitingForConfirmation),
                    events: fixture.helloEvents
                )
            )
        )
    }

    func sendDiagnostic() {
        let continuation = lock.withLock {
            guard let continuation = streamContinuation else {
                preconditionFailure("No stream invocation is ready")
            }
            return continuation
        }
        continuation.yield(
            .diagnostic(
                SignalboxDecodingDiagnostic(
                    message: StreamHelloHistoryReuseFixture.bufferedDiagnosticMessage
                )
            )
        )
    }

    func sendDeletedEventAndFinish() {
        let continuation = lock.withLock {
            guard let continuation = streamContinuation else {
                preconditionFailure("No stream invocation is ready")
            }
            streamContinuation = nil
            return continuation
        }
        continuation.yield(.eventDeleted(fixture.deletedEventID))
        continuation.finish()
    }

    func resumeHistoryEvents() {
        let continuation = lock.withLock {
            guard let continuation = listEventsContinuation else {
                preconditionFailure("No history load is ready")
            }
            listEventsContinuation = nil
            return continuation
        }
        continuation.resume(returning: historyEvents)
    }

    func testConnection() async throws {
        throw unexpectedCall()
    }
    func listTemplates() async throws -> [SignalboxTemplate] {
        throw unexpectedCall()
    }
    func listRunners() async throws -> [SignalboxRunner] {
        throw unexpectedCall()
    }
    func listSessions(archived: Bool) async throws -> [SignalboxSessionMetadata] {
        throw unexpectedCall()
    }
    func createSession(request: SignalboxCreateSessionRequest) async throws -> SignalboxSessionView {
        throw unexpectedCall()
    }
    func patchSessionArchive(
        sessionID: SignalboxSessionID,
        isArchived: Bool
    ) async throws -> SignalboxSessionMetadata {
        throw unexpectedCall()
    }
    func listEvents(sessionID: SignalboxSessionID) async throws -> [SignalboxStoredEvent] {
        let shouldFail = lock.withLock {
            lockedListEventsCallCount += 1
            if lockedHistoryFailuresRemaining > 0 {
                lockedHistoryFailuresRemaining -= 1
                return true
            }
            return false
        }
        if shouldFail {
            throw Self.historySynchronizationFailure
        }
        return await withCheckedContinuation { continuation in
            lock.lock()
            precondition(
                listEventsContinuation == nil,
                "History load is already pending"
            )
            listEventsContinuation = continuation
            let waiters = listEventsInvocationWaiters
            listEventsInvocationWaiters = []
            lock.unlock()
            waiters.forEach { $0.resume() }
        }
    }
    func appendUserMessage(
        sessionID: SignalboxSessionID,
        text: String
    ) async throws -> SignalboxAppendUserMessageResponse {
        throw unexpectedCall()
    }
    func confirmInvocation(
        sessionID: SignalboxSessionID,
        invocationID: SignalboxToolInvocationID
    ) async throws {
        throw unexpectedCall()
    }
    func denyInvocation(
        sessionID: SignalboxSessionID,
        invocationID: SignalboxToolInvocationID,
        reason: String?
    ) async throws {
        throw unexpectedCall()
    }
    func listArtifacts(sessionID: SignalboxSessionID) async throws -> [SignalboxArtifact] {
        Self.noArtifacts
    }
    func listMonitorSessions() async throws -> [SignalboxMonitorSessionSummary] {
        throw unexpectedCall()
    }

    func streamMessages(
        sessionID: SignalboxSessionID
    ) -> AsyncThrowingStream<SignalboxServerMessage, Error> {
        AsyncThrowingStream { continuation in
            lock.lock()
            streamContinuation = continuation
            lockedStreamInvocationCount += 1
            let invocationCount = lockedStreamInvocationCount
            let waiters = streamInvocationWaiters
                .filter { expectedCount, _ in expectedCount <= invocationCount }
                .flatMap(\.value)
            streamInvocationWaiters = streamInvocationWaiters.filter { expectedCount, _ in
                expectedCount > invocationCount
            }
            lock.unlock()
            waiters.forEach { $0.resume() }
        }
    }

    private func unexpectedCall() -> SignalboxClientError {
        .requestFailed("Unexpected history-reuse fixture call")
    }
}

private final class UnknownFrameSignalboxService: SignalboxClientProtocol, @unchecked Sendable {
    static let frameKind = "turn_started"

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
                    kind: Self.frameKind,
                    payload: ["turn_id": .string("turn-1")],
                    decodingDiagnostic: nil
                )
            )
            continuation.finish()
        }
    }
}

private final class ControlledReconnectSignalboxService: SignalboxClientProtocol, @unchecked Sendable {
    private let lock = NSLock()
    private var streamContinuations: [AsyncThrowingStream<SignalboxServerMessage, Error>.Continuation] = []
    private var invocationWaiters: [Int: [CheckedContinuation<Void, Never>]] = [:]

    func waitForStreamInvocationCount(_ expectedCount: Int) async {
        await withCheckedContinuation { continuation in
            lock.lock()
            if streamContinuations.count >= expectedCount {
                lock.unlock()
                continuation.resume()
            } else {
                invocationWaiters[expectedCount, default: []].append(continuation)
                lock.unlock()
            }
        }
    }

    func finishStream(at index: Int) {
        lock.lock()
        let continuation = streamContinuations[index]
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
            lock.lock()
            streamContinuations.append(continuation)
            let invocationCount = streamContinuations.count
            let readyWaiters = invocationWaiters
                .filter { expectedCount, _ in expectedCount <= invocationCount }
                .flatMap(\.value)
            invocationWaiters = invocationWaiters.filter { expectedCount, _ in
                expectedCount > invocationCount
            }
            lock.unlock()
            readyWaiters.forEach { $0.resume() }
        }
    }
}

private actor QuietSignalboxWebSocketTransport: SignalboxWebSocketTransport {
    private var heartbeat: SignalboxWebSocketMessage?
    private(set) var sentMessages: [SignalboxWebSocketMessage] = []
    private var sentMessageWaiters: [Int: [CheckedContinuation<Void, Never>]] = [:]

    init(heartbeat: SignalboxWebSocketMessage?) {
        self.heartbeat = heartbeat
    }

    func receive() async throws -> SignalboxWebSocketMessage {
        if let heartbeat {
            self.heartbeat = nil
            return heartbeat
        }
        try await Task.sleep(for: .seconds(60))
        throw StubSignalboxWebSocketTransportError.endOfStream
    }

    func send(_ message: SignalboxWebSocketMessage) async throws {
        sentMessages.append(message)
        let sentMessageCount = sentMessages.count
        let readyWaiters = sentMessageWaiters
            .filter { expectedCount, _ in expectedCount <= sentMessageCount }
            .flatMap(\.value)
        sentMessageWaiters = sentMessageWaiters.filter { expectedCount, _ in
            expectedCount > sentMessageCount
        }
        readyWaiters.forEach { $0.resume() }
    }

    func waitForSentMessageCount(_ expectedCount: Int) async {
        await withCheckedContinuation { continuation in
            if sentMessages.count >= expectedCount {
                continuation.resume()
            } else {
                sentMessageWaiters[expectedCount, default: []].append(continuation)
            }
        }
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
