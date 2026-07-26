import XCTest

@testable import SignalboxNative

final class ProcessServiceIntegrationTests: XCTestCase {
  func testMockHarnessListsRealMetadataFrames() async throws {
    let service = makeService(policy: ProcessDriverFixture.oneRowMetadataPolicy)

    let sessions = try await service.listSessions(includeArchived: true)

    XCTAssertEqual(sessions.count, MockProcessProtocolFixtures.sessionCount)
  }

  func testArchiveUsesCompleteMetadataReplace() async throws {
    let service = makeService()
    let before = try await service.listSessions(includeArchived: true)
    let subject = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: before)

    let replacement = try await service.setArchived(true, session: subject)
    let after = try await service.listSessions(includeArchived: true)

    XCTAssertTrue(replacement.archived)
    XCTAssertEqual(after.first { $0.id == replacement.id }?.archived, true)
  }

  func testPreparedSubmissionUsesReceiptIdentity() async throws {
    let service = makeService()
    let sessions = try await service.listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let content = "fixture owner input"

    let prepared = try await service.prepareInputSubmission(
      session: session,
      content: content
    )
    let submitted = try await service.submit(prepared)

    XCTAssertEqual(submitted.sessionID, session.id)
    XCTAssertEqual(submitted.turnID.rawValue, MockProcessProtocolFixtures.submittedTurnID)
  }

  func testDriverPublishesSnapshotThatProjectsThroughIncrementalNormalizer() async throws {
    let service = makeService()
    let sessions = try await service.listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let recorder = ProcessDriverUpdateRecorder()
    let synchronization = await service.makeSynchronization(sessionID: session.id) {
      await recorder.append($0)
    }

    await synchronization.start()
    let snapshot = try await recorder.authoritativeSnapshot()
    await synchronization.stop()
    var projector = SignalboxProcessTranscriptProjector()
    let projection = try projector.projectAuthoritativeSnapshot(snapshot)
    let normalizer = try SignalboxIncrementalEventNormalizer(records: projection.records)

    XCTAssertEqual(
      projection.records.count,
      MockProcessProtocolFixtures.conversationRecordCount
    )
    XCTAssertEqual(normalizer.timelineItems.count, projection.records.count)
    XCTAssertEqual(projection.activity, .unavailable)
  }

  func testMalformedProcessPresentationEventDegradesToUnknownRecord() throws {
    let record = try SignalboxJSONCoding.decoder().decode(
      SignalboxStoredEvent.self,
      from: ProcessPresentationFixture.malformedMessage
    )

    let unknown = try unknownEvent(record.event)

    XCTAssertEqual(unknown.kind, ProcessPresentationFixture.messageKind)
    XCTAssertEqual(
      unknown.decodingDiagnostic?.message,
      ProcessPresentationFixture.missingTextDiagnostic
    )
  }

  @MainActor
  func testFailedSubmissionPreservesComposer() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      RejectingProcessService()
    }
    viewModel.composerText = ProcessSubmissionFixture.content

    await viewModel.send()

    XCTAssertEqual(viewModel.composerText, ProcessSubmissionFixture.content)
    XCTAssertEqual(viewModel.errorMessage, ProcessSubmissionFixture.failureMessage)
  }

  func testDriverSerializesSideMergeBeforeNewerPrimaryEvent() async throws {
    let requester = ControlledSynchronizationRequester()
    let recorder = OrderedProcessDriverUpdateRecorder()
    let driver = SignalboxSessionSynchronizationDriver(
      requester: requester,
      sessionID: try ProcessDriverFixture.sessionID(),
      policy: SignalboxProcessApplicationPolicy.nativeDefault.synchronization
    ) {
      await recorder.append($0)
    }

    await driver.start()
    await requester.waitForFollowOpen()
    await requester.primary.send(
      try ProcessDriverFixture.snapshotStart(cursor: ProcessDriverFixture.snapshotCursor)
    )
    await requester.primary.send(
      try ProcessDriverFixture.snapshotEnd(cursor: ProcessDriverFixture.snapshotCursor)
    )
    await requester.primary.waitForNextCallCount(ProcessDriverFixture.initialFollowReadCount)
    await requester.primary.send(
      try ProcessDriverFixture.completedEvent(cursor: ProcessDriverFixture.triggerCursor)
    )
    await requester.waitForSideOpen()
    await requester.primary.send(
      try ProcessDriverFixture.preparedEvent(cursor: ProcessDriverFixture.bufferedCursor)
    )
    await requester.primary.waitForNextCallCount(ProcessDriverFixture.bufferedFollowReadCount)
    await requester.side.send(
      try ProcessDriverFixture.snapshotStart(cursor: ProcessDriverFixture.triggerCursor)
    )
    await requester.side.waitForNextCallCount(ProcessDriverFixture.sideEndReadCount)
    await recorder.pauseNextPhase()
    await requester.side.send(
      try ProcessDriverFixture.snapshotEnd(cursor: ProcessDriverFixture.triggerCursor)
    )
    await recorder.waitUntilPhaseIsPaused()
    await requester.primary.send(
      try ProcessDriverFixture.activatedEvent(cursor: ProcessDriverFixture.newerCursor)
    )
    await recorder.releasePausedPhase()
    let cursors = try await recorder.eventCursors(count: ProcessDriverFixture.expectedCursors.count)
    await driver.stop()

    XCTAssertEqual(cursors, ProcessDriverFixture.expectedCursors)
  }

  func testSideProjectionIncludesMaterializedUserEntryForTriggerTurn() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithUserEntry()
    let trigger = try ProcessProjectionFixture.completedTrigger()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectSideSnapshot(snapshot, attributableTo: trigger)
    let message = try ProcessProjectionFixture.onlyMessage(in: projection)

    XCTAssertEqual(message.role, .user)
    XCTAssertEqual(message.text, ProcessProjectionFixture.userText)
    XCTAssertEqual(
      projection.materializedAcceptedInputIDs,
      try ProcessProjectionFixture.materializedAcceptedInputIDs()
    )
  }

  func testProposedToolSideProjectionIncludesProducingAssistantText() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithProposedTool()
    let trigger = try ProcessProjectionFixture.proposedToolTrigger()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectSideSnapshot(snapshot, attributableTo: trigger)
    let message = try ProcessProjectionFixture.onlyMessage(in: projection)
    let tool = try ProcessProjectionFixture.onlyTool(in: projection)

    XCTAssertEqual(message.text, ProcessProjectionFixture.proposedAssistantText)
    XCTAssertEqual(tool.toolName, ProcessProjectionFixture.proposedToolName)
  }

  @MainActor
  func testSideSnapshotPreservesOrderedStateAndReconcilesTriggerInput() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }
    viewModel.apply(.event(try ProcessProjectionFixture.acceptedEvent()))
    viewModel.apply(.event(try ProcessProjectionFixture.activatedEvent()))

    viewModel.apply(
      .sideSnapshot(
        snapshot: try ProcessProjectionFixture.snapshotWithUserEntry(),
        trigger: try ProcessProjectionFixture.completedTrigger()
      )
    )

    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.runningActivity)
    XCTAssertTrue(viewModel.pendingInputs.isEmpty)
  }

  @MainActor
  func testSavingSocketPathInvalidatesInstalledService() {
    let coordinator = AppCoordinator(isMockMode: false, resetPersistedSettings: true)
    coordinator.processService = RejectingProcessService()
    coordinator.processSettings.socketPath = ProcessSubmissionFixture.replacementSocketPath

    coordinator.saveProcessSocketPath()

    XCTAssertNil(coordinator.processService)
    XCTAssertEqual(coordinator.processSettings.connectionStatus, .unknown)
  }

  @MainActor
  func testAmbiguousSubmissionRetryReusesPreparedCommandIdentity() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let service = AmbiguousThenAcceptingProcessService()
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      service
    }
    viewModel.composerText = ProcessSubmissionFixture.content

    await viewModel.send()
    await viewModel.send()
    let submittedCommandIDs = await service.submittedCommandIDs

    XCTAssertEqual(submittedCommandIDs, ProcessSubmissionFixture.retriedCommandIDs)
  }

  @MainActor
  func testEditedComposerPreparesNewCommandAfterAmbiguousSubmission() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let service = AmbiguousThenAcceptingProcessService()
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      service
    }
    viewModel.composerText = ProcessSubmissionFixture.content

    await viewModel.send()
    viewModel.composerText = ProcessSubmissionFixture.replacementContent
    await viewModel.send()
    let submittedCommandIDs = await service.submittedCommandIDs
    let submittedContents = await service.submittedContents

    XCTAssertEqual(submittedCommandIDs, ProcessSubmissionFixture.editedComposerCommandIDs)
    XCTAssertEqual(submittedContents, ProcessSubmissionFixture.editedComposerContents)
  }

  @MainActor
  func testCanonicallyEquivalentEditPreparesNewCommandAfterAmbiguity() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let service = AmbiguousThenAcceptingProcessService()
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      service
    }
    viewModel.composerText = ProcessSubmissionFixture.precomposedContent

    await viewModel.send()
    viewModel.composerText = ProcessSubmissionFixture.decomposedContent
    await viewModel.send()
    let submittedCommandIDs = await service.submittedCommandIDs
    let submittedContentBytes = await service.submittedContentBytes

    XCTAssertEqual(submittedCommandIDs, ProcessSubmissionFixture.editedComposerCommandIDs)
    XCTAssertEqual(submittedContentBytes, ProcessSubmissionFixture.canonicallyEditedContentBytes)
  }

  @MainActor
  func testSubmissionPreservesExactNonblankComposerText() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let service = ImmediateAcceptingProcessService()
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      service
    }
    viewModel.composerText = ProcessSubmissionFixture.whitespaceSensitiveContent

    await viewModel.send()
    let submittedContents = await service.submittedContents

    XCTAssertEqual(submittedContents, ProcessSubmissionFixture.whitespaceSensitiveContents)
  }

  @MainActor
  func testSubmissionReceiptDeduplicatesFollowedAcceptedInput() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      ImmediateAcceptingProcessService()
    }
    viewModel.apply(.event(try ProcessProjectionFixture.acceptedEvent()))
    viewModel.composerText = ProcessSubmissionFixture.content

    await viewModel.send()

    XCTAssertEqual(viewModel.pendingInputs, try ProcessSubmissionFixture.singlePendingInput())
  }

  @MainActor
  func testConcurrentSendIsSuppressed() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let service = SuspendedSubmissionService()
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      service
    }
    viewModel.composerText = ProcessSubmissionFixture.content

    let firstSend = Task { await viewModel.send() }
    await service.waitUntilSubmitStarted()
    await viewModel.send()
    let callsWhileSuspended = await service.callCounts
    await service.completeSubmission()
    await firstSend.value
    let finalCalls = await service.callCounts

    XCTAssertEqual(callsWhileSuspended, ProcessSubmissionFixture.singleCallCounts)
    XCTAssertEqual(finalCalls, ProcessSubmissionFixture.singleCallCounts)
    XCTAssertFalse(viewModel.isSubmitting)
  }

  @MainActor
  func testSubmissionReceiptDoesNotRestoreMaterializedPendingInput() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let service = SuspendedSubmissionService()
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      service
    }
    viewModel.composerText = ProcessSubmissionFixture.content

    let send = Task { await viewModel.send() }
    await service.waitUntilSubmitStarted()
    viewModel.apply(
      .sideSnapshot(
        snapshot: try ProcessProjectionFixture.snapshotWithUserEntry(),
        trigger: try ProcessProjectionFixture.completedTrigger()
      )
    )
    await service.completeSubmission()
    await send.value
    viewModel.apply(.event(try ProcessProjectionFixture.acceptedEvent()))
    let message = try ProcessProjectionFixture.onlyTimelineMessage(in: viewModel.timeline)

    XCTAssertTrue(viewModel.pendingInputs.isEmpty)
    XCTAssertEqual(message.text, ProcessProjectionFixture.userText)
    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.unavailableActivity)
  }

  @MainActor
  func testRefusedTurnRemovesMatchingPendingInput() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }
    viewModel.apply(.event(try ProcessProjectionFixture.acceptedEvent()))

    viewModel.apply(.event(try ProcessProjectionFixture.refusedEvent()))

    XCTAssertTrue(viewModel.pendingInputs.isEmpty)
    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.refusedActivity)
  }

  func testConnectionRejectsMetadataProbeWithoutTerminalBoundary() async throws {
    let requester = StaticProcessRequester(
      frames: [try ProcessDriverFixture.metadataPageStart()]
    )
    let service = SignalboxProcessService(
      requester: requester,
      policy: .nativeDefault
    )

    let error = await capturedServiceError {
      try await service.testConnection()
    }

    XCTAssertEqual(error, ProcessDriverFixture.incompleteMetadataPageError)
  }

  func testMutationReceiptLossRetriesTheSameDurableCommand() async throws {
    let submission = try ProcessSubmissionFixture.preparedSubmission()
    let requester = SequencedProcessRequester(
      pages: [
        [],
        [try ProcessDriverFixture.inputSubmitted()],
      ]
    )
    let service = SignalboxProcessService(
      requester: requester,
      policy: ProcessDriverFixture.oneImmediateMutationRetryPolicy
    )

    let receipt = try await service.submit(submission)
    let openedRequests = await requester.openedRequests

    XCTAssertEqual(receipt, try ProcessSubmissionFixture.submittedReceipt())
    XCTAssertEqual(
      openedRequests,
      ProcessSubmissionFixture.retriedRequests(for: submission)
    )
  }

  func testConnectionRejectsMetadataPageAboveRequestedCapacity() async throws {
    let requester = StaticProcessRequester(
      frames: [
        try ProcessDriverFixture.metadataPageStart(),
        try ProcessDriverFixture.metadataSummary(sessionID: ProcessDriverFixture.metadataSessionA),
        try ProcessDriverFixture.metadataSummary(sessionID: ProcessDriverFixture.metadataSessionB),
      ]
    )
    let service = SignalboxProcessService(
      requester: requester,
      policy: .nativeDefault
    )

    let error = await capturedServiceError {
      try await service.testConnection()
    }

    XCTAssertEqual(error, ProcessDriverFixture.metadataCapacityError)
  }

  func testConnectionRejectsMismatchedMetadataEndCursor() async throws {
    let requester = StaticProcessRequester(
      frames: [
        try ProcessDriverFixture.metadataPageStart(),
        try ProcessDriverFixture.metadataSummary(sessionID: ProcessDriverFixture.metadataSessionA),
        try ProcessDriverFixture.metadataPageEnd(
          count: ProcessDriverFixture.singleMetadataCount,
          nextSessionID: ProcessDriverFixture.metadataSessionB
        ),
      ]
    )
    let service = SignalboxProcessService(
      requester: requester,
      policy: .nativeDefault
    )

    let error = await capturedServiceError {
      try await service.testConnection()
    }

    XCTAssertEqual(error, ProcessDriverFixture.metadataEndCursorError)
  }

  func testMetadataPageRejectsSummaryAtRequestCursor() async throws {
    let requester = SequencedProcessRequester(
      pages: [
        [
          try ProcessDriverFixture.metadataPageStart(),
          try ProcessDriverFixture.metadataSummary(
            sessionID: ProcessDriverFixture.metadataSessionA
          ),
          try ProcessDriverFixture.metadataPageEnd(
            count: ProcessDriverFixture.singleMetadataCount,
            nextSessionID: ProcessDriverFixture.metadataSessionA
          ),
        ],
        [
          try ProcessDriverFixture.metadataPageStart(),
          try ProcessDriverFixture.metadataSummary(
            sessionID: ProcessDriverFixture.metadataSessionA
          ),
        ],
      ]
    )
    let service = SignalboxProcessService(
      requester: requester,
      policy: .nativeDefault
    )

    let error = await capturedServiceError {
      _ = try await service.listSessions(includeArchived: false)
    }

    XCTAssertEqual(error, ProcessDriverFixture.metadataRequestCursorError)
  }

  func testMarkdownScreenshotHarnessProjectsScenarioSpecificContent() async throws {
    let service = makeService(scenario: .markdownCode)
    let sessions = try await service.listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.markdownCodeSessionID, in: sessions)
    let snapshot = try await authoritativeSnapshot(service: service, session: session)
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)
    let assistant = try ProcessProjectionFixture.assistantMessage(in: projection)

    XCTAssertEqual(assistant.text, MockSignalboxFixtures.markdownCodeAssistantText)
  }

  func testCompletedToolScreenshotHarnessProjectsCompletedTool() async throws {
    let service = makeService(scenario: .completedTool)
    let sessions = try await service.listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let snapshot = try await authoritativeSnapshot(service: service, session: session)
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)
    let tool = try ProcessProjectionFixture.onlyTool(in: projection)

    XCTAssertEqual(tool.toolName, MockProcessProtocolFixtures.completedToolName)
    XCTAssertEqual(tool.output, MockProcessProtocolFixtures.completedToolOutput)
    XCTAssertEqual(tool.status, .completed)
  }

  func testCompletedProcessToolNormalizesToNeutralCard() async throws {
    let service = makeService(scenario: .completedTool)
    let sessions = try await service.listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let snapshot = try await authoritativeSnapshot(service: service, session: session)
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)
    let normalizer = try SignalboxIncrementalEventNormalizer(records: projection.records)
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(tool.status, ProcessProjectionFixture.neutralToolCardStatus)
  }

  private func makeService(
    scenario: ScreenshotScenario? = nil,
    policy: SignalboxProcessApplicationPolicy = .nativeDefault
  ) -> SignalboxProcessService {
    SignalboxProcessService(
      requester: SignalboxProcessClient(
        connectionFactory: MockProcessProtocolConnectionFactory(scenario: scenario)
      ),
      policy: policy
    )
  }

  private func authoritativeSnapshot(
    service: SignalboxProcessService,
    session: SignalboxProcessSession
  ) async throws -> SignalboxSynchronizationSnapshot {
    let recorder = ProcessDriverUpdateRecorder()
    let synchronization = await service.makeSynchronization(sessionID: session.id) {
      await recorder.append($0)
    }
    await synchronization.start()
    let snapshot = try await recorder.authoritativeSnapshot()
    await synchronization.stop()
    return snapshot
  }

  private func fixtureSession(
    _ sessionID: String,
    in sessions: [SignalboxProcessSession]
  ) throws -> SignalboxProcessSession {
    guard let session = sessions.first(where: { $0.id.rawValue == sessionID }) else {
      throw ProcessDriverUpdateRecorderError.missingFixtureSession
    }
    return session
  }

  private func unknownEvent(
    _ event: SignalboxConversationEvent
  ) throws -> SignalboxUnknownEvent {
    guard case .unknown(let unknown) = event else {
      throw ProcessDriverUpdateRecorderError.expectedUnknownEvent
    }
    return unknown
  }

  private func capturedServiceError(
    operation: () async throws -> Void
  ) async -> SignalboxProcessServiceError? {
    do {
      try await operation()
      return nil
    } catch let error as SignalboxProcessServiceError {
      return error
    } catch {
      return nil
    }
  }
}

