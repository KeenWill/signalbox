import XCTest

@testable import SignalboxNative

final class ProcessServiceIntegrationTests: XCTestCase {
  /// S28: an imported transcript frontier creates an independent native session.
  func testImportedTranscriptCanContinueAsANativeSession() async throws {
    let service = makeService()
    let conversations = try await service.listConversations(includeArchived: true)
    let imported = try fixtureConversation(
      MockProcessProtocolFixtures.importedConversationID,
      in: conversations
    )
    let transcript = try await service.readImportedConversation(conversation: imported)
    let aliases = try await service.listModelAliases()
    let alias = try XCTUnwrap(aliases.first)
    let lastPosition = try XCTUnwrap(transcript.entries.last?.position)
    let prepared = try await service.prepareImportedSessionCreation(
      conversation: imported,
      throughPosition: lastPosition,
      relationship: .resume,
      modelSelection: .alias(aliasID: alias.aliasID)
    )

    let sessionID = try await service.createSessionFromImportedFrontier(prepared)
    let refreshed = try await service.listConversations(includeArchived: true)
    let continued = try fixtureConversation(sessionID.rawValue, in: refreshed)

    XCTAssertEqual(
      transcript.entries.count,
      MockProcessProtocolFixtures.importedEntryCount
    )
    XCTAssertEqual(transcript.entries.first?.sourceSpeakerLabel, "User")
    XCTAssertEqual(transcript.entries.last?.sourceSpeakerLabel, "Assistant")
    XCTAssertEqual(sessionID.rawValue, MockProcessProtocolFixtures.continuedSessionID)
    XCTAssertEqual(continued.origin, .native)
  }

  /// S28: imported transcript inspection rejects a noncontiguous frontier inventory.
  func testImportedTranscriptRejectsANoncontiguousFirstPosition() async throws {
    let conversations = try await makeService().listConversations(includeArchived: true)
    let imported = try fixtureConversation(
      MockProcessProtocolFixtures.importedConversationID,
      in: conversations
    )
    let firstPosition = SignalboxCanonicalUInt64(rawValue: 2)
    let requester = StaticProcessRequester(
      frames: [
        try ProcessDriverFixture.importedConversationStart(
          conversationID: imported.conversationID
        ),
        try ProcessDriverFixture.importedConversationEntry(
          position: firstPosition,
          entryID: MockProcessProtocolFixtures.importedUserEntryID
        ),
      ]
    )
    let service = SignalboxProcessService(requester: requester, policy: .nativeDefault)

    let error = await capturedServiceError {
      _ = try await service.readImportedConversation(conversation: imported)
    }

    XCTAssertEqual(error, ProcessDriverFixture.noncontiguousImportedPositionError)
  }

  func testImportedTranscriptCountsUnknownContentKindTowardCapacity() async throws {
    let conversations = try await makeService().listConversations(includeArchived: true)
    let imported = try fixtureConversation(
      MockProcessProtocolFixtures.importedConversationID,
      in: conversations
    )
    let requester = StaticProcessRequester(
      frames: [
        try ProcessDriverFixture.importedConversationStart(
          conversationID: imported.conversationID
        ),
        try ProcessDriverFixture.importedConversationEntry(
          position: SignalboxCanonicalUInt64(rawValue: 1),
          entryID: MockProcessProtocolFixtures.importedUserEntryID,
          sourceSpeaker: #"{"type":"not_attested"}"#,
          contentKind: ProcessDriverFixture.unknownImportedContentKind
        ),
      ]
    )
    let service = SignalboxProcessService(
      requester: requester,
      policy: ProcessDriverFixture.zeroImportedScalarCapacityPolicy
    )

    let error = await capturedServiceError {
      _ = try await service.readImportedConversation(conversation: imported)
    }

    XCTAssertEqual(error, ProcessDriverFixture.importedTranscriptTextCapacityError)
  }

  func testImportedTranscriptCountsUnknownAttestedSpeakerTowardCapacity() async throws {
    let conversations = try await makeService().listConversations(includeArchived: true)
    let imported = try fixtureConversation(
      MockProcessProtocolFixtures.importedConversationID,
      in: conversations
    )
    let requester = StaticProcessRequester(
      frames: [
        try ProcessDriverFixture.importedConversationStart(
          conversationID: imported.conversationID
        ),
        try ProcessDriverFixture.importedConversationEntry(
          position: SignalboxCanonicalUInt64(rawValue: 1),
          entryID: MockProcessProtocolFixtures.importedUserEntryID,
          sourceSpeaker:
            #"{"type":"attested","speaker":"fixture_future_speaker"}"#,
          contentKind: "source_event"
        ),
      ]
    )
    let service = SignalboxProcessService(
      requester: requester,
      policy: ProcessDriverFixture.zeroImportedScalarCapacityPolicy
    )

    let error = await capturedServiceError {
      _ = try await service.readImportedConversation(conversation: imported)
    }

    XCTAssertEqual(error, ProcessDriverFixture.importedTranscriptTextCapacityError)
  }

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

  func testConversationListCountsUnknownSourceFormatTowardCapacity() async throws {
    let requester = StaticProcessRequester(
      frames: [
        try ProcessDriverFixture.conversationPageStart(),
        try ProcessDriverFixture.importedConversationSummary(
          sourceFormat: ProcessDriverFixture.unknownImportedSourceFormat
        ),
        try ProcessDriverFixture.conversationPageEnd(),
      ]
    )
    let service = SignalboxProcessService(
      requester: requester,
      policy: ProcessDriverFixture.zeroConversationScalarCapacityPolicy
    )

    let error = await capturedServiceError {
      _ = try await service.listConversations(includeArchived: true)
    }

    XCTAssertEqual(error, ProcessDriverFixture.conversationListTextCapacityError)
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
    let content = "fixture user input"

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

  func testImportedConversationTitlesRetainTheirSourceDerivationConstraints() {
    XCTAssertTrue(
      signalboxImportedConversationTitleIsAdmissible(
        ProcessConversationTitleFixture.valid
      )
    )
    XCTAssertTrue(signalboxImportedConversationTitleIsAdmissible(nil))
    XCTAssertFalse(
      signalboxImportedConversationTitleIsAdmissible(
        ProcessConversationTitleFixture.empty
      )
    )
    XCTAssertFalse(
      signalboxImportedConversationTitleIsAdmissible(
        ProcessConversationTitleFixture.leadingSpace
      )
    )
    XCTAssertFalse(
      signalboxImportedConversationTitleIsAdmissible(
        ProcessConversationTitleFixture.trailingSpace
      )
    )
    XCTAssertFalse(
      signalboxImportedConversationTitleIsAdmissible(
        ProcessConversationTitleFixture.leadingTab
      )
    )
    XCTAssertFalse(
      signalboxImportedConversationTitleIsAdmissible(
        ProcessConversationTitleFixture.trailingTab
      )
    )
    XCTAssertFalse(
      signalboxImportedConversationTitleIsAdmissible(
        ProcessConversationTitleFixture.lineFeed
      )
    )
    XCTAssertFalse(
      signalboxImportedConversationTitleIsAdmissible(
        ProcessConversationTitleFixture.carriageReturn
      )
    )
    XCTAssertFalse(
      signalboxImportedConversationTitleIsAdmissible(
        ProcessConversationTitleFixture.nul
      )
    )
    XCTAssertFalse(
      signalboxImportedConversationTitleIsAdmissible(
        ProcessConversationTitleFixture.tooManyScalars
      )
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
    await requester.primary.send(try ProcessDriverFixture.modelCallsEnd())
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
    await requester.side.waitForNextCallCount(ProcessDriverFixture.sideStartReadCount)
    await requester.side.send(try ProcessDriverFixture.modelCallsEnd())
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

  func testRefusedSideProjectionRejectsMissingTurnEvidence() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithUserEntry()
    let trigger = try ProcessProjectionFixture.refusedEvent()
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

  func testCompletionSideProjectionRejectsImportedAssistantCollision() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithImportedAssistantCollision()
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

  func testCompletedTurnSideProjectionExcludesPriorToolEvidence() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithCompletedTurnAndPriorToolResult()
    let trigger = try ProcessProjectionFixture.completedTrigger()
    var projector = SignalboxProcessTranscriptProjector()
    _ = try projector.projectAuthoritativeSnapshot(snapshot)

    let projection = try projector.projectSideSnapshot(snapshot, attributableTo: trigger)

    XCTAssertTrue(ProcessProjectionFixture.toolNames(in: projection).isEmpty)
  }

  func testFailedTurnSideProjectionIncludesContiguousTerminalResult() throws {
    let seed = try ProcessProjectionFixture.snapshotWithProposedTool()
    let snapshot = try ProcessProjectionFixture.snapshotWithContiguousToolResultAndFailure()
    let trigger = try ProcessProjectionFixture.failedEvent()
    var projector = SignalboxProcessTranscriptProjector()
    _ = try projector.projectAuthoritativeSnapshot(seed)

    let projection = try projector.projectSideSnapshot(snapshot, attributableTo: trigger)

    XCTAssertEqual(
      ProcessProjectionFixture.toolNames(in: projection),
      ProcessProjectionFixture.terminalFailureToolNames
    )
  }

  func testFailedTurnSideProjectionExcludesSeparatedToolResult() throws {
    let seed = try ProcessProjectionFixture.snapshotWithProposedTool()
    let snapshot = try ProcessProjectionFixture.snapshotWithSeparatedToolResultAndFailure()
    let trigger = try ProcessProjectionFixture.failedEvent()
    var projector = SignalboxProcessTranscriptProjector()
    _ = try projector.projectAuthoritativeSnapshot(seed)

    let projection = try projector.projectSideSnapshot(snapshot, attributableTo: trigger)

    XCTAssertTrue(ProcessProjectionFixture.toolNames(in: projection).isEmpty)
  }

  func testFailedTurnSideProjectionExcludesImportedMarkerIdentityCollision() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithImportedFailureMarkerCollision()
    let trigger = try ProcessProjectionFixture.failedEvent()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectSideSnapshot(snapshot, attributableTo: trigger)

    XCTAssertEqual(
      projection.records.map(\.event.kind),
      ProcessProjectionFixture.terminalFailureEventKinds
    )
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

  func testToolReconciliationSideProjectionIncludesCompleteTerminalResultSuffix() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithReconciliationResultSuffix()
    let trigger = try ProcessProjectionFixture.toolReconciliationTrigger()
    var projector = SignalboxProcessTranscriptProjector()
    _ = try projector.projectAuthoritativeSnapshot(snapshot)

    let projection = try projector.projectSideSnapshot(snapshot, attributableTo: trigger)

    XCTAssertEqual(
      ProcessProjectionFixture.toolNames(in: projection),
      ProcessProjectionFixture.reconciliationSuffixToolNames
    )
  }

  func testToolReconciliationSideProjectionFindsSuffixBeforeLaterTurnEntry() throws {
    let snapshot = try ProcessProjectionFixture
      .snapshotWithReconciliationResultSuffixAndLaterTurnEntry()
    let trigger = try ProcessProjectionFixture.toolReconciliationTrigger()
    var projector = SignalboxProcessTranscriptProjector()
    _ = try projector.projectAuthoritativeSnapshot(snapshot)

    let projection = try projector.projectSideSnapshot(snapshot, attributableTo: trigger)

    XCTAssertEqual(
      ProcessProjectionFixture.toolNames(in: projection),
      ProcessProjectionFixture.reconciliationSuffixToolNames
    )
  }

  func testToolReconciliationSideProjectionFindsClosedSuffixBeforeLaterTurnEntry() throws {
    let snapshot = try ProcessProjectionFixture
      .snapshotWithClosedReconciliationResultAndLaterTurnEntry()
    let trigger = try ProcessProjectionFixture.toolReconciliationTrigger()
    var projector = SignalboxProcessTranscriptProjector()
    _ = try projector.projectAuthoritativeSnapshot(snapshot)

    let projection = try projector.projectSideSnapshot(snapshot, attributableTo: trigger)

    XCTAssertEqual(
      ProcessProjectionFixture.toolNames(in: projection),
      ProcessProjectionFixture.reconciliationClosedToolNames
    )
  }

  func testToolReconciliationSideProjectionAcceptsMixedClosedResultSuffix() throws {
    let snapshot = try ProcessProjectionFixture
      .snapshotWithMixedClosedReconciliationResult()
    let trigger = try ProcessProjectionFixture.toolReconciliationTrigger()
    var projector = SignalboxProcessTranscriptProjector()
    _ = try projector.projectAuthoritativeSnapshot(snapshot)

    let projection = try projector.projectSideSnapshot(snapshot, attributableTo: trigger)

    XCTAssertEqual(
      ProcessProjectionFixture.toolNames(in: projection),
      ProcessProjectionFixture.reconciliationSuffixToolNames
    )
  }

  func testToolReconciliationSideProjectionAcceptsForegroundDelegationResult() throws {
    let snapshot = try ProcessProjectionFixture
      .snapshotWithForegroundDelegationReconciliationResult()
    let trigger = try ProcessProjectionFixture.toolReconciliationTrigger()
    var projector = SignalboxProcessTranscriptProjector()
    _ = try projector.projectAuthoritativeSnapshot(snapshot)

    let projection = try projector.projectSideSnapshot(snapshot, attributableTo: trigger)

    XCTAssertEqual(
      ProcessProjectionFixture.toolNames(in: projection),
      ProcessProjectionFixture.reconciliationSuffixToolNames
    )
    XCTAssertEqual(
      ProcessProjectionFixture.conservativeKinds(in: projection),
      ProcessProjectionFixture.foregroundDelegationResultKinds
    )
    XCTAssertEqual(
      ProcessProjectionFixture.toolSummaries(in: projection),
      ProcessProjectionFixture.foregroundDelegationToolSummaries
    )
  }

  func testToolReconciliationSideProjectionRejectsAttemptlessDeniedSuffix() throws {
    let snapshot = try ProcessProjectionFixture
      .snapshotWithAttemptlessDeniedReconciliationResults()
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

  func testToolReconciliationSideProjectionRejectsUncorrelatedClosedSuffix() throws {
    let snapshot = try ProcessProjectionFixture
      .snapshotWithDeniedAndUncorrelatedClosedReconciliationResults()
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

  func testToolReconciliationSideProjectionRejectsStaleSameTurnResultRun() throws {
    let snapshot = try ProcessProjectionFixture
      .snapshotWithStaleSameTurnReconciliationResult()
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
      ProcessProjectionFixture.presentationOrders(in: projection),
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

  func testProposedToolSideProjectionExcludesImportedSourceCollision() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithSourceQualifiedReusedToolRequest()
    let trigger = try ProcessProjectionFixture.proposedToolTrigger()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectSideSnapshot(snapshot, attributableTo: trigger)

    XCTAssertEqual(
      ProcessProjectionFixture.toolNames(in: projection),
      ProcessProjectionFixture.terminalFailureToolNames
    )
  }

  func testProposedToolSideProjectionExcludesLaterUsageForExactModelCall() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithProposedToolAndUnrelatedUsage()
    let trigger = try ProcessProjectionFixture.proposedToolTrigger()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectSideSnapshot(snapshot, attributableTo: trigger)

    XCTAssertTrue(ProcessProjectionFixture.modelCallUsageIDs(in: projection).isEmpty)
  }

  func testProjectedResultsSideProjectionExcludesLaterUsageForExactModelCall() throws {
    let seed = try ProcessProjectionFixture.snapshotWithProposedTool()
    let snapshot = try ProcessProjectionFixture.snapshotWithProjectedResultAndLaterUsage()
    let trigger = try ProcessProjectionFixture.projectedResultsTrigger()
    var projector = SignalboxProcessTranscriptProjector()
    _ = try projector.projectAuthoritativeSnapshot(seed)

    let projection = try projector.projectSideSnapshot(snapshot, attributableTo: trigger)

    XCTAssertEqual(
      ProcessProjectionFixture.toolNames(in: projection),
      ProcessProjectionFixture.terminalFailureToolNames
    )
    XCTAssertTrue(ProcessProjectionFixture.modelCallUsageIDs(in: projection).isEmpty)
  }

  func testPreparedModelCallSideProjectionExcludesLaterUsageForSameCall() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithLaterModelUsageOnly()
    let trigger = try ProcessProjectionFixture.preparedModelCallTrigger()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectSideSnapshot(snapshot, attributableTo: trigger)

    XCTAssertTrue(ProcessProjectionFixture.modelCallUsageIDs(in: projection).isEmpty)
  }

  func testApprovalDecisionSideProjectionPromotesSuccessorWait() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithAwaitingProposedTool()
    let trigger = try ProcessProjectionFixture.userApprovalEvent()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectSideSnapshot(snapshot, attributableTo: trigger)
    let tool = try ProcessProjectionFixture.onlyTool(in: projection)

    XCTAssertEqual(tool.status, .awaitingDecision)
  }

  func testContextCompactionSideProjectionIncludesItsSummary() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithContextSummary()
    let trigger = try ProcessProjectionFixture.contextCompactedTrigger()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectSideSnapshot(snapshot, attributableTo: trigger)
    let normalizer = try SignalboxIncrementalEventNormalizer(records: projection.records)
    let message = try ProcessProjectionFixture.onlyTimelineMessage(in: normalizer.timelineItems)

