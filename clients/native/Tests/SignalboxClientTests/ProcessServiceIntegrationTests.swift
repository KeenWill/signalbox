import XCTest

@testable import SignalboxNative

final class ProcessServiceIntegrationTests: XCTestCase {
  func testModelAliasCatalogCreatesAUnifiedNativeConversation() async throws {
    let service = makeService()

    let aliases = try await service.listModelAliases()
    let alias = try XCTUnwrap(aliases.first)
    let prepared = try await service.prepareSessionCreation(
      modelSelection: .alias(aliasID: alias.aliasID),
      systemPrompt: ProcessSubmissionFixture.systemPrompt
    )
    let createdSessionID = try await service.createSession(prepared)
    let conversations = try await service.listConversations(includeArchived: true)
    let createdConversation = try XCTUnwrap(
      conversations.first {
        $0.conversationID.rawValue == MockProcessProtocolFixtures.createdSessionID
      }
    )

    XCTAssertEqual(alias.aliasID.rawValue, MockProcessProtocolFixtures.aliasID)
    XCTAssertEqual(alias.selectionID.rawValue, MockProcessProtocolFixtures.selectionID)
    XCTAssertEqual(createdSessionID.rawValue, MockProcessProtocolFixtures.createdSessionID)
    XCTAssertEqual(
      createdConversation.conversationID.rawValue,
      MockProcessProtocolFixtures.createdSessionID
    )
  }

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
    XCTAssertEqual(projection.activity, ProcessProjectionFixture.completedActivity)
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
    await viewModel.connect()
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