private enum ProcessPresentationFixture {
  static let messageKind = "process_message"
  static let missingTextDiagnostic = "Missing required field at event.text."
  static let malformedMessage = Data(
    """
    {"event_id":41,"event":{"kind":"process_message","role":"assistant"}}
    """.utf8
  )
}

private enum ProcessSubmissionFixture {
  static let content = "fixture composer draft"
  static let replacementContent = "fixture replacement composer draft"
  static let precomposedContent = "fixture caf\u{00e9}"
  static let decomposedContent = "fixture cafe\u{0301}"
  static let whitespaceSensitiveContent = "  fixture indented composer draft\n"
  static let commandID = "abababab-0000-4000-8000-000000000001"
  static let replacementCommandID = "abababab-0000-4000-8000-000000000004"
  static let failureMessage = "Fixture submission rejection."
  static let replacementSocketPath = "/tmp/signalbox-review-fixture.sock"
  static let acceptedInputID = "abababab-0000-4000-8000-000000000002"
  static let acceptedTurnID = "abababab-0000-4000-8000-000000000003"
  static let retriedCommandIDs = [commandID, commandID]
  static let editedComposerCommandIDs = [commandID, replacementCommandID]
  static let editedComposerContents = [content, replacementContent]
  static let canonicallyEditedContentBytes = [
    Array(precomposedContent.utf8),
    Array(decomposedContent.utf8),
  ]
  static let whitespaceSensitiveContents = [whitespaceSensitiveContent]
  static let singleCallCounts = SubmissionCallCounts(prepare: 1, submit: 1)