    XCTAssertEqual(message.text, ProcessProjectionFixture.contextSummaryText)
    XCTAssertEqual(message.role, .assistant)
    XCTAssertEqual(message.label, ProcessProjectionFixture.contextSummaryLabel)
    XCTAssertEqual(
      ProcessProjectionFixture.noticeTitles(in: normalizer.timelineItems),
      ProcessProjectionFixture.contextCompactionNoticeTitles
    )
  }

  func testContextCompactionSideProjectionKeepsExistingIndexZeroEntry() throws {
    let initialSnapshot = try ProcessProjectionFixture.snapshotWithCompletedTurnEntries()
    let sideSnapshot = try ProcessProjectionFixture.snapshotWithContextSummary()
    let trigger = try ProcessProjectionFixture.contextCompactedTrigger()
    var projector = SignalboxProcessTranscriptProjector()

    let initial = try projector.projectAuthoritativeSnapshot(initialSnapshot)
    let side = try projector.projectSideSnapshot(sideSnapshot, attributableTo: trigger)
    let initialRecord = try XCTUnwrap(initial.records.first)
    let summaryRecord = try ProcessProjectionFixture.contextSummaryRecord(in: side)
    let normalizer = try SignalboxIncrementalEventNormalizer(records: initial.records)
    normalizer.upsert(contentsOf: side.records)

    XCTAssertNotEqual(initialRecord.eventID, summaryRecord.eventID)
    XCTAssertEqual(
      normalizer.records.count,
      initial.records.count + side.records.count
    )
  }

  func testAuthoritativeRefreshReanchorsToolAfterCompaction() throws {
    var projector = SignalboxProcessTranscriptProjector()
    let before = try projector.projectAuthoritativeSnapshot(
      ProcessProjectionFixture.snapshotWithToolBeforeCompaction()
    )

    let after = try projector.projectAuthoritativeSnapshot(
      ProcessProjectionFixture.snapshotWithToolAfterCompaction()
    )
    let beforeTool = try ProcessProjectionFixture.onlyToolRecord(in: before)
    let afterTool = try ProcessProjectionFixture.onlyToolRecord(in: after)
    let normalizer = try SignalboxIncrementalEventNormalizer(records: after.records)

    XCTAssertEqual(afterTool.eventID, beforeTool.eventID)
    XCTAssertEqual(
      ProcessProjectionFixture.timelineKinds(in: normalizer.timelineItems),
      ProcessProjectionFixture.compactedToolTimelineKinds
    )
  }

  func testAuthoritativeRefreshReleasesCompactedPresentationIdentity() throws {
    var projector = SignalboxProcessTranscriptProjector()
    let before = try projector.projectAuthoritativeSnapshot(
      ProcessProjectionFixture.snapshotWithToolBeforeCompaction()
    )
    _ = try projector.projectAuthoritativeSnapshot(
      ProcessProjectionFixture.snapshotWithToolAfterCompaction()
    )

    let reintroduced = try projector.projectAuthoritativeSnapshot(
      ProcessProjectionFixture.snapshotWithToolBeforeCompaction()
    )

    XCTAssertNotEqual(
      try ProcessProjectionFixture.importedContentEventID(.sourceEvent, in: before),
      try ProcessProjectionFixture.importedContentEventID(.sourceEvent, in: reintroduced)
    )
  }

  func testToolProjectionQualifiesReusedRequestBySourceEntry() throws {
    var projector = SignalboxProcessTranscriptProjector()
    let projection = try projector.projectAuthoritativeSnapshot(
      ProcessProjectionFixture.snapshotWithSourceQualifiedReusedToolRequest()
    )
    let normalizer = try SignalboxIncrementalEventNormalizer(records: projection.records)

    XCTAssertEqual(
      ProcessProjectionFixture.toolSummaries(in: projection),
      ProcessProjectionFixture.sourceQualifiedToolSummaries
    )
    XCTAssertEqual(
      Set(normalizer.timelineItems.map(\.id)).count,
      ProcessProjectionFixture.sourceQualifiedToolSummaries.count
    )
    XCTAssertEqual(
      try ProcessProjectionFixture.nativeToolRequestPositionName(in: projection),
      ProcessProjectionFixture.proposedToolName
    )
  }

  func testImportedSemanticMarkersProjectAsTypedTimelineNotices() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithImportedMarkers()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)
    let normalizer = try SignalboxIncrementalEventNormalizer(records: projection.records)

    XCTAssertEqual(
      try ProcessProjectionFixture.importedContentKinds(in: projection),
      ProcessProjectionFixture.importedContentKinds
    )
    XCTAssertEqual(
      ProcessProjectionFixture.noticeTitles(in: normalizer.timelineItems),
      ProcessProjectionFixture.importedNoticeTitles
    )
    XCTAssertEqual(
      ProcessProjectionFixture.noticeDetailValues(in: normalizer.timelineItems),
      ProcessProjectionFixture.importedSpeakerLabels
    )
    XCTAssertEqual(
      ProcessProjectionFixture.unknownKinds(in: normalizer.timelineItems),
      ProcessProjectionFixture.futureImportedPresentationKinds
    )
  }

  func testModelIdentityAndUsageProjectAsTypedTimelineNotices() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithModelPresentationEvidence()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)
    let normalizer = try SignalboxIncrementalEventNormalizer(records: projection.records)

    XCTAssertEqual(
      ProcessProjectionFixture.noticeTitles(in: normalizer.timelineItems),
      ProcessProjectionFixture.modelNoticeTitles
    )
    XCTAssertEqual(
      ProcessProjectionFixture.noticeDetailValues(in: normalizer.timelineItems),
      ProcessProjectionFixture.modelNoticeDetailValues
    )
    XCTAssertEqual(
      projection.records.map(\.event.kind),
      ProcessProjectionFixture.modelPresentationEventKinds
    )
  }

  func testModelUsagePresentationIDsSurviveEarlierInsertion() throws {
    let initialSnapshot = try ProcessProjectionFixture.snapshotWithLaterModelUsageOnly()
    let updatedSnapshot = try ProcessProjectionFixture.snapshotWithEarlierModelUsageInserted()
    var projector = SignalboxProcessTranscriptProjector()

    let initial = try projector.projectAuthoritativeSnapshot(initialSnapshot)
    let updated = try projector.projectAuthoritativeSnapshot(updatedSnapshot)
    let normalizer = try SignalboxIncrementalEventNormalizer(records: initial.records)
    try normalizer.replaceAll(with: updated.records)

    XCTAssertEqual(
      try ProcessProjectionFixture.modelCallUsageEventID(
        ProcessDriverFixture.modelCall,
        in: initial
      ),
      try ProcessProjectionFixture.modelCallUsageEventID(
        ProcessDriverFixture.modelCall,
        in: updated
      )
    )
    XCTAssertEqual(
      ProcessProjectionFixture.modelCallUsageIDs(in: updated),
      ProcessProjectionFixture.orderedModelCallUsageIDs
    )
    XCTAssertEqual(
      ProcessProjectionFixture.modelCallUsageIDs(in: normalizer.timelineItems),
      ProcessProjectionFixture.orderedModelCallUsageIDs
    )
  }

  func testModelUsageGainsAnchorOrderWithoutChangingStableIdentity() throws {
    let initialSnapshot = try ProcessProjectionFixture.snapshotWithLaterModelUsageOnly()
    let anchoredSnapshot =
      try ProcessProjectionFixture.snapshotWithFailedUsageMarkerAndLaterImportedContent()
    var projector = SignalboxProcessTranscriptProjector()

    let initial = try projector.projectAuthoritativeSnapshot(initialSnapshot)
    let anchored = try projector.projectAuthoritativeSnapshot(anchoredSnapshot)
    let normalizer = try SignalboxIncrementalEventNormalizer(records: initial.records)
    try normalizer.replaceAll(with: anchored.records)

    XCTAssertEqual(
      try ProcessProjectionFixture.modelCallUsageEventID(
        ProcessDriverFixture.modelCall,
        in: initial
      ),
      try ProcessProjectionFixture.modelCallUsageEventID(
        ProcessDriverFixture.modelCall,
        in: anchored
      )
    )
    XCTAssertEqual(
      ProcessProjectionFixture.noticeTitles(in: normalizer.timelineItems),
      ProcessProjectionFixture.reanchoredUsageNoticeTitles
    )
  }

  func testModelUsageIDsStayDistinctWhenAnchorsAreReused() throws {
    var projector = SignalboxProcessTranscriptProjector()
    let initial = try projector.projectAuthoritativeSnapshot(
      ProcessProjectionFixture.snapshotBeforeUsageAnchorReuse()
    )
    let updated = try projector.projectAuthoritativeSnapshot(
      ProcessProjectionFixture.snapshotAfterUsageAnchorReuse()
    )

    XCTAssertEqual(
      Set(ProcessProjectionFixture.modelCallUsageEventIDs(in: updated)).count,
      ProcessProjectionFixture.anchorReuseUsageCount
    )
    XCTAssertEqual(
      try ProcessProjectionFixture.modelCallUsageEventID(
        ProcessDriverFixture.modelCall,
        in: updated
      ),
      try ProcessProjectionFixture.modelCallUsageEventID(
        ProcessDriverFixture.modelCall,
        in: initial
      )
    )
  }

  func testImportedModelCallIdentityCannotReplaceNativeUsageAnchor() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithImportedModelCallAnchorCollision()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)

    XCTAssertEqual(
      ProcessProjectionFixture.modelCallUsageIDs(in: projection),
      ProcessProjectionFixture.triggeredModelCallUsageIDs
    )
  }

  @MainActor
  func testAuthoritativeRefreshReordersUsageWhenTerminalAnchorAppears() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }

    viewModel.apply(
      .authoritativeSnapshot(
        try ProcessProjectionFixture.snapshotWithRecoveryUsageMarkerAndLaterImportedContent()
      )
    )
    viewModel.apply(
      .authoritativeSnapshot(
        try ProcessProjectionFixture.snapshotWithFailedUsageMarkerAndLaterImportedContent()
      )
    )

    XCTAssertEqual(
      ProcessProjectionFixture.noticeTitles(in: viewModel.timeline),
      ProcessProjectionFixture.reanchoredUsageNoticeTitles
    )
  }

  @MainActor
  func testAuthoritativeRefreshPublishesEarlierInsertedUsageInSnapshotOrder() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }

    viewModel.apply(
      .authoritativeSnapshot(try ProcessProjectionFixture.snapshotWithLaterModelUsageOnly())
    )
    viewModel.apply(
      .authoritativeSnapshot(try ProcessProjectionFixture.snapshotWithEarlierModelUsageInserted())
    )

    XCTAssertEqual(
      ProcessProjectionFixture.modelCallUsageIDs(in: viewModel.timeline),
      ProcessProjectionFixture.orderedModelCallUsageIDs
    )
  }

  @MainActor
  func testAuthoritativeRefreshDoesNotMoveRetainedRowsAcrossLiveUnknownEvent() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }

    viewModel.apply(
      .authoritativeSnapshot(try ProcessProjectionFixture.snapshotWithEarlierModelUsageOnly())
    )
    viewModel.apply(.event(try ProcessProjectionFixture.unknownFollowedEvent()))
    viewModel.apply(
      .authoritativeSnapshot(try ProcessProjectionFixture.snapshotWithEarlierModelUsageInserted())
    )

    XCTAssertEqual(
      ProcessProjectionFixture.timelineKinds(in: viewModel.timeline),
      ProcessProjectionFixture.retainedRowTimelineKindsBeforeRemoval
    )

    viewModel.apply(
      .authoritativeSnapshot(try ProcessProjectionFixture.snapshotWithLaterModelUsageOnly())
    )

    XCTAssertEqual(
      ProcessProjectionFixture.timelineKinds(in: viewModel.timeline),
      ProcessProjectionFixture.retainedRowTimelineKindsAfterRemoval
    )
  }

  func testAnchoredUsageAndAdjacentTurnStateKeepDistinctTimelineRows() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithAdjacentUsageAndUnknownTurnState()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)
    let normalizer = try SignalboxIncrementalEventNormalizer(records: projection.records)

    XCTAssertEqual(
      ProcessProjectionFixture.timelineKinds(in: normalizer.timelineItems),
      ProcessProjectionFixture.adjacentUsageAndTurnStateTimelineKinds
    )
  }

  func testCrossTurnModelUsageAnchorIsRejected() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithCrossTurnModelUsageAnchor()
    var projector = SignalboxProcessTranscriptProjector()

    XCTAssertThrowsError(
      try projector.projectAuthoritativeSnapshot(snapshot)
    ) { error in
      XCTAssertEqual(
        error as? SignalboxProcessTranscriptProjectionError,
        .mismatchedModelCallUsageTurn
      )
    }
  }

  func testUnknownFollowedEventProjectsAsConservativeEvidence() throws {
    let followed = try ProcessProjectionFixture.unknownFollowedEvent()
    let projector = SignalboxProcessTranscriptProjector()

    let event = try XCTUnwrap(projector.projectUnrecognizedFollowedEvent(followed))

    XCTAssertEqual(event.kind, ProcessProjectionFixture.futureFollowedEventKind)
  }

  func testUnknownFollowedModelCallStateRetainsAttribution() throws {
    let followed = try ProcessProjectionFixture.unknownModelCallFollowedEvent()
    let projector = SignalboxProcessTranscriptProjector()

    let event = try XCTUnwrap(projector.projectUnrecognizedFollowedEvent(followed))

    XCTAssertEqual(
      event.diagnostic,
      ProcessProjectionFixture.futureFollowedModelCallDiagnostic
    )
  }

  func testUnknownSnapshotTurnStateProjectsAsVisibleTimelineCard() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithUnknownTurnStates()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)
    let normalizer = try SignalboxIncrementalEventNormalizer(records: projection.records)

    XCTAssertEqual(
      ProcessProjectionFixture.unknownKinds(in: normalizer.timelineItems),
      ProcessProjectionFixture.futureSnapshotStatePresentationKinds
    )
    XCTAssertEqual(
      ProcessProjectionFixture.unknownDiagnostics(in: normalizer.timelineItems),
      ProcessProjectionFixture.futureSnapshotStateDiagnostics
    )
  }

  func testUnknownSnapshotTurnStateAnchorsBeforeOwningTranscriptEntries() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithTranscriptBeforeUnknownTurnState()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)
    let normalizer = try SignalboxIncrementalEventNormalizer(records: projection.records)

    XCTAssertEqual(
      ProcessProjectionFixture.timelineKinds(in: normalizer.timelineItems),
      ProcessProjectionFixture.unknownThenTranscriptTimelineKinds
    )
  }

  func testUnknownSnapshotTurnStateReanchorsAfterAuthoritativeRefresh() throws {
    var projector = SignalboxProcessTranscriptProjector()
    let unanchored = try projector.projectAuthoritativeSnapshot(
      ProcessProjectionFixture.snapshotWithUnknownTurnState()
    )

    let anchored = try projector.projectAuthoritativeSnapshot(
      ProcessProjectionFixture.snapshotWithTranscriptBeforeUnknownTurnState()
    )
    let normalizer = try SignalboxIncrementalEventNormalizer(records: anchored.records)

    XCTAssertEqual(anchored.records.first?.eventID, unanchored.records.first?.eventID)
    XCTAssertEqual(
      ProcessProjectionFixture.timelineKinds(in: normalizer.timelineItems),
      ProcessProjectionFixture.unknownThenTranscriptTimelineKinds
    )
  }

  func testCompactionKeepsDistinctUnknownTurnPresentationIdentities() throws {
    var projector = SignalboxProcessTranscriptProjector()
    _ = try projector.projectAuthoritativeSnapshot(
      ProcessProjectionFixture.snapshotWithTranscriptBeforeUnknownTurnState()
    )

    let projection = try projector.projectAuthoritativeSnapshot(
      ProcessProjectionFixture.snapshotWithCompactedAndNewUnknownTurn()
    )
    let normalizer = try SignalboxIncrementalEventNormalizer(records: projection.records)

    XCTAssertEqual(
      ProcessProjectionFixture.unknownKinds(in: normalizer.timelineItems),
      ProcessProjectionFixture.twoUnknownTurnStatePresentationKinds
    )
    XCTAssertEqual(Set(projection.records.map(\.eventID)).count, projection.records.count)
  }

  func testPlanReadUsesTypedFaithfulTimelineSummary() throws {
    let record = ProcessProjectionFixture.planReadToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(tool.displayName, ProcessProjectionFixture.planReadDisplayName)
    XCTAssertEqual(
      tool.compactArgumentSummary,
      ProcessProjectionFixture.planReadArgumentPresentation
    )
    XCTAssertEqual(tool.outputPreview, ProcessProjectionFixture.planReadOutputPresentation)
  }

  func testPlanCreateUsesTypedArgumentsAndRawOutput() throws {
    let record = ProcessProjectionFixture.planCreateToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(tool.displayName, ProcessProjectionFixture.planWriteDisplayName)
    XCTAssertEqual(
      tool.compactArgumentSummary,
      ProcessProjectionFixture.planCreateArgumentPresentation
    )
    XCTAssertEqual(tool.outputPreview, ProcessProjectionFixture.planCreateRawOutputPreview)
  }

  func testPlanWriteWithMismatchedRequestProvenanceKeepsRawOutputVisible() throws {
    let record = ProcessProjectionFixture.planCreateWithMismatchedRequestProvenanceToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(tool.outputPreview, ProcessProjectionFixture.planCreateRawOutputPreview)
  }

  func testPlanWriteWithMismatchedTurnProvenanceKeepsRawOutputVisible() throws {
    let record = ProcessProjectionFixture.planCreateWithMismatchedTurnProvenanceToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(tool.outputPreview, ProcessProjectionFixture.planCreateRawOutputPreview)
  }

  func testPlanWriteWithMismatchedAttemptProvenanceKeepsRawOutputVisible() throws {
    let record = ProcessProjectionFixture.planCreateWithMismatchedAttemptProvenanceToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(tool.outputPreview, ProcessProjectionFixture.planCreateRawOutputPreview)
  }

  func testPlanWriteWithMismatchedIssuingAttemptKeepsRawOutputVisible() throws {
    let record = ProcessProjectionFixture.planCreateWithMismatchedIssuingAttemptToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(
      tool.outputPreview,
      ProcessProjectionFixture.mismatchedIssuingAttemptPlanCreateOutputPreview
    )
  }

  func testDeniedPlanWriteKeepsResultShapedOutputRaw() throws {
    let record = ProcessProjectionFixture.deniedPlanCreateToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(tool.outputLabel, ProcessProjectionFixture.rawToolOutputLabel)
    XCTAssertEqual(tool.outputPreview, ProcessProjectionFixture.planCreateRawOutputPreview)
  }

  func testClosedPlanReadKeepsResultShapedOutputRaw() throws {
    let record = ProcessProjectionFixture.closedPlanReadToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(tool.outputLabel, ProcessProjectionFixture.rawToolOutputLabel)
    XCTAssertEqual(tool.outputPreview, ProcessProjectionFixture.planReadRawOutputPreview)
  }

  func testPlanRevisionUsesTypedArgumentsAndRawOutput() throws {
    let record = ProcessProjectionFixture.planReviseToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(tool.displayName, ProcessProjectionFixture.planWriteDisplayName)
    XCTAssertEqual(
      tool.compactArgumentSummary,
      ProcessProjectionFixture.planReviseArgumentPresentation
    )
    XCTAssertEqual(tool.outputPreview, ProcessProjectionFixture.planReviseRawOutputPreview)
  }

  func testPlanStatusUsesTypedArgumentsAndRawOutput() throws {
    let record = ProcessProjectionFixture.planStatusToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(tool.displayName, ProcessProjectionFixture.planWriteDisplayName)
    XCTAssertEqual(
      tool.compactArgumentSummary,
      ProcessProjectionFixture.planStatusArgumentPresentation
    )
    XCTAssertEqual(tool.outputPreview, ProcessProjectionFixture.planStatusRawOutputPreview)
  }

  func testPlanDependencyUsesTypedArgumentsAndRawOutput() throws {
    let record = ProcessProjectionFixture.planDependencyToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(tool.displayName, ProcessProjectionFixture.planWriteDisplayName)
    XCTAssertEqual(
      tool.compactArgumentSummary,
      ProcessProjectionFixture.planDependencyArgumentPresentation
    )
    XCTAssertEqual(tool.outputPreview, ProcessProjectionFixture.planDependencyRawOutputPreview)
  }

  func testMalformedPlanWriteKeepsRawArgumentsVisible() throws {
    let record = ProcessProjectionFixture.malformedPlanWriteToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(
      tool.compactArgumentSummary,
      ProcessProjectionFixture.malformedPlanArguments
    )
  }

  func testMalformedPlanReadKeepsRawArgumentsVisible() throws {
    let record = ProcessProjectionFixture.malformedPlanReadToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(
      tool.compactArgumentSummary,
      ProcessProjectionFixture.malformedPlanReadArguments
    )
  }

  func testFloatingPlanReadIdentityKeepsRawArgumentsVisible() throws {
    let record = ProcessProjectionFixture.floatingPlanReadIdentityToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(
      tool.compactArgumentSummary,
      ProcessProjectionFixture.floatingPlanReadArguments
    )
  }

  func testExponentPlanOrdinalKeepsRawOutputVisible() throws {
    let record = ProcessProjectionFixture.exponentPlanOrdinalToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(
      tool.outputPreview,
      ProcessProjectionFixture.exponentPlanOrdinalOutput
    )
  }

  func testPlanEventWithoutProvenanceKeepsRawOutputVisible() throws {
    let record = ProcessProjectionFixture.planOutputWithoutProvenanceToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(
      tool.outputPreview,
      ProcessProjectionFixture.planOutputWithoutProvenance
    )
  }

  func testMalformedPlanReadOutputKeepsRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.malformedPlanReadOutputToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(
      tool.outputPreview,
      ProcessProjectionFixture.malformedPlanReadOutput
    )
  }

  func testPlanReadWithOtherwiseCorrelatedHistoryKeepsRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.priorSessionTurnPlanHistoryToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(
      tool.outputPreview,
      ProcessProjectionFixture.uncorrelatedPlanHistoryPreview
    )
  }

  func testPlanReadWithTurnOutsideSessionKeepsRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.uncorrelatedPlanHistoryToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(
      tool.outputPreview,
      ProcessProjectionFixture.uncorrelatedPlanHistoryPreview
    )
  }

  func testPlanReadWithFutureSessionTurnKeepsRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.futureSessionTurnPlanHistoryToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(
      tool.outputPreview,
      ProcessProjectionFixture.uncorrelatedPlanHistoryPreview
    )
  }

  func testPlanReadWithLaterSameTurnHistoryKeepsRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.laterSameTurnPlanHistoryToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(
      tool.outputPreview,
      ProcessProjectionFixture.planReadRawOutputPreview
    )
  }

  func testPlanReadWithNonPlanHistoryProvenanceKeepsRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.nonPlanSameTurnHistoryToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(
      tool.outputPreview,
      ProcessProjectionFixture.planReadRawOutputPreview
    )
  }

  func testPlanReadWithMismatchedHistoryAttemptKeepsRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.mismatchedAttemptPlanHistoryToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(
      tool.outputPreview,
      ProcessProjectionFixture.planReadRawOutputPreview
    )
  }

  func testPlanReadWithAlteredHistoryEventKeepsRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.alteredPlanHistoryToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(
      tool.outputPreview,
      ProcessProjectionFixture.alteredPlanHistoryOutputPreview
    )
  }

  func testExpandedPlanWriteOutputKeepsRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.expandedPlanWriteOutputToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(
      tool.outputPreview,
      ProcessProjectionFixture.expandedPlanWriteOutput
    )
  }

  func testContradictoryPlanReadCursorKeepsRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.contradictoryPlanReadCursorToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(
      tool.outputPreview,
      ProcessProjectionFixture.contradictoryPlanReadCursorOutput
    )
  }

  func testTruncatedAbsentPlanHistoryKeepsRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.planReadToolRecord(
      output: ProcessProjectionFixture.truncatedAbsentPlanHistoryOutput
    )
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(
      tool.outputPreview,
      ProcessProjectionFixture.truncatedAbsentPlanHistoryOutput
    )
  }

  func testTruncatedEmptyPlanHistoryKeepsRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.planReadToolRecord(
      output: ProcessProjectionFixture.truncatedEmptyPlanHistoryOutput
    )
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(
      tool.outputPreview,
      ProcessProjectionFixture.truncatedEmptyPlanHistoryOutput
    )
  }

  func testExcessivePlanDependenciesKeepRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.planReadToolRecord(
      output: ProcessProjectionFixture.excessivePlanDependenciesOutput
    )
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(
      tool.outputPreview,
      ProcessProjectionFixture.excessivePlanDependenciesOutput
    )
  }

  func testUnorderedPlanEntriesKeepRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.planReadToolRecord(
      output: ProcessProjectionFixture.unorderedPlanEntriesOutput
    )
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(tool.outputPreview, ProcessProjectionFixture.unorderedPlanEntriesOutput)
  }

  func testSelfDependentPlanEntryKeepsRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.planReadToolRecord(
      output: ProcessProjectionFixture.selfDependentPlanEntryOutput
    )
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(tool.outputPreview, ProcessProjectionFixture.selfDependentPlanEntryOutput)
  }

  func testCyclicPlanEntriesKeepRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.planReadToolRecord(
      output: ProcessProjectionFixture.cyclicPlanEntriesOutput
    )
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(tool.outputPreview, ProcessProjectionFixture.cyclicPlanEntriesOutput)
  }

  func testReadyPlanEntryWithIncompleteDependencyKeepsRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.planReadToolRecord(
      output: ProcessProjectionFixture.inconsistentReadyPlanEntryOutput
    )
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(
      tool.outputPreview,
      ProcessProjectionFixture.inconsistentReadyPlanEntryOutput
    )
  }

  func testWaitingPlanEntryWithCompletedDependencyKeepsRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.planReadToolRecord(
      output: ProcessProjectionFixture.inconsistentWaitingPlanEntryOutput
    )
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(
      tool.outputPreview,
      ProcessProjectionFixture.inconsistentWaitingPlanEntryOutput
    )
  }

  func testFuturePlanEntryReferenceKeepsRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.futurePlanEntryReferenceToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(
      tool.outputPreview,
      ProcessProjectionFixture.futurePlanEntryReferenceOutput
    )
  }

  func testCompletionSideProjectionExcludesEarlierModelIdentityMarker() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithCompletedModelIdentityMarker()
    let trigger = try ProcessProjectionFixture.completedTrigger()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectSideSnapshot(snapshot, attributableTo: trigger)
    let normalizer = try SignalboxIncrementalEventNormalizer(records: projection.records)
    let message = try ProcessProjectionFixture.onlyMessage(in: projection)

    XCTAssertEqual(
      ProcessProjectionFixture.noticeTitles(in: normalizer.timelineItems),
      ProcessProjectionFixture.noNoticeTitles
    )
    XCTAssertEqual(message.text, ProcessProjectionFixture.completedAssistantText)
  }

  func testProposedToolSideProjectionExcludesEarlierModelIdentityMarker() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithCompletedModelIdentityMarker()
    let trigger = try ProcessProjectionFixture.proposedToolTrigger()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectSideSnapshot(snapshot, attributableTo: trigger)
    let normalizer = try SignalboxIncrementalEventNormalizer(records: projection.records)
    let tool = try ProcessProjectionFixture.onlyTool(in: projection)

    XCTAssertEqual(
      ProcessProjectionFixture.noticeTitles(in: normalizer.timelineItems),
      ProcessProjectionFixture.noNoticeTitles
    )
    XCTAssertEqual(tool.toolName, ProcessProjectionFixture.proposedToolName)
  }

  func testRefusedSideProjectionRejectsMismatchedTurnState() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithCompletedModelIdentityMarker()
    let trigger = try ProcessProjectionFixture.refusedEvent()
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

  func testModelReconciliationSideProjectionRejectsMismatchedTurnState() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithCompletedModelIdentityMarker()
    let trigger = try ProcessProjectionFixture.modelReconciliationTrigger(cursor: 1)
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

  func testRefusedSideProjectionExcludesSameTurnModelUsage() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithRefusedUsageAfterImportedMarker()
    let trigger = try ProcessProjectionFixture.refusedEvent()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectSideSnapshot(snapshot, attributableTo: trigger)

    XCTAssertTrue(ProcessProjectionFixture.modelCallUsageIDs(in: projection).isEmpty)
  }

  func testModelReconciliationSideProjectionExcludesSameTurnModelUsage() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithReconciliationUsageAfterImportedMarker()
    let trigger = try ProcessProjectionFixture.modelReconciliationTrigger(cursor: 1)
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectSideSnapshot(snapshot, attributableTo: trigger)

    XCTAssertTrue(ProcessProjectionFixture.modelCallUsageIDs(in: projection).isEmpty)
  }

  func testToolRecoverySideProjectionExcludesModelIdentityMarker() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithCompletedModelIdentityMarker()
    let trigger = try ProcessProjectionFixture.toolRecoveryTrigger()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectSideSnapshot(snapshot, attributableTo: trigger)

    XCTAssertTrue(projection.records.isEmpty)
  }

  func testUnknownTranscriptEntryPresentationKindIsBounded() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithUnknownEntryKind(
      ProcessProjectionFixture.oversizedUnknownState
    )
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)
    let record = try XCTUnwrap(projection.records.first)
    let event = try ProcessProjectionFixture.conservativeEvent(in: record)

    XCTAssertEqual(
      event.kind.utf8.count,
      SignalboxProcessPresentation.maximumLabelUTF8Bytes
    )
  }

  func testUnknownTranscriptTextEntryPreservesItsPresentationKind() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithUnknownTextEntryKind()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)
    let message = try ProcessProjectionFixture.onlyMessage(in: projection)
    let normalizer = try SignalboxIncrementalEventNormalizer(records: projection.records)
    let timelineMessage = try ProcessProjectionFixture.onlyTimelineMessage(
      in: normalizer.timelineItems
    )

    XCTAssertEqual(message.role, .unknown)
    XCTAssertEqual(message.text, ProcessProjectionFixture.unknownTextEntryContent)
    XCTAssertEqual(message.unrecognizedKind, ProcessProjectionFixture.unknownTextEntryKind)
    XCTAssertEqual(
      timelineMessage.unrecognizedKind,
      ProcessProjectionFixture.unknownTextEntryKind
    )
  }

  func testUnknownImportedSpeakerWrapperPreservesItsPresentationKind() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithImportedTextSpeaker(
      #"{"type":"fixture_future_speaker_wrapper"}"#
    )
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)
    let message = try ProcessProjectionFixture.onlyMessage(in: projection)

    XCTAssertEqual(message.role, .unknown)
    XCTAssertEqual(
      message.unrecognizedKind,
      ProcessProjectionFixture.unknownSpeakerWrapperLabel
    )
  }

  func testUnknownAttestedImportedSpeakerPreservesItsPresentationKind() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithImportedTextSpeaker(
      #"{"type":"attested","speaker":"fixture_future_speaker"}"#
    )
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)
    let message = try ProcessProjectionFixture.onlyMessage(in: projection)

    XCTAssertEqual(message.role, .unknown)
    XCTAssertEqual(
      message.unrecognizedKind,
      ProcessProjectionFixture.unknownAttestedSpeakerLabel
    )
  }

  func testImportedUserRoleTextRetainsWireRoleLabel() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithImportedTextSpeaker(
      #"{"type":"attested","speaker":"user"}"#
    )
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)
    let message = try ProcessProjectionFixture.onlyMessage(in: projection)
    let normalizer = try SignalboxIncrementalEventNormalizer(records: projection.records)
    let timelineMessage = try ProcessProjectionFixture.onlyTimelineMessage(
      in: normalizer.timelineItems
    )

    XCTAssertEqual(
      message.sourceAttribution,
      ProcessProjectionFixture.importedUserRoleAttribution
    )
    XCTAssertEqual(
      timelineMessage.label,
      ProcessProjectionFixture.importedUserRoleLabel
    )
  }

  func testImportedUnattestedTextRetainsWireSpeakerLabel() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithImportedTextSpeaker(
      #"{"type":"not_attested"}"#
    )
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)
    let message = try ProcessProjectionFixture.onlyMessage(in: projection)
    let normalizer = try SignalboxIncrementalEventNormalizer(records: projection.records)
    let timelineMessage = try ProcessProjectionFixture.onlyTimelineMessage(
      in: normalizer.timelineItems
    )

    XCTAssertEqual(
      message.sourceAttribution,
      ProcessProjectionFixture.importedUnattestedAttribution
    )
    XCTAssertEqual(
      timelineMessage.label,
      ProcessProjectionFixture.importedUnattestedLabel
    )
  }

  func testImportedAttestedAbsentTextRetainsWireSpeakerLabel() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithImportedTextSpeaker(
      #"{"type":"attested_absent"}"#
    )
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)
    let message = try ProcessProjectionFixture.onlyMessage(in: projection)
    let normalizer = try SignalboxIncrementalEventNormalizer(records: projection.records)
    let timelineMessage = try ProcessProjectionFixture.onlyTimelineMessage(
      in: normalizer.timelineItems
    )

    XCTAssertEqual(
      message.sourceAttribution,
      ProcessProjectionFixture.importedAttestedAbsentAttribution
    )
    XCTAssertEqual(
      timelineMessage.label,
      ProcessProjectionFixture.importedAttestedAbsentLabel
    )
  }

  func testUnknownImportedContentPresentationKindIsBounded() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithImportedContentKind(
      ProcessProjectionFixture.oversizedUnknownState
    )
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)
    let record = try XCTUnwrap(projection.records.first)
    let event = try ProcessProjectionFixture.conservativeEvent(in: record)

    XCTAssertEqual(
      event.kind.utf8.count,
      SignalboxProcessPresentation.maximumLabelUTF8Bytes
    )
  }

  func testFutureRemoteErrorPresentationIsBounded() throws {
    let error = SignalboxProcessServiceError.remote(
      code: .unknown(ProcessProjectionFixture.oversizedUnknownState),
      message: ProcessProjectionFixture.remoteErrorMessage,
      detail: nil
    )

    let description = try XCTUnwrap(error.errorDescription)

    XCTAssertEqual(
      description.utf8.count,
      SignalboxProcessPresentation.maximumLabelUTF8Bytes
    )
  }

  func testMutationRetryErrorPresentationPreservesGuidanceWhenBounded() throws {
    let error = SignalboxProcessServiceError.mutationRetryExhausted(
      code: .unknown(ProcessProjectionFixture.oversizedUnknownState),
      message: ProcessProjectionFixture.remoteErrorMessage
    )

    let description = try XCTUnwrap(error.errorDescription)

    XCTAssertEqual(
      description.utf8.count,
      SignalboxProcessPresentation.maximumLabelUTF8Bytes
    )
    XCTAssertTrue(description.hasSuffix(ProcessProjectionFixture.mutationRetryGuidance))
  }

  func testFailedProviderCauseAppearsInNativeActivity() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithFailedProviderCause()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)

    XCTAssertEqual(projection.activity, ProcessProjectionFixture.quotaExhaustedActivity)
  }

  func testUnknownFailedProviderCauseActivityIsBounded() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithUnknownFailedProviderCause(
      ProcessProjectionFixture.oversizedUnknownState
    )
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)

    XCTAssertEqual(
      projection.activity.label.utf8.count,
      SignalboxProcessPresentation.maximumLabelUTF8Bytes
    )
  }

  func testUnknownFailedProviderCauseProjectsAsVisibleTimelineCard() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithUnknownFailedProviderCause(
      ProcessProjectionFixture.futureProviderFailureCause
    )
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)
    let normalizer = try SignalboxIncrementalEventNormalizer(records: projection.records)

    XCTAssertEqual(
      ProcessProjectionFixture.unknownKinds(in: normalizer.timelineItems),
      [ProcessProjectionFixture.unknownProviderCausePresentationKind]
    )
    XCTAssertEqual(
      ProcessProjectionFixture.unknownDiagnostics(in: normalizer.timelineItems),
      [ProcessProjectionFixture.unknownProviderCausePresentationDiagnostic]
    )
  }

  func testUnknownFailedDispositionAppearsInNativeActivity() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithUnknownFailedDisposition()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)

    XCTAssertEqual(projection.activity, ProcessProjectionFixture.unknownFailedActivity)
  }

  func testUnknownFailedDispositionProjectsAsVisibleTimelineCard() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithUnknownFailedDisposition()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)
    let normalizer = try SignalboxIncrementalEventNormalizer(records: projection.records)

    XCTAssertEqual(
      ProcessProjectionFixture.unknownKinds(in: normalizer.timelineItems),
      [ProcessProjectionFixture.unknownDispositionPresentationKind]
    )
  }

  func testUnknownFailedDispositionActivityIsBounded() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithUnknownFailedDisposition(
      ProcessProjectionFixture.oversizedUnknownState
    )
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)

    XCTAssertEqual(
      projection.activity.label.utf8.count,
      SignalboxSessionSynchronizationMachine.maximumRetainedDiagnosticMessageUTF8Bytes
    )
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

  func testUnknownTurnActivityPrecedesLaterQueuedActivity() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithUnknownAndQueuedTurns()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)

    XCTAssertEqual(projection.activity, ProcessProjectionFixture.recoveryActivity)
  }

  func testUnknownCurrentModelCallStateRequiresRecovery() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithUnknownCurrentModelCallState()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)

    XCTAssertEqual(projection.activity, ProcessProjectionFixture.recoveryActivity)
  }

  @MainActor
  func testUnknownCurrentModelCallStateGatesMutations() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let snapshot = try ProcessProjectionFixture.snapshotWithUnknownCurrentModelCallState()
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      ImmediateAcceptingProcessService()
    }
    await viewModel.connect()
    viewModel.apply(.phase(ProcessProjectionFixture.steadyPhase))

    viewModel.apply(.authoritativeSnapshot(snapshot))

    XCTAssertFalse(viewModel.canSend)
    XCTAssertFalse(viewModel.canStopAndSend)
    XCTAssertFalse(viewModel.canDecideToolRequest)
  }

  @MainActor
  func testKnownNestedTransitionsDoNotClearUnknownTopLevelTurnGate() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      ImmediateAcceptingProcessService()
    }
    await viewModel.connect()
    viewModel.apply(.phase(ProcessProjectionFixture.steadyPhase))
    viewModel.apply(
      .authoritativeSnapshot(try ProcessProjectionFixture.snapshotWithUnknownTurnState())
    )

    viewModel.apply(.event(try ProcessProjectionFixture.completedModelCallEvent()))
    viewModel.apply(.event(try ProcessProjectionFixture.proposedToolTrigger()))

    XCTAssertFalse(viewModel.canSend)
    XCTAssertFalse(viewModel.canStopAndSend)
  }

  @MainActor
  func testKnownModelCallTransitionClearsUnknownNestedTurnGate() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      ImmediateAcceptingProcessService()
    }
    await viewModel.connect()
    viewModel.apply(.phase(ProcessProjectionFixture.steadyPhase))
    viewModel.apply(
      .authoritativeSnapshot(
        try ProcessProjectionFixture.snapshotWithUnknownCurrentModelCallState()
      )
    )

    viewModel.apply(.event(try ProcessProjectionFixture.completedModelCallEvent()))

    XCTAssertTrue(viewModel.canStopAndSend)
  }

  @MainActor
  func testUnknownTerminalDispositionGatesMutations() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      ImmediateAcceptingProcessService()
    }
    await viewModel.connect()
    viewModel.apply(.phase(ProcessProjectionFixture.steadyPhase))
    viewModel.apply(.event(try ProcessProjectionFixture.activatedEvent()))

    viewModel.apply(.event(try ProcessProjectionFixture.unknownDispositionModelCallEvent()))

    XCTAssertFalse(viewModel.canSend)
    XCTAssertFalse(viewModel.canStopAndSend)
  }

  @MainActor
  func testSideSnapshotUnknownStateReplacesRunningActivity() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      ImmediateAcceptingProcessService()
    }
    await viewModel.connect()
    viewModel.apply(.phase(ProcessProjectionFixture.steadyPhase))
    let trigger = try ProcessProjectionFixture.activatedEvent()
    viewModel.apply(.event(trigger))

    viewModel.apply(
      .sideSnapshot(
        snapshot: try ProcessProjectionFixture.snapshotWithUnknownCurrentModelCallState(),
        trigger: trigger
      )
    )

    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.recoveryActivity)
    XCTAssertFalse(viewModel.canStopAndSend)
  }

  @MainActor
  func testNewerSideSnapshotGateSurvivesBufferedKnownTransition() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      ImmediateAcceptingProcessService()
    }
    await viewModel.connect()
    viewModel.apply(.phase(ProcessProjectionFixture.steadyPhase))
    let trigger = try ProcessProjectionFixture.activatedEvent()
    viewModel.apply(.event(trigger))
    viewModel.apply(
      .sideSnapshot(
        snapshot: try ProcessProjectionFixture.snapshotWithUnknownCurrentModelCallState(
          cursor: ProcessProjectionFixture.sideSnapshotCursor
        ),
        trigger: trigger
      )
    )

    viewModel.apply(
      .event(
        try ProcessProjectionFixture.completedModelCallEvent(
          cursor: ProcessProjectionFixture.bufferedTransitionCursor
        )
      )
    )

    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.recoveryActivity)
    XCTAssertFalse(viewModel.canStopAndSend)
  }

  @MainActor
  func testRecognizedSideSnapshotClearsPriorRecoveryActivity() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      ImmediateAcceptingProcessService()
    }
    await viewModel.connect()
    viewModel.apply(.phase(ProcessProjectionFixture.steadyPhase))
    let trigger = try ProcessProjectionFixture.activatedEvent()
    viewModel.apply(.event(trigger))
    viewModel.apply(
      .sideSnapshot(
        snapshot: try ProcessProjectionFixture.snapshotWithUnknownCurrentModelCallState(),
        trigger: trigger
      )
    )

    viewModel.apply(
      .sideSnapshot(
        snapshot: try ProcessProjectionFixture.snapshotWithKnownActiveTurn(
          cursor: ProcessProjectionFixture.sideSnapshotCursor
        ),
        trigger: trigger
      )
    )

    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.runningActivity)
    XCTAssertTrue(viewModel.canStopAndSend)
  }

  @MainActor
  func testKnownRecoverySideSnapshotSurvivesBufferedTransition() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      ImmediateAcceptingProcessService()
    }
    await viewModel.connect()
    viewModel.apply(.phase(ProcessProjectionFixture.steadyPhase))
    let trigger = try ProcessProjectionFixture.activatedEvent()
    viewModel.apply(.event(trigger))
    viewModel.apply(
      .sideSnapshot(
        snapshot: try ProcessProjectionFixture.snapshotWithKnownRecoveryTurn(
          cursor: ProcessProjectionFixture.sideSnapshotCursor
        ),
        trigger: trigger
      )
    )

    viewModel.apply(
      .event(
        try ProcessProjectionFixture.completedModelCallEvent(
          cursor: ProcessProjectionFixture.bufferedTransitionCursor
        )
      )
    )

    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.recoveryActivity)
    XCTAssertFalse(viewModel.canStopAndSend)
  }

  @MainActor
  func testNewerRecognizedSideSnapshotRejectsBufferedUnknownTransition() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      ImmediateAcceptingProcessService()
    }
    await viewModel.connect()
    viewModel.apply(.phase(ProcessProjectionFixture.steadyPhase))
    let trigger = try ProcessProjectionFixture.activatedEvent()
    viewModel.apply(.event(trigger))
    viewModel.apply(
      .sideSnapshot(
        snapshot: try ProcessProjectionFixture.snapshotWithKnownActiveTurn(
          cursor: ProcessProjectionFixture.sideSnapshotCursor
        ),
        trigger: trigger
      )
    )

    viewModel.apply(
      .event(
        try ProcessProjectionFixture.unknownStateModelCallEvent(
          cursor: ProcessProjectionFixture.bufferedTransitionCursor
        )
      )
    )

    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.runningActivity)
    XCTAssertTrue(viewModel.canStopAndSend)
    XCTAssertEqual(
      ProcessProjectionFixture.unknownKinds(in: viewModel.timeline),
      [ProcessProjectionFixture.unknownNestedStateKind]
    )
  }

  @MainActor
  func testNewerRecognizedSideSnapshotRetainsBufferedUnknownToolTransition() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      ImmediateAcceptingProcessService()
    }
    await viewModel.connect()
    viewModel.apply(.phase(ProcessProjectionFixture.steadyPhase))
    let trigger = try ProcessProjectionFixture.activatedEvent()
    viewModel.apply(.event(trigger))
    viewModel.apply(
      .sideSnapshot(
        snapshot: try ProcessProjectionFixture.snapshotWithKnownActiveTurn(
          cursor: ProcessProjectionFixture.sideSnapshotCursor
        ),
        trigger: trigger
      )
    )

    viewModel.apply(
      .event(
        try ProcessProjectionFixture.unknownToolBatchEvent(
          cursor: ProcessProjectionFixture.bufferedTransitionCursor
        )
      )
    )

    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.runningActivity)
    XCTAssertTrue(viewModel.canStopAndSend)
    XCTAssertEqual(
      ProcessProjectionFixture.unknownKinds(in: viewModel.timeline),
      [ProcessProjectionFixture.unknownToolBatchTimelineKind]
    )
  }

  @MainActor
  func testSideSnapshotFenceKeepsBufferedActivationLifecycle() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }
    let trigger = try ProcessProjectionFixture.activatedEvent()
    viewModel.apply(
      .sideSnapshot(
        snapshot: try ProcessProjectionFixture.snapshotWithKnownActiveTurn(
          cursor: ProcessProjectionFixture.sideSnapshotCursor
        ),
        trigger: trigger
      )
    )

    viewModel.apply(
      .event(
        try ProcessProjectionFixture.activatedEvent(
          cursor: ProcessProjectionFixture.bufferedTransitionCursor
        )
      )
    )

    XCTAssertEqual(viewModel.activeTurnID?.rawValue, ProcessDriverFixture.turn)
    XCTAssertFalse(viewModel.canSend)
  }

  @MainActor
  func testTerminalSideSnapshotRejectsBufferedActivationLifecycle() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let trigger = try ProcessProjectionFixture.activatedEvent(
      cursor: ProcessProjectionFixture.bufferedTransitionCursor
    )
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      ImmediateAcceptingProcessService()
    }
    await viewModel.connect()
    viewModel.apply(.phase(ProcessProjectionFixture.steadyPhase))
    viewModel.apply(
      .sideSnapshot(
        snapshot: try ProcessProjectionFixture.snapshotWithCompletedTurn(
          cursor: ProcessProjectionFixture.sideSnapshotCursor
        ),
        trigger: trigger
      )
    )

    viewModel.apply(.event(trigger))

    XCTAssertNil(viewModel.activeTurnID)
    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.completedActivity)
    XCTAssertTrue(viewModel.canSend)
    XCTAssertFalse(viewModel.canStopAndSend)
  }

  @MainActor
  func testModelReconciliationSideSnapshotRejectsBufferedActivationLifecycle() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let trigger = try ProcessProjectionFixture.activatedEvent(
      cursor: ProcessProjectionFixture.bufferedTransitionCursor
    )
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      ImmediateAcceptingProcessService()
    }
    await viewModel.connect()
    viewModel.apply(.phase(ProcessProjectionFixture.steadyPhase))
    viewModel.apply(
      .sideSnapshot(
        snapshot: try ProcessProjectionFixture.snapshotWithModelReconciliationTurn(
          cursor: ProcessProjectionFixture.sideSnapshotCursor
        ),
        trigger: trigger
      )
    )

    viewModel.apply(.event(trigger))

    XCTAssertNil(viewModel.activeTurnID)
    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.recoveryActivity)
    XCTAssertTrue(viewModel.canSend)
    XCTAssertFalse(viewModel.canStopAndSend)
  }

  @MainActor
  func testToolReconciliationSideSnapshotRejectsBufferedActivationLifecycle() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let trigger = try ProcessProjectionFixture.activatedEvent(
      cursor: ProcessProjectionFixture.bufferedTransitionCursor
    )
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      ImmediateAcceptingProcessService()
    }
    await viewModel.connect()
    viewModel.apply(.phase(ProcessProjectionFixture.steadyPhase))
    viewModel.apply(
      .sideSnapshot(
        snapshot: try ProcessProjectionFixture.snapshotWithToolReconciliationTurn(
          cursor: ProcessProjectionFixture.sideSnapshotCursor
        ),
        trigger: trigger
      )
    )

    viewModel.apply(.event(trigger))

    XCTAssertNil(viewModel.activeTurnID)
    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.recoveryActivity)
    XCTAssertTrue(viewModel.canSend)
    XCTAssertFalse(viewModel.canStopAndSend)
  }

  @MainActor
  func testSideSnapshotAdoptsRunningSuccessorAfterReconciliation() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let trigger = try ProcessProjectionFixture.modelReconciliationTrigger(
      cursor: ProcessProjectionFixture.bufferedTransitionCursor
    )
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      ImmediateAcceptingProcessService()
    }
    await viewModel.connect()
    viewModel.apply(.phase(ProcessProjectionFixture.steadyPhase))
    viewModel.apply(.event(trigger))

    viewModel.apply(
      .sideSnapshot(
        snapshot: try ProcessProjectionFixture.snapshotWithReconciliationAndActiveSuccessor(
          cursor: ProcessProjectionFixture.sideSnapshotCursor
        ),
        trigger: trigger
      )
    )

    XCTAssertEqual(viewModel.activeTurnID?.rawValue, ProcessProjectionFixture.secondPendingTurn)
    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.runningActivity)
    XCTAssertTrue(viewModel.canStopAndSend)
  }

  @MainActor
  func testSideSnapshotAdoptsRecoveryTurnBeforeBufferedActivation() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let trigger = try ProcessProjectionFixture.activatedEvent(
      cursor: ProcessProjectionFixture.bufferedTransitionCursor
    )
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      ImmediateAcceptingProcessService()
    }
    await viewModel.connect()
    viewModel.apply(.phase(ProcessProjectionFixture.steadyPhase))

    viewModel.apply(
      .sideSnapshot(
        snapshot: try ProcessProjectionFixture.snapshotWithKnownRecoveryTurn(
          cursor: ProcessProjectionFixture.sideSnapshotCursor
        ),
        trigger: trigger
      )
    )

    XCTAssertEqual(viewModel.activeTurnID?.rawValue, ProcessDriverFixture.turn)
    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.recoveryActivity)
    XCTAssertFalse(viewModel.canSend)
    XCTAssertFalse(viewModel.canStopAndSend)

    viewModel.apply(.event(trigger))

    XCTAssertEqual(viewModel.activeTurnID?.rawValue, ProcessDriverFixture.turn)
    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.recoveryActivity)
  }

  @MainActor
  func testTerminalSideSnapshotAdoptsActivityAcrossBufferedModelCallTransition() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let trigger = try ProcessProjectionFixture.completedModelCallEvent(
      cursor: ProcessProjectionFixture.bufferedTransitionCursor
    )
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      ImmediateAcceptingProcessService()
    }
    await viewModel.connect()
    viewModel.apply(.phase(ProcessProjectionFixture.steadyPhase))
    viewModel.apply(.event(try ProcessProjectionFixture.activatedEvent()))

    viewModel.apply(
      .sideSnapshot(
        snapshot: try ProcessProjectionFixture.snapshotWithCompletedTurn(
          cursor: ProcessProjectionFixture.sideSnapshotCursor
        ),
        trigger: trigger
      )
    )

    XCTAssertNil(viewModel.activeTurnID)
    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.completedActivity)
    XCTAssertTrue(viewModel.canSend)

    viewModel.apply(.event(trigger))

    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.completedActivity)
  }

  @MainActor
  func testTerminalSideSnapshotAdoptsQueuedSuccessorAcrossBufferedTransition() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let trigger = try ProcessProjectionFixture.completedModelCallEvent(
      cursor: ProcessProjectionFixture.bufferedTransitionCursor
    )
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      ImmediateAcceptingProcessService()
    }
    await viewModel.connect()
    viewModel.apply(.phase(ProcessProjectionFixture.steadyPhase))
    viewModel.apply(.event(try ProcessProjectionFixture.activatedEvent()))
    viewModel.apply(.event(try ProcessProjectionFixture.secondAcceptedEvent()))

    viewModel.apply(
      .sideSnapshot(
        snapshot: try ProcessProjectionFixture.snapshotWithCancelledAndQueuedTurns(
          cursor: ProcessProjectionFixture.sideSnapshotCursor
        ),
        trigger: trigger
      )
    )

    XCTAssertNil(viewModel.activeTurnID)
    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.queuedActivity)
    XCTAssertTrue(viewModel.canSend)

    viewModel.apply(.event(trigger))

    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.queuedActivity)
  }

  @MainActor
  func testSideSnapshotFenceKeepsRecoveryAcrossBufferedActivation() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      ImmediateAcceptingProcessService()
    }
    await viewModel.connect()
    viewModel.apply(.phase(ProcessProjectionFixture.steadyPhase))
    let trigger = try ProcessProjectionFixture.activatedEvent()
    viewModel.apply(
      .sideSnapshot(
        snapshot: try ProcessProjectionFixture.snapshotWithUnknownCurrentModelCallState(
          cursor: ProcessProjectionFixture.sideSnapshotCursor
        ),
        trigger: trigger
      )
    )

    viewModel.apply(
      .event(
        try ProcessProjectionFixture.activatedEvent(
          cursor: ProcessProjectionFixture.bufferedTransitionCursor
        )
      )
    )

    XCTAssertEqual(viewModel.activeTurnID?.rawValue, ProcessDriverFixture.turn)
    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.recoveryActivity)
    XCTAssertFalse(viewModel.canStopAndSend)
  }

  @MainActor
  func testNewerActivationPreservesAnotherTurnsUnknownActivity() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let trigger = try ProcessProjectionFixture.activatedEvent()
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }
    viewModel.apply(
      .sideSnapshot(
        snapshot: try ProcessProjectionFixture.snapshotWithUnknownAndQueuedTurns(
          cursor: ProcessProjectionFixture.sideSnapshotCursor
        ),
        trigger: trigger
      )
    )

    viewModel.apply(
      .event(
        try ProcessProjectionFixture.queuedTurnActivatedEvent(
          cursor: ProcessProjectionFixture.newerTransitionCursor
        )
      )
    )

    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.recoveryActivity)
    XCTAssertFalse(viewModel.canSend)
  }

  @MainActor
  func testMismatchedSameTurnSnapshotDoesNotOverrideBufferedTerminalReplay() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let terminal = try ProcessProjectionFixture.refusedEvent(
      cursor: ProcessProjectionFixture.bufferedTransitionCursor
    )
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }
    viewModel.apply(
      .sideSnapshot(
        snapshot: try ProcessProjectionFixture.snapshotWithUnknownTurnState(
          cursor: ProcessProjectionFixture.sideSnapshotCursor
        ),
        trigger: terminal
      )
    )

    viewModel.apply(.event(terminal))

    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.refusedActivity)
    XCTAssertFalse(viewModel.canSend)
  }

  @MainActor
  func testUnknownTurnActivitySurvivesNewerModelCallTransition() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let trigger = try ProcessProjectionFixture.activatedEvent()
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }
    viewModel.apply(
      .sideSnapshot(
        snapshot: try ProcessProjectionFixture.snapshotWithUnknownTurnState(
          cursor: ProcessProjectionFixture.sideSnapshotCursor
        ),
        trigger: trigger
      )
    )

    viewModel.apply(
      .event(
        try ProcessProjectionFixture.completedModelCallEvent(
          cursor: ProcessProjectionFixture.newerTransitionCursor
        )
      )
    )

    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.recoveryActivity)
    XCTAssertFalse(viewModel.canSend)
  }

  @MainActor
  func testUnknownTurnActivitySurvivesNewerToolBatchTransition() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let trigger = try ProcessProjectionFixture.activatedEvent()
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }
    viewModel.apply(
      .sideSnapshot(
        snapshot: try ProcessProjectionFixture.snapshotWithUnknownTurnState(
          cursor: ProcessProjectionFixture.sideSnapshotCursor
        ),
        trigger: trigger
      )
    )

    viewModel.apply(
      .event(
        try ProcessProjectionFixture.proposedToolTrigger(
          cursor: ProcessProjectionFixture.newerTransitionCursor
        )
      )
    )

    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.recoveryActivity)
    XCTAssertFalse(viewModel.canSend)
  }

  @MainActor
  func testDuplicateTurnInSideSnapshotDegradesWithoutTrapping() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }

    viewModel.apply(
      .sideSnapshot(
        snapshot: try ProcessProjectionFixture.snapshotWithDuplicateTurnRecords(),
        trigger: try ProcessProjectionFixture.activatedEvent()
      )
    )

    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.runningActivity)
    XCTAssertFalse(viewModel.canSend)
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
  func testUnknownTurnStateGatesMutations() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let snapshot = try ProcessProjectionFixture.snapshotWithUnknownTurnState()
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
  func testHistoricalUnknownTurnDoesNotGateMutationsAfterKnownTerminalSuccessor() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let snapshot = try ProcessProjectionFixture.snapshotWithHistoricalUnknownTurn()
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      ImmediateAcceptingProcessService()
    }
    await viewModel.connect()
    viewModel.apply(.phase(ProcessProjectionFixture.steadyPhase))

    viewModel.apply(.authoritativeSnapshot(snapshot))

    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.cancelledActivity)
    XCTAssertTrue(viewModel.canSend)
  }

  @MainActor
  func testTerminalEventClearsUnknownTurnMutationGate() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let snapshot = try ProcessProjectionFixture.snapshotWithUnknownTurnState()
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      ImmediateAcceptingProcessService()
    }
    await viewModel.connect()
    viewModel.apply(.phase(ProcessProjectionFixture.steadyPhase))
    viewModel.apply(.authoritativeSnapshot(snapshot))

    viewModel.apply(.event(try ProcessProjectionFixture.refusedEvent()))

    XCTAssertNil(viewModel.activeTurnID)
    XCTAssertTrue(viewModel.canSubmit)
    XCTAssertTrue(viewModel.canSend)
    XCTAssertTrue(viewModel.canDecideToolRequest)
  }

  @MainActor
  func testMismatchedSnapshotDoesNotOverrideAnotherTurnsTerminalReplay() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let terminal = try ProcessProjectionFixture.submittedTurnRefusedEvent()
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      ImmediateAcceptingProcessService()
    }
    await viewModel.connect()
    viewModel.apply(.phase(ProcessProjectionFixture.steadyPhase))
    viewModel.apply(
      .sideSnapshot(
        snapshot: try ProcessProjectionFixture.snapshotWithUnknownTurnState(),
        trigger: terminal
      )
    )

    viewModel.apply(.event(terminal))

    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.refusedActivity)
    XCTAssertTrue(viewModel.canSend)
  }

  @MainActor
  func testUnknownToolBatchStateRequiresRecovery() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }

    viewModel.apply(.event(try ProcessProjectionFixture.unknownToolBatchEvent()))

    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.recoveryActivity)
    XCTAssertEqual(
      viewModel.latestDiagnostic,
      ProcessProjectionFixture.unknownToolBatchDiagnostic
    )
  }

  @MainActor
  func testEventPresentationBoundsUnknownToolBatchDiagnostic() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }

    viewModel.apply(
      .event(
        try ProcessProjectionFixture.unknownToolBatchEvent(
          kind: ProcessProjectionFixture.oversizedUnknownState
        )
      )
    )

    XCTAssertEqual(
      viewModel.latestDiagnostic?.utf8.count,
      SignalboxSessionSynchronizationMachine.maximumRetainedDiagnosticMessageUTF8Bytes
    )
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
  func testMissingRuntimeAndSavedPathExposeTheTransportSetupGate() {
    UserDefaults.standard.removeObject(
      forKey: NativeProcessConstants.socketDefaultsKey
    )

    let settings = SignalboxProcessSettingsViewModel(environment: [:])

    XCTAssertEqual(settings.socketPath, ProcessSubmissionFixture.noSocketPath)
    XCTAssertEqual(settings.connectionStatus, .notConfigured)
    XCTAssertNil(NativeProcessConstants.defaultSocketPath(environment: [:]))
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

  @MainActor
  func testFutureMutationErrorRetryReusesPreparedCommandIdentity() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let service = AmbiguousThenAcceptingProcessService(
      firstError: ProcessDriverFixture.futureMutationRemoteError
    )
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
  func testAmbiguousStopRetryReusesPreparedCommandIdentity() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let service = AmbiguousThenAcceptingStopProcessService()
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      service
    }
    await viewModel.connect()
    viewModel.apply(.event(try ProcessProjectionFixture.acceptedEvent()))
    viewModel.apply(.event(try ProcessProjectionFixture.activatedEvent()))
    viewModel.composerText = ProcessSubmissionFixture.content

    await viewModel.stopAndSendSuccessor()
    let submittedCommandIDs = await service.submittedCommandIDs
    XCTAssertEqual(submittedCommandIDs, [ProcessSubmissionFixture.commandID])

    XCTAssertEqual(viewModel.activeTurnID?.rawValue, ProcessDriverFixture.turn)
    viewModel.apply(.event(try ProcessProjectionFixture.successorActivatedEvent()))
    XCTAssertEqual(viewModel.activeTurnID?.rawValue, ProcessSubmissionFixture.acceptedTurnID)

    await viewModel.stopAndSendSuccessor()
    let finalCommandIDs = await service.submittedCommandIDs
    let submittedActiveTurnIDs = await service.submittedActiveTurnIDs

    XCTAssertEqual(finalCommandIDs, ProcessSubmissionFixture.retriedCommandIDs)
    XCTAssertEqual(
      submittedActiveTurnIDs,
      [ProcessDriverFixture.turn, ProcessDriverFixture.turn]
    )
  }

  @MainActor
  func testAmbiguousToolDecisionRetryReusesPreparedCommandIdentity() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let service = AmbiguousThenAcceptingToolDecisionProcessService()
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      service
    }
    await viewModel.connect()
    let invocationID = SignalboxToolInvocationID(
      rawValue: MockSignalboxFixtures.invocationID
    )

    await viewModel.decideToolRequest(invocationID, decision: .approve)
    await viewModel.decideToolRequest(invocationID, decision: .approve)
    let submittedCommandIDs = await service.submittedCommandIDs

    XCTAssertEqual(submittedCommandIDs, ProcessSubmissionFixture.retriedCommandIDs)
  }

  @MainActor
  func testToolDecisionPresentationGateClosesWhileDecisionIsInFlight() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let service = AmbiguousThenAcceptingToolDecisionProcessService(
      suspendsFirstDecision: true
    )
    let viewModel = ProcessSessionDetailViewModel(session: session) {
      service
    }
    await viewModel.connect()
    let invocationID = SignalboxToolInvocationID(
      rawValue: MockSignalboxFixtures.invocationID
    )

    XCTAssertTrue(viewModel.canDecideToolRequest)
    let decision = Task {
      await viewModel.decideToolRequest(invocationID, decision: .approve)
    }
    await service.waitUntilDecisionStarted()

    XCTAssertFalse(viewModel.canDecideToolRequest)
    await service.completeDecision()
    await decision.value
    XCTAssertTrue(viewModel.canDecideToolRequest)
  }

  @MainActor
  func testProviderTextCapacityFailureDropsOverlayAndRequestsRecovery() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }

    ProcessStreamedTextFixture.applyCapacityFailure(to: viewModel)

    XCTAssertNil(viewModel.streamedText)
    XCTAssertEqual(viewModel.errorMessage, ProcessStreamedTextFixture.capacityError)
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

    XCTAssertEqual(
      viewModel.acceptedInputsAwaitingTranscript,
      try ProcessSubmissionFixture.livePendingInput()
    )

    viewModel.apply(
      .sideSnapshot(
        snapshot: try ProcessProjectionFixture.snapshotWithTerminalResponseMissingUserEntry(),
        trigger: completed
      )
    )

    XCTAssertEqual(
      viewModel.acceptedInputsAwaitingTranscript,
      try ProcessSubmissionFixture.livePendingInput()
    )

    XCTAssertEqual(
      ProcessProjectionFixture.transcriptRowKinds(in: viewModel.transcriptRows),
      ProcessProjectionFixture.acceptedThenTimelineRowKinds
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
  func testUnknownModelCallDispositionReplacesActiveActivity() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }
    viewModel.apply(.event(try ProcessProjectionFixture.activatedEvent()))

    viewModel.apply(.event(try ProcessProjectionFixture.unknownDispositionModelCallEvent()))

    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.recoveryActivity)
    XCTAssertEqual(
      viewModel.latestDiagnostic,
      ProcessProjectionFixture.unknownDispositionDiagnostic
    )
  }

  @MainActor
  func testUnknownModelCallStateReplacesActiveActivity() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }
    viewModel.apply(.event(try ProcessProjectionFixture.activatedEvent()))

    viewModel.apply(.event(try ProcessProjectionFixture.unknownStateModelCallEvent()))

    XCTAssertEqual(viewModel.activity, ProcessProjectionFixture.recoveryActivity)
    XCTAssertEqual(
      viewModel.latestDiagnostic,
      ProcessProjectionFixture.unknownStateDiagnostic
    )
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

  func testFutureMutationErrorIsSurfacedWithoutRetry() async throws {
    let submission = try ProcessSubmissionFixture.preparedSubmission()
    let requester = SequencedProcessRequester(
      pages: [
        [try ProcessDriverFixture.futureMutationError()],
        [try ProcessDriverFixture.inputSubmitted()],
      ]
    )
    let service = SignalboxProcessService(
      requester: requester,
      policy: ProcessDriverFixture.oneImmediateMutationRetryPolicy
    )

    let error = await capturedServiceError {
      _ = try await service.submit(submission)
    }
    let openedRequests = await requester.openedRequests

    XCTAssertEqual(error, ProcessDriverFixture.futureMutationRemoteError)
    XCTAssertEqual(
      openedRequests,
      ProcessSubmissionFixture.singleRequest(for: submission)
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

  func testStopReceiptRejectsSettingsForAnotherDirectModel() async throws {
    let expectedSelection = try SignalboxCanonicalUUID(
      validating: ProcessDriverFixture.modelCall
    )
    let prepared = SignalboxPreparedTurnStop(
      commandID: try SignalboxCommandID(validating: ProcessSubmissionFixture.commandID),
      sessionID: try SignalboxCanonicalUUID(validating: ProcessDriverFixture.session),
      activeTurnID: try SignalboxCanonicalUUID(
        validating: ProcessSubmissionFixture.acceptedTurnID
      ),
      content: ProcessSubmissionFixture.content,
      expectedDefaultsVersion: SignalboxCanonicalUInt64(rawValue: 1),
      modelSelection: .direct(selectionID: expectedSelection)
    )
    let requester = StaticProcessRequester(
      frames: [
        try ProcessDriverFixture.inputSubmitted(
          validatedForSelectionID: ProcessDriverFixture.metadataSessionA
        )
      ]
    )
    let service = SignalboxProcessService(requester: requester, policy: .nativeDefault)

    let error = await capturedServiceError {
      _ = try await service.stopTurn(prepared)
    }

    XCTAssertEqual(error, ProcessDriverFixture.mismatchedStopSettingsError)
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
    XCTAssertEqual(tool.turnID?.rawValue, MockProcessProtocolFixtures.activeTurnID)
    XCTAssertEqual(
      tool.sessionTurnAcceptancePositions?[
        try SignalboxCanonicalUUID(validating: MockProcessProtocolFixtures.activeTurnID)
      ]?.rawValue.description,
      MockProcessProtocolFixtures.firstAcceptancePosition
    )
    XCTAssertEqual(
      tool.sessionToolRequestPositions?[
        try SignalboxCanonicalUUID(
          validating: MockProcessProtocolFixtures.completedToolRequestID
        )
      ]?.entryIndex.rawValue.description,
      MockProcessProtocolFixtures.completedToolRequestEntryIndex
    )
    XCTAssertEqual(
      tool.sessionToolRequestPositions?[
        try SignalboxCanonicalUUID(
          validating: MockProcessProtocolFixtures.completedToolRequestID
        )
      ]?.turnID.rawValue,
      MockProcessProtocolFixtures.activeTurnID
    )
    XCTAssertEqual(
      tool.sessionToolRequestPositions?[
        try SignalboxCanonicalUUID(
          validating: MockProcessProtocolFixtures.completedToolRequestID
        )
      ]?.toolName,
      MockProcessProtocolFixtures.completedToolName
    )
    XCTAssertEqual(
      tool.sessionToolRequestPositions?[
        try SignalboxCanonicalUUID(
          validating: MockProcessProtocolFixtures.completedToolRequestID
        )
      ]?.toolAttemptID?.rawValue,
      MockProcessProtocolFixtures.completedToolAttemptID
    )
    XCTAssertEqual(
      tool.sessionToolRequestPositions?[
        try SignalboxCanonicalUUID(
          validating: MockProcessProtocolFixtures.completedToolRequestID
        )
      ]?.toolOutput,
      MockProcessProtocolFixtures.completedToolOutput
    )
    XCTAssertEqual(
      tool.toolAttemptID?.rawValue,
      MockProcessProtocolFixtures.completedToolAttemptID
    )
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

private enum ProcessConversationTitleFixture {
  static let valid = "Imported planning"
  static let empty = ""
  static let leadingSpace = " Imported planning"
  static let trailingSpace = "Imported planning "
  static let leadingTab = "\tImported planning"
  static let trailingTab = "Imported planning\t"
  static let lineFeed = "Imported\nplanning"
  static let carriageReturn = "Imported\rplanning"
  static let nul = "Imported\0planning"
  static let tooManyScalars = String(
    repeating: "x",
    count: SignalboxProcessProtocol.maximumImportedConversationTitleScalars + 1
  )
}

private enum ProcessStreamedTextFixture {
  static let capacityError =
    "The live provider-text overlay exceeded its retained UTF-8 byte limit."
  private static let fragment = String(
    repeating: "x",
    count: SignalboxProcessProtocol.maximumContentFragmentUTF8Bytes
  )

  @MainActor
  static func applyCapacityFailure(to viewModel: ProcessSessionDetailViewModel) {
    for _ in 0..<9 {
      viewModel.apply(.providerTextDelta(delta()))
    }
  }

  private static func delta() -> SignalboxProviderTextDelta {
    SignalboxProviderTextDelta(
      sessionID: try! SignalboxCanonicalUUID(
        validating: MockSignalboxFixtures.activeSessionID
      ),
      turnID: try! SignalboxCanonicalUUID(
        validating: ProcessSubmissionFixture.acceptedTurnID
      ),
      modelCallID: try! SignalboxCanonicalUUID(
        validating: "abababab-0000-4000-8000-000000000005"
      ),
      partIndex: SignalboxCanonicalUInt64(rawValue: 0),
      content: fragment
    )
  }
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
  static let noSocketPath = ""
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
          "type":"input_submitted",
          "session_id":"\(sessionID.rawValue)",
          "accepted_input_id":"\(acceptedInputID)",
          "acceptance_position":"1",
          "turn_id":"\(acceptedTurnID)",
          "model_settings":{
            "precedence":{
              "per_call":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},
              "session":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},
              "profile":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},
              "global_default":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}}
            },
            "effective":{"reasoning_level":null,"fast_mode":"disabled","service_tier":null},
            "reasoning_source":null,
            "fast_mode_source":null,
            "service_tier_source":null,
            "validated_for_selection_id":null
          }
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
          "model_settings":{
            "precedence":{
              "per_call":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},
              "session":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},
              "profile":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},
              "global_default":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}}
            },
            "effective":{"reasoning_level":null,"fast_mode":"disabled","service_tier":null},
            "reasoning_source":null,
            "fast_mode_source":null,
            "service_tier_source":null,
            "validated_for_selection_id":null
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
      expectedDefaultsVersion: SignalboxCanonicalUInt64(rawValue: 1),
      modelSelection: .direct(
        selectionID: try SignalboxCanonicalUUID(validating: ProcessDriverFixture.modelCall)
      )
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

  static func singleRequest(
    for submission: SignalboxPreparedInputSubmission
  ) -> [SignalboxProcessClientRequest] {
    [
      .submitInput(
        commandID: submission.commandID,
        sessionID: submission.sessionID,
        content: submission.content,
        expectedDefaultsVersion: submission.expectedDefaultsVersion
      )
    ]
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
      expectedDefaultsVersion: session.defaultsVersion,
      modelSelection: session.modelSelection
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
  private let firstError: SignalboxProcessServiceError
  private(set) var submittedCommandIDs: [String] = []
  private(set) var submittedContents: [String] = []
  private(set) var submittedContentBytes: [[UInt8]] = []

  init(
    firstError: SignalboxProcessServiceError = ProcessSubmissionFixture.ambiguousMutationError
  ) {
    self.firstError = firstError
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
      expectedDefaultsVersion: session.defaultsVersion,
      modelSelection: session.modelSelection
    )
  }

  func submit(
    _ submission: SignalboxPreparedInputSubmission
  ) async throws -> SignalboxInputSubmitted {
    submittedCommandIDs.append(submission.commandID.rawValue.rawValue)
    submittedContents.append(submission.content)
    submittedContentBytes.append(Array(submission.content.utf8))
    guard submittedCommandIDs.count > 1 else {
      throw firstError
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

private actor AmbiguousThenAcceptingStopProcessService: SignalboxProcessServiceProtocol {
  private(set) var submittedCommandIDs: [String] = []
  private(set) var submittedActiveTurnIDs: [String] = []

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
    try ProcessSubmissionFixture.preparedSubmission()
  }

  func submit(
    _ submission: SignalboxPreparedInputSubmission
  ) async throws -> SignalboxInputSubmitted {
    try ProcessSubmissionFixture.submittedReceipt(sessionID: submission.sessionID)
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
      expectedDefaultsVersion: session.defaultsVersion,
      modelSelection: session.modelSelection
    )
  }

  func stopTurn(
    _ prepared: SignalboxPreparedTurnStop
  ) async throws -> SignalboxInputSubmitted {
    submittedCommandIDs.append(prepared.commandID.rawValue.rawValue)
    submittedActiveTurnIDs.append(prepared.activeTurnID.rawValue)
    guard submittedCommandIDs.count > 1 else {
      throw SignalboxProcessServiceError.mutationRetryExhausted(
        code: .commitAmbiguous,
        message: ProcessSubmissionFixture.failureMessage
      )
    }
    return try ProcessSubmissionFixture.submittedReceipt(sessionID: prepared.sessionID)
  }

  func makeSynchronization(
    sessionID: SignalboxCanonicalUUID,
    updates: @escaping @Sendable (SignalboxSessionSynchronizationDriverUpdate) async -> Void
  ) async -> any SignalboxSessionSynchronizing {
    NoopProcessSynchronization()
  }
}