  func testSideProjectionDoesNotSelectAcceptedInputTextByTurnAlone() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithUserEntry()
    let trigger = try ProcessProjectionFixture.refusedEvent()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectSideSnapshot(snapshot, attributableTo: trigger)
    XCTAssertTrue(projection.records.isEmpty)
    XCTAssertTrue(projection.materializedAcceptedInputIDs.isEmpty)
  }

  func testSideProjectionRejectsMissingCompletionEvidence() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithUserEntry()
    let trigger = try ProcessProjectionFixture.completedTrigger()
    var projector = SignalboxProcessTranscriptProjector()

    XCTAssertThrowsError(
      try projector.projectSideSnapshot(snapshot, attributableTo: trigger)
    ) { error in
      XCTAssertEqual(
        error as? SignalboxProcessTranscriptProjectionError,
        .missingTriggerEvidence
      )
    }
  }

  func testSideProjectionRejectsCompletionWithoutAssistantText() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithCompletionMarkerOnly()
    let trigger = try ProcessProjectionFixture.completedTrigger()
    var projector = SignalboxProcessTranscriptProjector()

    XCTAssertThrowsError(
      try projector.projectSideSnapshot(snapshot, attributableTo: trigger)
    ) { error in
      XCTAssertEqual(
        error as? SignalboxProcessTranscriptProjectionError,
        .missingTriggerEvidence
      )
    }
  }

  func testSideProjectionRejectsReconciliationResultFromAnotherTurn() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithCrossTurnReconciliationResult()
    let trigger = try ProcessProjectionFixture.toolReconciliationTrigger()
    var projector = SignalboxProcessTranscriptProjector()
    _ = try projector.projectAuthoritativeSnapshot(snapshot)

    XCTAssertThrowsError(
      try projector.projectSideSnapshot(snapshot, attributableTo: trigger)
    ) { error in
      XCTAssertEqual(
        error as? SignalboxProcessTranscriptProjectionError,
        .missingTriggerEvidence
      )
    }
  }

  func testAuthoritativeProjectionRestoresWireOrderAfterSideProjection() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithCompletedTurnEntries()
    let trigger = try ProcessProjectionFixture.completedTrigger()
    var projector = SignalboxProcessTranscriptProjector()

    _ = try projector.projectSideSnapshot(snapshot, attributableTo: trigger)
    let projection = try projector.projectAuthoritativeSnapshot(snapshot)

    XCTAssertEqual(
      projection.records.map(\.eventID.rawValue),
      ProcessProjectionFixture.orderedPresentationIDs
    )
    XCTAssertEqual(
      try ProcessProjectionFixture.messageRoles(in: projection),
      ProcessProjectionFixture.orderedMessageRoles
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

  func testSnapshotPreservesPendingAcceptanceOrderAndActiveActivity() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithQueuedAndActiveTurns()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)

    XCTAssertEqual(
      projection.pendingInputs.map(\.id.rawValue),
      ProcessProjectionFixture.pendingIDsInAcceptanceOrder
    )
    XCTAssertEqual(projection.activity, ProcessProjectionFixture.runningActivity)
  }

  @MainActor
  func testProposedToolWaitsOnlyWhenSideSnapshotShowsApproval() async throws {
    let service = makeService(scenario: .pendingApproval)
    let sessions = try await service.listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.approvalSessionID, in: sessions)
    let snapshot = try await authoritativeSnapshot(service: service, session: session)
    let trigger = try ProcessProjectionFixture.approvalToolTrigger()
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }

    viewModel.apply(.event(trigger))
    let proposedActivity = viewModel.activity
    viewModel.apply(.sideSnapshot(snapshot: snapshot, trigger: trigger))

    XCTAssertEqual(proposedActivity, ProcessProjectionFixture.runningActivity)
    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.waitingActivity)
  }

  @MainActor
  func testActiveTurnGatesSendAndApprovalWaitGatesStop() async throws {
    let service = makeService(scenario: .pendingApproval)
    let sessions = try await service.listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.approvalSessionID, in: sessions)
    let snapshot = try await authoritativeSnapshot(service: service, session: session)
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      ImmediateAcceptingProcessService()
    }
    await viewModel.connect()
    viewModel.apply(.phase(ProcessProjectionFixture.steadyPhase))

    viewModel.apply(.authoritativeSnapshot(snapshot))

    XCTAssertFalse(viewModel.canSend)
    XCTAssertFalse(viewModel.canStopAndSend)
  }

  @MainActor
  func testSideSnapshotDoesNotPublishLaterProposalApprovalState() async throws {
    let service = makeService(scenario: .pendingApproval)
    let sessions = try await service.listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.approvalSessionID, in: sessions)
    let snapshot = try await authoritativeSnapshot(service: service, session: session)
    let trigger = try ProcessProjectionFixture.proposedToolTrigger()
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }

    viewModel.apply(.event(trigger))
    viewModel.apply(.sideSnapshot(snapshot: snapshot, trigger: trigger))

    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.runningActivity)
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
        trigger: try ProcessProjectionFixture.refusedEvent()
      )
    )

    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.runningActivity)
    XCTAssertEqual(viewModel.pendingInputs, try ProcessSubmissionFixture.livePendingInput())
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
  func testChangedSocketPathDiscardsInFlightConnectionTest() async {
    let service = SuspendedConnectionProcessService()
    UserDefaults.standard.removeObject(forKey: NativeProcessConstants.socketDefaultsKey)
    let settings = SignalboxProcessSettingsViewModel(environment: [:])
    settings.socketPath = ProcessSubmissionFixture.initialSocketPath

    let test = Task {
      await settings.test(
        using: service,
        expectedSocketPath: ProcessSubmissionFixture.initialSocketPath
      )
    }
    await service.waitUntilTestStarted()
    settings.socketPath = ProcessSubmissionFixture.replacementSocketPath
    await service.completeTest()
    await test.value

    XCTAssertEqual(settings.connectionStatus, .unknown)
    XCTAssertNil(
      UserDefaults.standard.string(forKey: NativeProcessConstants.socketDefaultsKey)
    )
  }

  @MainActor
  func testOlderSessionRefreshCannotReplaceNewerServiceResult() async throws {
    let fixtures = try await makeService().listConversations(includeArchived: true)
    let olderConversations = [
      try fixtureConversation(MockSignalboxFixtures.activeSessionID, in: fixtures)
    ]
    let newerConversations = [
      try fixtureConversation(MockSignalboxFixtures.approvalSessionID, in: fixtures)
    ]
    let olderService = SuspendedSessionListProcessService(conversations: olderConversations)
    let newerService = SuspendedSessionListProcessService(conversations: newerConversations)
    var currentService: (any SignalboxProcessServiceProtocol)? = olderService
    let viewModel = ProcessSessionListViewModel { currentService }

    let olderRefresh = Task { await viewModel.refresh() }
    await olderService.waitUntilListStarted()
    currentService = newerService
    let newerRefresh = Task { await viewModel.refresh() }
    await newerService.waitUntilListStarted()
    await newerService.completeList()
    await newerRefresh.value
    await olderService.completeList()
    await olderRefresh.value

    XCTAssertEqual(viewModel.conversations, newerConversations)
    XCTAssertFalse(viewModel.isLoading)
  }

  @MainActor
  func testArchiveCommitWinsOverRacingStaleRefresh() async throws {
    let backingService = makeService()
    let fixtures = try await backingService.listConversations(includeArchived: true)
    let conversation = try fixtureConversation(MockSignalboxFixtures.activeSessionID, in: fixtures)
    let archived = try await backingService.setConversationArchived(
      true,
      conversation: conversation
    )
    let service = SuspendedArchiveProcessService(
      staleConversations: [conversation],
      replacement: archived
    )
    let viewModel = ProcessSessionListViewModel { service }
    await viewModel.refresh()

    let mutation = Task { await viewModel.toggleArchive(conversation) }
    await service.waitUntilMutationStarted()
    await viewModel.refresh()
    await service.completeMutation()
    await mutation.value

    XCTAssertEqual(viewModel.conversations, [archived])
    XCTAssertFalse(viewModel.isLoading)
  }

  @MainActor
  func testReplacingListServiceClearsRowsAndInvalidatesOldArchive() async throws {
    let backingService = makeService()
    let fixtures = try await backingService.listConversations(includeArchived: true)
    let conversation = try fixtureConversation(MockSignalboxFixtures.activeSessionID, in: fixtures)
    let archived = try await backingService.setConversationArchived(
      true,
      conversation: conversation
    )
    let oldService = SuspendedArchiveProcessService(
      staleConversations: [conversation],
      replacement: archived
    )
    var currentService: (any SignalboxProcessServiceProtocol)? = oldService
    let viewModel = ProcessSessionListViewModel { currentService }
    await viewModel.refresh()
    let mutation = Task { await viewModel.toggleArchive(conversation) }
    await oldService.waitUntilMutationStarted()

    currentService = RejectingProcessService()
    viewModel.replaceServiceProvider { currentService }
    await oldService.completeMutation()
    await mutation.value

    XCTAssertTrue(viewModel.conversations.isEmpty)
    XCTAssertNil(viewModel.errorMessage)
  }

  @MainActor
  func testAmbiguousSubmissionRetryReusesPreparedCommandIdentity() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let service = AmbiguousThenAcceptingProcessService()
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      service
    }
    await viewModel.connect()
    viewModel.composerText = ProcessSubmissionFixture.content

    await viewModel.send()
    await viewModel.send()
    let submittedCommandIDs = await service.submittedCommandIDs

    XCTAssertEqual(submittedCommandIDs, ProcessSubmissionFixture.retriedCommandIDs)
  }

  func testAmbiguousCreationRetryReusesPreparedCommandIdentity() throws {
    var state = ProcessSessionCreationRetryState()
    let prepared = try ProcessSubmissionFixture.preparedCreation()
    state.recordFailure(
      ProcessSubmissionFixture.ambiguousMutationError,
      prepared: prepared,
      reusedUnresolvedCreation: false
    )

    let retried = state.reusableCreation(
      modelSelection: prepared.modelSelection,
      systemPrompt: prepared.systemPrompt
    )
    let edited = state.reusableCreation(
      modelSelection: prepared.modelSelection,
      systemPrompt: ProcessSubmissionFixture.replacementContent
    )

    XCTAssertEqual(retried, prepared)
    XCTAssertNil(edited)
  }

  @MainActor
  func testCancelledSubmissionRetryReusesPreparedCommandIdentity() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let service = CancellationThenAcceptingProcessService()
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      service
    }
    await viewModel.connect()
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
    await viewModel.connect()
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
    await viewModel.connect()
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
    await viewModel.connect()
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
    await viewModel.connect()
    viewModel.apply(.event(try ProcessProjectionFixture.acceptedEvent()))
    viewModel.composerText = ProcessSubmissionFixture.content

    await viewModel.send()

    XCTAssertEqual(viewModel.pendingInputs, try ProcessSubmissionFixture.singlePendingInput())
  }

  @MainActor
  func testSubmissionReceiptRetainsAcceptanceOrder() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      ImmediateAcceptingProcessService()
    }
    await viewModel.connect()
    viewModel.apply(.event(try ProcessProjectionFixture.secondAcceptedEvent()))
    viewModel.composerText = ProcessSubmissionFixture.content

    await viewModel.send()

    XCTAssertEqual(
      viewModel.pendingInputs.map(\.id.rawValue),
      ProcessSubmissionFixture.receiptReconciledPendingIDs
    )
  }

  @MainActor
  func testStopReceiptDeduplicatesPreviouslyFollowedSuccessor() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      ImmediateAcceptingProcessService()
    }
    await viewModel.connect()
    viewModel.composerText = ProcessSubmissionFixture.content
    viewModel.apply(.event(try ProcessProjectionFixture.activatedEvent()))
    viewModel.apply(.event(try ProcessProjectionFixture.acceptedSuccessorEvent()))

    await viewModel.stopAndSendSuccessor()

    XCTAssertEqual(
      viewModel.pendingInputs.map(\.id.rawValue),
      ProcessSubmissionFixture.singleAcceptedInputID
    )
  }

  @MainActor
  func testConcurrentSendIsSuppressed() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let service = SuspendedSubmissionService()
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      service
    }
    await viewModel.connect()
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
  func testSuccessfulSubmissionPreservesComposerEditMadeWhilePending() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let service = SuspendedSubmissionService()
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      service
    }
    await viewModel.connect()
    viewModel.composerText = ProcessSubmissionFixture.content

    let send = Task { await viewModel.send() }
    await service.waitUntilSubmitStarted()
    viewModel.composerText = ProcessSubmissionFixture.replacementContent
    await service.completeSubmission()
    await send.value

    XCTAssertEqual(viewModel.composerText, ProcessSubmissionFixture.replacementContent)
  }

  @MainActor
  func testSendUsesServiceThatOwnsCurrentSynchronization() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let firstService = ImmediateAcceptingProcessService()
    let secondService = ImmediateAcceptingProcessService()
    var currentService: (any SignalboxProcessServiceProtocol)? = firstService
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      currentService
    }
    await viewModel.connect()
    currentService = secondService
    viewModel.composerText = ProcessSubmissionFixture.content

    await viewModel.send()
    let firstContents = await firstService.submittedContents
    let secondContents = await secondService.submittedContents

    XCTAssertEqual(firstContents, [ProcessSubmissionFixture.content])
    XCTAssertTrue(secondContents.isEmpty)
  }

  @MainActor
  func testDisconnectedSynchronizationCallbackCannotOverwriteState() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let service = UpdateEmittingProcessService()
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      service
    }
    await viewModel.connect()
    viewModel.disconnect()

    await service.emit(
      .event(try ProcessProjectionFixture.activatedEvent()),
      synchronizationIndex: 0
    )

    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.unavailableActivity)
  }

  @MainActor
  func testReplacingServiceClearsPriorServicePresentation() async throws {
    let service = makeService()
    let sessions = try await service.listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let snapshot = try await authoritativeSnapshot(service: service, session: session)
    var currentService: (any SignalboxProcessServiceProtocol)? = service
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      currentService
    }
    viewModel.apply(.authoritativeSnapshot(snapshot))
    viewModel.apply(.event(try ProcessProjectionFixture.secondAcceptedEvent()))
    viewModel.apply(.phase(ProcessProjectionFixture.steadyPhase))
    viewModel.apply(.diagnostic(ProcessProjectionFixture.transportDiagnostic))
    viewModel.apply(.retryLimitReached)
    currentService = UpdateEmittingProcessService()

    await viewModel.connect(replacingService: true)

    XCTAssertTrue(viewModel.timeline.isEmpty)
    XCTAssertTrue(viewModel.pendingInputs.isEmpty)
    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.unavailableActivity)
    XCTAssertEqual(viewModel.phase, ProcessProjectionFixture.stoppedPhase)
    XCTAssertNil(viewModel.latestDiagnostic)
    XCTAssertNil(viewModel.errorMessage)
  }

  @MainActor
  func testRetryExhaustionDisablesFurtherSubmission() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let service = SuspendedSubmissionService()
    let viewModel = ProcessSessionDetailViewModel(session: session) { service }
    await viewModel.connect()
    viewModel.composerText = ProcessSubmissionFixture.content

    viewModel.apply(.retryLimitReached)
    await viewModel.send()
    let callCounts = await service.callCounts

    XCTAssertFalse(viewModel.canSubmit)
    XCTAssertEqual(callCounts, ProcessSubmissionFixture.noCallCounts)
  }

  @MainActor
  func testSubmissionReceiptDoesNotRestoreMaterializedPendingInput() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let service = SuspendedSubmissionService()
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      service
    }
    await viewModel.connect()
    viewModel.composerText = ProcessSubmissionFixture.content

    let send = Task { await viewModel.send() }
    await service.waitUntilSubmitStarted()
    viewModel.apply(
      .sideSnapshot(
        snapshot: try ProcessProjectionFixture.snapshotWithUserEntry(),
        trigger: try ProcessProjectionFixture.refusedEvent()
      )
    )
    await service.completeSubmission()
    await send.value
    viewModel.apply(.event(try ProcessProjectionFixture.acceptedEvent()))

    XCTAssertEqual(viewModel.pendingInputs, try ProcessSubmissionFixture.livePendingInput())
    XCTAssertTrue(viewModel.timeline.isEmpty)
    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.queuedActivity)
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

  @MainActor
  func testCompletedTurnRemovesMatchingPendingInput() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }
    viewModel.apply(.event(try ProcessProjectionFixture.acceptedEvent()))

    viewModel.apply(.event(try ProcessProjectionFixture.completedEvent()))

    XCTAssertTrue(viewModel.pendingInputs.isEmpty)
    XCTAssertEqual(
      viewModel.acceptedInputsAwaitingTranscript,
      try ProcessSubmissionFixture.livePendingInput()
    )
    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.completedActivity)
  }

  @MainActor
  func testTerminalSideSnapshotKeepsAcceptedInputBeforeResponse() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }
    let completed = try ProcessProjectionFixture.completedEvent()
    viewModel.apply(.event(try ProcessProjectionFixture.acceptedEvent()))
    viewModel.apply(.event(completed))

    viewModel.apply(
      .sideSnapshot(
        snapshot: try ProcessProjectionFixture.snapshotWithTerminalResponseMissingUserEntry(),
        trigger: completed
      )
    )

    XCTAssertEqual(
      viewModel.transcriptRows.map(\.id).first,
      ProcessProjectionFixture.acceptedTranscriptRowID
    )
    XCTAssertEqual(
      viewModel.transcriptRows.map(\.id).last,
      ProcessProjectionFixture.completedAssistantTranscriptRowID
    )
  }

  @MainActor
  func testLateReceiptKeepsTerminalInputOutOfPendingState() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let service = SuspendedSubmissionService()
    let viewModel = ProcessSessionDetailViewModel(session: session) { service }
    await viewModel.connect()
    viewModel.composerText = ProcessSubmissionFixture.content
    let send = Task { await viewModel.send() }
    await service.waitUntilSubmitStarted()

    viewModel.apply(.event(try ProcessProjectionFixture.submittedTurnCompletedEvent()))
    await service.completeSubmission()
    await send.value

    XCTAssertTrue(viewModel.pendingInputs.isEmpty)
    XCTAssertEqual(
      viewModel.acceptedInputsAwaitingTranscript,
      try ProcessSubmissionFixture.singlePendingInput()
    )
    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.completedActivity)
  }

  @MainActor
  func testDefinitelyUnsentExactRetryPreservesAmbiguousCommandIdentity() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let service = AmbiguousThenUnsentSubmissionService()
    let viewModel = ProcessSessionDetailViewModel(session: session) { service }
    await viewModel.connect()
    viewModel.composerText = ProcessSubmissionFixture.content

    await viewModel.send()
    await viewModel.send()
    await viewModel.send()
    let submittedCommandIDs = await service.submittedCommandIDs
    let prepareCallCount = await service.prepareCallCount

    XCTAssertEqual(
      submittedCommandIDs,
      ProcessSubmissionFixture.threeIdenticalCommandIDs
    )
    XCTAssertEqual(prepareCallCount, ProcessSubmissionFixture.singleRequestCount)
  }

  @MainActor
  func testTerminalTurnPreservesRemainingQueuedInputActivity() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }
    viewModel.apply(.event(try ProcessProjectionFixture.acceptedEvent()))
    viewModel.apply(.event(try ProcessProjectionFixture.secondAcceptedEvent()))

    viewModel.apply(.event(try ProcessProjectionFixture.completedEvent()))

    XCTAssertEqual(
      viewModel.pendingInputs.map(\.id.rawValue),
      ProcessProjectionFixture.remainingPendingIDs
    )
    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.queuedActivity)
  }

  @MainActor
  func testReconciliationRequiredClearsLiveActiveTurn() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }
    viewModel.apply(.event(try ProcessProjectionFixture.activatedEvent()))

    viewModel.apply(.event(try ProcessProjectionFixture.toolReconciliationTrigger()))

    XCTAssertNil(viewModel.activeTurnID)
    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.recoveryActivity)
  }

  @MainActor
  func testDefaultsMismatchRefreshesTheNextSubmissionEpoch() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let refreshed = try ProcessSubmissionFixture.refreshedSession(from: session)
    let service = DefaultsMismatchThenAcceptingProcessService(refreshed: refreshed)
    let viewModel = ProcessSessionDetailViewModel(session: session) { service }
    await viewModel.connect()
    viewModel.composerText = ProcessSubmissionFixture.content

    await viewModel.send()
    await viewModel.send()
    let preparedVersions = await service.preparedVersions

    XCTAssertEqual(
      preparedVersions,
      ProcessSubmissionFixture.submissionVersions(from: session, refreshed: refreshed)
    )
    XCTAssertEqual(viewModel.session.defaultsVersion, refreshed.defaultsVersion)
  }

  @MainActor
  func testFailedTurnRemovesMatchingPendingInput() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }
    viewModel.apply(.event(try ProcessProjectionFixture.acceptedEvent()))

    viewModel.apply(.event(try ProcessProjectionFixture.failedEvent()))

    XCTAssertTrue(viewModel.pendingInputs.isEmpty)
    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.failedActivity)
  }

  @MainActor
  func testCancelledTurnRemovesMatchingPendingInput() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }
    viewModel.apply(.event(try ProcessProjectionFixture.acceptedEvent()))

    viewModel.apply(.event(try ProcessProjectionFixture.cancelledEvent()))

    XCTAssertTrue(viewModel.pendingInputs.isEmpty)
    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.cancelledActivity)
  }

  @MainActor
  func testCompletedModelCallDoesNotCompleteOwningTurn() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }

    viewModel.apply(.event(try ProcessProjectionFixture.completedModelCallEvent()))

    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.runningActivity)
  }

  @MainActor
  func testAmbiguousModelCallPreservesRecoveryDiagnosticState() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }

    viewModel.apply(.event(try ProcessProjectionFixture.ambiguousModelCallEvent()))

    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.recoveryActivity)
  }

  @MainActor
  func testAcceptedInputDoesNotReplaceActiveTurnActivity() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }
    viewModel.apply(.event(try ProcessProjectionFixture.activatedEvent()))

    viewModel.apply(.event(try ProcessProjectionFixture.secondAcceptedEvent()))

    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.runningActivity)
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

  func testMutationReceiveErrorRetriesTheSameDurableCommand() async throws {
    let submission = try ProcessSubmissionFixture.preparedSubmission()
    let requester = ReceiveErrorThenFrameRequester(
      successFrame: try ProcessDriverFixture.inputSubmitted()
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

  func testUnknownMutationReceiptRetriesTheSameDurableCommand() async throws {
    let submission = try ProcessSubmissionFixture.preparedSubmission()
    let requester = SequencedProcessRequester(
      pages: [
        [try ProcessDriverFixture.unknownMutationReceipt()],
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

  func testDefinitelyUnsentMutationIsNotRetried() async throws {
    let requester = DefinitelyUnsentProcessRequester()
    let service = SignalboxProcessService(
      requester: requester,
      policy: ProcessDriverFixture.oneImmediateMutationRetryPolicy
    )

    let error = await capturedError {
      try await service.submit(ProcessSubmissionFixture.preparedSubmission())
    }
    let openCount = await requester.openCount

    XCTAssertEqual(
      error as? SignalboxProcessRequestOpenError,
      ProcessDriverFixture.definitelyUnsentError
    )
    XCTAssertEqual(openCount, ProcessSubmissionFixture.singleRequestCount)
  }

  func testOneShotReadClosesAtItsTypedDeadline() async throws {
    let requester = DeadlineProcessRequester()
    let service = SignalboxProcessService(
      requester: requester,
      policy: ProcessDriverFixture.immediateDeadlinePolicy
    )

    let error = await capturedServiceError {
      try await service.testConnection()
    }
    let wasClosed = await requester.exchange.wasClosed

    XCTAssertEqual(error, ProcessDriverFixture.deadlineError)
    XCTAssertTrue(wasClosed)
  }

  func testOneShotOpeningStopsAtItsTypedDeadline() async throws {
    let requester = OpeningDeadlineProcessRequester()
    let service = SignalboxProcessService(
      requester: requester,
      policy: ProcessDriverFixture.immediateDeadlinePolicy
    )

    let error = await capturedServiceError {
      try await service.testConnection()
    }
    let wasCancelled = await requester.wasCancelled

    XCTAssertEqual(error, ProcessDriverFixture.openingDeadlineError)
    XCTAssertTrue(wasCancelled)
  }

  func testOneShotOpeningClosesExchangeThatFinishesAfterDeadline() async throws {
    let requester = LateOpeningProcessRequester()
    let service = SignalboxProcessService(
      requester: requester,
      policy: ProcessDriverFixture.immediateDeadlinePolicy
    )

    let error = await capturedServiceError {
      try await service.testConnection()
    }
    let wasClosed = await requester.exchange.wasClosed

    XCTAssertEqual(error, ProcessDriverFixture.openingDeadlineError)
    XCTAssertTrue(wasClosed)
  }

  func testSubmissionReceiptRejectsDifferentSessionIdentity() async throws {
    let submission = try ProcessSubmissionFixture.preparedSubmission()
    let requester = StaticProcessRequester(
      frames: [
        try ProcessDriverFixture.inputSubmitted(
          sessionID: ProcessDriverFixture.metadataSessionA
        )
      ]
    )
    let service = SignalboxProcessService(requester: requester, policy: .nativeDefault)

    let error = await capturedServiceError {
      _ = try await service.submit(submission)
    }

    XCTAssertEqual(error, ProcessDriverFixture.mismatchedSubmissionSessionError)
  }

  func testMetadataReadRejectsDuplicateTagsBeforeReplacement() async throws {
    let sessions = try await makeService().listSessions(includeArchived: true)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let requester = StaticProcessRequester(
      frames: [try ProcessDriverFixture.metadataRead(tagsJSON: #"["duplicate","duplicate"]"#)]
    )
    let service = SignalboxProcessService(requester: requester, policy: .nativeDefault)

    let error = await capturedServiceError {
      _ = try await service.setArchived(true, session: session)
    }

    XCTAssertEqual(error, ProcessDriverFixture.invalidMetadataReadError)
  }

  func testMetadataReceiptRejectsInvalidAttributeKey() async throws {
    let sessions = try await makeService().listSessions(includeArchived: true)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let requester = SequencedProcessRequester(
      pages: [
        [try ProcessDriverFixture.metadataRead()],
        [
          try ProcessDriverFixture.metadataRead(
            type: "session_metadata_replaced",
            attributesJSON: #"{"":"value"}"#
          )
        ],
      ]
    )
    let service = SignalboxProcessService(requester: requester, policy: .nativeDefault)

    let error = await capturedServiceError {
      _ = try await service.setArchived(true, session: session)
    }

    XCTAssertEqual(error, ProcessDriverFixture.invalidMetadataReceiptError)
  }

  func testMutationCancellationIsNotRetriedAsAmbiguous() async throws {
    let requester = CancellationProcessRequester()
    let service = SignalboxProcessService(
      requester: requester,
      policy: ProcessDriverFixture.oneImmediateMutationRetryPolicy
    )

    let error = await capturedError {
      try await service.submit(ProcessSubmissionFixture.preparedSubmission())
    }
    let openCount = await requester.openCount

    XCTAssertTrue(error is CancellationError)
    XCTAssertEqual(openCount, ProcessSubmissionFixture.singleRequestCount)
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

  func testMetadataPageRejectsRegressingCursorAfterMalformedSummary() async throws {
    let requester = StaticProcessRequester(
      frames: [
        try ProcessDriverFixture.metadataPageStart(),
        try ProcessDriverFixture.metadataSummary(
          sessionID: ProcessDriverFixture.metadataSessionB
        ),
        try ProcessDriverFixture.malformedMetadataSummary(),
        try ProcessDriverFixture.metadataPageEnd(
          count: ProcessDriverFixture.twoMetadataCount,
          nextSessionID: ProcessDriverFixture.metadataSessionA
        ),
      ]
    )
    let service = SignalboxProcessService(
      requester: requester,
      policy: ProcessDriverFixture.twoRowMetadataPolicy
    )

    let error = await capturedServiceError {
      _ = try await service.listSessions(includeArchived: true)
    }

    XCTAssertEqual(error, ProcessDriverFixture.metadataRegressingEndCursorError)
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

  func testMetadataListRejectsCumulativeTextAboveTypedCapacity() async throws {
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
            sessionID: ProcessDriverFixture.metadataSessionB
          ),
          try ProcessDriverFixture.metadataPageEnd(
            count: ProcessDriverFixture.singleMetadataCount,
            nextSessionID: nil
          ),
        ],
      ]
    )
    let service = SignalboxProcessService(
      requester: requester,
      policy: ProcessDriverFixture.oneSummaryTextMetadataPolicy
    )

    let error = await capturedServiceError {
      _ = try await service.listSessions(includeArchived: false)
    }

    XCTAssertEqual(error, ProcessDriverFixture.metadataListTextCapacityError)
  }

  func testMetadataPageSkipsSummaryWithNoncanonicalTagOrder() async throws {
    let requester = StaticProcessRequester(
      frames: [
        try ProcessDriverFixture.metadataPageStart(),
        try ProcessDriverFixture.metadataSummaryWithUnorderedTags(),
        try ProcessDriverFixture.metadataPageEnd(
          count: ProcessDriverFixture.oneMetadataCount,
          nextSessionID: nil
        ),
      ]
    )
    let service = SignalboxProcessService(requester: requester, policy: .nativeDefault)

    let sessions = try await service.listSessions(includeArchived: true)

    XCTAssertTrue(sessions.isEmpty)
  }

  func testMetadataPageSkipsSummaryWithEmptyTag() async throws {
    let requester = StaticProcessRequester(
      frames: [
        try ProcessDriverFixture.metadataPageStart(),
        try ProcessDriverFixture.metadataSummaryWithEmptyTag(),
        try ProcessDriverFixture.metadataPageEnd(
          count: ProcessDriverFixture.oneMetadataCount,
          nextSessionID: nil
        ),
      ]
    )
    let service = SignalboxProcessService(requester: requester, policy: .nativeDefault)

    let sessions = try await service.listSessions(includeArchived: true)

    XCTAssertTrue(sessions.isEmpty)
  }

  func testMetadataPageSkipsSummaryWithNullTagScalar() async throws {
    let requester = StaticProcessRequester(
      frames: [
        try ProcessDriverFixture.metadataPageStart(),
        try ProcessDriverFixture.metadataSummaryWithNullTagScalar(),
        try ProcessDriverFixture.metadataPageEnd(
          count: ProcessDriverFixture.oneMetadataCount,
          nextSessionID: nil
        ),
      ]
    )
    let service = SignalboxProcessService(requester: requester, policy: .nativeDefault)

    let sessions = try await service.listSessions(includeArchived: true)

    XCTAssertTrue(sessions.isEmpty)
  }

  func testMetadataPageSkipsSummaryWithOversizedTag() async throws {
    let requester = StaticProcessRequester(
      frames: [
        try ProcessDriverFixture.metadataPageStart(),
        try ProcessDriverFixture.metadataSummaryWithOversizedTag(),
        try ProcessDriverFixture.metadataPageEnd(
          count: ProcessDriverFixture.oneMetadataCount,
          nextSessionID: nil
        ),
      ]
    )
    let service = SignalboxProcessService(requester: requester, policy: .nativeDefault)

    let sessions = try await service.listSessions(includeArchived: true)

    XCTAssertTrue(sessions.isEmpty)
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
    XCTAssertEqual(projection.activity, ProcessProjectionFixture.completedActivity)
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

  func testClosedProcessToolNormalizesToClosedCard() throws {
    let record = SignalboxStoredEvent(
      eventID: SignalboxEventID(rawValue: 1),
      event: .processTool(
        SignalboxProcessToolEvent(
          toolRequestID: SignalboxToolInvocationID(rawValue: ProcessProjectionFixture.closedToolID),
          toolName: ProcessProjectionFixture.closedToolName,
          arguments: nil,
          output: nil,
          status: .closed
        )
      )
    )

    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(tool.status, SignalboxToolCardStatus.closed)
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

  private func fixtureConversation(
    _ conversationID: String,
    in conversations: [SignalboxProcessConversation]
  ) throws -> SignalboxProcessConversation {
    guard
      let conversation = conversations.first(where: {
        $0.conversationID.rawValue == conversationID
      })
    else {
      throw ProcessDriverUpdateRecorderError.missingFixtureSession
    }
    return conversation
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

  private func capturedError<Success>(
    operation: () async throws -> Success
  ) async -> (any Error)? {
    do {
      _ = try await operation()
      return nil
    } catch {
      return error
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
  static let systemPrompt = "Stay concise."
  static let content = "fixture composer draft"
  static let replacementContent = "fixture replacement composer draft"
  static let precomposedContent = "fixture caf\u{00e9}"
  static let decomposedContent = "fixture cafe\u{0301}"
  static let whitespaceSensitiveContent = "  fixture indented composer draft\n"
  static let commandID = "abababab-0000-4000-8000-000000000001"
  static let replacementCommandID = "abababab-0000-4000-8000-000000000004"
  static let failureMessage = "Fixture submission rejection."
  static let defaultsMismatchMessage = "Fixture defaults changed."
  static let ambiguousMutationError = SignalboxProcessServiceError.mutationRetryExhausted(
    code: .commitAmbiguous,
    message: failureMessage
  )
  static let initialSocketPath = "/tmp/signalbox-initial-review-fixture.sock"
  static let replacementSocketPath = "/tmp/signalbox-review-fixture.sock"
  static let acceptedInputID = "abababab-0000-4000-8000-000000000002"
  static let acceptedTurnID = "abababab-0000-4000-8000-000000000003"
  static let singleAcceptedInputID = [acceptedInputID]
  static let retriedCommandIDs = [commandID, commandID]
  static let threeIdenticalCommandIDs = [commandID, commandID, commandID]
  static let editedComposerCommandIDs = [commandID, replacementCommandID]
  static let editedComposerContents = [content, replacementContent]
  static let canonicallyEditedContentBytes = [
    Array(precomposedContent.utf8),
    Array(decomposedContent.utf8),
  ]
  static let whitespaceSensitiveContents = [whitespaceSensitiveContent]
  static let singleRequestCount = 1
  static let singleCallCounts = SubmissionCallCounts(prepare: 1, submit: 1)
  static let noCallCounts = SubmissionCallCounts(prepare: 0, submit: 0)
  static let acceptancePosition = SignalboxCanonicalUInt64(rawValue: 1)
  static let receiptReconciledPendingIDs = [
    acceptedInputID,
    ProcessProjectionFixture.secondPendingID,
  ]

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

  static func refreshedSession(
    from session: SignalboxProcessSession
  ) throws -> SignalboxProcessSession {
    let refreshedDefaultsVersion = SignalboxCanonicalUInt64(
      rawValue: session.defaultsVersion.rawValue + 1
    )
    let defaults = try SignalboxJSONCoding.decoder().decode(
      SignalboxSessionDefaultsRead.self,
      from: Data(
        """
        {
          "type":"session_defaults",
          "session_id":"\(session.id.rawValue)",
          "defaults_version":"\(refreshedDefaultsVersion.rawValue)",
          "model_selection":{
            "kind":"direct",
            "selection_id":"\(ProcessDriverFixture.modelCall)"
          },
          "dangerous_tool_auto_approval":false,
          "system_prompt":null
        }
        """.utf8
      )
    )
    return SignalboxProcessSession(
      id: session.id,
      defaults: defaults,
      metadata: SignalboxProcessSessionMetadata(
        title: session.title,
        tags: session.tags,
        attributes: [:],
        archived: session.archived
      )
    )
  }

  static func submissionVersions(
    from session: SignalboxProcessSession,
    refreshed: SignalboxProcessSession
  ) -> [SignalboxCanonicalUInt64] {
    [session.defaultsVersion, refreshed.defaultsVersion]
  }

  static func preparedSubmission() throws -> SignalboxPreparedInputSubmission {
    SignalboxPreparedInputSubmission(
      commandID: try SignalboxCommandID(validating: commandID),
      sessionID: try SignalboxCanonicalUUID(validating: ProcessDriverFixture.session),
      content: content,
      expectedDefaultsVersion: SignalboxCanonicalUInt64(rawValue: 1)
    )
  }

  static func preparedCreation() throws -> SignalboxPreparedSessionCreation {
    SignalboxPreparedSessionCreation(
      commandID: try SignalboxCommandID(validating: commandID),
      modelSelection: .alias(
        aliasID: try SignalboxCanonicalUUID(validating: MockProcessProtocolFixtures.aliasID)
      ),
      systemPrompt: systemPrompt
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
        acceptancePosition: acceptancePosition,
        content: content
      )
    ]
  }

  static func livePendingInput() throws -> [SignalboxProcessPendingInput] {
    [
      SignalboxProcessPendingInput(
        id: try SignalboxCanonicalUUID(validating: acceptedInputID),
        turnID: try SignalboxCanonicalUUID(validating: ProcessDriverFixture.turn),
        acceptancePosition: acceptancePosition,
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

private actor UpdateEmittingProcessService: SignalboxProcessServiceProtocol {
  private var updateHandlers:
    [@Sendable (SignalboxSessionSynchronizationDriverUpdate) async -> Void] = []

  func testConnection() async {}

  func listSessions(includeArchived: Bool) async -> [SignalboxProcessSession] {
    []
  }

  func setArchived(
    _ archived: Bool,
    session: SignalboxProcessSession
  ) async -> SignalboxProcessSession {
    session
  }

  func prepareInputSubmission(
    session: SignalboxProcessSession,
    content: String
  ) async throws -> SignalboxPreparedInputSubmission {
    try ProcessSubmissionFixture.preparedSubmission()
  }

  func submit(
    _ submission: SignalboxPreparedInputSubmission
  ) async throws -> SignalboxInputSubmitted {
    try ProcessSubmissionFixture.submittedReceipt(sessionID: submission.sessionID)
  }

  func makeSynchronization(
    sessionID: SignalboxCanonicalUUID,
    updates: @escaping @Sendable (SignalboxSessionSynchronizationDriverUpdate) async -> Void
  ) async -> any SignalboxSessionSynchronizing {
    updateHandlers.append(updates)
    return NoopProcessSynchronization()
  }

  func emit(
    _ update: SignalboxSessionSynchronizationDriverUpdate,
    synchronizationIndex: Int
  ) async {
    await updateHandlers[synchronizationIndex](update)
  }
}

private actor SuspendedConnectionProcessService: SignalboxProcessServiceProtocol {
  private var testStarted = false
  private var testStartedWaiter: CheckedContinuation<Void, Never>?
  private var completionWaiter: CheckedContinuation<Void, Never>?

  func testConnection() async {
    testStarted = true
    testStartedWaiter?.resume()
    testStartedWaiter = nil
    await withCheckedContinuation { continuation in
      completionWaiter = continuation
    }
  }

  func waitUntilTestStarted() async {
    guard !testStarted else {
      return
    }
    await withCheckedContinuation { continuation in
      testStartedWaiter = continuation
    }
  }

  func completeTest() {
    completionWaiter?.resume()
    completionWaiter = nil
  }

  func listSessions(includeArchived: Bool) async -> [SignalboxProcessSession] {
    []
  }

  func setArchived(
    _ archived: Bool,
    session: SignalboxProcessSession
  ) async -> SignalboxProcessSession {
    session
  }

  func prepareInputSubmission(
    session: SignalboxProcessSession,
    content: String
  ) async throws -> SignalboxPreparedInputSubmission {
    try ProcessSubmissionFixture.preparedSubmission()
  }

  func submit(
    _ submission: SignalboxPreparedInputSubmission
  ) async throws -> SignalboxInputSubmitted {
    try ProcessSubmissionFixture.submittedReceipt(sessionID: submission.sessionID)
  }

  func makeSynchronization(
    sessionID: SignalboxCanonicalUUID,
    updates: @escaping @Sendable (SignalboxSessionSynchronizationDriverUpdate) async -> Void
  ) async -> any SignalboxSessionSynchronizing {
    NoopProcessSynchronization()
  }
}

private actor SuspendedSessionListProcessService: SignalboxProcessServiceProtocol {
  private let conversations: [SignalboxProcessConversation]
  private var listStarted = false
  private var listStartedWaiter: CheckedContinuation<Void, Never>?
  private var completionWaiter: CheckedContinuation<Void, Never>?

  init(conversations: [SignalboxProcessConversation]) {
    self.conversations = conversations
  }

  func testConnection() async {}

  func listConversations(
    includeArchived: Bool
  ) async -> [SignalboxProcessConversation] {
    listStarted = true
    listStartedWaiter?.resume()
    listStartedWaiter = nil
    await withCheckedContinuation { continuation in
      completionWaiter = continuation
    }
    return conversations
  }

  func listSessions(includeArchived: Bool) async -> [SignalboxProcessSession] {
    []
  }

  func waitUntilListStarted() async {
    guard !listStarted else {
      return
    }
    await withCheckedContinuation { continuation in
      listStartedWaiter = continuation
    }
  }

  func completeList() {
    completionWaiter?.resume()
    completionWaiter = nil
  }

  func setArchived(
    _ archived: Bool,
    session: SignalboxProcessSession
  ) async -> SignalboxProcessSession {
    session
  }

  func prepareInputSubmission(
    session: SignalboxProcessSession,
    content: String
  ) async throws -> SignalboxPreparedInputSubmission {
    try ProcessSubmissionFixture.preparedSubmission()
  }

  func submit(
    _ submission: SignalboxPreparedInputSubmission
  ) async throws -> SignalboxInputSubmitted {
    try ProcessSubmissionFixture.submittedReceipt(sessionID: submission.sessionID)
  }

  func makeSynchronization(
    sessionID: SignalboxCanonicalUUID,
    updates: @escaping @Sendable (SignalboxSessionSynchronizationDriverUpdate) async -> Void
  ) async -> any SignalboxSessionSynchronizing {
    NoopProcessSynchronization()
  }
}

private actor SuspendedArchiveProcessService: SignalboxProcessServiceProtocol {
  private let staleConversations: [SignalboxProcessConversation]
  private let replacement: SignalboxProcessConversation
  private var mutationStarted = false
  private var mutationStartedWaiter: CheckedContinuation<Void, Never>?
  private var completionWaiter: CheckedContinuation<Void, Never>?

  init(
    staleConversations: [SignalboxProcessConversation],
    replacement: SignalboxProcessConversation
  ) {
    self.staleConversations = staleConversations
    self.replacement = replacement
  }

  func testConnection() async {}

  func listConversations(
    includeArchived: Bool
  ) async -> [SignalboxProcessConversation] {
    staleConversations
  }

  func listSessions(includeArchived: Bool) async -> [SignalboxProcessSession] {
    []
  }

  func setConversationArchived(
    _ archived: Bool,
    conversation: SignalboxProcessConversation
  ) async -> SignalboxProcessConversation {
    mutationStarted = true
    mutationStartedWaiter?.resume()
    mutationStartedWaiter = nil
    await withCheckedContinuation { continuation in
      completionWaiter = continuation
    }
    return replacement
  }

  func setArchived(
    _ archived: Bool,
    session: SignalboxProcessSession
  ) async -> SignalboxProcessSession {
    session
  }

  func waitUntilMutationStarted() async {
    guard !mutationStarted else {
      return
    }
    await withCheckedContinuation { continuation in
      mutationStartedWaiter = continuation
    }
  }

  func completeMutation() {
    completionWaiter?.resume()
    completionWaiter = nil
  }

  func prepareInputSubmission(
    session: SignalboxProcessSession,
    content: String
  ) async throws -> SignalboxPreparedInputSubmission {
    try ProcessSubmissionFixture.preparedSubmission()
  }

  func submit(
    _ submission: SignalboxPreparedInputSubmission
  ) async throws -> SignalboxInputSubmitted {
    try ProcessSubmissionFixture.submittedReceipt(sessionID: submission.sessionID)
  }

  func makeSynchronization(
    sessionID: SignalboxCanonicalUUID,
    updates: @escaping @Sendable (SignalboxSessionSynchronizationDriverUpdate) async -> Void
  ) async -> any SignalboxSessionSynchronizing {
    NoopProcessSynchronization()
  }
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

private actor CancellationThenAcceptingProcessService: SignalboxProcessServiceProtocol {
  private(set) var submittedCommandIDs: [String] = []

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
    submittedCommandIDs.append(submission.commandID.rawValue.rawValue)
    guard submittedCommandIDs.count > 1 else {
      throw CancellationError()
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

  func prepareTurnStop(
    session: SignalboxProcessSession,
    activeTurnID: SignalboxCanonicalUUID,
    content: String
  ) async throws -> SignalboxPreparedTurnStop {
    SignalboxPreparedTurnStop(
      commandID: try SignalboxCommandID(validating: ProcessSubmissionFixture.commandID),
      sessionID: session.id,
      activeTurnID: activeTurnID,
      content: content,
      expectedDefaultsVersion: session.defaultsVersion
    )
  }

  func stopTurn(
    _ prepared: SignalboxPreparedTurnStop
  ) async throws -> SignalboxInputSubmitted {
    try ProcessSubmissionFixture.submittedReceipt(sessionID: prepared.sessionID)
  }

  func makeSynchronization(
    sessionID: SignalboxCanonicalUUID,
    updates: @escaping @Sendable (SignalboxSessionSynchronizationDriverUpdate) async -> Void
  ) async -> any SignalboxSessionSynchronizing {
    NoopProcessSynchronization()
  }
}

private actor DefaultsMismatchThenAcceptingProcessService: SignalboxProcessServiceProtocol {
  private let refreshed: SignalboxProcessSession
  private var submitCount = 0
  private(set) var preparedVersions: [SignalboxCanonicalUInt64] = []

  init(refreshed: SignalboxProcessSession) {
    self.refreshed = refreshed
  }

  func testConnection() async throws {}

  func listSessions(includeArchived: Bool) async throws -> [SignalboxProcessSession] {
    [refreshed]
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
    preparedVersions.append(session.defaultsVersion)
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
    submitCount += 1
    guard submitCount > 1 else {
      throw SignalboxProcessServiceError.remote(
        code: .rejected,
        message: ProcessSubmissionFixture.defaultsMismatchMessage,
        detail: .defaultsVersionMismatch(
          sessionID: submission.sessionID,
          expected: submission.expectedDefaultsVersion,
          current: refreshed.defaultsVersion
        )
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

private actor AmbiguousThenUnsentSubmissionService: SignalboxProcessServiceProtocol {
  private(set) var prepareCallCount = 0
  private(set) var submittedCommandIDs: [String] = []

  func testConnection() async {}

  func listSessions(includeArchived: Bool) async -> [SignalboxProcessSession] {
    []
  }

  func setArchived(
    _ archived: Bool,
    session: SignalboxProcessSession
  ) async -> SignalboxProcessSession {
    session
  }

  func prepareInputSubmission(
    session: SignalboxProcessSession,
    content: String
  ) throws -> SignalboxPreparedInputSubmission {
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
  ) throws -> SignalboxInputSubmitted {
    submittedCommandIDs.append(submission.commandID.rawValue.rawValue)
    switch submittedCommandIDs.count {
    case 1:
      throw SignalboxProcessServiceError.mutationRetryExhausted(
        code: .commitAmbiguous,
        message: ProcessSubmissionFixture.failureMessage
      )
    case 2:
      throw ProcessDriverFixture.definitelyUnsentError
    default:
      return try ProcessSubmissionFixture.submittedReceipt(sessionID: submission.sessionID)
    }
  }

  func makeSynchronization(
    sessionID: SignalboxCanonicalUUID,
    updates: @escaping @Sendable (SignalboxSessionSynchronizationDriverUpdate) async -> Void
  ) -> any SignalboxSessionSynchronizing {
    NoopProcessSynchronization()
  }
}

private enum ProcessSubmissionFixtureError: LocalizedError {
  case rejected
  case receiveFailed

  var errorDescription: String? {
    switch self {
    case .rejected:
      ProcessSubmissionFixture.failureMessage
    case .receiveFailed:
      "Fixture receive failure."
    }
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

private actor ReceiveErrorThenFrameRequester: SignalboxProcessRequesting {
  private let successFrame: SignalboxProcessServerFrame
  private(set) var openedRequests: [SignalboxProcessClientRequest] = []

  init(successFrame: SignalboxProcessServerFrame) {
    self.successFrame = successFrame
  }

  func open(
    _ request: SignalboxProcessClientRequest
  ) async -> any SignalboxProcessExchange {
    openedRequests.append(request)
    if openedRequests.count == 1 {
      return ReceiveErrorProcessExchange()
    }
    return StaticProcessExchange(frames: [successFrame])
  }
}

private actor ReceiveErrorProcessExchange: SignalboxProcessExchange {
  func next() throws -> SignalboxProcessServerFrame? {
    throw ProcessSubmissionFixtureError.receiveFailed
  }

  func close() {}
}

private actor CancellationProcessRequester: SignalboxProcessRequesting {
  private(set) var openCount = 0

  func open(
    _ request: SignalboxProcessClientRequest
  ) async -> any SignalboxProcessExchange {
    openCount += 1
    return CancellationProcessExchange()
  }
}

private actor CancellationProcessExchange: SignalboxProcessExchange {
  func next() throws -> SignalboxProcessServerFrame? {
    throw CancellationError()
  }

  func close() {}
}

private actor DefinitelyUnsentProcessRequester: SignalboxProcessRequesting {
  private(set) var openCount = 0

  func open(
    _ request: SignalboxProcessClientRequest
  ) async throws -> any SignalboxProcessExchange {
    openCount += 1
    throw ProcessDriverFixture.definitelyUnsentError
  }
}

private struct DeadlineProcessRequester: SignalboxProcessRequesting {
  let exchange = DeadlineProcessExchange()

  func open(
    _ request: SignalboxProcessClientRequest
  ) async -> any SignalboxProcessExchange {
    exchange
  }
}

private actor OpeningDeadlineProcessRequester: SignalboxProcessRequesting {
  private(set) var wasCancelled = false

  func open(
    _ request: SignalboxProcessClientRequest
  ) async throws -> any SignalboxProcessExchange {
    do {
      try await Task.sleep(for: ProcessDriverFixture.suspendedOpeningDuration)
      throw ProcessDriverUpdateRecorderError.unexpectedRequest
    } catch is CancellationError {
      wasCancelled = true
      throw CancellationError()
    }
  }
}

private struct LateOpeningProcessRequester: SignalboxProcessRequesting {
  let exchange = DeadlineProcessExchange()

  func open(
    _ request: SignalboxProcessClientRequest
  ) async -> any SignalboxProcessExchange {
    do {
      try await Task.sleep(for: ProcessDriverFixture.suspendedOpeningDuration)
    } catch is CancellationError {
      return exchange
    } catch {
      return exchange
    }
    return exchange
  }
}

private actor DeadlineProcessExchange: SignalboxProcessExchange {
  private var continuation: CheckedContinuation<SignalboxProcessServerFrame?, Never>?
  private(set) var wasClosed = false

  func next() async -> SignalboxProcessServerFrame? {
    await withCheckedContinuation { continuation in
      self.continuation = continuation
    }
  }

  func close() {
    wasClosed = true
    continuation?.resume(returning: nil)
    continuation = nil
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
  static let twoMetadataCount: UInt64 = 2
  static let oneMetadataCount: UInt64 = 1
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
  static let metadataRegressingEndCursorError = SignalboxProcessServiceError.invalidPage(
    "The metadata page cursor regressed behind an admitted summary."
  )
  static let metadataListTextCapacityError = SignalboxProcessServiceError.invalidPage(
    "The native session list exceeded its retained UTF-8 byte limit."
  )
  static let definitelyUnsentError = SignalboxProcessRequestOpenError.definitelyUnsent(
    "Fixture connection failed."
  )
  static let deadlineError = SignalboxProcessServiceError.deadlineExceeded(
    "The process request exceeded its response deadline."
  )
  static let openingDeadlineError = SignalboxProcessServiceError.deadlineExceeded(
    "The process request exceeded its response deadline while opening."
  )
  static let suspendedOpeningDuration = Duration.seconds(60)
  static let mismatchedSubmissionSessionError = SignalboxProcessServiceError.unexpectedMessage(
    "The input-submission receipt named a different session."
  )
  static let invalidMetadataReadError = SignalboxProcessServiceError.unexpectedMessage(
    "The metadata read violated the metadata contract."
  )
  static let invalidMetadataReceiptError = SignalboxProcessServiceError.unexpectedMessage(
    "The metadata replacement receipt violated the metadata contract."
  )
  static let oneRowMetadataPolicy = SignalboxProcessApplicationPolicy(
    metadataPageSize: SignalboxCanonicalUInt64(rawValue: 1),
    maximumMetadataPages: SignalboxProcessApplicationPolicy.nativeDefault.maximumMetadataPages,
    ambiguousMutationRetryDelays:
      SignalboxProcessApplicationPolicy.nativeDefault.ambiguousMutationRetryDelays,
    synchronization: SignalboxProcessApplicationPolicy.nativeDefault.synchronization
  )
  static let oneSummaryTextMetadataPolicy = SignalboxProcessApplicationPolicy(
    metadataPageSize: SignalboxCanonicalUInt64(rawValue: 1),
    maximumMetadataPages: SignalboxProcessApplicationPolicy.nativeDefault.maximumMetadataPages,
    maximumMetadataListUTF8Bytes: UInt("Fixture metadata session".utf8.count),
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
  static let twoRowMetadataPolicy = SignalboxProcessApplicationPolicy(
    metadataPageSize: SignalboxCanonicalUInt64(rawValue: twoMetadataCount),
    maximumMetadataPages: SignalboxProcessApplicationPolicy.nativeDefault.maximumMetadataPages,
    ambiguousMutationRetryDelays:
      SignalboxProcessApplicationPolicy.nativeDefault.ambiguousMutationRetryDelays,
    synchronization: SignalboxProcessApplicationPolicy.nativeDefault.synchronization
  )
  static let immediateDeadlinePolicy = SignalboxProcessApplicationPolicy(
    metadataPageSize: SignalboxProcessApplicationPolicy.nativeDefault.metadataPageSize,
    maximumMetadataPages: SignalboxProcessApplicationPolicy.nativeDefault.maximumMetadataPages,
    ambiguousMutationRetryDelays: [],
    oneShotResponseDeadline: .milliseconds(1),
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

  static func metadataRead(
    type: String = "session_metadata",
    tagsJSON: String = "[]",
    attributesJSON: String = "{}"
  ) throws -> SignalboxProcessServerFrame {
    try frame(
      """
      {
        "type":"\(type)",
        "session_id":"\(session)",
        "metadata":{
          "title":"Fixture metadata session",
          "tags":\(tagsJSON),
          "attributes":\(attributesJSON),
          "archived":false
        },
        "last_writer":null
      }
      """
    )
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

  static func metadataSummaryWithUnorderedTags() throws -> SignalboxProcessServerFrame {
    try frame(
      """
      {
        "type":"session_metadata_summary",
        "session_id":"\(metadataSessionA)",
        "defaults_version":"1",
        "model_selection":{
          "kind":"direct",
          "selection_id":"\(modelCall)"
        },
        "dangerous_tool_auto_approval":false,
        "title":"Fixture metadata session",
        "tags":["zeta","alpha"],
        "archived":false,
        "last_writer":null
      }
      """
    )
  }

  static func metadataSummaryWithEmptyTag() throws -> SignalboxProcessServerFrame {
    try metadataSummary(tagsJSON: #"[""]"#)
  }

  static func metadataSummaryWithNullTagScalar() throws -> SignalboxProcessServerFrame {
    try metadataSummary(tagsJSON: #"["alpha\u0000"]"#)
  }

  static func metadataSummaryWithOversizedTag() throws -> SignalboxProcessServerFrame {
    let tag = String(
      repeating: "x",
      count: SignalboxProcessProtocol.maximumIndexedMetadataUTF8Bytes + 1
    )
    return try metadataSummary(tagsJSON: "[\"\(tag)\"]")
  }

  private static func metadataSummary(
    tagsJSON: String
  ) throws -> SignalboxProcessServerFrame {
    try frame(
      """
      {
        "type":"session_metadata_summary",
        "session_id":"\(metadataSessionA)",
        "defaults_version":"1",
        "model_selection":{
          "kind":"direct",
          "selection_id":"\(modelCall)"
        },
        "dangerous_tool_auto_approval":false,
        "title":"Fixture metadata session",
        "tags":\(tagsJSON),
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

  static func inputSubmitted(
    sessionID: String = session
  ) throws -> SignalboxProcessServerFrame {
    try frame(
      """
      {
        "type":"input_submitted",
        "session_id":"\(sessionID)",
        "accepted_input_id":"\(ProcessSubmissionFixture.acceptedInputID)",
        "acceptance_position":"1",
        "turn_id":"\(ProcessSubmissionFixture.acceptedTurnID)"
      }
      """
    )
  }

  static func unknownMutationReceipt() throws -> SignalboxProcessServerFrame {
    try frame(#"{"type":"future_mutation_receipt","opaque":"fixture"}"#)
  }

  static func malformedMetadataSummary() throws -> SignalboxProcessServerFrame {
    try frame(
      """
      {
        "type":"session_metadata_summary",
        "session_id":"not-a-canonical-session-id"
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
    try SignalboxProcessServerFrame.decode(
      from: Data(
        """
        {
          "version":18,
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
  static let crossTurn = "cccccccc-3333-4333-8333-333333333333"
  static let reconciliationAttempt = "dddddddd-3333-4333-8333-333333333333"
  static let reconciliationResultEntry = "eeeeeeee-3333-4333-8333-333333333333"
  static let reconciliationOutput = "Fixture cross-turn result."
  static let completedUserEntry = "aaaaaaaa-2222-4222-8222-222222222222"
  static let completedAssistantEntry = "bbbbbbbb-2222-4222-8222-222222222222"
  static let completedAssistantText = "Fixture terminal assistant response."
  static let closedToolID = "cccccccc-1111-4111-8111-111111111111"
  static let closedToolName = "closed_fixture_tool"
  static let firstPendingID = "ffffffff-ffff-4fff-8fff-ffffffffffff"
  static let secondPendingID = "00000000-0000-4000-8000-000000000001"
  static let firstPendingTurn = "ffffffff-ffff-4fff-8fff-fffffffffffe"
  static let secondPendingTurn = "00000000-0000-4000-8000-000000000002"
  static let pendingIDsInAcceptanceOrder = [firstPendingID, secondPendingID]
  static let remainingPendingIDs = [secondPendingID]
  static let runningActivity = SignalboxProcessActivity(state: .running, label: "Running")
  static let queuedActivity = SignalboxProcessActivity(state: .queued, label: "Queued")
  static let completedActivity = SignalboxProcessActivity(state: .completed, label: "Completed")
  static let failedActivity = SignalboxProcessActivity(state: .failed, label: "Failed")
  static let cancelledActivity = SignalboxProcessActivity(state: .cancelled, label: "Cancelled")
  static let waitingActivity = SignalboxProcessActivity(
    state: .waitingForToolDecision,
    label: "Tool decision unavailable"
  )
  static let unavailableActivity = SignalboxProcessActivity.unavailable
  static let refusedActivity = SignalboxProcessActivity(state: .refused, label: "Refused")
  static let recoveryActivity = SignalboxProcessActivity(
    state: .recoveryRequired,
    label: "Recovery required"
  )
  static let stoppedPhase = SignalboxSessionSynchronizationPhase.stopped
  static let steadyPhase = SignalboxSessionSynchronizationPhase.steady(
    generation: 1,
    cursor: SignalboxCanonicalUInt64(rawValue: 4),
    refreshID: nil
  )
  static let transportDiagnostic = SignalboxSynchronizationDiagnostic(
    kind: .transport,
    stage: .steady,
    message: "Fixture transport diagnostic."
  )
  static let neutralToolCardStatus = SignalboxToolCardStatus.completed
  static let orderedPresentationIDs = [1, 2]
  static let orderedMessageRoles = [SignalboxMessageRole.user, .assistant]
  static let acceptedTranscriptRowID = "accepted-\(ProcessSubmissionFixture.acceptedInputID)"
  static let completedAssistantTranscriptRowID = "timeline-message-1"

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

  static func snapshotWithCrossTurnReconciliationResult() throws
    -> SignalboxSynchronizationSnapshot
  {
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
          "type":"transcript_entry",
          "entry_index":"0",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(proposedToolEntry)",
          "entry":{
            "type":"assistant_tool_use",
            "turn_id":"\(crossTurn)",
            "model_call_id":"\(ProcessDriverFixture.modelCall)",
            "tool_request_id":"\(proposedToolRequest)",
            "tool_name":"\(proposedToolName)",
            "arguments":"{}"
          }
        }
        """,
        """
        {
          "type":"transcript_entry",
          "entry_index":"1",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(reconciliationResultEntry)",
          "entry":{
            "type":"tool_execution_result",
            "tool_request_id":"\(proposedToolRequest)",
            "tool_attempt_id":"\(reconciliationAttempt)",
            "content":"\(reconciliationOutput)"
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

  static func snapshotWithCompletedTurnEntries() throws -> SignalboxSynchronizationSnapshot {
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
          "entry_id":"\(completedUserEntry)",
          "entry":{
            "type":"user",
            "accepted_input_id":"\(ProcessSubmissionFixture.acceptedInputID)",
            "turn_id":"\(ProcessDriverFixture.turn)"
          }
        }
        """,
        """
        {
          "type":"transcript_content",
          "entry_index":"0",
          "fragment_index":"0",
          "final_fragment":true,
          "content_fragment":"\(userText)"
        }
        """,
        """
        {
          "type":"transcript_text_entry",
          "entry_index":"1",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(completedAssistantEntry)",
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
          "entry_index":"1",
          "fragment_index":"0",
          "final_fragment":true,
          "content_fragment":"\(completedAssistantText)"
        }
        """,
        """
        {
          "type":"transcript_entry",
          "entry_index":"2",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(ProcessDriverFixture.completionEntry)",
          "entry":{
            "type":"turn_completed",
            "turn_id":"\(ProcessDriverFixture.turn)"
          }
        }
        """,
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "turn_count":"0",
          "entry_count":"3"
        }
        """,
      ]
    )
  }

  static func snapshotWithCompletionMarkerOnly() throws -> SignalboxSynchronizationSnapshot {
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
          "type":"transcript_entry",
          "entry_index":"0",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(ProcessDriverFixture.completionEntry)",
          "entry":{
            "type":"turn_completed",
            "turn_id":"\(ProcessDriverFixture.turn)"
          }
        }
        """,
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "turn_count":"0",
          "entry_count":"1"
        }
        """,
      ]
    )
  }

  static func snapshotWithTerminalResponseMissingUserEntry() throws
    -> SignalboxSynchronizationSnapshot
  {
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
          "entry_id":"\(completedAssistantEntry)",
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
          "content_fragment":"\(completedAssistantText)"
        }
        """,
        """
        {
          "type":"transcript_entry",
          "entry_index":"1",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(ProcessDriverFixture.completionEntry)",
          "entry":{
            "type":"turn_completed",
            "turn_id":"\(ProcessDriverFixture.turn)"
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

  static func snapshotWithQueuedAndActiveTurns() throws -> SignalboxSynchronizationSnapshot {
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
          "type":"transcript_turn",
          "turn_id":"\(firstPendingTurn)",
          "acceptance_position":"1",
          "state":{
            "type":"queued",
            "accepted_input_id":"\(firstPendingID)",
            "content":"first"
          }
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "acceptance_position":"2",
          "state":{
            "type":"active_running",
            "current_attempt_id":"\(ProcessDriverFixture.attempt)",
            "current_model_call":null
          }
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(secondPendingTurn)",
          "acceptance_position":"3",
          "state":{
            "type":"queued",
            "accepted_input_id":"\(secondPendingID)",
            "content":"second"
          }
        }
        """,
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "turn_count":"3",
          "entry_count":"0"
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

  static func approvalToolTrigger() throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"tool_batch_transition",
        "turn_id":"\(MockProcessProtocolFixtures.approvalTurnID)",
        "model_call_id":"\(MockProcessProtocolFixtures.approvalModelCallID)",
        "state":{
          "type":"proposed",
          "frontier_id":"\(ProcessDriverFixture.frontier)"
        }
      }
      """,
      sessionID: MockSignalboxFixtures.approvalSessionID
    )
  }

  static func toolReconciliationTrigger() throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"turn_tool_reconciliation_required",
        "turn_id":"\(ProcessDriverFixture.turn)",
        "tool_attempt_id":"\(reconciliationAttempt)",
        "terminal_frontier_id":"\(ProcessDriverFixture.frontier)"
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

  static func acceptedSuccessorEvent() throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"input_accepted",
        "accepted_input_id":"\(ProcessSubmissionFixture.acceptedInputID)",
        "turn_id":"\(ProcessSubmissionFixture.acceptedTurnID)",
        "acceptance_position":"1",
        "content":"\(ProcessSubmissionFixture.content)"
      }
      """
    )
  }

  static func secondAcceptedEvent() throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"input_accepted",
        "accepted_input_id":"\(secondPendingID)",
        "turn_id":"\(secondPendingTurn)",
        "acceptance_position":"2",
        "content":"second"
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

  static func completedEvent() throws -> SignalboxFollowedSessionEvent {
    try completedEvent(turnID: ProcessDriverFixture.turn)
  }

  static func submittedTurnCompletedEvent() throws -> SignalboxFollowedSessionEvent {
    try completedEvent(turnID: ProcessSubmissionFixture.acceptedTurnID)
  }

  private static func completedEvent(
    turnID: String
  ) throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"turn_completed",
        "turn_id":"\(turnID)",
        "model_call_id":"\(ProcessDriverFixture.modelCall)",
        "completion_entry_id":"\(ProcessDriverFixture.completionEntry)",
        "terminal_frontier_id":"\(ProcessDriverFixture.frontier)"
      }
      """
    )
  }

  static func failedEvent() throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"turn_failed",
        "turn_id":"\(ProcessDriverFixture.turn)",
        "failure_entry_id":"\(ProcessDriverFixture.completionEntry)",
        "terminal_frontier_id":"\(ProcessDriverFixture.frontier)"
      }
      """
    )
  }

  static func cancelledEvent() throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"turn_cancelled",
        "turn_id":"\(ProcessDriverFixture.turn)",
        "cancellation_entry_id":"\(ProcessDriverFixture.completionEntry)",
        "terminal_frontier_id":"\(ProcessDriverFixture.frontier)"
      }
      """
    )
  }

  static func completedModelCallEvent() throws -> SignalboxFollowedSessionEvent {
    try modelCallEvent(disposition: "completed")
  }

  static func ambiguousModelCallEvent() throws -> SignalboxFollowedSessionEvent {
    try modelCallEvent(disposition: "ambiguous")
  }

  private static func modelCallEvent(
    disposition: String
  ) throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"model_call_transition",
        "turn_id":"\(ProcessDriverFixture.turn)",
        "model_call_id":"\(ProcessDriverFixture.modelCall)",
        "state":{
          "type":"terminal",
          "disposition":"\(disposition)"
        }
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

  static func messageRoles(
    in projection: SignalboxProcessTranscriptProjection
  ) throws -> [SignalboxMessageRole] {
    let messages = projection.records.compactMap { record -> SignalboxProcessMessageEvent? in
      guard case .processMessage(let message) = record.event else {
        return nil
      }
      return message
    }
    guard messages.count == orderedMessageRoles.count else {
      throw ProcessDriverUpdateRecorderError.missingFixtureMessage
    }
    return messages.map(\.role)
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
    _ event: String,
    sessionID: String = ProcessDriverFixture.session
  ) throws -> SignalboxFollowedSessionEvent {
    let message = try message(
      """
      {
        "type":"session_event",
        "cursor":"1",
        "session_id":"\(sessionID)",
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