  static func submittedReceipt(
    sessionID: SignalboxCanonicalUUID
  ) throws -> SignalboxInputSubmitted {
    try SignalboxJSONCoding.decoder().decode(
      SignalboxInputSubmitted.self,
      from: Data(
        """
        {
          "session_id":"\(sessionID.rawValue)",
          "accepted_input_id":"\(acceptedInputID)",
          "acceptance_position":"1",
          "turn_id":"\(acceptedTurnID)"
        }
        """.utf8
      )
    )
  }

  static func submittedReceipt() throws -> SignalboxInputSubmitted {
    try submittedReceipt(
      sessionID: SignalboxCanonicalUUID(validating: ProcessDriverFixture.session)
    )
  }

  static func preparedSubmission() throws -> SignalboxPreparedInputSubmission {
    SignalboxPreparedInputSubmission(
      commandID: try SignalboxCommandID(validating: commandID),
      sessionID: try SignalboxCanonicalUUID(validating: ProcessDriverFixture.session),
      content: content,
      expectedDefaultsVersion: SignalboxCanonicalUInt64(rawValue: 1)
    )
  }

  static func retriedRequests(
    for submission: SignalboxPreparedInputSubmission
  ) -> [SignalboxProcessClientRequest] {
    let request = SignalboxProcessClientRequest.submitInput(
      commandID: submission.commandID,
      sessionID: submission.sessionID,
      content: submission.content,
      expectedDefaultsVersion: submission.expectedDefaultsVersion
    )
    return [request, request]
  }