private actor AmbiguousThenAcceptingToolDecisionProcessService:
  SignalboxProcessServiceProtocol
{
  private let suspendsFirstDecision: Bool
  private var prepareCallCount = 0
  private(set) var submittedCommandIDs: [String] = []
  private var decisionStarted = false
  private var decisionStartedWaiter: CheckedContinuation<Void, Never>?
  private var completionWaiter: CheckedContinuation<Void, Never>?

  init(suspendsFirstDecision: Bool = false) {
    self.suspendsFirstDecision = suspendsFirstDecision
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
    try ProcessSubmissionFixture.preparedSubmission()
  }

  func submit(
    _ submission: SignalboxPreparedInputSubmission
  ) async throws -> SignalboxInputSubmitted {
    try ProcessSubmissionFixture.submittedReceipt(sessionID: submission.sessionID)
  }

  func prepareToolRequestDecision(
    sessionID: SignalboxCanonicalUUID,
    toolRequestID: SignalboxCanonicalUUID,
    decision: SignalboxProcessToolDecision
  ) async throws -> SignalboxPreparedToolRequestDecision {
    prepareCallCount += 1
    let commandID =
      prepareCallCount == 1
      ? ProcessSubmissionFixture.commandID
      : ProcessSubmissionFixture.replacementCommandID
    return SignalboxPreparedToolRequestDecision(
      commandID: try SignalboxCommandID(validating: commandID),
      sessionID: sessionID,
      toolRequestID: toolRequestID,
      decision: decision
    )
  }

  func decideToolRequest(
    _ prepared: SignalboxPreparedToolRequestDecision
  ) async throws -> SignalboxToolRequestDecided {
    submittedCommandIDs.append(prepared.commandID.rawValue.rawValue)
    decisionStarted = true
    decisionStartedWaiter?.resume()
    decisionStartedWaiter = nil
    if suspendsFirstDecision, submittedCommandIDs.count == 1 {
      await withCheckedContinuation { continuation in
        completionWaiter = continuation
      }
    }
    guard submittedCommandIDs.count > 1 else {
      throw ProcessSubmissionFixture.ambiguousMutationError
    }
    return SignalboxToolRequestDecided(
      toolRequestID: prepared.toolRequestID,
      decision: prepared.decision
    )
  }

  func waitUntilDecisionStarted() async {
    guard !decisionStarted else {
      return
    }
    await withCheckedContinuation { continuation in
      decisionStartedWaiter = continuation
    }
  }

  func completeDecision() {
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
      expectedDefaultsVersion: session.defaultsVersion,
      modelSelection: session.modelSelection
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
      expectedDefaultsVersion: session.defaultsVersion,
      modelSelection: session.modelSelection
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
      expectedDefaultsVersion: session.defaultsVersion,
      modelSelection: session.modelSelection
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
      expectedDefaultsVersion: session.defaultsVersion,
      modelSelection: session.modelSelection
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
      expectedDefaultsVersion: session.defaultsVersion,
      modelSelection: session.modelSelection
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
      expectedDefaultsVersion: session.defaultsVersion,
      modelSelection: session.modelSelection
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
  static let initialFollowReadCount = 4
  static let bufferedFollowReadCount = 6
  static let sideStartReadCount = 2
  static let sideEndReadCount = 3
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
  static let mismatchedStopSettingsError = SignalboxProcessServiceError.unexpectedMessage(
    "The stop receipt settings named a different direct model."
  )
  static let invalidMetadataReadError = SignalboxProcessServiceError.unexpectedMessage(
    "The metadata read violated the metadata contract."
  )
  static let invalidMetadataReceiptError = SignalboxProcessServiceError.unexpectedMessage(
    "The metadata replacement receipt violated the metadata contract."
  )
  static let noncontiguousImportedPositionError = SignalboxProcessServiceError.invalidPage(
    "Imported transcript positions were not contiguous and one-based."
  )
  static let conversationListTextCapacityError = SignalboxProcessServiceError.invalidPage(
    "The native conversation list exceeded its retained UTF-8 byte limit."
  )
  static let importedTranscriptTextCapacityError = SignalboxProcessServiceError.invalidPage(
    "The imported transcript exceeded the native preview-retention cap."
  )
  static let unknownImportedContentKind = "fixture_future_content_kind"
  static let unknownImportedSourceFormat = "fixture_future_source_format"

  static func importedConversationStart(
    conversationID: SignalboxCanonicalUUID
  ) throws -> SignalboxProcessServerFrame {
    try SignalboxProcessServerFrame.decode(
      from: Data(
        """
        {
          "version":1,
          "request_id":"1",
          "message":{
            "type":"imported_conversation_start",
            "imported_conversation_id":"\(conversationID.rawValue)"
          }
        }
        """.utf8
      )
    )
  }

  static func importedConversationEntry(
    position: SignalboxCanonicalUInt64,
    entryID: String
  ) throws -> SignalboxProcessServerFrame {
    try SignalboxProcessServerFrame.decode(
      from: Data(
        """
        {
          "version":1,
          "request_id":"1",
          "message":{
            "type":"imported_conversation_entry",
            "position":"\(position.rawValue)",
            "imported_entry_id":"\(entryID)",
            "source_speaker":{"type":"attested","speaker":"user"},
            "content_kind":"text",
            "text_preview":{"preview":"Fixture text","truncated":false}
          }
        }
        """.utf8
      )
    )
  }

  static func importedConversationEntry(
    position: SignalboxCanonicalUInt64,
    entryID: String,
    sourceSpeaker: String,
    contentKind: String
  ) throws -> SignalboxProcessServerFrame {
    try frame(
      """
      {
        "type":"imported_conversation_entry",
        "position":"\(position.rawValue)",
        "imported_entry_id":"\(entryID)",
        "source_speaker":\(sourceSpeaker),
        "content_kind":"\(contentKind)",
        "text_preview":null
      }
      """
    )
  }

  static func conversationPageStart() throws -> SignalboxProcessServerFrame {
    try frame(#"{"type":"conversation_page_start"}"#)
  }

  static func importedConversationSummary(
    sourceFormat: String
  ) throws -> SignalboxProcessServerFrame {
    try frame(
      """
      {
        "type":"conversation_summary",
        "conversation":{
          "origin":"imported_conversation",
          "imported_conversation_id":"\(MockProcessProtocolFixtures.importedConversationID)",
          "title":null,
          "entry_count":"1",
          "source_format":"\(sourceFormat)"
        }
      }
      """
    )
  }

  static func conversationPageEnd() throws -> SignalboxProcessServerFrame {
    try frame(
      """
      {
        "type":"conversation_page_end",
        "conversation_count":"1",
        "next_after":null
      }
      """
    )
  }
  static let oneRowMetadataPolicy = SignalboxProcessApplicationPolicy(
    metadataPageSize: SignalboxCanonicalUInt64(rawValue: 1),
    maximumMetadataPages: SignalboxProcessApplicationPolicy.nativeDefault.maximumMetadataPages,
    ambiguousMutationRetryDelays:
      SignalboxProcessApplicationPolicy.nativeDefault.ambiguousMutationRetryDelays,
    synchronization: SignalboxProcessApplicationPolicy.nativeDefault.synchronization
  )
  static let zeroConversationScalarCapacityPolicy = SignalboxProcessApplicationPolicy(
    metadataPageSize: SignalboxCanonicalUInt64(rawValue: 1),
    maximumMetadataPages: SignalboxProcessApplicationPolicy.nativeDefault.maximumMetadataPages,
    maximumMetadataListUTF8Bytes: 0,
    ambiguousMutationRetryDelays:
      SignalboxProcessApplicationPolicy.nativeDefault.ambiguousMutationRetryDelays,
    synchronization: SignalboxProcessApplicationPolicy.nativeDefault.synchronization
  )
  static let zeroImportedScalarCapacityPolicy = SignalboxProcessApplicationPolicy(
    metadataPageSize: SignalboxProcessApplicationPolicy.nativeDefault.metadataPageSize,
    maximumMetadataPages: SignalboxProcessApplicationPolicy.nativeDefault.maximumMetadataPages,
    maximumImportedPreviewUTF8Bytes: 0,
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
        "cursor":"\(cursor)",
        "runner":null
      }
      """
    )
  }

  static func modelCallsEnd() throws -> SignalboxProcessServerFrame {
    try frame(
      """
      {
        "type":"transcript_model_calls_end",
        "model_call_count":"0"
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
    sessionID: String = session,
    validatedForSelectionID: String? = nil
  ) throws -> SignalboxProcessServerFrame {
    let validationIdentity = validatedForSelectionID.map { "\"\($0)\"" } ?? "null"
    return try frame(
      """
      {
        "type":"input_submitted",
        "session_id":"\(sessionID)",
        "accepted_input_id":"\(ProcessSubmissionFixture.acceptedInputID)",
        "acceptance_position":"1",
        "turn_id":"\(ProcessSubmissionFixture.acceptedTurnID)",
        "model_settings":{
          "precedence":{
            "per_call":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},
            "session":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},
            "profile":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},
            "global_default":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}}
          },
          "effective":{"reasoning_level":null,"fast_mode":"disabled","service_tier":null},
          "reasoning_source":null,
          "fast_mode_source":null,
          "service_tier_source":null,
          "validated_for_selection_id":\(validationIdentity)
        }
      }
      """
    )
  }

  static func unknownMutationReceipt() throws -> SignalboxProcessServerFrame {
    try frame(#"{"type":"future_mutation_receipt","opaque":"fixture"}"#)
  }

  static func futureMutationError() throws -> SignalboxProcessServerFrame {
    try frame(
      """
      {
        "type":"error",
        "code":"fixture_future_mutation_error",
        "message":"Fixture future mutation outcome."
      }
      """
    )
  }

  static let futureMutationRemoteError = SignalboxProcessServiceError.remote(
    code: .unknown("fixture_future_mutation_error"),
    message: "Fixture future mutation outcome.",
    detail: nil
  )

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
          "version":1,
          "request_id":"1",
          "message":\(message)
        }
        """.utf8
      )
    )
  }
}

private enum ProcessProjectionFixture {
  static let emptyModelCallsBoundary = """
    {
      "type":"transcript_model_calls_end",
      "model_call_count":"0"
    }
    """
  static let userText = "fixture materialized user input"
  static let proposedAssistantText = "I will inspect the fixture before using the tool."
  static let proposedToolName = "inspect_fixture"
  static let terminalFailureToolNames = [proposedToolName]
  static let proposedAssistantEntry = "99999999-9999-4999-8999-999999999999"
  static let modelIdentityEntry = "99999999-1111-4111-8111-111111111111"
  static let proposedToolEntry = "aaaaaaaa-1111-4111-8111-111111111111"
  static let proposedToolRequest = "bbbbbbbb-1111-4111-8111-111111111111"
  static let reusedToolSourceSession = "aaaaaaaa-5555-4555-8555-555555555555"
  static let reusedToolEntry = "bbbbbbbb-5555-4555-8555-555555555555"
  static let reusedToolResultEntry = "cccccccc-5555-4555-8555-555555555555"
  static let reusedToolAttempt = "dddddddd-5555-4555-8555-555555555555"
  static let reusedToolName = "inspect_imported_fixture"
  static let reusedToolOutput = "Imported fixture inspected."
  static let sourceQualifiedToolSummaries = [
    "\(proposedToolName): \(reconciliationOutput)",
    "\(reusedToolName): \(reusedToolOutput)",
  ]
  static let crossTurn = "cccccccc-3333-4333-8333-333333333333"
  static let reconciliationAttempt = "dddddddd-3333-4333-8333-333333333333"
  static let reconciliationResultEntry = "eeeeeeee-3333-4333-8333-333333333333"
  static let reconciliationOutput = "Fixture cross-turn result."
  static let reconciliationSuffixToolRequest = "ffffffff-3333-4333-8333-333333333333"
  static let reconciliationSuffixToolEntry = "99999999-3333-4333-8333-333333333333"
  static let reconciliationSuffixAttempt = "88888888-3333-4333-8333-333333333333"
  static let reconciliationSuffixResultEntry = "77777777-3333-4333-8333-333333333333"
  static let reconciliationSuffixToolName = "inspect_related_fixture"
  static let reconciliationSuffixOutput = "Related fixture inspected."
  static let laterTurnToolEntry = "66666666-3333-4333-8333-333333333333"
  static let laterTurnToolRequest = "55555555-3333-4333-8333-333333333333"
  static let laterTurnModelCall = "44444444-3333-4333-8333-333333333333"
  static let laterTurnToolName = "inspect_later_fixture"
  static let reconciliationClosedEntry = "33333333-3333-4333-8333-333333333333"
  static let reconciliationClosedOutput = "Ambiguous fixture tool closed."
  static let reconciliationDeniedOutput = "Fixture tool request denied."
  static let delegationSpawningRequest = "22222222-3333-4333-8333-333333333333"
  static let delegationChildSession = "11111111-3333-4333-8333-333333333333"
  static let delegationChildTurn = "aaaaaaaa-3333-4333-8333-333333333333"
  static let delegationResultContent = "Delegated fixture inspected."
  static let foregroundDelegationResultKinds = ["delegation_result"]
  static let foregroundDelegationToolSummaries = [
    "\(proposedToolName): \(delegationResultContent)",
    "\(reconciliationSuffixToolName): \(reconciliationSuffixOutput)",
  ]
  static let reconciliationSuffixToolNames = [
    proposedToolName, reconciliationSuffixToolName,
  ]
  static let reconciliationClosedToolNames = [proposedToolName]
  static let completedUserEntry = "aaaaaaaa-2222-4222-8222-222222222222"
  static let completedAssistantEntry = "bbbbbbbb-2222-4222-8222-222222222222"
  static let completedAssistantText = "Fixture terminal assistant response."
  static let contextSummaryText = "Fixture compacted context summary."
  static let contextSummaryLabel = "Context summary"
  static let contextCompactionNoticeTitles = ["Model usage"]
  static let compactedToolTimelineKinds: [ProcessTimelineFixtureKind] = [
    .message, .tool, .unknown,
  ]
  static let contextSummaryEntry = "cccccccc-2222-4222-8222-222222222222"
  static let contextCompactionID = "dddddddd-2222-4222-8222-222222222222"
  static let closedToolID = "cccccccc-1111-4111-8111-111111111111"
  static let closedToolName = "closed_fixture_tool"
  static let importedConversation = "eeeeeeee-4444-4444-8444-444444444444"
  static let importedAssistantSpeaker = #"{"type":"attested","speaker":"assistant"}"#
  static let importedSourceEventEntry = "eeeeeeee-4444-4444-8444-444444444441"
  static let importedThinkingEntry = "eeeeeeee-4444-4444-8444-444444444442"
  static let importedToolCallEntry = "eeeeeeee-4444-4444-8444-444444444443"
  static let importedToolResultEntry = "eeeeeeee-4444-4444-8444-444444444444"
  static let importedFutureEntry = "eeeeeeee-4444-4444-8444-444444444445"
  static let futureImportedContentKind = "fixture_future_imported_content"
  static let futureImportedPresentationKind = "imported_\(futureImportedContentKind)"
  static let futureImportedPresentationKinds = [futureImportedPresentationKind]
  static let importedContentKinds: [SignalboxProcessImportedContentKind] = [
    .sourceEvent, .thinking, .toolCall, .toolResult,
  ]
  static let importedNoticeTitles = [
    "Imported source event", "Imported thinking", "Imported tool call",
    "Imported tool result",
  ]
  static let terminalUsageNoticeTitles = ["Imported source event", "Model usage"]
  static let triggeredModelCallUsageIDs = [ProcessDriverFixture.modelCall]
  static let outputPreviewLimit = 480
  static let outputPreviewTruncationMarker = "..."
  static let importedSpeakerLabels = [
    SignalboxProcessMessageSourceAttribution.importedSpeakerNotAttested.presentationLabel,
    SignalboxProcessMessageSourceAttribution.importedAssistantRole.presentationLabel,
    SignalboxProcessMessageSourceAttribution.importedAssistantRole.presentationLabel,
    SignalboxProcessMessageSourceAttribution.importedUserRole.presentationLabel,
  ]
  static let importedUserRoleAttribution = SignalboxProcessMessageSourceAttribution.importedUserRole
  static let importedUserRoleLabel = importedUserRoleAttribution.presentationLabel
  static let importedUnattestedAttribution =
    SignalboxProcessMessageSourceAttribution.importedSpeakerNotAttested
  static let importedUnattestedLabel = importedUnattestedAttribution.presentationLabel
  static let importedAttestedAbsentAttribution =
    SignalboxProcessMessageSourceAttribution.importedSpeakerAbsent
  static let importedAttestedAbsentLabel = importedAttestedAbsentAttribution.presentationLabel
  static let modelUsageNoticeTitle = "Model usage"
  static let modelCallDetailLabel = "Model call"
  static let modelNoticeTitles = ["Model changed", modelUsageNoticeTitle]
  static let noNoticeTitles: [String] = []
  static let modelPresentationEventKinds = [
    "process_model_identity", "process_message", "process_message",
    "process_model_call_usage",
  ]
  static let modelIdentityDefaultsVersion = UInt64(7)
  static let modelInputTokens = UInt64(12)
  static let modelOutputTokens = UInt64(3)
  static let modelCacheCreationTokens = UInt64(4)
  static let modelUsageProvenance = "reported"
  static let modelCostAmountUSD = "0.0012"
  static let modelCostRateVersion = "fixture-rate-v1"
  static let modelCostLabel = "real"
  static let earlierModelCall = "11111111-2222-4222-8222-222222222222"
  static let orderedModelCallUsageIDs = [earlierModelCall, ProcessDriverFixture.modelCall]
  static let anchorReuseUsageCount = orderedModelCallUsageIDs.count
  static let adjacentUsageAndTurnStateTimelineKinds: [ProcessTimelineFixtureKind] = [
    .message, .processEvidence, .unknown, .message,
  ]
  static let modelNoticeDetailValues = [
    ProcessDriverFixture.turn,
    ProcessDriverFixture.modelCall,
    modelIdentityDefaultsVersion.description,
    ProcessDriverFixture.turn,
    ProcessDriverFixture.modelCall,
    modelUsageProvenance,
    modelInputTokens.description,
    modelOutputTokens.description,
    modelCacheCreationTokens.description,
    "Not reported",
    modelCostAmountUSD,
    modelCostRateVersion,
    modelCostLabel,
  ]
  static let futureFollowedEventKind = "future_session_event"
  static let futureTurnStateKind = "fixture_future_turn_state"
  static let futureTurnStatePresentationKind = "turn.state.\(futureTurnStateKind)"
  static let twoUnknownTurnStatePresentationKinds = [
    futureTurnStatePresentationKind, futureTurnStatePresentationKind,
  ]
  static let futureCurrentModelStateKind = "fixture_future_current_model_state"
  static let futureCurrentModelStatePresentationKind =
    "current_model_call.state.\(futureCurrentModelStateKind)"
  static let futureFollowedModelCallDiagnostic =
    "Turn \(ProcessDriverFixture.turn), model call \(ProcessDriverFixture.modelCall): "
    + "the daemon reported an unrecognized model-call state."
  static let unknownDispositionPresentationKind =
    "model_call_transition.disposition.\(unknownDisposition)"
  static let unknownDispositionPresentationDiagnostic =
    "Turn \(ProcessDriverFixture.turn), model call \(ProcessDriverFixture.modelCall): "
    + "the daemon reported an unrecognized terminal disposition."
  static let futureProviderFailureCause = "fixture_future_provider_failure"
  static let unknownProviderCausePresentationKind =
    "model_call_failure.cause.\(futureProviderFailureCause)"
  static let unknownProviderCausePresentationDiagnostic =
    "Turn \(ProcessDriverFixture.turn), model call \(ProcessDriverFixture.modelCall): "
    + "the snapshot retained an unrecognized provider-failure cause."
  static let futureSnapshotStatePresentationKinds = [
    futureTurnStatePresentationKind, futureCurrentModelStatePresentationKind,
  ]
  static let futureSnapshotStateDiagnostics = [
    "Turn \(ProcessDriverFixture.turn): the snapshot retained an unrecognized turn state.",
    "Turn \(crossTurn), model call \(ProcessDriverFixture.modelCall): "
      + "the snapshot retained an unrecognized current model-call state.",
  ]
  static let unknownThenTranscriptTimelineKinds: [ProcessTimelineFixtureKind] = [
    .unknown, .message, .message,
  ]
  static let planReadRequestID = "ffffffff-5555-4555-8555-555555555551"
  static let planWriteRequestID = "ffffffff-5555-4555-8555-555555555552"
  static let planCreateRequestID = "ffffffff-5555-4555-8555-555555555553"
  static let planReviseRequestID = "ffffffff-5555-4555-8555-555555555554"
  static let planStatusRequestID = "ffffffff-5555-4555-8555-555555555555"
  static let malformedPlanRequestID = "ffffffff-5555-4555-8555-555555555556"
  static let malformedPlanReadRequestID = "ffffffff-5555-4555-8555-555555555557"
  static let planTurnID = "aaaaaaaa-5555-4555-8555-555555555551"
  static let planIssuingAttemptID = "aaaaaaaa-5555-4555-8555-555555555552"
  static let planProvenanceRequestID = "aaaaaaaa-5555-4555-8555-555555555553"
  static let planAttemptID = "aaaaaaaa-5555-4555-8555-555555555554"
  static let planGeneration = UInt64(3)
  static let planProvenance = #""provenance":{"turn_id":"\#(planTurnID)","issuing_attempt_id":"\#(planIssuingAttemptID)","request_id":"\#(planProvenanceRequestID)","attempt_id":"\#(planAttemptID)","generation":\#(planGeneration)}"#
  static let planReadArguments = #"{"include_history":true}"#
  static let planReadOutput = #"{"entries":[{"entry_id":1,"text":"Audit protocol","status":"pending","dependencies":[],"readiness":"ready"}],"next_after_entry_id":null,"plan_truncated":false,"history":[{"ordinal":1,"kind":"created","entry_id":1,"text":"Audit protocol",\#(planProvenance)}],"history_truncated":false}"#
  static let planReadWithoutHistoryArguments = #"{"include_history":false}"#
  static let planReadWithoutHistoryOutput = #"{"entries":[{"entry_id":1,"text":"Audit protocol","status":"pending","dependencies":[],"readiness":"ready"}],"next_after_entry_id":null,"plan_truncated":false,"history":null,"history_truncated":false}"#
  static let planHistoryWriteOutput = #"{"event":{"ordinal":1,"kind":"created","entry_id":1,"text":"Audit protocol",\#(planProvenance)}}"#
  static let uncorrelatedPlanHistoryOutput = planReadOutput.replacingOccurrences(
    of: planTurnID,
    with: crossTurn
  )
  static let uncorrelatedPlanHistoryWriteOutput =
    planHistoryWriteOutput.replacingOccurrences(of: planTurnID, with: crossTurn)
  static let alteredPlanHistoryOutput = planReadOutput.replacingOccurrences(
    of: "Audit protocol",
    with: "Altered protocol"
  )
  static let planCreateArguments = #"{"kind":"create","text":"Draft protocol"}"#
  static let planCreateOutput = #"{"event":{"ordinal":1,"kind":"created","entry_id":1,"text":"Draft protocol",\#(planWriteProvenanceJSON(requestID: planCreateRequestID))}}"#
  static let mismatchedIssuingAttemptPlanCreateOutput = planCreateOutput.replacingOccurrences(
    of: planIssuingAttemptID,
    with: crossTurn
  )
  static let mismatchedPlanCreateOutput = #"{"event":{"ordinal":1,"kind":"created","entry_id":1,"text":"Different text",\#(planWriteProvenanceJSON(requestID: planCreateRequestID))}}"#
  static let planOutputWithoutProvenance =
    #"{"event":{"ordinal":1,"kind":"created","entry_id":1,"text":"Draft protocol"}}"#
  static let planReviseArguments = #"{"kind":"revise","entry_id":1,"text":"Audit protocol"}"#
  static let planReviseOutput = #"{"event":{"ordinal":2,"kind":"text_revised","entry_id":1,"text":"Audit protocol",\#(planWriteProvenanceJSON(requestID: planReviseRequestID))}}"#
  static let planStatusArguments = #"{"kind":"set_status","entry_id":1,"status":"completed"}"#
  static let planStatusOutput = #"{"event":{"ordinal":3,"kind":"status_changed","entry_id":1,"status":"completed",\#(planWriteProvenanceJSON(requestID: planStatusRequestID))}}"#
  static let planWriteArguments = #"{"kind":"depends_on","entry_id":1,"dependency_id":2}"#
  static let planWriteOutput = #"{"event":{"ordinal":4,"kind":"depends_on","entry_id":1,"dependency_id":2,\#(planWriteProvenanceJSON(requestID: planWriteRequestID))}}"#
  static let malformedPlanArguments = #"{"kind":"set_status","entry_id":0,"status":"completed"}"#
  static let malformedPlanReadArguments = #"{"after_entry_id":0,"unexpected":true}"#
  static let nullPlanReadHistoryArguments = #"{"include_history":null}"#
  static let duplicatePlanReadArguments =
    #"{"include_history":false,"include_history":true}"#
  static let duplicatePlanWriteArguments =
    #"{"kind":"create","text":"First","text":"Second"}"#
  static let malformedPlanReadOutput = #"{"entries":[{"entry_id":0,"text":"Audit protocol","status":"pending","dependencies":[],"readiness":"ready"}],"next_after_entry_id":null,"plan_truncated":false,"history":null,"history_truncated":false}"#
  static let expandedPlanWriteOutput = #"{"event":{"ordinal":1,"kind":"created","entry_id":1,"text":"Draft protocol",\#(planWriteProvenanceJSON(requestID: planCreateRequestID))},"future_field":"retained"}"#
  static let contradictoryPlanReadCursorOutput = #"{"entries":[{"entry_id":1,"text":"Audit protocol","status":"in_progress","dependencies":[],"readiness":"ready"}],"next_after_entry_id":1,"plan_truncated":false,"history":null,"history_truncated":false}"#
  static let floatingPlanReadArguments = #"{"after_entry_id":1.0,"include_history":false}"#
  static let exponentPlanOrdinalOutput = #"{"event":{"ordinal":1e0,"kind":"created","entry_id":1,"text":"Draft protocol",\#(planWriteProvenanceJSON(requestID: planCreateRequestID))}}"#
  static let truncatedAbsentPlanHistoryOutput = rawPlanReadOutput(
    entries: "[]",
    history: "null",
    historyTruncated: true
  )
  static let truncatedEmptyPlanHistoryOutput = rawPlanReadOutput(
    entries: "[]",
    history: "[]",
    historyTruncated: true
  )
  static let excessivePlanDependenciesOutput = rawPlanReadOutput(
    entries: #"[{"entry_id":1,"text":"Audit protocol","status":"pending","dependencies":[\#(excessivePlanDependencies)],"readiness":"ready"}]"#
  )
  static let unorderedPlanEntriesOutput = rawPlanReadOutput(
    entries: #"[{"entry_id":2,"text":"Second","status":"pending","dependencies":[],"readiness":"ready"},{"entry_id":1,"text":"First","status":"pending","dependencies":[],"readiness":"ready"}]"#
  )
  static let selfDependentPlanEntryOutput = rawPlanReadOutput(
    entries: #"[{"entry_id":1,"text":"Audit protocol","status":"pending","dependencies":[1],"readiness":"waiting"}]"#
  )
  static let cyclicPlanEntriesOutput = rawPlanReadOutput(
    entries: #"[{"entry_id":1,"text":"First","status":"pending","dependencies":[2],"readiness":"waiting"},{"entry_id":2,"text":"Second","status":"pending","dependencies":[1],"readiness":"waiting"}]"#
  )
  static let inconsistentReadyPlanEntryOutput = rawPlanReadOutput(
    entries: #"[{"entry_id":1,"text":"First","status":"pending","dependencies":[2],"readiness":"ready"},{"entry_id":2,"text":"Second","status":"pending","dependencies":[],"readiness":"ready"}]"#
  )
  static let inconsistentWaitingPlanEntryOutput = rawPlanReadOutput(
    entries: #"[{"entry_id":1,"text":"First","status":"pending","dependencies":[2],"readiness":"waiting"},{"entry_id":2,"text":"Second","status":"completed","dependencies":[],"readiness":"ready"}]"#
  )
  static let futurePlanEntryReferenceOutput = #"{"event":{"ordinal":2,"kind":"status_changed","entry_id":3,"status":"completed",\#(planWriteProvenanceJSON(requestID: planStatusRequestID))}}"#
  static let planReadBeforeCursorArguments =
    #"{"after_entry_id":1,"include_history":false}"#
  static let planReadBeforeCursorOutput = rawPlanReadOutput(
    entries: #"[{"entry_id":1,"text":"Audit protocol","status":"pending","dependencies":[],"readiness":"ready"}]"#
  )
  static let incompletePlanHistoryOutput = rawPlanReadOutput(
    entries: #"[{"entry_id":2,"text":"Second","status":"pending","dependencies":[],"readiness":"ready"}]"#,
    history: #"[{"ordinal":2,"kind":"created","entry_id":2,"text":"Second",\#(planProvenance)}]"#
  )
  static let mismatchedPlanHistoryOutput = rawPlanReadOutput(
    entries: #"[{"entry_id":1,"text":"Audit protocol","status":"pending","dependencies":[],"readiness":"ready"}]"#,
    history: #"[{"ordinal":1,"kind":"created","entry_id":1,"text":"Draft protocol",\#(planProvenance)}]"#
  )
  static let repeatedPlanHistoryAttemptOutput = rawPlanReadOutput(
    entries: #"[{"entry_id":1,"text":"First","status":"pending","dependencies":[],"readiness":"ready"},{"entry_id":2,"text":"Second","status":"pending","dependencies":[],"readiness":"ready"}]"#,
    history: #"[{"ordinal":1,"kind":"created","entry_id":1,"text":"First",\#(planProvenance)},{"ordinal":2,"kind":"created","entry_id":2,"text":"Second",\#(planProvenance)}]"#
  )
  static let planHistoryDependencyOverflowOutput = makePlanHistoryDependencyOverflowOutput()
  static let emptyIncludedPlanHistoryOutput = rawPlanReadOutput(
    entries: "[]",
    history: "[]"
  )
  static let cyclicPlanHistoryOutput = makeCyclicPlanHistoryOutput()
  static let denseAcyclicPlanOutput = makeDenseAcyclicPlanOutput()
  static let multilinePlanText = "Draft\nHistory"
  static let multilinePlanArguments = #"{"kind":"create","text":"Draft\nHistory"}"#
  static let multilinePlanOutput = #"{"event":{"ordinal":1,"kind":"created","entry_id":1,"text":"Draft\nHistory",\#(planWriteProvenanceJSON(requestID: planCreateRequestID))}}"#
  static let alternateNewlinePlanArguments =
    #"{"kind":"create","text":"Carriage\rVertical\u000bForm\u000cNext\u0085Line\u2028Paragraph\u2029End"}"#
  static let alternateNewlinePlanOutput =
    #"{"event":{"ordinal":1,"kind":"created","entry_id":1,"text":"Carriage\rVertical\u000bForm\u000cNext\u0085Line\u2028Paragraph\u2029End",\#(planWriteProvenanceJSON(requestID: planCreateRequestID))}}"#
  static let incompletePlanHistoryPreview = rawToolOutputPreview(incompletePlanHistoryOutput)
  static let mismatchedPlanHistoryPreview = rawToolOutputPreview(mismatchedPlanHistoryOutput)
  static let repeatedPlanHistoryAttemptPreview = rawToolOutputPreview(
    repeatedPlanHistoryAttemptOutput
  )
  static let planHistoryDependencyOverflowPreview = rawToolOutputPreview(
    planHistoryDependencyOverflowOutput
  )
  static let cyclicPlanHistoryPreview = rawToolOutputPreview(cyclicPlanHistoryOutput)
  static let uncorrelatedPlanHistoryPreview = rawToolOutputPreview(
    uncorrelatedPlanHistoryOutput
  )
  static let planReadDisplayName = "Plan read"
  static let planWriteDisplayName = "Plan update"
  static let rawToolOutputLabel = "Output"
  static let planReadRawOutputPreview = rawToolOutputPreview(planReadOutput)
  static let alteredPlanHistoryOutputPreview = rawToolOutputPreview(
    alteredPlanHistoryOutput
  )
  static let planCreateRawOutputPreview = rawToolOutputPreview(planCreateOutput)
  static let mismatchedIssuingAttemptPlanCreateOutputPreview = rawToolOutputPreview(
    mismatchedIssuingAttemptPlanCreateOutput
  )
  static let planReadArgumentPresentation = "After entry: Beginning\nInclude history: No"
  static let planCreateArgumentPresentation = "Create entry: Draft protocol"
  static let planReviseArgumentPresentation = "Revise entry #1: Audit protocol"
  static let planStatusArgumentPresentation = "Set entry #1 to Completed"
  static let planDependencyArgumentPresentation = "Make entry #1 depend on entry #2"
  static let planReadOutputPresentation = """
    Entries
    #1 [Pending, Ready] Audit protocol
    Dependencies: None
    Continue after: None
    Plan truncated: No
    History
    Not included
    History truncated: No
    """
  static let planReviseRawOutputPreview = rawToolOutputPreview(planReviseOutput)
  static let planStatusRawOutputPreview = rawToolOutputPreview(planStatusOutput)
  static let planDependencyRawOutputPreview = rawToolOutputPreview(planWriteOutput)
  static let multilinePlanArgumentPresentation = "Create entry: Draft\\nHistory"
  static let multilinePlanRawOutputPreview = rawToolOutputPreview(multilinePlanOutput)
  static let alternateNewlinePlanArgumentPresentation =
    "Create entry: Carriage\\rVertical\\u{B}Form\\u{C}Next\\u{85}Line\\u{2028}Paragraph\\u{2029}End"
  static let alternateNewlinePlanRawOutputPreview = rawToolOutputPreview(
    alternateNewlinePlanOutput
  )
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
  static let quotaExhaustedActivity = SignalboxProcessActivity(
    state: .failed,
    label: "Failed: provider quota exhausted"
  )
  static let unknownFailedActivity = SignalboxProcessActivity(
    state: .failed,
    label: "Failed: unrecognized disposition (\(unknownDisposition))"
  )
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
  static let unknownDisposition = "fixture_future_disposition"
  static let unknownDispositionDiagnostic =
    "Preserved an unrecognized model-call disposition: \(unknownDisposition)."
  static let unknownModelCallState = "fixture_future_model_call_state"
  static let unknownStateDiagnostic =
    "Preserved an unrecognized model-call state: \(unknownModelCallState)."
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
  static let orderedPresentationIDs = [1, 5]
  static let anchoredUsageTimelineKinds: [ProcessTimelineFixtureKind] = [
    .message, .processEvidence, .message,
  ]
  static let terminalMarkerUsageTimelineKinds: [ProcessTimelineFixtureKind] = [
    .processEvidence, .turnFailure, .processEvidence,
  ]
  static let terminalFailureEventKinds = ["process_turn_failure"]
  static let reanchoredUsageNoticeTitles = [
    "Imported source event", modelUsageNoticeTitle, "Imported thinking",
  ]
  static let orderedMessageRoles = [SignalboxMessageRole.user, .assistant]
  static let acceptedThenTimelineRowKinds: [ProcessTranscriptRowFixtureKind] = [
    .accepted, .timeline,
  ]
  static let singleRecordCount = 1
  static let activationModelIdentityDefaultsVersion: UInt64 = 1
  static let unknownTextEntryKind = "fixture_future_text_entry"
  static let unknownTextEntryContent = "Fixture future text."
  static let unknownSpeakerWrapperLabel = "Unknown speaker (fixture_future_speaker_wrapper)"
  static let unknownAttestedSpeakerLabel = "Unrecognized speaker (fixture_future_speaker)"
  static let unknownToolBatchState = "fixture_future_tool_batch_state"
  static let unknownToolBatchTimelineKind =
    "tool_batch_transition.state.\(unknownToolBatchState)"
  static let unknownToolBatchDiagnostic =
    "Preserved an unrecognized tool-batch state: \(unknownToolBatchState)."
  static let remoteErrorMessage = "Fixture remote error."
  static let mutationRetryGuidance = " The exact command can be retried."
  static let oversizedUnknownState = String(
    repeating: "x",
    count: SignalboxSessionSynchronizationMachine.maximumRetainedDiagnosticMessageUTF8Bytes + 1
  )
  static let bufferedTransitionCursor: UInt64 = 2
  static let sideSnapshotCursor: UInt64 = 3
  static let newerTransitionCursor: UInt64 = 4

  static func materializedAcceptedInputIDs() throws -> Set<SignalboxCanonicalUUID> {
    [try SignalboxCanonicalUUID(validating: ProcessSubmissionFixture.acceptedInputID)]
  }

  static func modelIdentityEvent(
    in record: SignalboxStoredEvent
  ) throws -> SignalboxProcessModelIdentityEvent {
    guard case .processModelIdentity(let event) = record.event else {
      throw ProcessDriverUpdateRecorderError.expectedUnknownEvent
    }
    return event
  }

  static func contextSummaryRecord(
    in projection: SignalboxProcessTranscriptProjection
  ) throws -> SignalboxStoredEvent {
    let records = projection.records.filter {
      if case .processContextSummary = $0.event {
        return true
      }
      return false
    }
    guard records.count == singleRecordCount, let record = records.first else {
      throw ProcessDriverUpdateRecorderError.missingFixtureMessage
    }
    return record
  }

  static func conservativeEvent(
    in record: SignalboxStoredEvent
  ) throws -> SignalboxProcessConservativeEvent {
    guard case .processConservative(let event) = record.event else {
      throw ProcessDriverUpdateRecorderError.expectedUnknownEvent
    }
    return event
  }

  static func snapshotWithFailedProviderCause() throws -> SignalboxSynchronizationSnapshot {
    try snapshotWithFailedModelCall(
      disposition: "known_failed",
      causeMember: ",\"cause\":\"quota_exhausted\""
    )
  }

  static func snapshotWithUnknownFailedProviderCause(
    _ cause: String
  ) throws -> SignalboxSynchronizationSnapshot {
    try snapshotWithFailedModelCall(
      disposition: "known_failed",
      causeMember: ",\"cause\":\"\(cause)\""
    )
  }

  static func snapshotWithUnknownFailedDisposition(
    _ disposition: String = unknownDisposition
  ) throws
    -> SignalboxSynchronizationSnapshot
  {
    try snapshotWithFailedModelCall(disposition: disposition, causeMember: "")
  }

  private static func snapshotWithFailedModelCall(
    disposition: String,
    causeMember: String
  ) throws -> SignalboxSynchronizationSnapshot {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "acceptance_position":"1",
          "state":{
            "type":"failed",
            "terminal_frontier_id":"\(ProcessDriverFixture.frontier)",
            "terminal_attempt_id":"\(ProcessDriverFixture.attempt)",
            "terminal_model_call":{
              "model_call_id":"\(ProcessDriverFixture.modelCall)",
              "disposition":"\(disposition)"\(causeMember)
            }
          }
        }
        """,
        """
        {
          "type":"transcript_model_call_usage",
          "model_call_index":"0",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "model_call_id":"\(ProcessDriverFixture.modelCall)",
          "usage_provenance":"reported",
          "usage":{
            "input_tokens":null,
            "output_tokens":null,
            "cache_creation_input_tokens":null,
            "cache_read_input_tokens":null
          },
          "cost":null
        }
        """,
        """
        {
          "type":"transcript_model_calls_end",
          "model_call_count":"1"
        }
        """,
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "turn_count":"1",
          "entry_count":"0"
        }
        """,
      ]
    )
  }

  static func snapshotWithUnknownCurrentModelCallState(
    cursor: UInt64 = 1
  ) throws
    -> SignalboxSynchronizationSnapshot
  {
    try snapshotWithActiveTurnState(
      """
      {
        "type":"active_running",
        "current_attempt_id":"\(ProcessDriverFixture.attempt)",
        "current_model_call":{
          "model_call_id":"\(ProcessDriverFixture.modelCall)",
          "state":{"type":"\(unknownModelCallState)"}
        }
      }
      """,
      cursor: cursor
    )
  }

  static func snapshotWithUnknownTurnState(
    cursor: UInt64 = 1
  ) throws -> SignalboxSynchronizationSnapshot {
    try snapshotWithActiveTurnState(
      """
      {"type":"fixture_future_turn_state","retained":true}
      """,
      cursor: cursor
    )
  }

  static func snapshotWithHistoricalUnknownTurn() throws -> SignalboxSynchronizationSnapshot {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "acceptance_position":"1",
          "state":{"type":"fixture_future_turn_state","retained":true}
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(secondPendingTurn)",
          "acceptance_position":"2",
          "state":{
            "type":"cancelled",
            "terminal_frontier_id":"\(ProcessDriverFixture.frontier)",
            "terminal_attempt_id":"\(ProcessDriverFixture.attempt)",
            "terminal_model_call_id":null
          }
        }
        """,
        emptyModelCallsBoundary,
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "turn_count":"2",
          "entry_count":"0"
        }
        """,
      ]
    )
  }

  static func snapshotWithKnownActiveTurn(
    cursor: UInt64
  ) throws -> SignalboxSynchronizationSnapshot {
    try snapshotWithActiveTurnState(
      """
      {
        "type":"active_running",
        "current_attempt_id":"\(ProcessDriverFixture.attempt)",
        "current_model_call":null
      }
      """,
      cursor: cursor
    )
  }

  static func snapshotWithDuplicateTurnRecords() throws -> SignalboxSynchronizationSnapshot {
    let snapshot = try snapshotWithKnownActiveTurn(cursor: sideSnapshotCursor)
    return SignalboxSynchronizationSnapshot(
      sessionID: snapshot.sessionID,
      cursor: snapshot.cursor,
      records: snapshot.records + snapshot.records
    )
  }

  static func snapshotWithCompletedTurn(
    cursor: UInt64
  ) throws -> SignalboxSynchronizationSnapshot {
    try snapshotWithTerminalTurnState(
      """
      {
        "type":"completed",
        "terminal_frontier_id":"\(ProcessDriverFixture.frontier)",
        "terminal_attempt_id":"\(ProcessDriverFixture.attempt)",
        "terminal_model_call_id":"\(ProcessDriverFixture.modelCall)"
      }
      """,
      cursor: cursor
    )
  }

  static func snapshotWithModelReconciliationTurn(
    cursor: UInt64
  ) throws -> SignalboxSynchronizationSnapshot {
    try snapshotWithTerminalTurnState(
      """
      {
        "type":"reconciliation_required",
        "terminal_frontier_id":"\(ProcessDriverFixture.frontier)",
        "terminal_attempt_id":"\(ProcessDriverFixture.attempt)",
        "terminal_model_call_id":"\(ProcessDriverFixture.modelCall)"
      }
      """,
      cursor: cursor
    )
  }

  static func snapshotWithToolReconciliationTurn(
    cursor: UInt64
  ) throws -> SignalboxSynchronizationSnapshot {
    try snapshotWithTerminalTurnState(
      """
      {
        "type":"tool_reconciliation_required",
        "terminal_frontier_id":"\(ProcessDriverFixture.frontier)",
        "terminal_attempt_id":"\(ProcessDriverFixture.attempt)",
        "terminal_tool_attempt_id":"\(reconciliationAttempt)"
      }
      """,
      cursor: cursor
    )
  }

  static func snapshotWithReconciliationAndActiveSuccessor(
    cursor: UInt64
  ) throws -> SignalboxSynchronizationSnapshot {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"\(cursor)",
          "runner":null
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "acceptance_position":"1",
          "state":{
            "type":"reconciliation_required",
            "terminal_frontier_id":"\(ProcessDriverFixture.frontier)",
            "terminal_attempt_id":"\(ProcessDriverFixture.attempt)",
            "terminal_model_call_id":"\(ProcessDriverFixture.modelCall)"
          }
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(secondPendingTurn)",
          "acceptance_position":"2",
          "state":{
            "type":"active_running",
            "current_attempt_id":"\(reconciliationAttempt)",
            "current_model_call":null
          }
        }
        """,
        """
        {
          "type":"transcript_model_call_usage",
          "model_call_index":"0",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "model_call_id":"\(ProcessDriverFixture.modelCall)",
          "usage_provenance":"reported",
          "usage":{
            "input_tokens":null,
            "output_tokens":null,
            "cache_creation_input_tokens":null,
            "cache_read_input_tokens":null
          },
          "cost":null
        }
        """,
        """
        {
          "type":"transcript_model_calls_end",
          "model_call_count":"1"
        }
        """,
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"\(cursor)",
          "turn_count":"2",
          "entry_count":"0"
        }
        """,
      ]
    )
  }

  private static func snapshotWithTerminalTurnState(
    _ state: String,
    cursor: UInt64
  ) throws -> SignalboxSynchronizationSnapshot {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"\(cursor)",
          "runner":null
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "acceptance_position":"1",
          "state":\(state)
        }
        """,
        """
        {
          "type":"transcript_model_call_usage",
          "model_call_index":"0",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "model_call_id":"\(ProcessDriverFixture.modelCall)",
          "usage_provenance":"reported",
          "usage":{
            "input_tokens":null,
            "output_tokens":null,
            "cache_creation_input_tokens":null,
            "cache_read_input_tokens":null
          },
          "cost":null
        }
        """,
        """
        {
          "type":"transcript_model_calls_end",
          "model_call_count":"1"
        }
        """,
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"\(cursor)",
          "turn_count":"1",
          "entry_count":"0"
        }
        """,
      ]
    )
  }

  static func snapshotWithKnownRecoveryTurn(
    cursor: UInt64
  ) throws -> SignalboxSynchronizationSnapshot {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"\(cursor)",
          "runner":null
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "acceptance_position":"1",
          "state":{
            "type":"active_awaiting_model_call_recovery",
            "ended_attempt_id":"\(ProcessDriverFixture.attempt)",
            "recovery_model_call_id":"\(ProcessDriverFixture.modelCall)"
          }
        }
        """,
        """
        {
          "type":"transcript_model_call_usage",
          "model_call_index":"0",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "model_call_id":"\(ProcessDriverFixture.modelCall)",
          "usage_provenance":"reported",
          "usage":{
            "input_tokens":null,
            "output_tokens":null,
            "cache_creation_input_tokens":null,
            "cache_read_input_tokens":null
          },
          "cost":null
        }
        """,
        """
        {
          "type":"transcript_model_calls_end",
          "model_call_count":"1"
        }
        """,
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"\(cursor)",
          "turn_count":"1",
          "entry_count":"0"
        }
        """,
      ]
    )
  }

  static func snapshotWithUnknownEntryKind(
    _ kind: String
  ) throws -> SignalboxSynchronizationSnapshot {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
        emptyModelCallsBoundary,
        """
        {
          "type":"transcript_entry",
          "entry_index":"0",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(proposedToolEntry)",
          "entry":{"type":"\(kind)"}
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

  static func snapshotWithUnknownTextEntryKind() throws -> SignalboxSynchronizationSnapshot {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
        emptyModelCallsBoundary,
        """
        {
          "type":"transcript_text_entry",
          "entry_index":"0",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(proposedToolEntry)",
          "entry":{"type":"\(unknownTextEntryKind)"}
        }
        """,
        """
        {
          "type":"transcript_content",
          "entry_index":"0",
          "fragment_index":"0",
          "final_fragment":true,
          "content_fragment":"\(unknownTextEntryContent)"
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

  static func snapshotWithImportedTextSpeaker(
    _ sourceSpeaker: String
  ) throws -> SignalboxSynchronizationSnapshot {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
        emptyModelCallsBoundary,
        """
        {
          "type":"transcript_text_entry",
          "entry_index":"0",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(proposedToolEntry)",
          "entry":{
            "type":"imported",
            "imported_conversation_id":"\(ProcessDriverFixture.session)",
            "imported_entry_id":"\(completedUserEntry)",
            "source_speaker":\(sourceSpeaker)
          }
        }
        """,
        """
        {
          "type":"transcript_content",
          "entry_index":"0",
          "fragment_index":"0",
          "final_fragment":true,
          "content_fragment":"\(unknownTextEntryContent)"
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

  static func snapshotWithImportedContentKind(
    _ kind: String
  ) throws -> SignalboxSynchronizationSnapshot {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
        emptyModelCallsBoundary,
        """
        {
          "type":"transcript_entry",
          "entry_index":"0",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(proposedToolEntry)",
          "entry":{
            "type":"imported",
            "imported_conversation_id":"\(ProcessDriverFixture.session)",
            "imported_entry_id":"\(completedUserEntry)",
            "source_speaker":{"type":"not_attested"},
            "content_kind":"\(kind)"
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

  static func snapshotWithCompletedModelIdentityMarker() throws
    -> SignalboxSynchronizationSnapshot
  {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(crossTurn)",
          "acceptance_position":"1",
          "state":{
            "type":"queued",
            "accepted_input_id":"\(firstPendingID)",
            "content":[{"type":"text","text":"\(userText)"}]
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
        emptyModelCallsBoundary,
        """
        {
          "type":"transcript_user_entry",
          "entry_index":"0",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(reconciliationResultEntry)",
          "accepted_input_id":"\(firstPendingID)",
          "turn_id":"\(crossTurn)",
          "content":[{"type":"text","text":"\(userText)"}]
        }
        """,
        """
        {
          "type":"transcript_entry",
          "entry_index":"1",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(modelIdentityEntry)",
          "entry":{
            "type":"model_identity_changed",
            "turn_id":"\(ProcessDriverFixture.turn)",
            "defaults_version":"\(activationModelIdentityDefaultsVersion)",
            "selected_model_id":"\(ProcessDriverFixture.modelCall)"
          }
        }
        """,
        """
        {
          "type":"transcript_user_entry",
          "entry_index":"2",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(completedUserEntry)",
          "accepted_input_id":"\(ProcessSubmissionFixture.acceptedInputID)",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "content":[{"type":"text","text":"\(userText)"}]
        }
        """,
        """
        {
          "type":"transcript_text_entry",
          "entry_index":"3",
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
          "entry_index":"3",
          "fragment_index":"0",
          "final_fragment":true,
          "content_fragment":"\(completedAssistantText)"
        }
        """,
        """
        {
          "type":"transcript_entry",
          "entry_index":"4",
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
          "type":"transcript_entry",
          "entry_index":"5",
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
          "turn_count":"2",
          "entry_count":"6"
        }
        """,
      ]
    )
  }

  private static func snapshotWithActiveTurnState(
    _ state: String,
    cursor: UInt64 = 1
  ) throws -> SignalboxSynchronizationSnapshot {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"\(cursor)",
          "runner":null
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "acceptance_position":"1",
          "state":\(state)
        }
        """,
        emptyModelCallsBoundary,
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"\(cursor)",
          "turn_count":"1",
          "entry_count":"0"
        }
        """,
      ]
    )
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
            "cursor":"1",
            "runner":null
          }
          """
        )
      )
    )
    _ = machine.receive(
      .frame(
        generation: 1,
        message: try message(emptyModelCallsBoundary)
      )
    )
    _ = machine.receive(
      .frame(
        generation: 1,
        message: try message(
          """
          {
            "type":"transcript_user_entry",
            "entry_index":"0",
            "source_session_id":"\(ProcessDriverFixture.session)",
            "entry_id":"\(ProcessDriverFixture.completionEntry)",
            "accepted_input_id":"\(ProcessSubmissionFixture.acceptedInputID)",
            "turn_id":"\(ProcessDriverFixture.turn)",
            "content":[{"type":"text","text":"\(userText)"}]
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
    try snapshotWithProposedTool(turnEvidence: [], usageEvidence: [], approvalMember: "")
  }

  static func snapshotWithProposedTool(
    approvalMember: String
  ) throws -> SignalboxSynchronizationSnapshot {
    try snapshotWithProposedTool(
      turnEvidence: [],
      usageEvidence: [],
      approvalMember: approvalMember
    )
  }

  static func snapshotWithContiguousToolResultAndFailure() throws
    -> SignalboxSynchronizationSnapshot
  {
    try snapshotWithToolResultAndFailure(
      interveningEntries: [],
      failureEntryIndex: 3
    )
  }

  static func snapshotWithSeparatedToolResultAndFailure() throws
    -> SignalboxSynchronizationSnapshot
  {
    try snapshotWithToolResultAndFailure(
      interveningEntries: [
        importedMarker(
          index: 3,
          entryID: importedThinkingEntry,
          speaker: importedAssistantSpeaker,
          kind: "thinking"
        )
      ],
      failureEntryIndex: 4
    )
  }

  static func snapshotWithImportedFailureMarkerCollision() throws
    -> SignalboxSynchronizationSnapshot
  {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
        emptyModelCallsBoundary,
        importedMarker(
          index: 0,
          entryID: ProcessDriverFixture.completionEntry,
          sourceSessionID: crossTurn,
          speaker: importedAssistantSpeaker,
          kind: "thinking"
        ),
        """
        {
          "type":"transcript_entry",
          "entry_index":"1",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(ProcessDriverFixture.completionEntry)",
          "entry":{
            "type":"turn_failed",
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

  private static func snapshotWithToolResultAndFailure(
    interveningEntries: [String],
    failureEntryIndex: UInt64
  ) throws -> SignalboxSynchronizationSnapshot {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
        emptyModelCallsBoundary,
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
          "type":"transcript_entry",
          "entry_index":"2",
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
      ] + interveningEntries + [
        """
        {
          "type":"transcript_entry",
          "entry_index":"\(failureEntryIndex)",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(ProcessDriverFixture.completionEntry)",
          "entry":{
            "type":"turn_failed",
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
          "entry_count":"\(interveningEntries.count + 4)"
        }
        """,
      ]
    )
  }

  static func snapshotWithProposedToolAndUnrelatedUsage() throws
    -> SignalboxSynchronizationSnapshot
  {
    try snapshotWithProposedTool(
      turnEvidence: [
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "acceptance_position":"1",
          "state":{
            "type":"completed",
            "terminal_frontier_id":"\(ProcessDriverFixture.frontier)",
            "terminal_attempt_id":"\(ProcessDriverFixture.attempt)",
            "terminal_model_call_id":"\(ProcessDriverFixture.modelCall)"
          }
        }
        """,
      ],
      usageEvidence: [
        modelUsageMessage(index: 0, modelCallID: earlierModelCall),
        modelUsageMessage(index: 1, modelCallID: ProcessDriverFixture.modelCall),
      ]
    )
  }

  static func snapshotWithProjectedResultAndLaterUsage() throws
    -> SignalboxSynchronizationSnapshot
  {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"2",
          "runner":null
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "acceptance_position":"1",
          "state":{
            "type":"completed",
            "terminal_frontier_id":"\(ProcessDriverFixture.frontier)",
            "terminal_attempt_id":"\(ProcessDriverFixture.attempt)",
            "terminal_model_call_id":"\(ProcessDriverFixture.modelCall)"
          }
        }
        """,
        modelUsageMessage(index: 0, modelCallID: ProcessDriverFixture.modelCall),
        """
        {
          "type":"transcript_model_calls_end",
          "model_call_count":"1"
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
          "cursor":"2",
          "turn_count":"1",
          "entry_count":"2"
        }
        """,
      ]
    )
  }

  private static func snapshotWithProposedTool(
    turnEvidence: [String],
    usageEvidence: [String],
    approvalMember: String = ""
  ) throws -> SignalboxSynchronizationSnapshot {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
      ] + turnEvidence + usageEvidence + [
        """
        {
          "type":"transcript_model_calls_end",
          "model_call_count":"\(usageEvidence.count)"
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
            "arguments":"{}"\(approvalMember)
          }
        }
        """,
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "turn_count":"\(turnEvidence.count)",
          "entry_count":"2"
        }
        """,
      ]
    )
  }

  static func snapshotWithDelegateDenial() throws -> SignalboxSynchronizationSnapshot {
    try snapshotWithProposedTool(
      approvalMember:
        """
        ,"approval":{
          "decision":{"type":"deny","reason":null},
          "decider":{
            "type":"delegate",
            "model_selection_id":"\(delegateModelSelection)",
            "model_call_id":"\(delegateModelCall)"
          },
          "rationale":"\(delegateRationale)"
        }
        """
    )
  }

  static func snapshotWithContextSummary() throws -> SignalboxSynchronizationSnapshot {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "acceptance_position":"1",
          "state":{
            "type":"completed",
            "terminal_frontier_id":"\(ProcessDriverFixture.frontier)",
            "terminal_attempt_id":"\(ProcessDriverFixture.attempt)",
            "terminal_model_call_id":"\(ProcessDriverFixture.modelCall)"
          }
        }
        """,
        modelUsageMessage(index: 0, modelCallID: ProcessDriverFixture.modelCall),
        """
        {
          "type":"transcript_model_calls_end",
          "model_call_count":"1"
        }
        """,
        """
        {
          "type":"transcript_text_entry",
          "entry_index":"0",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(contextSummaryEntry)",
          "entry":{
            "type":"context_summary",
            "model_call_id":"\(ProcessDriverFixture.modelCall)",
            "first_source_session_id":"\(ProcessDriverFixture.session)",
            "first_entry_id":"\(completedUserEntry)",
            "through_source_session_id":"\(ProcessDriverFixture.session)",
            "through_entry_id":"\(completedAssistantEntry)"
          }
        }
        """,
        """
        {
          "type":"transcript_content",
          "entry_index":"0",
          "fragment_index":"0",
          "final_fragment":true,
          "content_fragment":"\(contextSummaryText)"
        }
        """,
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "turn_count":"1",
          "entry_count":"1"
        }
        """,
      ]
    )
  }

  static func snapshotWithToolBeforeCompaction() throws -> SignalboxSynchronizationSnapshot {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
        emptyModelCallsBoundary,
        importedMarker(
          index: 0,
          entryID: importedSourceEventEntry,
          speaker: importedAssistantSpeaker,
          kind: "source_event"
        ),
        importedMarker(
          index: 1,
          entryID: importedThinkingEntry,
          speaker: importedAssistantSpeaker,
          kind: "thinking"
        ),
        importedMarker(
          index: 2,
          entryID: importedToolCallEntry,
          speaker: importedAssistantSpeaker,
          kind: "tool_call"
        ),
        importedMarker(
          index: 3,
          entryID: importedToolResultEntry,
          speaker: importedAssistantSpeaker,
          kind: "tool_result"
        ),
        toolRequestMessage(index: 4),
        toolResultMessage(index: 5),
        importedMarker(
          index: 6,
          entryID: importedFutureEntry,
          speaker: importedAssistantSpeaker,
          kind: futureImportedContentKind
        ),
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "turn_count":"0",
          "entry_count":"7"
        }
        """,
      ]
    )
  }

  static func snapshotWithToolAfterCompaction() throws -> SignalboxSynchronizationSnapshot {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"2",
          "runner":null
        }
        """,
        emptyModelCallsBoundary,
        """
        {
          "type":"transcript_text_entry",
          "entry_index":"0",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(contextSummaryEntry)",
          "entry":{
            "type":"context_summary",
            "model_call_id":"\(ProcessDriverFixture.modelCall)",
            "first_source_session_id":"\(ProcessDriverFixture.session)",
            "first_entry_id":"\(importedSourceEventEntry)",
            "through_source_session_id":"\(ProcessDriverFixture.session)",
            "through_entry_id":"\(importedToolResultEntry)"
          }
        }
        """,
        """
        {
          "type":"transcript_content",
          "entry_index":"0",
          "fragment_index":"0",
          "final_fragment":true,
          "content_fragment":"\(contextSummaryText)"
        }
        """,
        toolRequestMessage(index: 1),
        toolResultMessage(index: 2),
        importedMarker(
          index: 3,
          entryID: importedFutureEntry,
          speaker: importedAssistantSpeaker,
          kind: futureImportedContentKind
        ),
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"2",
          "turn_count":"0",
          "entry_count":"4"
        }
        """,
      ]
    )
  }

  static func snapshotWithSourceQualifiedReusedToolRequest() throws
    -> SignalboxSynchronizationSnapshot
  {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
        emptyModelCallsBoundary,
        toolRequestMessage(index: 0),
        toolResultMessage(index: 1),
        """
        {
          "type":"transcript_entry",
          "entry_index":"2",
          "source_session_id":"\(reusedToolSourceSession)",
          "entry_id":"\(reusedToolEntry)",
          "entry":{
            "type":"assistant_tool_use",
            "turn_id":"\(ProcessDriverFixture.turn)",
            "model_call_id":"\(ProcessDriverFixture.modelCall)",
            "tool_request_id":"\(proposedToolRequest)",
            "tool_name":"\(reusedToolName)",
            "arguments":"{}"
          }
        }
        """,
        """
        {
          "type":"transcript_entry",
          "entry_index":"3",
          "source_session_id":"\(reusedToolSourceSession)",
          "entry_id":"\(reusedToolResultEntry)",
          "entry":{
            "type":"tool_execution_result",
            "tool_request_id":"\(proposedToolRequest)",
            "tool_attempt_id":"\(reusedToolAttempt)",
            "content":"\(reusedToolOutput)"
          }
        }
        """,
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "turn_count":"0",
          "entry_count":"4"
        }
        """,
      ]
    )
  }

  private static func toolRequestMessage(index: UInt64) -> String {
    """
    {
      "type":"transcript_entry",
      "entry_index":"\(index)",
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
    """
  }

  private static func toolResultMessage(index: UInt64) -> String {
    """
    {
      "type":"transcript_entry",
      "entry_index":"\(index)",
      "source_session_id":"\(ProcessDriverFixture.session)",
      "entry_id":"\(reconciliationResultEntry)",
      "entry":{
        "type":"tool_execution_result",
        "tool_request_id":"\(proposedToolRequest)",
        "tool_attempt_id":"\(reconciliationAttempt)",
        "content":"\(reconciliationOutput)"
      }
    }
    """
  }

  static func snapshotWithImportedMarkers() throws -> SignalboxSynchronizationSnapshot {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
        emptyModelCallsBoundary,
        importedMarker(
          index: 0,
          entryID: importedSourceEventEntry,
          speaker: #"{"type":"not_attested"}"#,
          kind: "source_event"
        ),
        importedMarker(
          index: 1,
          entryID: importedThinkingEntry,
          speaker: #"{"type":"attested","speaker":"assistant"}"#,
          kind: "thinking"
        ),
        importedMarker(
          index: 2,
          entryID: importedToolCallEntry,
          speaker: #"{"type":"attested","speaker":"assistant"}"#,
          kind: "tool_call"
        ),
        importedMarker(
          index: 3,
          entryID: importedToolResultEntry,
          speaker: #"{"type":"attested","speaker":"user"}"#,
          kind: "tool_result"
        ),
        importedMarker(
          index: 4,
          entryID: importedFutureEntry,
          speaker: #"{"type":"not_attested"}"#,
          kind: futureImportedContentKind
        ),
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "turn_count":"0",
          "entry_count":"5"
        }
        """,
      ]
    )
  }

  static func snapshotWithModelPresentationEvidence() throws
    -> SignalboxSynchronizationSnapshot
  {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(crossTurn)",
          "acceptance_position":"1",
          "state":{
            "type":"cancelled",
            "terminal_frontier_id":"\(ProcessDriverFixture.frontier)",
            "terminal_attempt_id":"\(ProcessDriverFixture.attempt)",
            "terminal_model_call_id":null
          }
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "acceptance_position":"2",
          "state":{
            "type":"completed",
            "terminal_frontier_id":"\(ProcessDriverFixture.frontier)",
            "terminal_attempt_id":"\(ProcessDriverFixture.attempt)",
            "terminal_model_call_id":"\(ProcessDriverFixture.modelCall)"
          }
        }
        """,
        """
        {
          "type":"transcript_model_call_usage",
          "model_call_index":"0",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "model_call_id":"\(ProcessDriverFixture.modelCall)",
          "usage_provenance":"\(modelUsageProvenance)",
          "usage":{
            "input_tokens":"\(modelInputTokens)",
            "output_tokens":"\(modelOutputTokens)",
            "cache_creation_input_tokens":"\(modelCacheCreationTokens)",
            "cache_read_input_tokens":null
          },
          "cost":{
            "amount_usd":"\(modelCostAmountUSD)",
            "rate_version":"\(modelCostRateVersion)",
            "label":"\(modelCostLabel)"
          }
        }
        """,
        """
        {
          "type":"transcript_model_calls_end",
          "model_call_count":"1"
        }
        """,
        """
        {
          "type":"transcript_entry",
          "entry_index":"0",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(proposedAssistantEntry)",
          "entry":{
            "type":"model_identity_changed",
            "turn_id":"\(ProcessDriverFixture.turn)",
            "defaults_version":"\(modelIdentityDefaultsVersion)",
            "selected_model_id":"\(ProcessDriverFixture.modelCall)"
          }
        }
        """,
        """
        {
          "type":"transcript_user_entry",
          "entry_index":"1",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(completedUserEntry)",
          "accepted_input_id":"\(ProcessSubmissionFixture.acceptedInputID)",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "content":[{"type":"text","text":"\(userText)"}]
        }
        """,
        """
        {
          "type":"transcript_text_entry",
          "entry_index":"2",
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
          "entry_index":"2",
          "fragment_index":"0",
          "final_fragment":true,
          "content_fragment":"\(completedAssistantText)"
        }
        """,
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "turn_count":"2",
          "entry_count":"3"
        }
        """,
      ]
    )
  }

  static func snapshotWithLaterModelUsageOnly() throws
    -> SignalboxSynchronizationSnapshot
  {
    try modelUsageSnapshot(
      evidence: [modelUsageMessage(index: 0, modelCallID: ProcessDriverFixture.modelCall)]
    )
  }

  static func snapshotWithEarlierModelUsageOnly() throws
    -> SignalboxSynchronizationSnapshot
  {
    try modelUsageSnapshot(
      evidence: [modelUsageMessage(index: 0, modelCallID: earlierModelCall)],
      terminalModelCallID: earlierModelCall
    )
  }

  static func snapshotWithEarlierModelUsageInserted() throws
    -> SignalboxSynchronizationSnapshot
  {
    try modelUsageSnapshot(
      evidence: [
        modelUsageMessage(index: 0, modelCallID: earlierModelCall),
        modelUsageMessage(index: 1, modelCallID: ProcessDriverFixture.modelCall),
      ]
    )
  }

  static func snapshotBeforeUsageAnchorReuse() throws
    -> SignalboxSynchronizationSnapshot
  {
    try anchoredUsageSnapshot(
      evidence: [
        modelUsageMessage(index: 0, modelCallID: ProcessDriverFixture.modelCall)
      ],
      transcriptEntries: assistantTextMessages(
        index: 0,
        entryID: completedAssistantEntry,
        modelCallID: ProcessDriverFixture.modelCall,
        text: completedAssistantText
      ),
      entryCount: 1
    )
  }

  static func snapshotAfterUsageAnchorReuse() throws
    -> SignalboxSynchronizationSnapshot
  {
    try anchoredUsageSnapshot(
      evidence: [
        modelUsageMessage(index: 0, modelCallID: earlierModelCall),
        modelUsageMessage(index: 1, modelCallID: ProcessDriverFixture.modelCall),
      ],
      transcriptEntries: assistantTextMessages(
        index: 0,
        entryID: proposedAssistantEntry,
        modelCallID: earlierModelCall,
        text: proposedAssistantText
      ) + assistantTextMessages(
        index: 1,
        entryID: completedAssistantEntry,
        modelCallID: ProcessDriverFixture.modelCall,
        text: completedAssistantText
      ),
      entryCount: 2
    )
  }

  static func snapshotWithImportedModelCallAnchorCollision() throws
    -> SignalboxSynchronizationSnapshot
  {
    try anchoredUsageSnapshot(
      evidence: [
        modelUsageMessage(index: 0, modelCallID: ProcessDriverFixture.modelCall)
      ],
      transcriptEntries: assistantTextMessages(
        index: 0,
        entryID: completedAssistantEntry,
        modelCallID: ProcessDriverFixture.modelCall,
        text: completedAssistantText
      ) + [
        """
        {
          "type":"transcript_entry",
          "entry_index":"1",
          "source_session_id":"\(reusedToolSourceSession)",
          "entry_id":"\(reusedToolEntry)",
          "entry":{
            "type":"assistant_tool_use",
            "turn_id":"\(crossTurn)",
            "model_call_id":"\(ProcessDriverFixture.modelCall)",
            "tool_request_id":"\(proposedToolRequest)",
            "tool_name":"\(reusedToolName)",
            "arguments":"{}"
          }
        }
        """
      ],
      entryCount: 2
    )
  }

  private static func anchoredUsageSnapshot(
    evidence: [String],
    transcriptEntries: [String],
    entryCount: UInt64
  ) throws -> SignalboxSynchronizationSnapshot {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "acceptance_position":"1",
          "state":{
            "type":"completed",
            "terminal_frontier_id":"\(ProcessDriverFixture.frontier)",
            "terminal_attempt_id":"\(ProcessDriverFixture.attempt)",
            "terminal_model_call_id":"\(ProcessDriverFixture.modelCall)"
          }
        }
        """,
      ] + evidence + [
        """
        {
          "type":"transcript_model_calls_end",
          "model_call_count":"\(evidence.count)"
        }
        """
      ] + transcriptEntries + [
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "turn_count":"1",
          "entry_count":"\(entryCount)"
        }
        """
      ]
    )
  }

  private static func assistantTextMessages(
    index: UInt64,
    entryID: String,
    modelCallID: String,
    text: String
  ) -> [String] {
    [
      """
      {
        "type":"transcript_text_entry",
        "entry_index":"\(index)",
        "source_session_id":"\(ProcessDriverFixture.session)",
        "entry_id":"\(entryID)",
        "entry":{
          "type":"assistant",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "model_call_id":"\(modelCallID)"
        }
      }
      """,
      """
      {
        "type":"transcript_content",
        "entry_index":"\(index)",
        "fragment_index":"0",
        "final_fragment":true,
        "content_fragment":"\(text)"
      }
      """,
    ]
  }

  static func snapshotWithAnchoredUsageAndLaterMessage() throws
    -> SignalboxSynchronizationSnapshot
  {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "acceptance_position":"1",
          "state":{
            "type":"completed",
            "terminal_frontier_id":"\(ProcessDriverFixture.frontier)",
            "terminal_attempt_id":"\(ProcessDriverFixture.attempt)",
            "terminal_model_call_id":"\(ProcessDriverFixture.modelCall)"
          }
        }
        """,
        modelUsageMessage(index: 0, modelCallID: ProcessDriverFixture.modelCall),
        """
        {
          "type":"transcript_model_calls_end",
          "model_call_count":"1"
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
          "type":"transcript_user_entry",
          "entry_index":"1",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(completedUserEntry)",
          "accepted_input_id":"\(ProcessSubmissionFixture.acceptedInputID)",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "content":[{"type":"text","text":"\(userText)"}]
        }
        """,
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "turn_count":"1",
          "entry_count":"2"
        }
        """,
      ]
    )
  }

  static func snapshotWithAdjacentUsageAndUnknownTurnState() throws
    -> SignalboxSynchronizationSnapshot
  {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "acceptance_position":"1",
          "state":{
            "type":"completed",
            "terminal_frontier_id":"\(ProcessDriverFixture.frontier)",
            "terminal_attempt_id":"\(ProcessDriverFixture.attempt)",
            "terminal_model_call_id":"\(ProcessDriverFixture.modelCall)"
          }
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(crossTurn)",
          "acceptance_position":"2",
          "state":{"type":"\(futureTurnStateKind)","retained":"fixture"}
        }
        """,
        modelUsageMessage(index: 0, modelCallID: ProcessDriverFixture.modelCall),
        """
        {
          "type":"transcript_model_calls_end",
          "model_call_count":"1"
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
          "type":"transcript_user_entry",
          "entry_index":"1",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(completedUserEntry)",
          "accepted_input_id":"\(ProcessSubmissionFixture.acceptedInputID)",
          "turn_id":"\(crossTurn)",
          "content":[{"type":"text","text":"\(userText)"}]
        }
        """,
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "turn_count":"2",
          "entry_count":"2"
        }
        """,
      ]
    )
  }

  static func snapshotWithCrossTurnModelUsageAnchor() throws
    -> SignalboxSynchronizationSnapshot
  {
    let snapshot = try snapshotWithAnchoredUsageAndLaterMessage()
    let usageMessage = try message(
      modelUsageMessage(
        index: 0,
        modelCallID: ProcessDriverFixture.modelCall,
        turnID: crossTurn
      )
    )
    guard case .transcriptModelCallUsage(let usage) = usageMessage else {
      throw ProcessDriverUpdateRecorderError.missingFixtureEvent
    }
    return SignalboxSynchronizationSnapshot(
      sessionID: snapshot.sessionID,
      cursor: snapshot.cursor,
      records: snapshot.records.compactMap { record in
        guard case .modelCallUsage = record else {
          return record
        }
        return nil
      } + [.modelCallUsage(usage)]
    )
  }

  static func snapshotWithFailedUsageAndMarker() throws
    -> SignalboxSynchronizationSnapshot
  {
    try snapshotWithUsageMarker(
      turnState: failedUsageTurnState,
      trailingEntries: []
    )
  }

  static func snapshotWithFailedUsageMarkerAndLaterImportedContent() throws
    -> SignalboxSynchronizationSnapshot
  {
    try snapshotWithUsageMarker(
      turnState: failedUsageTurnState,
      trailingEntries: [
        importedMarker(
          index: 2,
          entryID: importedThinkingEntry,
          speaker: importedAssistantSpeaker,
          kind: "thinking"
        )
      ]
    )
  }

  static func snapshotWithRecoveryUsageMarkerAndLaterImportedContent() throws
    -> SignalboxSynchronizationSnapshot
  {
    try snapshotWithUsageMarker(
      turnState: recoveryUsageTurnState,
      trailingEntries: [
        importedMarker(
          index: 2,
          entryID: importedThinkingEntry,
          speaker: importedAssistantSpeaker,
          kind: "thinking"
        )
      ]
    )
  }

  private static let failedUsageTurnState =
    """
    {
      "type":"failed",
      "terminal_frontier_id":"\(ProcessDriverFixture.frontier)",
      "terminal_attempt_id":"\(ProcessDriverFixture.attempt)",
      "terminal_model_call":{
        "model_call_id":"\(ProcessDriverFixture.modelCall)",
        "disposition":"known_failed"
      }
    }
    """

  private static let recoveryUsageTurnState =
    """
    {
      "type":"active_awaiting_model_call_recovery",
      "ended_attempt_id":"\(ProcessDriverFixture.attempt)",
      "recovery_model_call_id":"\(ProcessDriverFixture.modelCall)"
    }
    """

  private static func snapshotWithUsageMarker(
    turnState: String,
    trailingEntries: [String]
  ) throws -> SignalboxSynchronizationSnapshot {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "acceptance_position":"1",
          "state":\(turnState)
        }
        """,
        modelUsageMessage(index: 0, modelCallID: ProcessDriverFixture.modelCall),
        """
        {
          "type":"transcript_model_calls_end",
          "model_call_count":"1"
        }
        """,
        importedMarker(
          index: 0,
          entryID: importedSourceEventEntry,
          speaker: #"{"type":"not_attested"}"#,
          kind: "source_event"
        ),
        """
        {
          "type":"transcript_entry",
          "entry_index":"1",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(completedAssistantEntry)",
          "entry":{
            "type":"turn_failed",
            "turn_id":"\(ProcessDriverFixture.turn)"
          }
        }
        """,
      ] + trailingEntries + [
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "turn_count":"1",
          "entry_count":"\(2 + trailingEntries.count)"
        }
        """,
      ]
    )
  }

  static func snapshotWithCompletedUsageAndMarker() throws
    -> SignalboxSynchronizationSnapshot
  {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "acceptance_position":"1",
          "state":{
            "type":"completed",
            "terminal_frontier_id":"\(ProcessDriverFixture.frontier)",
            "terminal_attempt_id":"\(ProcessDriverFixture.attempt)",
            "terminal_model_call_id":"\(ProcessDriverFixture.modelCall)"
          }
        }
        """,
        modelUsageMessage(index: 0, modelCallID: ProcessDriverFixture.modelCall),
        """
        {
          "type":"transcript_model_calls_end",
          "model_call_count":"1"
        }
        """,
        importedMarker(
          index: 0,
          entryID: importedSourceEventEntry,
          speaker: #"{"type":"not_attested"}"#,
          kind: "source_event"
        ),
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
          "turn_count":"1",
          "entry_count":"2"
        }
        """,
      ]
    )
  }

  static func snapshotWithRefusedUsageAfterImportedMarker() throws
    -> SignalboxSynchronizationSnapshot
  {
    try snapshotWithTrailingUsage(
      turnState: """
        {
          "type":"refused",
          "terminal_frontier_id":"\(ProcessDriverFixture.frontier)",
          "terminal_attempt_id":"\(ProcessDriverFixture.attempt)",
          "terminal_model_call_id":"\(ProcessDriverFixture.modelCall)"
        }
        """
    )
  }

  static func snapshotWithReconciliationUsageAfterImportedMarker() throws
    -> SignalboxSynchronizationSnapshot
  {
    try snapshotWithTrailingUsage(
      turnState: """
        {
          "type":"reconciliation_required",
          "terminal_frontier_id":"\(ProcessDriverFixture.frontier)",
          "terminal_attempt_id":"\(ProcessDriverFixture.attempt)",
          "terminal_model_call_id":"\(ProcessDriverFixture.modelCall)"
        }
        """
    )
  }

  static func snapshotWithRecoveryUsageAfterImportedMarker() throws
    -> SignalboxSynchronizationSnapshot
  {
    try snapshotWithTrailingUsage(
      turnState: """
        {
          "type":"active_awaiting_model_call_recovery",
          "ended_attempt_id":"\(ProcessDriverFixture.attempt)",
          "recovery_model_call_id":"\(ProcessDriverFixture.modelCall)"
        }
        """
    )
  }

  private static func snapshotWithTrailingUsage(
    turnState: String
  ) throws -> SignalboxSynchronizationSnapshot {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "acceptance_position":"1",
          "state":\(turnState)
        }
        """,
        modelUsageMessage(index: 0, modelCallID: ProcessDriverFixture.modelCall),
        """
        {
          "type":"transcript_model_calls_end",
          "model_call_count":"1"
        }
        """,
        importedMarker(
          index: 0,
          entryID: importedSourceEventEntry,
          speaker: #"{"type":"not_attested"}"#,
          kind: "source_event"
        ),
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "turn_count":"1",
          "entry_count":"1"
        }
        """,
      ]
    )
  }

  private static func modelUsageSnapshot(
    evidence: [String],
    terminalModelCallID: String = ProcessDriverFixture.modelCall
  ) throws -> SignalboxSynchronizationSnapshot {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "acceptance_position":"1",
          "state":{
            "type":"completed",
            "terminal_frontier_id":"\(ProcessDriverFixture.frontier)",
            "terminal_attempt_id":"\(ProcessDriverFixture.attempt)",
            "terminal_model_call_id":"\(terminalModelCallID)"
          }
        }
        """,
      ] + evidence + [
        """
        {
          "type":"transcript_model_calls_end",
          "model_call_count":"\(evidence.count)"
        }
        """,
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "turn_count":"1",
          "entry_count":"0"
        }
        """,
      ]
    )
  }

  private static func modelUsageMessage(
    index: UInt64,
    modelCallID: String,
    turnID: String = ProcessDriverFixture.turn
  ) -> String {
    """
    {
      "type":"transcript_model_call_usage",
      "model_call_index":"\(index)",
      "turn_id":"\(turnID)",
      "model_call_id":"\(modelCallID)",
      "usage_provenance":"\(modelUsageProvenance)",
      "usage":{
        "input_tokens":"\(modelInputTokens)",
        "output_tokens":"\(modelOutputTokens)",
        "cache_creation_input_tokens":"\(modelCacheCreationTokens)",
        "cache_read_input_tokens":null
      },
      "cost":null
    }
    """
  }

  static func unknownFollowedEvent() throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"\(futureFollowedEventKind)",
        "retained":"fixture"
      }
      """
    )
  }

  static func unknownModelCallFollowedEvent() throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"model_call_transition",
        "turn_id":"\(ProcessDriverFixture.turn)",
        "model_call_id":"\(ProcessDriverFixture.modelCall)",
        "state":{"type":"\(futureCurrentModelStateKind)","retained":"fixture"}
      }
      """
    )
  }

  static func snapshotWithUnknownTurnStates() throws -> SignalboxSynchronizationSnapshot {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "acceptance_position":"1",
          "state":{"type":"\(futureTurnStateKind)","retained":"fixture"}
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(crossTurn)",
          "acceptance_position":"2",
          "state":{
            "type":"active_running",
            "current_attempt_id":"\(ProcessDriverFixture.attempt)",
            "current_model_call":{
              "model_call_id":"\(ProcessDriverFixture.modelCall)",
              "state":{"type":"\(futureCurrentModelStateKind)","retained":"fixture"}
            }
          }
        }
        """,
        emptyModelCallsBoundary,
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "turn_count":"2",
          "entry_count":"0"
        }
        """,
      ]
    )
  }

  static func snapshotWithTranscriptBeforeUnknownTurnState() throws
    -> SignalboxSynchronizationSnapshot
  {
    let transcript = try snapshotWithCompletedTurnEntries()
    let unknown = try snapshotWithUnknownTurnState()
    guard case .turn(let unknownTurn)? = unknown.records.first else {
      throw ProcessDriverUpdateRecorderError.missingFixtureEvent
    }
    return SignalboxSynchronizationSnapshot(
      sessionID: transcript.sessionID,
      cursor: transcript.cursor,
      records: [.turn(unknownTurn)] + transcript.records
    )
  }

  static func snapshotWithUnknownTurnState() throws -> SignalboxSynchronizationSnapshot {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "acceptance_position":"1",
          "state":{"type":"\(futureTurnStateKind)","retained":"fixture"}
        }
        """,
        emptyModelCallsBoundary,
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "turn_count":"1",
          "entry_count":"0"
        }
        """,
      ]
    )
  }

  static func snapshotWithCompactedAndNewUnknownTurn() throws
    -> SignalboxSynchronizationSnapshot
  {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"2",
          "runner":null
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "acceptance_position":"1",
          "state":{"type":"\(futureTurnStateKind)","retained":"fixture"}
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(crossTurn)",
          "acceptance_position":"2",
          "state":{"type":"\(futureTurnStateKind)","retained":"fixture"}
        }
        """,
        emptyModelCallsBoundary,
        """
        {
          "type":"transcript_user_entry",
          "entry_index":"0",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(completedUserEntry)",
          "accepted_input_id":"\(firstPendingID)",
          "turn_id":"\(crossTurn)",
          "content":[{"type":"text","text":"\(userText)"}]
        }
        """,
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"2",
          "turn_count":"2",
          "entry_count":"1"
        }
        """,
      ]
    )
  }

  static func planReadToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planReadRequestID,
      toolName: "plan_read",
      arguments: planReadWithoutHistoryArguments,
      output: planReadWithoutHistoryOutput,
      status: .completed
    )
  }

  static func planReadToolRecord(output: String) -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planReadRequestID,
      toolName: "plan_read",
      arguments: planReadArguments,
      output: output,
      status: .completed
    )
  }

  static func floatingPlanReadIdentityToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planReadRequestID,
      toolName: "plan_read",
      arguments: floatingPlanReadArguments,
      output: nil,
      status: .proposed
    )
  }

  static func exponentPlanOrdinalToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planCreateRequestID,
      toolName: "plan_write",
      arguments: planCreateArguments,
      output: exponentPlanOrdinalOutput,
      status: .completed
    )
  }

  static func closedPlanReadToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planReadRequestID,
      toolName: "plan_read",
      arguments: planReadArguments,
      output: planReadOutput,
      status: .closed
    )
  }

  static func planCreateToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planCreateRequestID,
      toolName: "plan_write",
      arguments: planCreateArguments,
      output: planCreateOutput,
      status: .completed
    )
  }

  static func planCreateWithMismatchedRequestProvenanceToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planReviseRequestID,
      toolName: "plan_write",
      arguments: planCreateArguments,
      output: planCreateOutput,
      status: .completed
    )
  }

  static func planCreateWithMismatchedTurnProvenanceToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planCreateRequestID,
      turnID: crossTurn,
      toolName: "plan_write",
      arguments: planCreateArguments,
      output: planCreateOutput,
      status: .completed
    )
  }

  static func planCreateWithMismatchedAttemptProvenanceToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planCreateRequestID,
      toolAttemptID: planIssuingAttemptID,
      toolName: "plan_write",
      arguments: planCreateArguments,
      output: planCreateOutput,
      status: .completed
    )
  }

  static func planCreateWithMismatchedIssuingAttemptToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planCreateRequestID,
      toolName: "plan_write",
      arguments: planCreateArguments,
      output: mismatchedIssuingAttemptPlanCreateOutput,
      status: .completed
    )
  }

  static func deniedPlanCreateToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planCreateRequestID,
      toolName: "plan_write",
      arguments: planCreateArguments,
      output: planCreateOutput,
      status: .denied
    )
  }

  static func planReviseToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planReviseRequestID,
      toolName: "plan_write",
      arguments: planReviseArguments,
      output: planReviseOutput,
      status: .completed
    )
  }

  static func planStatusToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planStatusRequestID,
      toolName: "plan_write",
      arguments: planStatusArguments,
      output: planStatusOutput,
      status: .completed
    )
  }

  static func planDependencyToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planWriteRequestID,
      toolName: "plan_write",
      arguments: planWriteArguments,
      output: planWriteOutput,
      status: .completed
    )
  }

  static func malformedPlanWriteToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: malformedPlanRequestID,
      toolName: "plan_write",
      arguments: malformedPlanArguments,
      output: nil,
      status: .proposed
    )
  }

  static func malformedPlanReadToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: malformedPlanReadRequestID,
      toolName: "plan_read",
      arguments: malformedPlanReadArguments,
      output: nil,
      status: .proposed
    )
  }

  static func planOutputWithoutProvenanceToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planCreateRequestID,
      toolName: "plan_write",
      arguments: planCreateArguments,
      output: planOutputWithoutProvenance,
      status: .completed
    )
  }

  static func malformedPlanReadOutputToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planReadRequestID,
      toolName: "plan_read",
      arguments: planReadArguments,
      output: malformedPlanReadOutput,
      status: .completed
    )
  }

  static func expandedPlanWriteOutputToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planCreateRequestID,
      toolName: "plan_write",
      arguments: planCreateArguments,
      output: expandedPlanWriteOutput,
      status: .completed
    )
  }

  static func contradictoryPlanReadCursorToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planReadRequestID,
      toolName: "plan_read",
      arguments: planReadArguments,
      output: contradictoryPlanReadCursorOutput,
      status: .completed
    )
  }

  static func uncorrelatedPlanHistoryToolRecord() -> SignalboxStoredEvent {
    planReadToolRecord(output: uncorrelatedPlanHistoryOutput)
  }

  static func priorSessionTurnPlanHistoryToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planReadRequestID,
      sessionTurnAcceptancePositions: [crossTurn: 1, planTurnID: 2],
      sessionToolRequestPositions: [
        planProvenanceRequestID: (
          crossTurn, 1, "plan_write", planAttemptID,
          uncorrelatedPlanHistoryWriteOutput
        ),
        planReadRequestID: (planTurnID, 2, "plan_read", planAttemptID, nil),
      ],
      toolName: "plan_read",
      arguments: planReadArguments,
      output: uncorrelatedPlanHistoryOutput,
      status: .completed
    )
  }

  static func futureSessionTurnPlanHistoryToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planReadRequestID,
      sessionTurnAcceptancePositions: [planTurnID: 1, crossTurn: 2],
      sessionToolRequestPositions: [
        planReadRequestID: (planTurnID, 1, "plan_read", planAttemptID, nil),
        planProvenanceRequestID: (
          crossTurn, 2, "plan_write", planAttemptID,
          uncorrelatedPlanHistoryWriteOutput
        ),
      ],
      toolName: "plan_read",
      arguments: planReadArguments,
      output: uncorrelatedPlanHistoryOutput,
      status: .completed
    )
  }

  static func laterSameTurnPlanHistoryToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planReadRequestID,
      sessionToolRequestPositions: [
        planReadRequestID: (planTurnID, 1, "plan_read", planAttemptID, nil),
        planProvenanceRequestID: (
          planTurnID, 2, "plan_write", planAttemptID, planHistoryWriteOutput
        ),
      ],
      toolName: "plan_read",
      arguments: planReadArguments,
      output: planReadOutput,
      status: .completed
    )
  }

  static func nonPlanSameTurnHistoryToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planReadRequestID,
      sessionToolRequestPositions: [
        planProvenanceRequestID: (
          planTurnID, 1, "save_report", planAttemptID, planHistoryWriteOutput
        ),
        planReadRequestID: (planTurnID, 2, "plan_read", planAttemptID, nil),
      ],
      toolName: "plan_read",
      arguments: planReadArguments,
      output: planReadOutput,
      status: .completed
    )
  }

  static func mismatchedAttemptPlanHistoryToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planReadRequestID,
      sessionToolRequestPositions: [
        planProvenanceRequestID: (
          planTurnID, 1, "plan_write", planIssuingAttemptID, planHistoryWriteOutput
        ),
        planReadRequestID: (planTurnID, 2, "plan_read", planAttemptID, nil),
      ],
      toolName: "plan_read",
      arguments: planReadArguments,
      output: planReadOutput,
      status: .completed
    )
  }

  static func alteredPlanHistoryToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planReadRequestID,
      toolName: "plan_read",
      arguments: planReadArguments,
      output: alteredPlanHistoryOutput,
      status: .completed
    )
  }

  static func futurePlanEntryReferenceToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planStatusRequestID,
      toolName: "plan_write",
      arguments: planStatusArguments,
      output: futurePlanEntryReferenceOutput,
      status: .completed
    )
  }

  static func duplicatePlanReadArgumentToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: malformedPlanReadRequestID,
      toolName: "plan_read",
      arguments: duplicatePlanReadArguments,
      output: nil,
      status: .proposed
    )
  }

  static func nullPlanReadHistoryArgumentToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: malformedPlanReadRequestID,
      toolName: "plan_read",
      arguments: nullPlanReadHistoryArguments,
      output: nil,
      status: .proposed
    )
  }

  static func duplicatePlanWriteArgumentToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: malformedPlanRequestID,
      toolName: "plan_write",
      arguments: duplicatePlanWriteArguments,
      output: nil,
      status: .proposed
    )
  }

  static func mismatchedPlanWriteResultToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planReviseRequestID,
      toolName: "plan_write",
      arguments: planReviseArguments,
      output: planCreateOutput,
      status: .completed
    )
  }

  static func mismatchedPlanWriteTextToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planCreateRequestID,
      toolName: "plan_write",
      arguments: planCreateArguments,
      output: mismatchedPlanCreateOutput,
      status: .completed
    )
  }

  static func planReadBeforeCursorToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planReadRequestID,
      toolName: "plan_read",
      arguments: planReadBeforeCursorArguments,
      output: planReadBeforeCursorOutput,
      status: .completed
    )
  }

  static func incompletePlanHistoryToolRecord() -> SignalboxStoredEvent {
    planReadToolRecord(output: incompletePlanHistoryOutput)
  }

  static func mismatchedPlanHistoryToolRecord() -> SignalboxStoredEvent {
    planReadToolRecord(output: mismatchedPlanHistoryOutput)
  }

  static func repeatedPlanHistoryAttemptToolRecord() -> SignalboxStoredEvent {
    planReadToolRecord(output: repeatedPlanHistoryAttemptOutput)
  }

  static func planHistoryDependencyOverflowToolRecord() -> SignalboxStoredEvent {
    planReadToolRecord(output: planHistoryDependencyOverflowOutput)
  }

  static func unrequestedPlanHistoryToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planReadRequestID,
      toolName: "plan_read",
      arguments: #"{"include_history":false}"#,
      output: emptyIncludedPlanHistoryOutput,
      status: .completed
    )
  }

  static func cyclicPlanHistoryToolRecord() -> SignalboxStoredEvent {
    planReadToolRecord(output: cyclicPlanHistoryOutput)
  }

  static func denseAcyclicPlanToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planReadRequestID,
      toolName: "plan_read",
      arguments: #"{"include_history":false}"#,
      output: denseAcyclicPlanOutput,
      status: .completed
    )
  }

  static func multilinePlanCreateToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planCreateRequestID,
      toolName: "plan_write",
      arguments: multilinePlanArguments,
      output: multilinePlanOutput,
      status: .completed
    )
  }

  static func alternateNewlinePlanCreateToolRecord() -> SignalboxStoredEvent {
    planToolRecord(
      requestID: planCreateRequestID,
      toolName: "plan_write",
      arguments: alternateNewlinePlanArguments,
      output: alternateNewlinePlanOutput,
      status: .completed
    )
  }

  private typealias ToolRequestFixturePosition = (
    turnID: String,
    entryIndex: UInt64,
    toolName: String,
    toolAttemptID: String?,
    toolOutput: String?
  )

  private static func planToolRecord(
    requestID: String,
    turnID: String = planTurnID,
    sessionTurnAcceptancePositions: [String: UInt64]? = nil,
    sessionToolRequestPositions: [String: ToolRequestFixturePosition]? = nil,
    toolAttemptID: String = planAttemptID,
    toolName: String,
    arguments: String,
    output: String?,
    status: SignalboxProcessToolStatus
  ) -> SignalboxStoredEvent {
    let positions = sessionTurnAcceptancePositions ?? [turnID: 1]
    let requestPositions = sessionToolRequestPositions
      ?? (toolName == "plan_read"
        ? [
          planProvenanceRequestID: (
            turnID, 1, "plan_write", planAttemptID, planHistoryWriteOutput
          ),
          requestID: (turnID, 2, "plan_read", planAttemptID, nil),
        ]
        : [:])
    return SignalboxStoredEvent(
      eventID: SignalboxEventID(rawValue: 1),
      event: .processTool(
        SignalboxProcessToolEvent(
          toolRequestID: SignalboxToolInvocationID(rawValue: requestID),
          turnID: try! SignalboxCanonicalUUID(validating: turnID),
          sessionTurnAcceptancePositions: Dictionary(
            uniqueKeysWithValues: positions.map {
              (
                try! SignalboxCanonicalUUID(validating: $0.key),
                SignalboxCanonicalUInt64(rawValue: $0.value)
              )
            }),
          sessionToolRequestPositions: Dictionary(
            uniqueKeysWithValues: requestPositions.map {
              (
                try! SignalboxCanonicalUUID(validating: $0.key),
                SignalboxProcessToolRequestPosition(
                  turnID: try! SignalboxCanonicalUUID(validating: $0.value.turnID),
                  entryIndex: SignalboxCanonicalUInt64(rawValue: $0.value.entryIndex),
                  toolName: $0.value.toolName,
                  toolAttemptID: $0.value.toolAttemptID.map {
                    try! SignalboxCanonicalUUID(validating: $0)
                  },
                  toolOutput: $0.value.toolOutput
                )
              )
            }),
          toolAttemptID: try! SignalboxCanonicalUUID(validating: toolAttemptID),
          toolName: toolName,
          arguments: arguments,
          output: output,
          status: status
        )
      )
    )
  }

  private static let excessivePlanDependencies =
    (2...34).map(String.init).joined(separator: ",")

  private static func makeCyclicPlanHistoryOutput() -> String {
    let events = [
      #"{"ordinal":1,"kind":"created","entry_id":1,"text":"First",\#(planProvenanceJSON(attemptOrdinal: 1))}"#,
      #"{"ordinal":2,"kind":"created","entry_id":2,"text":"Second",\#(planProvenanceJSON(attemptOrdinal: 2))}"#,
      #"{"ordinal":3,"kind":"depends_on","entry_id":1,"dependency_id":2,\#(planProvenanceJSON(attemptOrdinal: 3))}"#,
      #"{"ordinal":4,"kind":"depends_on","entry_id":2,"dependency_id":1,\#(planProvenanceJSON(attemptOrdinal: 4))}"#,
    ].joined(separator: ",")
    return rawPlanReadOutput(entries: "[]", history: "[\(events)]")
  }

  private static func makeDenseAcyclicPlanOutput() -> String {
    let entries = (1...SignalboxProcessProtocol.maximumPlanReadEntries).map { entryID in
      let dependencies: [Int]
      switch entryID {
      case 1:
        dependencies = []
      case 2:
        dependencies = [1]
      default:
        dependencies = [entryID - 2, entryID - 1]
      }
      let encodedDependencies = dependencies.map(String.init).joined(separator: ",")
      return #"{"entry_id":\#(entryID),"text":"Entry \#(entryID)","status":"completed","dependencies":[\#(encodedDependencies)],"readiness":"ready"}"#
    }.joined(separator: ",")
    return rawPlanReadOutput(entries: "[\(entries)]")
  }

  private static func makePlanHistoryDependencyOverflowOutput() -> String {
    let creations = (1...34).map { ordinal in
      #"{"ordinal":\#(ordinal),"kind":"created","entry_id":\#(ordinal),"text":"Entry \#(ordinal)",\#(planProvenanceJSON(attemptOrdinal: ordinal))}"#
    }
    let dependencies = (2...34).map { dependencyID in
      let ordinal = dependencyID + 33
      return #"{"ordinal":\#(ordinal),"kind":"depends_on","entry_id":1,"dependency_id":\#(dependencyID),\#(planProvenanceJSON(attemptOrdinal: ordinal))}"#
    }
    return rawPlanReadOutput(
      entries: "[]",
      history: "[\((creations + dependencies).joined(separator: ","))]"
    )
  }

  private static func planProvenanceJSON(attemptOrdinal: Int) -> String {
    let attemptID = String(
      format: "aaaaaaaa-5555-4555-8555-%012d",
      attemptOrdinal
    )
    return #""provenance":{"turn_id":"\#(planTurnID)","issuing_attempt_id":"\#(planIssuingAttemptID)","request_id":"\#(planProvenanceRequestID)","attempt_id":"\#(attemptID)","generation":\#(planGeneration)}"#
  }

  private static func planWriteProvenanceJSON(requestID: String) -> String {
    #""provenance":{"turn_id":"\#(planTurnID)","issuing_attempt_id":"\#(planIssuingAttemptID)","request_id":"\#(requestID)","attempt_id":"\#(planAttemptID)","generation":\#(planGeneration)}"#
  }

  private static func rawPlanReadOutput(
    entries: String,
    history: String = "null",
    historyTruncated: Bool = false
  ) -> String {
    #"{"entries":\#(entries),"next_after_entry_id":null,"plan_truncated":false,"history":\#(history),"history_truncated":\#(historyTruncated)}"#
  }

  private static func rawToolOutputPreview(_ output: String) -> String {
    let trimmed = output.trimmingCharacters(in: .whitespacesAndNewlines)
    return trimmed.count <= 480 ? trimmed : String(trimmed.prefix(480)) + "..."
  }

  static func importedContentKinds(
    in projection: SignalboxProcessTranscriptProjection
  ) throws -> [SignalboxProcessImportedContentKind] {
    let kinds: [SignalboxProcessImportedContentKind] = projection.records.compactMap {
      record -> SignalboxProcessImportedContentKind? in
      guard case .processImportedContent(let event) = record.event else {
        return nil
      }
      return event.contentKind
    }
    guard kinds.count == importedContentKinds.count else {
      throw ProcessDriverUpdateRecorderError.missingFixtureMessage
    }
    return kinds
  }

  static func importedContentEventID(
    _ kind: SignalboxProcessImportedContentKind,
    in projection: SignalboxProcessTranscriptProjection
  ) throws -> SignalboxEventID {
    guard let record = projection.records.first(where: { record in
      guard case .processImportedContent(let event) = record.event else {
        return false
      }
      return event.contentKind == kind
    }) else {
      throw ProcessDriverUpdateRecorderError.missingFixtureEvent
    }
    return record.eventID
  }

  static func noticeTitles(in timeline: [SignalboxTimelineItem]) -> [String] {
    timeline.compactMap {
      guard case .processEvidence(let notice) = $0 else {
        return nil
      }
      return notice.title
    }
  }

  static func noticeDetailValues(in timeline: [SignalboxTimelineItem]) -> [String] {
    let notices: [SignalboxProcessNoticeCard] = timeline.compactMap {
      item -> SignalboxProcessNoticeCard? in
      guard case .processEvidence(let notice) = item else {
        return nil
      }
      return notice
    }
    return notices.flatMap { $0.details }.map { $0.value }
  }

  static func modelCallUsageIDs(
    in projection: SignalboxProcessTranscriptProjection
  ) -> [String] {
    projection.records.compactMap { record in
      guard case .processModelCallUsage(let event) = record.event else {
        return nil
      }
      return event.modelCallID.rawValue
    }.sorted()
  }

  static func toolNames(
    in projection: SignalboxProcessTranscriptProjection
  ) -> [String] {
    projection.records.compactMap { record in
      guard case .processTool(let tool) = record.event else {
        return nil
      }
      return tool.toolName
    }
  }

  static func conservativeKinds(
    in projection: SignalboxProcessTranscriptProjection
  ) -> [String] {
    projection.records.compactMap { record in
      guard case .processConservative(let event) = record.event else {
        return nil
      }
      return event.kind
    }
  }

  static func toolSummaries(
    in projection: SignalboxProcessTranscriptProjection
  ) -> [String] {
    projection.records.compactMap { record in
      guard case .processTool(let tool) = record.event else {
        return nil
      }
      return "\(tool.toolName): \(tool.output ?? "")"
    }
  }

  static func nativeToolRequestPositionName(
    in projection: SignalboxProcessTranscriptProjection
  ) throws -> String {
    let requestID = try SignalboxCanonicalUUID(validating: proposedToolRequest)
    guard let tool = projection.records.compactMap({ record -> SignalboxProcessToolEvent? in
      guard case .processTool(let event) = record.event else {
        return nil
      }
      return event
    }).first,
      let position = tool.sessionToolRequestPositions?[requestID]
    else {
      throw ProcessDriverUpdateRecorderError.missingFixtureTool
    }
    return position.toolName
  }

  static func modelCallUsageIDs(
    in timeline: [SignalboxTimelineItem]
  ) -> [String] {
    timeline.compactMap { item in
      guard case .processEvidence(let notice) = item,
        notice.title == modelUsageNoticeTitle
      else {
        return nil
      }
      return notice.details.first { $0.label == modelCallDetailLabel }?.value
    }
  }

  static func modelCallUsageEventID(
    _ modelCallID: String,
    in projection: SignalboxProcessTranscriptProjection
  ) throws -> SignalboxEventID {
    guard let record = projection.records.first(where: { record in
      guard case .processModelCallUsage(let event) = record.event else {
        return false
      }
      return event.modelCallID.rawValue == modelCallID
    }) else {
      throw ProcessDriverUpdateRecorderError.missingFixtureEvent
    }
    return record.eventID
  }

  static func modelCallUsageEventIDs(
    in projection: SignalboxProcessTranscriptProjection
  ) -> [SignalboxEventID] {
    projection.records.compactMap { record in
      guard case .processModelCallUsage = record.event else {
        return nil
      }
      return record.eventID
    }
  }

  static func unknownKinds(in timeline: [SignalboxTimelineItem]) -> [String] {
    timeline.compactMap {
      guard case .unknown(let unknown) = $0 else {
        return nil
      }
      return unknown.kind
    }
  }

  static func unknownDiagnostics(in timeline: [SignalboxTimelineItem]) -> [String] {
    timeline.compactMap {
      guard case .unknown(let unknown) = $0 else {
        return nil
      }
      return unknown.diagnostic
    }
  }

  static func toolCards(in timeline: [SignalboxTimelineItem]) -> [SignalboxToolCard] {
    timeline.compactMap {
      guard case .tool(let tool) = $0 else {
        return nil
      }
      return tool
    }
  }

  private static func importedMarker(
    index: UInt64,
    entryID: String,
    sourceSessionID: String = ProcessDriverFixture.session,
    speaker: String,
    kind: String
  ) -> String {
    """
    {
      "type":"transcript_entry",
      "entry_index":"\(index)",
      "source_session_id":"\(sourceSessionID)",
      "entry_id":"\(entryID)",
      "entry":{
        "type":"imported",
        "imported_conversation_id":"\(importedConversation)",
        "imported_entry_id":"\(entryID)",
        "source_speaker":\(speaker),
        "content_kind":"\(kind)"
      }
    }
    """
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
          "cursor":"1",
          "runner":null
        }
        """,
        emptyModelCallsBoundary,
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

  static func snapshotWithReconciliationResultSuffix() throws
    -> SignalboxSynchronizationSnapshot
  {
    try reconciliationResultSnapshot(trailingEntries: [], entryCount: 4)
  }

  static func snapshotWithReconciliationResultSuffixAndLaterTurnEntry() throws
    -> SignalboxSynchronizationSnapshot
  {
    try reconciliationResultSnapshot(
      trailingEntries: [
        """
        {
          "type":"transcript_entry",
          "entry_index":"4",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(laterTurnToolEntry)",
          "entry":{
            "type":"assistant_tool_use",
            "turn_id":"\(crossTurn)",
            "model_call_id":"\(laterTurnModelCall)",
            "tool_request_id":"\(laterTurnToolRequest)",
            "tool_name":"\(laterTurnToolName)",
            "arguments":"{}"
          }
        }
        """
      ],
      entryCount: 5
    )
  }

  static func snapshotWithStaleSameTurnReconciliationResult() throws
    -> SignalboxSynchronizationSnapshot
  {
    try reconciliationResultSnapshot(
      trailingEntries: [
        """
        {
          "type":"transcript_entry",
          "entry_index":"4",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(laterTurnToolEntry)",
          "entry":{
            "type":"assistant_tool_use",
            "turn_id":"\(ProcessDriverFixture.turn)",
            "model_call_id":"\(laterTurnModelCall)",
            "tool_request_id":"\(laterTurnToolRequest)",
            "tool_name":"\(laterTurnToolName)",
            "arguments":"{}"
          }
        }
        """
      ],
      entryCount: 5
    )
  }

  static func snapshotWithMixedClosedReconciliationResult() throws
    -> SignalboxSynchronizationSnapshot
  {
    try reconciliationResultSnapshot(
      trailingEntries: [],
      entryCount: 4,
      resultEntries: [
        """
        {
          "type":"transcript_entry",
          "entry_index":"2",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(reconciliationClosedEntry)",
          "entry":{
            "type":"tool_closed",
            "tool_request_id":"\(proposedToolRequest)",
            "content":"\(reconciliationClosedOutput)"
          }
        }
        """,
        """
        {
          "type":"transcript_entry",
          "entry_index":"3",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(reconciliationSuffixResultEntry)",
          "entry":{
            "type":"tool_execution_result",
            "tool_request_id":"\(reconciliationSuffixToolRequest)",
            "tool_attempt_id":"\(reconciliationSuffixAttempt)",
            "content":"\(reconciliationSuffixOutput)"
          }
        }
        """,
      ]
    )
  }

  static func snapshotWithForegroundDelegationReconciliationResult() throws
    -> SignalboxSynchronizationSnapshot
  {
    try reconciliationResultSnapshot(
      trailingEntries: [],
      entryCount: 4,
      resultEntries: [
        """
        {
          "type":"transcript_entry",
          "entry_index":"2",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(reconciliationClosedEntry)",
          "entry":{
            "type":"delegation_result",
            "await_request_id":"\(proposedToolRequest)",
            "spawning_request_id":"\(delegationSpawningRequest)",
            "child_session_id":"\(delegationChildSession)",
            "mode":"foreground",
            "delivery_sequence":null,
            "outcome":"returned",
            "content":"\(delegationResultContent)",
            "reason":"child_completed",
            "provenance":{
              "type":"child_turn",
              "child_session_id":"\(delegationChildSession)",
              "child_turn_id":"\(delegationChildTurn)"
            }
          }
        }
        """,
        """
        {
          "type":"transcript_entry",
          "entry_index":"3",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(reconciliationSuffixResultEntry)",
          "entry":{
            "type":"tool_execution_result",
            "tool_request_id":"\(reconciliationSuffixToolRequest)",
            "tool_attempt_id":"\(reconciliationSuffixAttempt)",
            "content":"\(reconciliationSuffixOutput)"
          }
        }
        """,
      ]
    )
  }

  static func snapshotWithAttemptlessDeniedReconciliationResults() throws
    -> SignalboxSynchronizationSnapshot
  {
    try reconciliationResultSnapshot(
      trailingEntries: [],
      entryCount: 4,
      resultEntries: [
        toolDeniedMessage(
          index: 2,
          entryID: reconciliationClosedEntry,
          requestID: proposedToolRequest
        ),
        toolDeniedMessage(
          index: 3,
          entryID: reconciliationSuffixResultEntry,
          requestID: reconciliationSuffixToolRequest
        ),
      ]
    )
  }

  static func snapshotWithDeniedAndUncorrelatedClosedReconciliationResults() throws
    -> SignalboxSynchronizationSnapshot
  {
    try reconciliationResultSnapshot(
      trailingEntries: [],
      entryCount: 4,
      resultEntries: [
        toolDeniedMessage(
          index: 2,
          entryID: reconciliationClosedEntry,
          requestID: proposedToolRequest
        ),
        """
        {
          "type":"transcript_entry",
          "entry_index":"3",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(reconciliationSuffixResultEntry)",
          "entry":{
            "type":"tool_closed",
            "tool_request_id":"\(reconciliationSuffixToolRequest)",
            "content":"\(reconciliationClosedOutput)"
          }
        }
        """,
      ]
    )
  }

  private static func toolDeniedMessage(
    index: UInt64,
    entryID: String,
    requestID: String
  ) -> String {
    """
    {
      "type":"transcript_entry",
      "entry_index":"\(index)",
      "source_session_id":"\(ProcessDriverFixture.session)",
      "entry_id":"\(entryID)",
      "entry":{
        "type":"tool_denied",
        "tool_request_id":"\(requestID)",
        "content":"\(reconciliationDeniedOutput)"
      }
    }
    """
  }

  static func snapshotWithClosedReconciliationResultAndLaterTurnEntry() throws
    -> SignalboxSynchronizationSnapshot
  {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "acceptance_position":"1",
          "state":{
            "type":"tool_reconciliation_required",
            "terminal_frontier_id":"\(ProcessDriverFixture.frontier)",
            "terminal_attempt_id":"\(ProcessDriverFixture.attempt)",
            "terminal_tool_attempt_id":"\(reconciliationAttempt)"
          }
        }
        """,
        """
        {
          "type":"transcript_model_call_usage",
          "model_call_index":"0",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "model_call_id":"\(ProcessDriverFixture.modelCall)",
          "usage_provenance":"reported",
          "usage":{
            "input_tokens":null,
            "output_tokens":null,
            "cache_creation_input_tokens":null,
            "cache_read_input_tokens":null
          },
          "cost":null
        }
        """,
        """
        {
          "type":"transcript_model_calls_end",
          "model_call_count":"1"
        }
        """,
        toolRequestMessage(index: 0),
        """
        {
          "type":"transcript_entry",
          "entry_index":"1",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(reconciliationClosedEntry)",
          "entry":{
            "type":"tool_closed",
            "tool_request_id":"\(proposedToolRequest)",
            "content":"\(reconciliationClosedOutput)"
          }
        }
        """,
        """
        {
          "type":"transcript_entry",
          "entry_index":"2",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(laterTurnToolEntry)",
          "entry":{
            "type":"assistant_tool_use",
            "turn_id":"\(crossTurn)",
            "model_call_id":"\(laterTurnModelCall)",
            "tool_request_id":"\(laterTurnToolRequest)",
            "tool_name":"\(laterTurnToolName)",
            "arguments":"{}"
          }
        }
        """,
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "turn_count":"1",
          "entry_count":"3"
        }
        """,
      ]
    )
  }

  private static func reconciliationResultSnapshot(
    trailingEntries: [String],
    entryCount: UInt64,
    resultEntries: [String]? = nil
  ) throws -> SignalboxSynchronizationSnapshot {
    let terminalResults = resultEntries ?? [
      toolResultMessage(index: 2),
      """
      {
        "type":"transcript_entry",
        "entry_index":"3",
        "source_session_id":"\(ProcessDriverFixture.session)",
        "entry_id":"\(reconciliationSuffixResultEntry)",
        "entry":{
          "type":"tool_execution_result",
          "tool_request_id":"\(reconciliationSuffixToolRequest)",
          "tool_attempt_id":"\(reconciliationSuffixAttempt)",
          "content":"\(reconciliationSuffixOutput)"
        }
      }
      """,
    ]
    return try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "acceptance_position":"1",
          "state":{
            "type":"tool_reconciliation_required",
            "terminal_frontier_id":"\(ProcessDriverFixture.frontier)",
            "terminal_attempt_id":"\(ProcessDriverFixture.attempt)",
            "terminal_tool_attempt_id":"\(reconciliationAttempt)"
          }
        }
        """,
        """
        {
          "type":"transcript_model_call_usage",
          "model_call_index":"0",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "model_call_id":"\(ProcessDriverFixture.modelCall)",
          "usage_provenance":"reported",
          "usage":{
            "input_tokens":null,
            "output_tokens":null,
            "cache_creation_input_tokens":null,
            "cache_read_input_tokens":null
          },
          "cost":null
        }
        """,
        """
        {
          "type":"transcript_model_calls_end",
          "model_call_count":"1"
        }
        """,
        toolRequestMessage(index: 0),
        """
        {
          "type":"transcript_entry",
          "entry_index":"1",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(reconciliationSuffixToolEntry)",
          "entry":{
            "type":"assistant_tool_use",
            "turn_id":"\(ProcessDriverFixture.turn)",
            "model_call_id":"\(ProcessDriverFixture.modelCall)",
            "tool_request_id":"\(reconciliationSuffixToolRequest)",
            "tool_name":"\(reconciliationSuffixToolName)",
            "arguments":"{}"
          }
        }
        """,
      ] + terminalResults + trailingEntries + [
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "turn_count":"1",
          "entry_count":"\(entryCount)"
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
          "cursor":"1",
          "runner":null
        }
        """,
        emptyModelCallsBoundary,
        """
        {
          "type":"transcript_user_entry",
          "entry_index":"0",
          "source_session_id":"\(ProcessDriverFixture.session)",
          "entry_id":"\(completedUserEntry)",
          "accepted_input_id":"\(ProcessSubmissionFixture.acceptedInputID)",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "content":[{"type":"text","text":"\(userText)"}]
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

  static func snapshotWithCompletedTurnAndPriorToolResult() throws
    -> SignalboxSynchronizationSnapshot
  {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
        emptyModelCallsBoundary,
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
          "type":"transcript_entry",
          "entry_index":"2",
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
          "type":"transcript_entry",
          "entry_index":"3",
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
          "entry_count":"4"
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
          "cursor":"1",
          "runner":null
        }
        """,
        emptyModelCallsBoundary,
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

  static func snapshotWithImportedAssistantCollision() throws
    -> SignalboxSynchronizationSnapshot
  {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
        emptyModelCallsBoundary,
        """
        {
          "type":"transcript_text_entry",
          "entry_index":"0",
          "source_session_id":"\(reusedToolSourceSession)",
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

  static func snapshotWithTerminalResponseMissingUserEntry() throws
    -> SignalboxSynchronizationSnapshot
  {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"1",
          "runner":null
        }
        """,
        emptyModelCallsBoundary,
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
    try snapshotWithQueuedTurns(
      middleState: """
        {
          "type":"active_running",
          "current_attempt_id":"\(ProcessDriverFixture.attempt)",
          "current_model_call":null
        }
        """
    )
  }

  static func snapshotWithCancelledAndQueuedTurns(
    cursor: UInt64
  ) throws -> SignalboxSynchronizationSnapshot {
    try snapshotWithQueuedTurns(
      middleState: """
        {
          "type":"cancelled",
          "terminal_frontier_id":"\(ProcessDriverFixture.frontier)",
          "terminal_attempt_id":"\(ProcessDriverFixture.attempt)",
          "terminal_model_call_id":null
        }
        """,
      cursor: cursor
    )
  }

  static func snapshotWithUnknownAndQueuedTurns(
    cursor: UInt64 = 1
  ) throws -> SignalboxSynchronizationSnapshot {
    try snapshotWithQueuedTurns(
      middleState: """
        {"type":"fixture_future_turn_state","retained":true}
        """,
      cursor: cursor
    )
  }

  private static func snapshotWithQueuedTurns(
    middleState: String,
    cursor: UInt64 = 1
  ) throws -> SignalboxSynchronizationSnapshot {
    try snapshot(
      messages: [
        """
        {
          "type":"transcript_snapshot_start",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"\(cursor)",
          "runner":null
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
            "content":[{"type":"text","text":"first"}]
          }
        }
        """,
        """
        {
          "type":"transcript_turn",
          "turn_id":"\(ProcessDriverFixture.turn)",
          "acceptance_position":"2",
          "state":\(middleState)
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
            "content":[{"type":"text","text":"second"}]
          }
        }
        """,
        emptyModelCallsBoundary,
        """
        {
          "type":"transcript_snapshot_end",
          "session_id":"\(ProcessDriverFixture.session)",
          "cursor":"\(cursor)",
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

  static func contextCompactedTrigger() throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"context_compacted",
        "context_compaction_id":"\(contextCompactionID)",
        "model_call_id":"\(ProcessDriverFixture.modelCall)",
        "through_position":"19",
        "summary_entry_id":"\(contextSummaryEntry)",
        "result_frontier_id":"\(ProcessDriverFixture.frontier)"
      }
      """
    )
  }

  static func proposedToolTrigger(
    cursor: UInt64 = 1
  ) throws -> SignalboxFollowedSessionEvent {
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
      """,
      cursor: cursor
    )
  }

  static func projectedResultsTrigger() throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"tool_batch_transition",
        "turn_id":"\(ProcessDriverFixture.turn)",
        "model_call_id":"\(ProcessDriverFixture.modelCall)",
        "state":{
          "type":"results_projected",
          "frontier_id":"\(ProcessDriverFixture.frontier)"
        }
      }
      """,
      cursor: 2
    )
  }

  static func toolRecoveryTrigger() throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"tool_batch_transition",
        "turn_id":"\(ProcessDriverFixture.turn)",
        "model_call_id":"\(ProcessDriverFixture.modelCall)",
        "state":{
          "type":"recovery_required",
          "tool_attempt_id":"\(reconciliationAttempt)"
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

  static func modelReconciliationTrigger(
    cursor: UInt64
  ) throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"turn_reconciliation_required",
        "turn_id":"\(ProcessDriverFixture.turn)",
        "model_call_id":"\(ProcessDriverFixture.modelCall)",
        "terminal_frontier_id":"\(ProcessDriverFixture.frontier)"
      }
      """,
      cursor: cursor
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
        "content":[{"type":"text","text":"\(ProcessSubmissionFixture.content)"}]
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
        "content":[{"type":"text","text":"\(ProcessSubmissionFixture.content)"}]
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
        "content":[{"type":"text","text":"second"}]
      }
      """
    )
  }

  static func activatedEvent(
    cursor: UInt64 = 1
  ) throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"turn_activated",
        "turn_id":"\(ProcessDriverFixture.turn)",
        "current_attempt_id":"\(ProcessDriverFixture.attempt)"
      }
      """,
      cursor: cursor
    )
  }

  static func unknownToolBatchEvent(
    kind: String = unknownToolBatchState,
    cursor: UInt64 = 1
  ) throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"tool_batch_transition",
        "turn_id":"\(ProcessDriverFixture.turn)",
        "model_call_id":"\(ProcessDriverFixture.modelCall)",
        "state":{"type":"\(kind)"}
      }
      """,
      cursor: cursor
    )
  }

  static func successorActivatedEvent() throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"turn_activated",
        "turn_id":"\(ProcessSubmissionFixture.acceptedTurnID)",
        "current_attempt_id":"\(ProcessDriverFixture.attempt)"
      }
      """
    )
  }

  static func queuedTurnActivatedEvent(
    cursor: UInt64
  ) throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"turn_activated",
        "turn_id":"\(secondPendingTurn)",
        "current_attempt_id":"\(ProcessDriverFixture.attempt)"
      }
      """,
      cursor: cursor
    )
  }

  static func refusedEvent(
    cursor: UInt64 = 1
  ) throws -> SignalboxFollowedSessionEvent {
    try refusedEvent(turnID: ProcessDriverFixture.turn, cursor: cursor)
  }

  static func submittedTurnRefusedEvent() throws -> SignalboxFollowedSessionEvent {
    try refusedEvent(turnID: ProcessSubmissionFixture.acceptedTurnID, cursor: 1)
  }

  private static func refusedEvent(
    turnID: String,
    cursor: UInt64
  ) throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"turn_refused",
        "turn_id":"\(turnID)",
        "model_call_id":"\(ProcessDriverFixture.modelCall)",
        "terminal_frontier_id":"\(ProcessDriverFixture.frontier)"
      }
      """,
      cursor: cursor
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

  static func completedModelCallEvent(
    cursor: UInt64 = 1
  ) throws -> SignalboxFollowedSessionEvent {
    try modelCallEvent(disposition: "completed", cursor: cursor)
  }

  static func preparedModelCallTrigger() throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"model_call_transition",
        "turn_id":"\(ProcessDriverFixture.turn)",
        "model_call_id":"\(ProcessDriverFixture.modelCall)",
        "state":{"type":"prepared"}
      }
      """
    )
  }

  static func ambiguousModelCallEvent() throws -> SignalboxFollowedSessionEvent {
    try modelCallEvent(disposition: "ambiguous")
  }

  static func unknownDispositionModelCallEvent() throws -> SignalboxFollowedSessionEvent {
    try modelCallEvent(disposition: unknownDisposition)
  }

  static func unknownStateModelCallEvent(
    cursor: UInt64 = 1
  ) throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"model_call_transition",
        "turn_id":"\(ProcessDriverFixture.turn)",
        "model_call_id":"\(ProcessDriverFixture.modelCall)",
        "state":{"type":"\(unknownModelCallState)"}
      }
      """,
      cursor: cursor
    )
  }

  private static func modelCallEvent(
    disposition: String,
    cursor: UInt64 = 1
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
      """,
      cursor: cursor
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

  static func presentationOrders(
    in projection: SignalboxProcessTranscriptProjection
  ) -> [Int] {
    projection.records.map {
      ($0.presentationOrder ?? $0.eventID).rawValue
    }
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

  static func onlyToolRecord(
    in projection: SignalboxProcessTranscriptProjection
  ) throws -> SignalboxStoredEvent {
    let records = projection.records.filter {
      if case .processTool = $0.event {
        return true
      }
      return false
    }
    guard records.count == 1, let record = records.first else {
      throw ProcessDriverUpdateRecorderError.missingFixtureTool
    }
    return record
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
    sessionID: String = ProcessDriverFixture.session,
    cursor: UInt64 = 1
  ) throws -> SignalboxFollowedSessionEvent {
    let message = try message(
      """
      {
        "type":"session_event",
        "cursor":"\(cursor)",
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

extension ProcessServiceIntegrationTests {
  @MainActor
  func testAuthoritativeSnapshotPublishesRunnerStatus() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }

    viewModel.apply(
      .authoritativeSnapshot(try ProcessProjectionFixture.snapshotWithRunner())
    )

    XCTAssertEqual(viewModel.runner, try ProcessProjectionFixture.runnerProjection())
    XCTAssertNil(viewModel.runnerTransition)
    XCTAssertEqual(
      viewModel.runnerStatusLabel,
      ProcessProjectionFixture.runnerSnapshotStatusLabel
    )
  }

  @MainActor
  func testLiveRunnerTransitionReplacesPresentedRunnerStatus() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }
    viewModel.apply(
      .authoritativeSnapshot(try ProcessProjectionFixture.snapshotWithRunner())
    )

    viewModel.apply(.event(try ProcessProjectionFixture.runnerLossEvent()))

    XCTAssertEqual(
      viewModel.runnerTransition,
      try ProcessProjectionFixture.runnerLossTransition()
    )
    XCTAssertEqual(
      viewModel.runnerStatusLabel,
      ProcessProjectionFixture.runnerLossStatusLabel
    )
  }

  func testRunnerTransitionStatusPresentsRelocationTarget() throws {
    let transition = try ProcessProjectionFixture.runnerRelocationTransition()

    XCTAssertEqual(
      transition.statusLabel,
      ProcessProjectionFixture.runnerRelocationStatusLabel
    )
  }

  func testRunnerTransitionStatusPresentsRunnerDefaultDirectory() throws {
    let transition = try ProcessProjectionFixture.runnerDefaultDirectoryTransition()

    XCTAssertEqual(
      transition.statusLabel,
      ProcessProjectionFixture.runnerDefaultDirectoryStatusLabel
    )
  }

  @MainActor
  func testToolApprovalDecisionPresentsDelegateProvenanceAndRationale() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }
    viewModel.apply(
      .authoritativeSnapshot(try ProcessProjectionFixture.snapshotWithProposedTool())
    )

    viewModel.apply(.event(try ProcessProjectionFixture.delegateDenialEvent()))

    let tool = try ProcessProjectionFixture.onlyToolCard(in: viewModel.timeline)
    XCTAssertEqual(tool.status, .denied)
    XCTAssertEqual(tool.decisionReason, nil)
    XCTAssertEqual(tool.approvalDecider, ProcessProjectionFixture.delegateDenialLabel)
    XCTAssertEqual(tool.approvalRationale, ProcessProjectionFixture.delegateRationale)
    XCTAssertFalse(tool.decisionAvailable)
  }

  @MainActor
  func testAuthoritativeSnapshotPresentsHistoricalDelegateDecision() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }

    viewModel.apply(
      .authoritativeSnapshot(try ProcessProjectionFixture.snapshotWithDelegateDenial())
    )

    let tool = try ProcessProjectionFixture.onlyToolCard(in: viewModel.timeline)
    XCTAssertEqual(tool.status, .denied)
    XCTAssertEqual(tool.decisionReason, nil)
    XCTAssertEqual(tool.approvalDecider, ProcessProjectionFixture.delegateDenialLabel)
    XCTAssertEqual(tool.approvalRationale, ProcessProjectionFixture.delegateRationale)
    XCTAssertFalse(tool.decisionAvailable)
  }

  @MainActor
  func testUnknownLiveSessionEventAddsStableBoundedTimelineCard() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }
    let followed = try ProcessProjectionFixture.unknownSessionEvent()
    viewModel.apply(.authoritativeSnapshot(try ProcessProjectionFixture.snapshotWithUserEntry()))

    viewModel.apply(.event(followed))
    viewModel.apply(.event(followed))
    viewModel.apply(.diagnostic(ProcessProjectionFixture.transportDiagnostic))
    viewModel.apply(
      .sideSnapshot(
        snapshot: try ProcessProjectionFixture.snapshotWithProposedTool(),
        trigger: try ProcessProjectionFixture.proposedToolTrigger()
      )
    )

    let unknown = try ProcessProjectionFixture.onlyUnknownCard(in: viewModel.timeline)
    XCTAssertEqual(
      ProcessProjectionFixture.timelineKinds(in: viewModel.timeline),
      ProcessProjectionFixture.unknownEventSideSnapshotTimelineKinds
    )
    XCTAssertEqual(
      unknown.kind,
      SignalboxProcessPresentation.retainedLabel(ProcessProjectionFixture.oversizedUnknownState)
    )
    XCTAssertEqual(unknown.diagnostic, ProcessProjectionFixture.unknownSessionEventDiagnostic)
    XCTAssertEqual(viewModel.latestDiagnostic, ProcessProjectionFixture.transportDiagnostic.message)
  }

  @MainActor
  func testUnknownLiveSessionEventHistoryIsBoundedAndVisible() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }

    try ProcessProjectionFixture.fillUnknownSessionEventHistory(in: viewModel)
    let boundaryCards = try ProcessProjectionFixture.boundaryAndNewestUnknownCards(
      in: viewModel.timeline
    )

    XCTAssertEqual(viewModel.timeline.count, ProcessProjectionFixture.unknownHistoryCapacity)
    XCTAssertEqual(boundaryCards.boundary.kind, ProcessProjectionFixture.unknownHistoryKind)
    XCTAssertEqual(
      boundaryCards.boundary.diagnostic,
      ProcessProjectionFixture.unknownHistoryDiagnostic
    )
    XCTAssertEqual(boundaryCards.newest.kind, ProcessProjectionFixture.futureSessionEventKind)
  }

  @MainActor
  func testUnknownNestedStateHistoryIsBoundedAndVisible() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }

    try ProcessProjectionFixture.fillUnknownNestedStateHistory(in: viewModel)
    let boundaryCards = try ProcessProjectionFixture.boundaryAndNewestUnknownCards(
      in: viewModel.timeline
    )

    XCTAssertEqual(viewModel.timeline.count, ProcessProjectionFixture.unknownHistoryCapacity)
    XCTAssertEqual(boundaryCards.boundary.kind, ProcessProjectionFixture.unknownHistoryKind)
    XCTAssertEqual(
      boundaryCards.boundary.diagnostic,
      ProcessProjectionFixture.unknownHistoryDiagnostic
    )
    XCTAssertEqual(boundaryCards.newest.kind, ProcessProjectionFixture.unknownNestedStateKind)
  }

  @MainActor
  func testUnknownHistoryBoundaryHasDistinctTimelineIdentity() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }
    viewModel.apply(
      .authoritativeSnapshot(
        try ProcessProjectionFixture.snapshotWithUnknownEntryKind(
          ProcessProjectionFixture.futureSessionEventKind
        )
      )
    )

    try ProcessProjectionFixture.fillUnknownSessionEventHistory(in: viewModel)

    XCTAssertEqual(Set(viewModel.timeline.map(\.id)).count, viewModel.timeline.count)
  }

  func testModelUsageKeepsItsTranscriptAnchorAfterNormalization() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithAnchoredUsageAndLaterMessage()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)
    let normalizer = try SignalboxIncrementalEventNormalizer(records: projection.records)

    XCTAssertEqual(
      ProcessProjectionFixture.timelineKinds(in: normalizer.timelineItems),
      ProcessProjectionFixture.anchoredUsageTimelineKinds
    )
  }

  func testFailedCallUsageFallsBackToItsTerminalMarkerAnchor() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithFailedUsageAndMarker()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)
    let normalizer = try SignalboxIncrementalEventNormalizer(records: projection.records)

    XCTAssertEqual(
      ProcessProjectionFixture.timelineKinds(in: normalizer.timelineItems),
      ProcessProjectionFixture.terminalMarkerUsageTimelineKinds
    )
  }

  func testCompletedCallUsageFallsBackToItsCompletionMarkerAnchor() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithCompletedUsageAndMarker()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)
    let normalizer = try SignalboxIncrementalEventNormalizer(records: projection.records)

    XCTAssertEqual(
      ProcessProjectionFixture.noticeTitles(in: normalizer.timelineItems),
      ProcessProjectionFixture.terminalUsageNoticeTitles
    )
  }

  func testRefusedCallUsageStaysAfterEarlierTranscriptEvidence() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithRefusedUsageAfterImportedMarker()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)
    let normalizer = try SignalboxIncrementalEventNormalizer(records: projection.records)

    XCTAssertEqual(
      ProcessProjectionFixture.noticeTitles(in: normalizer.timelineItems),
      ProcessProjectionFixture.terminalUsageNoticeTitles
    )
  }

  func testReconciliationCallUsageStaysAfterEarlierTranscriptEvidence() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithReconciliationUsageAfterImportedMarker()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)
    let normalizer = try SignalboxIncrementalEventNormalizer(records: projection.records)

    XCTAssertEqual(
      ProcessProjectionFixture.noticeTitles(in: normalizer.timelineItems),
      ProcessProjectionFixture.terminalUsageNoticeTitles
    )
  }

  func testRecoveryCallUsageStaysAfterEarlierTranscriptEvidence() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithRecoveryUsageAfterImportedMarker()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)
    let normalizer = try SignalboxIncrementalEventNormalizer(records: projection.records)

    XCTAssertEqual(
      ProcessProjectionFixture.noticeTitles(in: normalizer.timelineItems),
      ProcessProjectionFixture.terminalUsageNoticeTitles
    )
  }

  func testUnanchoredUsageIdentityCannotCollideWithSemanticEntryIdentity() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithFormerUsageIdentityCollision()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)

    XCTAssertEqual(
      projection.records.map(\.eventID.rawValue),
      ProcessProjectionFixture.disjointUsageAndSemanticPresentationIDs
    )
    XCTAssertEqual(
      projection.records.map(\.event.kind),
      ProcessProjectionFixture.disjointUsageAndSemanticEventKinds
    )
  }

  func testMalformedFollowedEventDiagnosticIsBounded() throws {
    let followed = try ProcessProjectionFixture.malformedKnownFollowedEvent()
    let projector = SignalboxProcessTranscriptProjector()

    let event = try XCTUnwrap(projector.projectUnrecognizedFollowedEvent(followed))

    XCTAssertEqual(
      event.diagnostic,
      ProcessProjectionFixture.boundedMalformedFollowedEventDiagnostic
    )
  }

  func testMalformedSnapshotTurnDiagnosticIsBounded() throws {
    let snapshot = try ProcessProjectionFixture.snapshotWithMalformedKnownTurnState()
    var projector = SignalboxProcessTranscriptProjector()

    let projection = try projector.projectAuthoritativeSnapshot(snapshot)
    let record = try XCTUnwrap(projection.records.first)
    let event = try ProcessProjectionFixture.conservativeEvent(in: record)

    XCTAssertEqual(
      event.diagnostic,
      ProcessProjectionFixture.boundedMalformedTurnStateDiagnostic
    )
  }

  func testUnknownTerminalDispositionRetainsAttribution() throws {
    let followed = try ProcessProjectionFixture.unknownDispositionModelCallEvent()
    let projector = SignalboxProcessTranscriptProjector()

    let event = try XCTUnwrap(projector.projectUnrecognizedFollowedEvent(followed))

    XCTAssertEqual(event.kind, ProcessProjectionFixture.unknownDispositionPresentationKind)
    XCTAssertEqual(
      event.diagnostic,
      ProcessProjectionFixture.unknownDispositionPresentationDiagnostic
    )
  }

  @MainActor
  func testUnknownTerminalDispositionAddsTimelineCard() async throws {
    let sessions = try await makeService().listSessions(includeArchived: false)
    let session = try fixtureSession(MockSignalboxFixtures.activeSessionID, in: sessions)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }

    viewModel.apply(.event(try ProcessProjectionFixture.unknownDispositionModelCallEvent()))
    let unknown = try ProcessProjectionFixture.onlyUnknownCard(in: viewModel.timeline)

    XCTAssertEqual(unknown.kind, ProcessProjectionFixture.unknownDispositionPresentationKind)
  }

  func testDuplicatePlanReadArgumentKeepsRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.duplicatePlanReadArgumentToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(
      tool.compactArgumentSummary,
      ProcessProjectionFixture.duplicatePlanReadArguments
    )
  }

  func testNullPlanReadHistoryFlagKeepsRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.nullPlanReadHistoryArgumentToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(
      tool.compactArgumentSummary,
      ProcessProjectionFixture.nullPlanReadHistoryArguments
    )
  }

  func testDuplicatePlanWriteArgumentKeepsRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.duplicatePlanWriteArgumentToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(
      tool.compactArgumentSummary,
      ProcessProjectionFixture.duplicatePlanWriteArguments
    )
  }

  func testMismatchedPlanWriteResultKeepsRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.mismatchedPlanWriteResultToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(tool.outputPreview, ProcessProjectionFixture.planCreateOutput)
  }

  func testMismatchedPlanWriteTextKeepsRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.mismatchedPlanWriteTextToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(tool.outputPreview, ProcessProjectionFixture.mismatchedPlanCreateOutput)
  }

  func testPlanReadResultBeforeRequestedCursorKeepsRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.planReadBeforeCursorToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(tool.outputPreview, ProcessProjectionFixture.planReadBeforeCursorOutput)
  }

  func testIncompletePlanHistoryKeepsRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.incompletePlanHistoryToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(tool.outputPreview, ProcessProjectionFixture.incompletePlanHistoryPreview)
  }

  func testPlanHistoryMismatchKeepsRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.mismatchedPlanHistoryToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(tool.outputPreview, ProcessProjectionFixture.mismatchedPlanHistoryPreview)
  }

  func testRepeatedPlanHistoryAttemptKeepsRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.repeatedPlanHistoryAttemptToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(tool.outputPreview, ProcessProjectionFixture.repeatedPlanHistoryAttemptPreview)
  }

  func testPlanHistoryDependencyOverflowKeepsRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.planHistoryDependencyOverflowToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(
      tool.outputPreview,
      ProcessProjectionFixture.planHistoryDependencyOverflowPreview
    )
  }

  func testUnrequestedPlanHistoryKeepsRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.unrequestedPlanHistoryToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(tool.outputPreview, ProcessProjectionFixture.emptyIncludedPlanHistoryOutput)
  }

  func testCyclicPlanHistoryKeepsRawEvidenceVisible() throws {
    let record = ProcessProjectionFixture.cyclicPlanHistoryToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(tool.outputPreview, ProcessProjectionFixture.cyclicPlanHistoryPreview)
  }

  func testDenseAcyclicPlanPageUsesTypedEvidence() throws {
    let record = ProcessProjectionFixture.denseAcyclicPlanToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    let typedOutput = try XCTUnwrap(tool.presentationOutput)
    XCTAssertGreaterThan(
      typedOutput.count,
      ProcessProjectionFixture.outputPreviewLimit
    )
    XCTAssertNotEqual(typedOutput, ProcessProjectionFixture.denseAcyclicPlanOutput)
    XCTAssertEqual(
      tool.outputPreview.count,
      ProcessProjectionFixture.outputPreviewLimit
        + ProcessProjectionFixture.outputPreviewTruncationMarker.count
    )
    XCTAssertTrue(
      tool.outputPreview.hasSuffix(ProcessProjectionFixture.outputPreviewTruncationMarker)
    )
    XCTAssertTrue(
      typedOutput.hasPrefix(
        tool.outputPreview.dropLast(
          ProcessProjectionFixture.outputPreviewTruncationMarker.count
        )
      )
    )
  }

  func testMultilinePlanTextCannotCreateSummaryLabels() throws {
    let record = ProcessProjectionFixture.multilinePlanCreateToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(
      tool.compactArgumentSummary,
      ProcessProjectionFixture.multilinePlanArgumentPresentation
    )
    XCTAssertEqual(
      tool.outputPreview,
      ProcessProjectionFixture.multilinePlanRawOutputPreview
    )
  }

  func testAlternatePlanNewlineScalarsCannotCreateSummaryLabels() throws {
    let record = ProcessProjectionFixture.alternateNewlinePlanCreateToolRecord()
    let normalizer = try SignalboxIncrementalEventNormalizer(records: [record])
    let tool = try ProcessProjectionFixture.onlyToolCard(in: normalizer.timelineItems)

    XCTAssertEqual(
      tool.compactArgumentSummary,
      ProcessProjectionFixture.alternateNewlinePlanArgumentPresentation
    )
    XCTAssertEqual(
      tool.outputPreview,
      ProcessProjectionFixture.alternateNewlinePlanRawOutputPreview
    )
  }
}

extension ProcessProjectionFixture {
  static let delegateModelSelection = "dddddddd-4444-4444-8444-444444444444"
  static let delegateModelCall = "eeeeeeee-4444-4444-8444-444444444444"
  static let delegateRationale = "The requested effect exceeds the delegated scope."
  static let delegateDenialLabel =
    "Denied by delegate; model selection \(delegateModelSelection); call \(delegateModelCall)"
  static let runnerID = "44444444-4444-4444-8444-444444444444"
  static let runnerSnapshotStatusLabel =
    "Runner \(runnerID) · pinned · health suspect · revision 3"
    + " · sandbox workspace-restricted · selected directory \"workspace/project\""
  static let runnerLossStatusLabel =
    "Runner \(runnerID) · runner_lost · revision 4 · sandbox workspace-restricted"
    + " · selected directory \"workspace/project\""
  static let runnerRelocationStatusLabel =
    "Runner \(runnerID) · working_directory_changed · revision 5"
    + " · sandbox workspace-restricted · selected directory \"workspace/new\\nproject\""
  static let runnerDefaultDirectoryStatusLabel =
    "Runner \(runnerID) · replaced · revision 6 · sandbox ambient · runner-default directory"
  static let futureSessionEventKind = "fixture_future_session_event"

  static func runnerProjection() throws -> SignalboxRunnerProjection {
    try SignalboxRunnerProjection(
      selector: .capabilityClass(
        name: SignalboxRunnerCapabilityClass(validating: "linux.workspace")
      ),
      runnerID: SignalboxCanonicalUUID(validating: runnerID),
      placementRevision: SignalboxCanonicalUInt64(rawValue: 3),
      sandboxProfile: .workspaceRestricted,
      credentialProfile: SignalboxRunnerCredentialProfileName(validating: "readonly"),
      repository: SignalboxRunnerRepositoryKey(validating: "primary"),
      workingDirectory: SignalboxRunnerWorkingDirectory(validating: "workspace/project"),
      connectionHealth: .suspect,
      state: .pinned
    )
  }