  static func usesReplacementCommand(for content: String) -> Bool {
    content.utf8.elementsEqual(replacementContent.utf8)
      || content.utf8.elementsEqual(decomposedContent.utf8)
  }

  static func singlePendingInput() throws -> [SignalboxProcessPendingInput] {
    [
      SignalboxProcessPendingInput(
        id: try SignalboxCanonicalUUID(validating: acceptedInputID),
        turnID: try SignalboxCanonicalUUID(validating: acceptedTurnID),
        content: content
      )
    ]
  }
}

private struct SubmissionCallCounts: Equatable, Sendable {
  let prepare: Int
  let submit: Int
}

private struct RejectingProcessService: SignalboxProcessServiceProtocol {
  func testConnection() async throws {
    throw ProcessSubmissionFixtureError.rejected
  }

  func listSessions(includeArchived: Bool) async throws -> [SignalboxProcessSession] {
    throw ProcessSubmissionFixtureError.rejected
  }

  func setArchived(
    _ archived: Bool,
    session: SignalboxProcessSession
  ) async throws -> SignalboxProcessSession {
    throw ProcessSubmissionFixtureError.rejected
  }

  func prepareInputSubmission(
    session: SignalboxProcessSession,
    content: String
  ) async throws -> SignalboxPreparedInputSubmission {
    SignalboxPreparedInputSubmission(
      commandID: try SignalboxCommandID(validating: ProcessSubmissionFixture.commandID),
      sessionID: session.id,
      content: content,
      expectedDefaultsVersion: session.defaultsVersion
    )
  }

  func submit(
    _ submission: SignalboxPreparedInputSubmission
  ) async throws -> SignalboxInputSubmitted {
    throw ProcessSubmissionFixtureError.rejected
  }

  func makeSynchronization(
    sessionID: SignalboxCanonicalUUID,
    updates: @escaping @Sendable (SignalboxSessionSynchronizationDriverUpdate) async -> Void
  ) async -> any SignalboxSessionSynchronizing {
    NoopProcessSynchronization()
  }
}

private struct NoopProcessSynchronization: SignalboxSessionSynchronizing {
  func start() async {}
  func stop() async {}
}

private actor AmbiguousThenAcceptingProcessService: SignalboxProcessServiceProtocol {
  private(set) var submittedCommandIDs: [String] = []
  private(set) var submittedContents: [String] = []
  private(set) var submittedContentBytes: [[UInt8]] = []

  func testConnection() async throws {}

  func listSessions(includeArchived: Bool) async throws -> [SignalboxProcessSession] {
    []
  }

  func setArchived(
    _ archived: Bool,
    session: SignalboxProcessSession
  ) async throws -> SignalboxProcessSession {
    session
  }

  func prepareInputSubmission(
    session: SignalboxProcessSession,
    content: String
  ) async throws -> SignalboxPreparedInputSubmission {
    let commandID =
      if ProcessSubmissionFixture.usesReplacementCommand(for: content) {
        ProcessSubmissionFixture.replacementCommandID
      } else {
        ProcessSubmissionFixture.commandID
      }
    return SignalboxPreparedInputSubmission(
      commandID: try SignalboxCommandID(validating: commandID),
      sessionID: session.id,
      content: content,
      expectedDefaultsVersion: session.defaultsVersion
    )
  }

  func submit(
    _ submission: SignalboxPreparedInputSubmission
  ) async throws -> SignalboxInputSubmitted {
    submittedCommandIDs.append(submission.commandID.rawValue.rawValue)
    submittedContents.append(submission.content)
    submittedContentBytes.append(Array(submission.content.utf8))
    guard submittedCommandIDs.count > 1 else {
      throw SignalboxProcessServiceError.mutationRetryExhausted(
        code: .commitAmbiguous,
        message: ProcessSubmissionFixture.failureMessage
      )
    }
    return try ProcessSubmissionFixture.submittedReceipt(sessionID: submission.sessionID)
  }

  func makeSynchronization(
    sessionID: SignalboxCanonicalUUID,
    updates: @escaping @Sendable (SignalboxSessionSynchronizationDriverUpdate) async -> Void
  ) async -> any SignalboxSessionSynchronizing {
    NoopProcessSynchronization()
  }
}

private actor ImmediateAcceptingProcessService: SignalboxProcessServiceProtocol {
  private(set) var submittedContents: [String] = []

  func testConnection() async throws {}

  func listSessions(includeArchived: Bool) async throws -> [SignalboxProcessSession] {
    []
  }

  func setArchived(
    _ archived: Bool,
    session: SignalboxProcessSession
  ) async throws -> SignalboxProcessSession {
    session
  }

  func prepareInputSubmission(
    session: SignalboxProcessSession,
    content: String
  ) async throws -> SignalboxPreparedInputSubmission {
    SignalboxPreparedInputSubmission(
      commandID: try SignalboxCommandID(validating: ProcessSubmissionFixture.commandID),
      sessionID: session.id,
      content: content,
      expectedDefaultsVersion: session.defaultsVersion
    )
  }

  func submit(
    _ submission: SignalboxPreparedInputSubmission
  ) async throws -> SignalboxInputSubmitted {
    submittedContents.append(submission.content)
    return try ProcessSubmissionFixture.submittedReceipt(sessionID: submission.sessionID)
  }

  func makeSynchronization(
    sessionID: SignalboxCanonicalUUID,
    updates: @escaping @Sendable (SignalboxSessionSynchronizationDriverUpdate) async -> Void
  ) async -> any SignalboxSessionSynchronizing {
    NoopProcessSynchronization()
  }
}

private actor SuspendedSubmissionService: SignalboxProcessServiceProtocol {
  private var prepareCallCount = 0
  private var submitCallCount = 0
  private var submitStarted = false
  private var submitStartedWaiter: CheckedContinuation<Void, Never>?
  private var completionWaiter: CheckedContinuation<Void, Never>?

  var callCounts: SubmissionCallCounts {
    SubmissionCallCounts(prepare: prepareCallCount, submit: submitCallCount)
  }

  func testConnection() async throws {}

  func listSessions(includeArchived: Bool) async throws -> [SignalboxProcessSession] {
    []
  }

  func setArchived(
    _ archived: Bool,
    session: SignalboxProcessSession
  ) async throws -> SignalboxProcessSession {
    session
  }

  func prepareInputSubmission(
    session: SignalboxProcessSession,
    content: String
  ) async throws -> SignalboxPreparedInputSubmission {
    prepareCallCount += 1
    return SignalboxPreparedInputSubmission(
      commandID: try SignalboxCommandID(validating: ProcessSubmissionFixture.commandID),
      sessionID: session.id,
      content: content,
      expectedDefaultsVersion: session.defaultsVersion
    )
  }

  func submit(
    _ submission: SignalboxPreparedInputSubmission
  ) async throws -> SignalboxInputSubmitted {
    submitCallCount += 1
    submitStarted = true
    submitStartedWaiter?.resume()
    submitStartedWaiter = nil
    await withCheckedContinuation { continuation in
      completionWaiter = continuation
    }
    return try ProcessSubmissionFixture.submittedReceipt(sessionID: submission.sessionID)
  }

  func waitUntilSubmitStarted() async {
    guard !submitStarted else {
      return
    }
    await withCheckedContinuation { continuation in
      submitStartedWaiter = continuation
    }
  }

  func completeSubmission() {
    completionWaiter?.resume()
    completionWaiter = nil
  }

  func makeSynchronization(
    sessionID: SignalboxCanonicalUUID,
    updates: @escaping @Sendable (SignalboxSessionSynchronizationDriverUpdate) async -> Void
  ) async -> any SignalboxSessionSynchronizing {
    NoopProcessSynchronization()
  }
}

private enum ProcessSubmissionFixtureError: LocalizedError {
  case rejected

  var errorDescription: String? {
    ProcessSubmissionFixture.failureMessage
  }
}

private actor ControlledSynchronizationRequester: SignalboxProcessRequesting {
  let primary = ControlledProcessExchange()
  let side = ControlledProcessExchange()
  private var followIsOpen = false
  private var sideIsOpen = false
  private var followOpenWaiter: CheckedContinuation<Void, Never>?
  private var sideOpenWaiter: CheckedContinuation<Void, Never>?

  func open(
    _ request: SignalboxProcessClientRequest
  ) async throws -> any SignalboxProcessExchange {
    switch request {
    case .followSession:
      followIsOpen = true
      followOpenWaiter?.resume()
      followOpenWaiter = nil
      return primary
    case .readTranscript:
      sideIsOpen = true
      sideOpenWaiter?.resume()
      sideOpenWaiter = nil
      return side
    default:
      throw ProcessDriverUpdateRecorderError.unexpectedRequest
    }
  }

  func waitForFollowOpen() async {
    guard !followIsOpen else {
      return
    }
    await withCheckedContinuation { continuation in
      followOpenWaiter = continuation
    }
  }

  func waitForSideOpen() async {
    guard !sideIsOpen else {
      return
    }
    await withCheckedContinuation { continuation in
      sideOpenWaiter = continuation
    }
  }
}

private actor ControlledProcessExchange: SignalboxProcessExchange {
  private var frames: [SignalboxProcessServerFrame] = []
  private var nextWaiter: CheckedContinuation<SignalboxProcessServerFrame?, Never>?
  private var readCountWaiters: [(count: Int, continuation: CheckedContinuation<Void, Never>)] = []
  private var nextCallCount = 0

  func next() async throws -> SignalboxProcessServerFrame? {
    nextCallCount += 1
    resumeSatisfiedReadCountWaiters()
    if !frames.isEmpty {
      return frames.removeFirst()
    }
    return await withCheckedContinuation { continuation in
      nextWaiter = continuation
    }
  }

  func send(_ frame: SignalboxProcessServerFrame) {
    if let nextWaiter {
      self.nextWaiter = nil
      nextWaiter.resume(returning: frame)
      return
    }
    frames.append(frame)
  }

  func waitForNextCallCount(_ count: Int) async {
    guard nextCallCount < count else {
      return
    }
    await withCheckedContinuation { continuation in
      readCountWaiters.append((count: count, continuation: continuation))
    }
  }

  func close() {
    let waiter = nextWaiter
    nextWaiter = nil
    waiter?.resume(returning: nil)
  }

  private func resumeSatisfiedReadCountWaiters() {
    let satisfied = readCountWaiters.filter { $0.count <= nextCallCount }
    readCountWaiters.removeAll { $0.count <= nextCallCount }
    for waiter in satisfied {
      waiter.continuation.resume()
    }
  }
}