  static func snapshotWithRunner() throws -> SignalboxSynchronizationSnapshot {
    SignalboxSynchronizationSnapshot(
      sessionID: try ProcessDriverFixture.sessionID(),
      cursor: SignalboxCanonicalUInt64(rawValue: 8),
      runner: try runnerProjection(),
      records: []
    )
  }

  static func runnerLossTransition() throws -> ProcessRunnerTransition {
    ProcessRunnerTransition(
      runnerID: try SignalboxCanonicalUUID(validating: runnerID),
      placementRevision: SignalboxCanonicalUInt64(rawValue: 4),
      sandboxProfile: .workspaceRestricted,
      workingDirectory: try SignalboxRunnerWorkingDirectory(validating: "workspace/project"),
      state: .runnerLost
    )
  }

  static func runnerLossEvent() throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"runner_state_transition",
        "runner_id":"\(runnerID)",
        "placement_revision":"4",
        "sandbox_profile":"workspace-restricted",
        "working_directory":"workspace/project",
        "state":"runner_lost"
      }
      """
    )
  }

  static func runnerRelocationTransition() throws -> ProcessRunnerTransition {
    ProcessRunnerTransition(
      runnerID: try SignalboxCanonicalUUID(validating: runnerID),
      placementRevision: SignalboxCanonicalUInt64(rawValue: 5),
      sandboxProfile: .workspaceRestricted,
      workingDirectory: try SignalboxRunnerWorkingDirectory(
        validating: "workspace/new\nproject"
      ),
      state: .workingDirectoryChanged
    )
  }

  static func runnerDefaultDirectoryTransition() throws -> ProcessRunnerTransition {
    ProcessRunnerTransition(
      runnerID: try SignalboxCanonicalUUID(validating: runnerID),
      placementRevision: SignalboxCanonicalUInt64(rawValue: 6),
      sandboxProfile: .ambient,
      workingDirectory: nil,
      state: .replaced
    )
  }
  static let formerUsageCollisionEntryIndex = UInt64(7)
  static let disjointUsageAndSemanticPresentationIDs = [Int.min + 1, Int.min / 4]
  static let disjointUsageAndSemanticEventKinds = [
    "process_conservative", "process_model_call_usage",
  ]
  static let oversizedDiagnosticMemberName = String(
    repeating: "d",
    count: SignalboxProcessPresentation.maximumLabelUTF8Bytes + 1
  )
  static let boundedMalformedFollowedEventDiagnostic =
    SignalboxProcessPresentation.retainedLabel(
      "Invalid field value at event.\(oversizedDiagnosticMemberName)."
    )
  static let boundedMalformedTurnStateDiagnostic =
    SignalboxProcessPresentation.retainedLabel(
      "Turn \(ProcessDriverFixture.turn): Invalid field value at state."
        + "\(oversizedDiagnosticMemberName)."
    )
  static let unknownHistoryKind = "unrecognized_session_event_history_truncated"
  static let unknownHistoryDiagnostic =
    "Earlier unrecognized session events were removed to keep retained history bounded."
  static let unknownHistoryCapacity = Int(
    SignalboxProcessApplicationPolicy.nativeDefault.synchronization.eventBufferCapacity
      .maximumEvents
  )
  static let unknownSessionEventDiagnostic =
    "The session event kind is not rendered by this client."
  static let unknownNestedStateKind =
    "model_call_transition.state.\(unknownModelCallState)"
  static let unknownEventSideSnapshotTimelineKinds: [ProcessTimelineFixtureKind] = [
    .message, .unknown, .message, .tool,
  ]
  static let retainedRowTimelineKindsBeforeRemoval: [ProcessTimelineFixtureKind] = [
    .processEvidence, .unknown, .processEvidence,
  ]
  static let retainedRowTimelineKindsAfterRemoval: [ProcessTimelineFixtureKind] = [
    .unknown, .processEvidence,
  ]

  static func snapshotWithFormerUsageIdentityCollision() throws
    -> SignalboxSynchronizationSnapshot
  {
    let usageMessage = try message(
      modelUsageMessage(index: 0, modelCallID: ProcessDriverFixture.modelCall)
    )
    guard case .transcriptModelCallUsage(let usage) = usageMessage else {
      throw ProcessDriverUpdateRecorderError.missingFixtureEvent
    }
    let entryMessage = try message(
      """
      {
        "type":"transcript_entry",
        "entry_index":"\(formerUsageCollisionEntryIndex)",
        "source_session_id":"\(ProcessDriverFixture.session)",
        "entry_id":"\(importedFutureEntry)",
        "entry":{"type":"fixture_collision_entry"}
      }
      """
    )
    guard case .transcriptEntry(let entry) = entryMessage else {
      throw ProcessDriverUpdateRecorderError.missingFixtureEvent
    }
    return SignalboxSynchronizationSnapshot(
      sessionID: try SignalboxCanonicalUUID(validating: ProcessDriverFixture.session),
      cursor: SignalboxCanonicalUInt64(rawValue: 1),
      records: [.modelCallUsage(usage), .entry(entry)]
    )
  }

  static func malformedKnownFollowedEvent() throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"session_created",
        "\(oversizedDiagnosticMemberName)":true
      }
      """
    )
  }

  static func snapshotWithMalformedKnownTurnState() throws
    -> SignalboxSynchronizationSnapshot
  {
    let message = try message(
      """
      {
        "type":"transcript_turn",
        "turn_id":"\(ProcessDriverFixture.turn)",
        "acceptance_position":"1",
        "state":{
          "type":"completed",
          "terminal_frontier_id":"\(ProcessDriverFixture.frontier)",
          "terminal_attempt_id":"\(ProcessDriverFixture.attempt)",
          "terminal_model_call_id":"\(ProcessDriverFixture.modelCall)",
          "\(oversizedDiagnosticMemberName)":true
        }
      }
      """
    )
    guard case .transcriptTurn(let turn) = message else {
      throw ProcessDriverUpdateRecorderError.missingFixtureEvent
    }
    return SignalboxSynchronizationSnapshot(
      sessionID: try SignalboxCanonicalUUID(validating: ProcessDriverFixture.session),
      cursor: SignalboxCanonicalUInt64(rawValue: 1),
      records: [.turn(turn)]
    )
  }

  static func unknownSessionEvent(
    kind: String = oversizedUnknownState,
    cursor: UInt64 = 1
  ) throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"\(kind)",
        "retained_fixture_field":true
      }
      """,
      cursor: cursor
    )
  }

  static func delegateDenialEvent() throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"tool_approval_decided",
        "turn_id":"\(ProcessDriverFixture.turn)",
        "tool_request_id":"\(proposedToolRequest)",
        "decision":{"type":"deny","reason":null},
        "decider":{
          "type":"delegate",
          "model_selection_id":"\(delegateModelSelection)",
          "model_call_id":"\(delegateModelCall)"
        },
        "rationale":"\(delegateRationale)"
      }
      """
    )
  }

  static func snapshotWithAwaitingProposedTool() throws -> SignalboxSynchronizationSnapshot {
    let snapshot = try snapshotWithProposedTool()
    let message = try message(
      """
      {
        "type":"transcript_turn",
        "turn_id":"\(ProcessDriverFixture.turn)",
        "acceptance_position":"1",
        "state":{
          "type":"active_awaiting_tool_approval",
          "tool_request_id":"\(proposedToolRequest)"
        }
      }
      """
    )
    guard case .transcriptTurn(let turn) = message else {
      throw ProcessDriverUpdateRecorderError.missingFixtureMessage
    }
    return SignalboxSynchronizationSnapshot(
      sessionID: snapshot.sessionID,
      cursor: snapshot.cursor,
      records: [.turn(turn)] + snapshot.records
    )
  }

  static func userApprovalEvent() throws -> SignalboxFollowedSessionEvent {
    try followedEvent(
      """
      {
        "type":"tool_approval_decided",
        "turn_id":"\(ProcessDriverFixture.turn)",
        "tool_request_id":"\(closedToolID)",
        "decision":{"type":"approve"},
        "decider":{
          "type":"user",
          "command_id":"\(delegateModelSelection)"
        },
        "rationale":null
      }
      """
    )
  }

  @MainActor
  static func fillUnknownSessionEventHistory(
    in viewModel: ProcessSessionDetailViewModel
  ) throws {
    let capacity = SignalboxProcessApplicationPolicy.nativeDefault.synchronization
      .eventBufferCapacity.maximumEvents
    for cursor in 1...(capacity + 1) {
      viewModel.apply(
        .event(try unknownSessionEvent(kind: futureSessionEventKind, cursor: UInt64(cursor)))
      )
    }
  }

  @MainActor
  static func fillUnknownNestedStateHistory(
    in viewModel: ProcessSessionDetailViewModel
  ) throws {
    let capacity = SignalboxProcessApplicationPolicy.nativeDefault.synchronization
      .eventBufferCapacity.maximumEvents
    for cursor in 1...(capacity + 1) {
      viewModel.apply(
        .event(try unknownStateModelCallEvent(cursor: UInt64(cursor)))
      )
    }
  }

  static func boundaryAndNewestUnknownCards(
    in timeline: [SignalboxTimelineItem]
  ) throws -> (boundary: SignalboxUnknownEventCard, newest: SignalboxUnknownEventCard) {
    guard
      case .unknown(let boundary)? = timeline.first,
      case .unknown(let newest)? = timeline.last
    else {
      throw ProcessDriverUpdateRecorderError.expectedUnknownEvent
    }
    return (boundary, newest)
  }

  static func onlyUnknownCard(
    in timeline: [SignalboxTimelineItem]
  ) throws -> SignalboxUnknownEventCard {
    let unknownCards: [SignalboxUnknownEventCard] = timeline.compactMap {
      guard case .unknown(let unknown) = $0 else {
        return nil
      }
      return unknown
    }
    guard unknownCards.count == singleRecordCount, let unknown = unknownCards.first else {
      throw ProcessDriverUpdateRecorderError.expectedUnknownEvent
    }
    return unknown
  }

  static func timelineKinds(
    in timeline: [SignalboxTimelineItem]
  ) -> [ProcessTimelineFixtureKind] {
    timeline.map { item in
      switch item {
      case .message:
        return .message
      case .tool:
        return .tool
      case .processEvidence:
        return .processEvidence
      case .turnFailure:
        return .turnFailure
      case .unknown:
        return .unknown
      }
    }
  }

  static func transcriptRowKinds(
    in rows: [ProcessSessionDetailViewModel.TranscriptRow]
  ) -> [ProcessTranscriptRowFixtureKind] {
    rows.map { row in
      switch row {
      case .accepted:
        return .accepted
      case .timeline:
        return .timeline
      }
    }
  }
}

private enum ProcessTranscriptRowFixtureKind: Equatable {
  case accepted
  case timeline
}

private enum ProcessTimelineFixtureKind: Equatable {
  case message
  case tool
  case processEvidence
  case turnFailure
  case unknown
}