private actor OrderedProcessDriverUpdateRecorder {
  private var updates: [SignalboxSessionSynchronizationDriverUpdate] = []
  private var shouldPauseNextPhase = false
  private var phaseIsPaused = false
  private var pausedWaiter: CheckedContinuation<Void, Never>?
  private var releaseWaiter: CheckedContinuation<Void, Never>?

  func append(_ update: SignalboxSessionSynchronizationDriverUpdate) async {
    if case .phase = update, shouldPauseNextPhase {
      shouldPauseNextPhase = false
      phaseIsPaused = true
      pausedWaiter?.resume()
      pausedWaiter = nil
      await withCheckedContinuation { continuation in
        releaseWaiter = continuation
      }
      phaseIsPaused = false
    }
    updates.append(update)
  }

  func pauseNextPhase() {
    shouldPauseNextPhase = true
  }

  func waitUntilPhaseIsPaused() async {
    guard !phaseIsPaused else {
      return
    }
    await withCheckedContinuation { continuation in
      pausedWaiter = continuation
    }
  }

  func releasePausedPhase() {
    releaseWaiter?.resume()
    releaseWaiter = nil
  }

  func eventCursors(count: Int) async throws -> [UInt64] {
    for _ in 0..<100 {
      let cursors = updates.compactMap(Self.eventCursor)
      if cursors.count == count {
        return cursors
      }
      try await Task.sleep(for: .milliseconds(10))
    }
    throw ProcessDriverUpdateRecorderError.eventTimeout
  }

  private static func eventCursor(
    _ update: SignalboxSessionSynchronizationDriverUpdate
  ) -> UInt64? {
    guard case .event(let event) = update else {
      return nil
    }
    return event.cursor.rawValue
  }
}

private struct StaticProcessRequester: SignalboxProcessRequesting {
  let frames: [SignalboxProcessServerFrame]

  func open(
    _ request: SignalboxProcessClientRequest
  ) async throws -> any SignalboxProcessExchange {
    StaticProcessExchange(frames: frames)
  }
}

private actor SequencedProcessRequester: SignalboxProcessRequesting {
  private var pages: [[SignalboxProcessServerFrame]]
  private(set) var openedRequests: [SignalboxProcessClientRequest] = []

  init(pages: [[SignalboxProcessServerFrame]]) {
    self.pages = pages
  }

  func open(
    _ request: SignalboxProcessClientRequest
  ) async throws -> any SignalboxProcessExchange {
    openedRequests.append(request)
    guard !pages.isEmpty else {
      throw ProcessDriverUpdateRecorderError.unexpectedRequest
    }
    return StaticProcessExchange(frames: pages.removeFirst())
  }
}

private actor StaticProcessExchange: SignalboxProcessExchange {
  private var frames: [SignalboxProcessServerFrame]

  init(frames: [SignalboxProcessServerFrame]) {
    self.frames = frames
  }

  func next() async throws -> SignalboxProcessServerFrame? {
    guard !frames.isEmpty else {
      return nil
    }
    return frames.removeFirst()
  }

  func close() {}
}

private enum ProcessDriverFixture {
  static let session = "11111111-1111-4111-8111-111111111111"
  static let turn = "22222222-2222-4222-8222-222222222222"
  static let modelCall = "33333333-3333-4333-8333-333333333333"
  static let attempt = "44444444-4444-4444-8444-444444444444"
  static let completionEntry = "55555555-5555-4555-8555-555555555555"
  static let frontier = "66666666-6666-4666-8666-666666666666"
  static let metadataSessionA = "77777777-7777-4777-8777-777777777777"
  static let metadataSessionB = "88888888-8888-4888-8888-888888888888"
  static let singleMetadataCount: UInt64 = 1
  static let initialFollowReadCount = 3
  static let bufferedFollowReadCount = 5
  static let sideEndReadCount = 2
  static let snapshotCursor: UInt64 = 0
  static let triggerCursor: UInt64 = 1
  static let bufferedCursor: UInt64 = 2
  static let newerCursor: UInt64 = 3
  static let expectedCursors = [triggerCursor, bufferedCursor, newerCursor]
  static let incompleteMetadataPageError = SignalboxProcessServiceError.invalidPage(
    "The metadata page ended before its terminal boundary."
  )
  static let metadataCapacityError = SignalboxProcessServiceError.invalidPage(
    "The metadata page exceeded its requested row limit."
  )
  static let metadataEndCursorError = SignalboxProcessServiceError.invalidPage(
    "The metadata page cursor did not match its last emitted identity."
  )
  static let metadataRequestCursorError = SignalboxProcessServiceError.invalidPage(
    "A metadata summary did not advance beyond the request cursor."
  )
  static let oneRowMetadataPolicy = SignalboxProcessApplicationPolicy(
    metadataPageSize: SignalboxCanonicalUInt64(rawValue: 1),
    maximumMetadataPages: SignalboxProcessApplicationPolicy.nativeDefault.maximumMetadataPages,
    ambiguousMutationRetryDelays:
      SignalboxProcessApplicationPolicy.nativeDefault.ambiguousMutationRetryDelays,
    synchronization: SignalboxProcessApplicationPolicy.nativeDefault.synchronization
  )
  static let oneImmediateMutationRetryPolicy = SignalboxProcessApplicationPolicy(
    metadataPageSize: SignalboxProcessApplicationPolicy.nativeDefault.metadataPageSize,
    maximumMetadataPages: SignalboxProcessApplicationPolicy.nativeDefault.maximumMetadataPages,
    ambiguousMutationRetryDelays: [.zero],
    synchronization: SignalboxProcessApplicationPolicy.nativeDefault.synchronization
  )

  static func sessionID() throws -> SignalboxCanonicalUUID {
    try SignalboxCanonicalUUID(validating: session)
  }

  static func snapshotStart(
    cursor: UInt64
  ) throws -> SignalboxProcessServerFrame {
    try frame(
      """
      {
        "type":"transcript_snapshot_start",
        "session_id":"\(session)",
        "cursor":"\(cursor)"
      }
      """
    )
  }

  static func snapshotEnd(
    cursor: UInt64
  ) throws -> SignalboxProcessServerFrame {
    try frame(
      """
      {
        "type":"transcript_snapshot_end",
        "session_id":"\(session)",
        "cursor":"\(cursor)",
        "turn_count":"0",
        "entry_count":"0"
      }
      """
    )
  }

  static func completedEvent(
    cursor: UInt64
  ) throws -> SignalboxProcessServerFrame {
    try followedFrame(
      cursor: cursor,
      event:
        """
        {
          "type":"turn_completed",
          "turn_id":"\(turn)",
          "model_call_id":"\(modelCall)",
          "completion_entry_id":"\(completionEntry)",
          "terminal_frontier_id":"\(frontier)"
        }
        """
    )
  }

  static func preparedEvent(
    cursor: UInt64
  ) throws -> SignalboxProcessServerFrame {
    try followedFrame(
      cursor: cursor,
      event:
        """
        {
          "type":"model_call_transition",
          "turn_id":"\(turn)",
          "model_call_id":"\(modelCall)",
          "state":{"type":"prepared"}
        }
        """
    )
  }

  static func activatedEvent(
    cursor: UInt64
  ) throws -> SignalboxProcessServerFrame {
    try followedFrame(
      cursor: cursor,
      event:
        """
        {
          "type":"turn_activated",
          "turn_id":"\(turn)",
          "current_attempt_id":"\(attempt)"
        }
        """
    )
  }

  static func metadataPageStart() throws -> SignalboxProcessServerFrame {
    try frame(#"{"type":"session_metadata_page_start"}"#)
  }

  static func metadataSummary(
    sessionID: String
  ) throws -> SignalboxProcessServerFrame {
    try frame(
      """
      {
        "type":"session_metadata_summary",
        "session_id":"\(sessionID)",
        "defaults_version":"1",
        "model_selection":{
          "kind":"direct",
          "selection_id":"\(modelCall)"
        },
        "dangerous_tool_auto_approval":false,
        "title":"Fixture metadata session",
        "tags":[],
        "archived":false,
        "last_writer":null
      }
      """
    )
  }

  static func metadataPageEnd(
    count: UInt64,
    nextSessionID: String?
  ) throws -> SignalboxProcessServerFrame {
    let next =
      if let nextSessionID {
        "\"\(nextSessionID)\""
      } else {
        "null"
      }
    return try frame(
      """
      {
        "type":"session_metadata_page_end",
        "session_count":"\(count)",
        "next_after_session_id":\(next)
      }
      """
    )
  }

  static func inputSubmitted() throws -> SignalboxProcessServerFrame {
    try frame(
      """
      {
        "type":"input_submitted",
        "session_id":"\(session)",
        "accepted_input_id":"\(ProcessSubmissionFixture.acceptedInputID)",
        "acceptance_position":"1",
        "turn_id":"\(ProcessSubmissionFixture.acceptedTurnID)"
      }
      """
    )
  }

  private static func followedFrame(
    cursor: UInt64,
    event: String
  ) throws -> SignalboxProcessServerFrame {
    try frame(
      """
      {
        "type":"session_event",
        "cursor":"\(cursor)",
        "session_id":"\(session)",
        "event":\(event)
      }
      """
    )
  }

  private static func frame(
    _ message: String
  ) throws -> SignalboxProcessServerFrame {
    try SignalboxJSONCoding.decoder().decode(
      SignalboxProcessServerFrame.self,
      from: Data(
        """
        {
          "version":5,
          "request_id":"1",
          "message":\(message)
        }
        """.utf8
      )
    )
  }
}

private enum ProcessProjectionFixture {
  static let userText = "fixture materialized owner input"
  static let proposedAssistantText = "I will inspect the fixture before using the tool."
  static let proposedToolName = "inspect_fixture"
  static let proposedAssistantEntry = "99999999-9999-4999-8999-999999999999"
  static let proposedToolEntry = "aaaaaaaa-1111-4111-8111-111111111111"
  static let proposedToolRequest = "bbbbbbbb-1111-4111-8111-111111111111"
  static let runningActivity = SignalboxProcessActivity(state: .running, label: "Running")
  static let unavailableActivity = SignalboxProcessActivity.unavailable
  static let refusedActivity = SignalboxProcessActivity(state: .refused, label: "Refused")
  static let neutralToolCardStatus = SignalboxToolCardStatus.completed

  static func materializedAcceptedInputIDs() throws -> Set<SignalboxCanonicalUUID> {
    [try SignalboxCanonicalUUID(validating: ProcessSubmissionFixture.acceptedInputID)]
  }

  static func snapshotWithUserEntry() throws -> SignalboxSynchronizationSnapshot {
    var machine = SignalboxSessionSynchronizationMachine(
      sessionID: try ProcessDriverFixture.sessionID(),
      policy: SignalboxProcessApplicationPolicy.nativeDefault.synchronization
    )
    _ = machine.receive(.start)
    _ = machine.receive(.connected(generation: 1))
    _ = machine.receive(
      .frame(
        generation: 1,
        message: try message(
          """
          {
            "type":"transcript_snapshot_start",
            "session_id":"\(ProcessDriverFixture.session)",
            "cursor":"1"
          }
          """
        )
      )
    )
    _ = machine.receive(
      .frame(
        generation: 1,
        message: try message(
          """
          {
            "type":"transcript_text_entry",
            "entry_index":"0",
            "source_session_id":"\(ProcessDriverFixture.session)",
            "entry_id":"\(ProcessDriverFixture.completionEntry)",
            "entry":{
              "type":"user",
              "accepted_input_id":"\(ProcessSubmissionFixture.acceptedInputID)",
              "turn_id":"\(ProcessDriverFixture.turn)"
            }
          }
          """
        )
      )
    )
    _ = machine.receive(
      .frame(
        generation: 1,
        message: try message(
          """
          {
            "type":"transcript_content",
            "entry_index":"0",
            "fragment_index":"0",
            "final_fragment":true,
            "content_fragment":"\(userText)"
          }
          """
        )
      )
    )
    let effects = machine.receive(
      .frame(
        generation: 1,
        message: try message(
          """
          {
            "type":"transcript_snapshot_end",
            "session_id":"\(ProcessDriverFixture.session)",
            "cursor":"1",
            "turn_count":"0",
            "entry_count":"1"
          }
          """
        )
      )
    )
    let snapshots: [SignalboxSynchronizationSnapshot] = effects.compactMap {
      effect -> SignalboxSynchronizationSnapshot? in
      guard case .publishSnapshot(let snapshot) = effect else {
        return nil
      }
      return snapshot
    }
    guard let snapshot = snapshots.first else {
      throw ProcessDriverUpdateRecorderError.missingSnapshotEffect
    }
    return snapshot
  }

  static func snapshotWithProposedTool() throws -> SignalboxSynchronizationSnapshot {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1"
        }
        """,
        """
        {
          "type":"transcript_text_entry",
          "entry_index":"0",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(proposedAssistantEntry)",
          "entry":{
            "type":"assistant",
            "turn_id":"\(ProcessDriverFixture.turn)",
            "model_call_id":"\(ProcessDriverFixture.modelCall)"
          }
        }
        """,
        """
        {
          "type":"transcript_content",
          "entry_index":"0",
          "fragment_index":"0",
          "final_fragment":true,
          "content_fragment":"\(proposedAssistantText)"
        }
        """,
        """
        {
          "type":"transcript_entry",
          "entry_index":"1",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(proposedToolEntry)",
          "entry":{
            "type":"assistant_tool_use",
            "turn_id":"\(ProcessDriverFixture.turn)",
            "model_call_id":"\(ProcessDriverFixture.modelCall)",
            "tool_request_id":"\(proposedToolRequest)",
            "tool_name":"\(proposedToolName)",
            "arguments":"{}"
          }
        }
        """,
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "turn_count":"0",
          "entry_count":"2"
        }
        """,
      ]
    )
  }

  static func completedTrigger() throws -> SignalboxFollowedSessionEvent {
    let message = try message(
      """
      {
        "type":"session_event",
        "cursor":"1",
        "session_id":"\(ProcessDriverFixture.session)",
        "event":{
          "type":"turn_completed",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "model_call_id":"\(ProcessDriverFixture.modelCall)",
          "completion_entry_id":"\(ProcessDriverFixture.completionEntry)",
          "terminal_frontier_id":"\(ProcessDriverFixture.frontier)"
        }
      }
      """
    )
    guard case .sessionEvent(let event) = message else {
      throw ProcessDriverUpdateRecorderError.missingFixtureEvent
    }
    return event
  }

  static func proposedToolTrigger() throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"tool_batch_transition",
        "turn_id":"\(ProcessDriverFixture.turn)",
        "model_call_id":"\(ProcessDriverFixture.modelCall)",
        "state":{
          "type":"proposed",
          "frontier_id":"\(ProcessDriverFixture.frontier)"
        }
      }
      """
    )
  }

  static func acceptedEvent() throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"input_accepted",
        "accepted_input_id":"\(ProcessSubmissionFixture.acceptedInputID)",
        "turn_id":"\(ProcessDriverFixture.turn)",
        "acceptance_position":"1",
        "content":"\(ProcessSubmissionFixture.content)"
      }
      """
    )
  }

  static func activatedEvent() throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"turn_activated",
        "turn_id":"\(ProcessDriverFixture.turn)",
        "current_attempt_id":"\(ProcessDriverFixture.attempt)"
      }
      """
    )
  }

  static func refusedEvent() throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"turn_refused",
        "turn_id":"\(ProcessDriverFixture.turn)",
        "model_call_id":"\(ProcessDriverFixture.modelCall)",
        "terminal_frontier_id":"\(ProcessDriverFixture.frontier)"
      }
      """
    )
  }

  static func onlyMessage(
    in projection: SignalboxProcessTranscriptProjection
  ) throws -> SignalboxProcessMessageEvent {
    let messages: [SignalboxProcessMessageEvent] = projection.records.compactMap {
      record -> SignalboxProcessMessageEvent? in
      guard case .processMessage(let message) = record.event else {
        return nil
      }
      return message
    }
    guard messages.count == 1, let message = messages.first else {
      throw ProcessDriverUpdateRecorderError.missingFixtureMessage
    }
    return message
  }

  static func assistantMessage(
    in projection: SignalboxProcessTranscriptProjection
  ) throws -> SignalboxProcessMessageEvent {
    let messages: [SignalboxProcessMessageEvent] = projection.records.compactMap {
      record -> SignalboxProcessMessageEvent? in
      guard case .processMessage(let message) = record.event,
        message.role == .assistant
      else {
        return nil
      }
      return message
    }
    guard messages.count == 1, let message = messages.first else {
      throw ProcessDriverUpdateRecorderError.missingFixtureMessage
    }
    return message
  }

  static func onlyTool(
    in projection: SignalboxProcessTranscriptProjection
  ) throws -> SignalboxProcessToolEvent {
    let tools: [SignalboxProcessToolEvent] = projection.records.compactMap {
      record -> SignalboxProcessToolEvent? in
      guard case .processTool(let tool) = record.event else {
        return nil
      }
      return tool
    }
    guard tools.count == 1, let tool = tools.first else {
      throw ProcessDriverUpdateRecorderError.missingFixtureTool
    }
    return tool
  }

  static func onlyToolCard(
    in timeline: [SignalboxTimelineItem]
  ) throws -> SignalboxToolCard {
    let tools: [SignalboxToolCard] = timeline.compactMap {
      guard case .tool(let tool) = $0 else {
        return nil
      }
      return tool
    }
    guard tools.count == 1, let tool = tools.first else {
      throw ProcessDriverUpdateRecorderError.missingFixtureTool
    }
    return tool
  }

  static func onlyTimelineMessage(
    in timeline: [SignalboxTimelineItem]
  ) throws -> SignalboxTimelineMessage {
    let messages: [SignalboxTimelineMessage] = timeline.compactMap {
      guard case .message(let message) = $0 else {
        return nil
      }
      return message
    }
    guard messages.count == 1, let message = messages.first else {
      throw ProcessDriverUpdateRecorderError.missingFixtureMessage
    }
    return message
  }

  private static func message(
    _ json: String
  ) throws -> SignalboxProcessServerMessage {
    try SignalboxJSONCoding.decoder().decode(
      SignalboxProcessServerMessage.self,
      from: Data(json.utf8)
    )
  }

  private static func followedEvent(
    _ event: String
  ) throws -> SignalboxFollowedSessionEvent {
    let message = try message(
      """
      {
        "type":"session_event",
        "cursor":"1",
        "session_id":"\(ProcessDriverFixture.session)",
        "event":\(event)
      }
      """
    )
    guard case .sessionEvent(let followed) = message else {
      throw ProcessDriverUpdateRecorderError.missingFixtureEvent
    }
    return followed
  }

  private static func snapshot(
    messages: [String]
  ) throws -> SignalboxSynchronizationSnapshot {
    var machine = SignalboxSessionSynchronizationMachine(
      sessionID: try ProcessDriverFixture.sessionID(),
      policy: SignalboxProcessApplicationPolicy.nativeDefault.synchronization
    )
    _ = machine.receive(.start)
    _ = machine.receive(.connected(generation: 1))
    var effects: [SignalboxSessionSynchronizationEffect] = []
    for messageJSON in messages {
      effects = machine.receive(
        .frame(
          generation: 1,
          message: try message(messageJSON)
        )
      )
    }
    let snapshots: [SignalboxSynchronizationSnapshot] = effects.compactMap {
      guard case .publishSnapshot(let snapshot) = $0 else {
        return nil
      }
      return snapshot
    }
    guard let snapshot = snapshots.first else {
      throw ProcessDriverUpdateRecorderError.missingSnapshotEffect
    }
    return snapshot
  }
}

private actor ProcessDriverUpdateRecorder {
  private var updates: [SignalboxSessionSynchronizationDriverUpdate] = []

  func append(_ update: SignalboxSessionSynchronizationDriverUpdate) {
    updates.append(update)
  }

  func authoritativeSnapshot() async throws -> SignalboxSynchronizationSnapshot {
    for _ in 0..<100 {
      if let snapshot = updates.compactMap(Self.snapshot).first {
        return snapshot
      }
      try await Task.sleep(for: .milliseconds(10))
    }
    throw ProcessDriverUpdateRecorderError.snapshotTimeout
  }

  private static func snapshot(
    _ update: SignalboxSessionSynchronizationDriverUpdate
  ) -> SignalboxSynchronizationSnapshot? {
    guard case .authoritativeSnapshot(let snapshot) = update else {
      return nil
    }
    return snapshot
  }
}

private enum ProcessDriverUpdateRecorderError: Error {
  case eventTimeout
  case expectedUnknownEvent
  case missingFixtureEvent
  case missingFixtureMessage
  case missingFixtureSession
  case missingFixtureTool
  case missingSnapshotEffect
  case snapshotTimeout
  case unexpectedRequest
}
