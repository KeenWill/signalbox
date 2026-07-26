import Foundation
import XCTest

@testable import SignalboxNative

final class SessionSynchronizationTests: XCTestCase {
  func testScriptedTransportTraversesEverySynchronizationPhase() throws {
    let snapshotCursor = SynchronizationFixture.initialCursor
    var transport = try SynchronizationFixture.transport()

    let connectEffects = transport.send(.start)
    let helloEffects = transport.send(.connected(generation: 1))
    let historyEffects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.snapshotStart(cursor: snapshotCursor)
      )
    )
    let turnEffects = transport.send(
      .frame(generation: 1, message: try SynchronizationFixture.queuedTurn())
    )
    let textEffects = transport.send(
      .frame(generation: 1, message: try SynchronizationFixture.textEntry())
    )
    let contentEffects = transport.send(
      .frame(generation: 1, message: try SynchronizationFixture.content())
    )
    let replayEffects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.snapshotEnd(
          cursor: snapshotCursor,
          turnCount: 1,
          entryCount: 1
        )
      )
    )
    let steadyEffects = transport.send(.replayCompleted(generation: 1))
    let snapshot = try SynchronizationFixture.publishedSnapshot(in: replayEffects)

    XCTAssertEqual(
      connectEffects,
      [
        .openFollow(sessionID: try SynchronizationFixture.sessionID(), generation: 1),
        .armDeadline(
          token: .connect(generation: 1),
          duration: SynchronizationFixture.policy.deadlines.connect
        ),
      ]
    )
    XCTAssertEqual(
      helloEffects,
      [
        .cancelDeadline(.connect(generation: 1)),
        .armDeadline(
          token: .hello(generation: 1),
          duration: SynchronizationFixture.policy.deadlines.hello
        ),
      ]
    )
    XCTAssertEqual(
      historyEffects,
      [
        .cancelDeadline(.hello(generation: 1)),
        .armDeadline(
          token: .history(generation: 1),
          duration: SynchronizationFixture.policy.deadlines.history
        ),
      ]
    )
    XCTAssertTrue(turnEffects.isEmpty)
    XCTAssertTrue(textEffects.isEmpty)
    XCTAssertTrue(contentEffects.isEmpty)
    XCTAssertEqual(snapshot.cursor.rawValue, snapshotCursor)
    XCTAssertEqual(snapshot.records.count, 3)
    XCTAssertEqual(
      steadyEffects,
      [.cancelDeadline(.replay(generation: 1))]
    )
    XCTAssertEqual(
      transport.machine.phase,
      .steady(
        generation: 1,
        cursor: SignalboxCanonicalUInt64(rawValue: snapshotCursor),
        refreshID: nil
      )
    )
  }

  func testS24INV032ReplayDeduplicatesSnapshotCursor() throws {
    let snapshotCursor = SynchronizationFixture.initialCursor
    let laterCursor = SynchronizationFixture.laterCursor
    var transport = try SynchronizationFixture.transportAtReplay(cursor: snapshotCursor)

    let duplicateEffects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.inputAcceptedEvent(cursor: snapshotCursor)
      )
    )
    let laterEffects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.inputAcceptedEvent(cursor: laterCursor)
      )
    )
    let replayEffects = transport.send(.replayCompleted(generation: 1))

    XCTAssertTrue(duplicateEffects.isEmpty)
    XCTAssertEqual(laterEffects.count, 0)
    XCTAssertEqual(
      SynchronizationFixture.publishedEventCursors(in: replayEffects),
      [laterCursor]
    )
    XCTAssertTrue(transport.machine.diagnostics.isEmpty)
    XCTAssertEqual(
      transport.machine.phase,
      .steady(
        generation: 1,
        cursor: SignalboxCanonicalUInt64(rawValue: laterCursor),
        refreshID: nil
      )
    )
  }

  func testReplayPreservesFutureUnknownEvent() throws {
    let snapshotCursor = SynchronizationFixture.initialCursor
    let unknownCursor = SynchronizationFixture.unknownCursor
    var transport = try SynchronizationFixture.transportAtReplay(cursor: snapshotCursor)

    let unknownEffects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.unknownEvent(cursor: unknownCursor)
      )
    )
    let replayEffects = transport.send(.replayCompleted(generation: 1))

    XCTAssertEqual(
      SynchronizationFixture.effectNames(unknownEffects),
      ["report_diagnostic"]
    )
    XCTAssertEqual(
      SynchronizationFixture.publishedEventCursors(in: replayEffects),
      [unknownCursor]
    )
    XCTAssertEqual(transport.machine.diagnostics.last?.kind, .decoding)
  }

  func testINV033MalformedKnownEventRecoversBeforeAdvancingCursor() throws {
    let eventCursor = SynchronizationFixture.unknownCursor
    var transport = try SynchronizationFixture.synchronizedTransport(
      cursor: SynchronizationFixture.initialCursor
    )

    let effects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.malformedKnownEvent(cursor: eventCursor)
      )
    )

    XCTAssertEqual(
      SynchronizationFixture.effectNames(effects),
      ["close_follow", "report_diagnostic", "schedule_reconnect"]
    )
    XCTAssertEqual(
      transport.machine.phase,
      SynchronizationFixture.firstRecovery(failedStage: .steady)
    )
  }

  func testINV033MalformedKnownEventInReplayRecoversWithoutBuffering() throws {
    var transport = try SynchronizationFixture.transportAtReplay(
      cursor: SynchronizationFixture.initialCursor
    )

    let effects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.malformedKnownEvent(
          cursor: SynchronizationFixture.unknownCursor
        )
      )
    )

    XCTAssertTrue(SynchronizationFixture.containsRetrySchedule(effects))
    XCTAssertEqual(
      transport.machine.phase,
      SynchronizationFixture.firstRecovery(failedStage: .replay)
    )
  }

  func testINV033MalformedSnapshotStartEntersHelloRecovery() throws {
    var transport = try SynchronizationFixture.transport()

    _ = transport.send(.start)
    _ = transport.send(.connected(generation: 1))
    let effects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.malformedSnapshotStart()
      )
    )

    XCTAssertEqual(
      SynchronizationFixture.effectNames(effects),
      ["cancel_deadline", "close_follow", "report_diagnostic", "schedule_reconnect"]
    )
    XCTAssertEqual(
      transport.machine.phase,
      .recovery(failedStage: .hello, failureCount: 1, nextGeneration: 2)
    )
  }

  func testINV033MalformedSideSnapshotStartEntersSideHistoryRecovery() throws {
    var transport = try SynchronizationFixture.synchronizedTransport(
      cursor: SynchronizationFixture.initialCursor
    )

    _ = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.completedEvent(cursor: 20)
      )
    )
    let effects = transport.send(
      .sideFrame(
        generation: 1,
        refreshID: 1,
        message: try SynchronizationFixture.malformedSnapshotStart()
      )
    )

    XCTAssertTrue(SynchronizationFixture.containsRetrySchedule(effects))
    XCTAssertEqual(
      transport.machine.phase,
      .recovery(failedStage: .sideHistory, failureCount: 1, nextGeneration: 2)
    )
  }

  func testINV033MalformedNestedTurnFailsSnapshotClosed() throws {
    var transport = try SynchronizationFixture.transportInHistory(
      cursor: SynchronizationFixture.initialCursor
    )

    let effects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.malformedQueuedTurn()
      )
    )

    XCTAssertFalse(SynchronizationFixture.containsPublishedSnapshot(effects))
    XCTAssertEqual(
      transport.machine.phase,
      .recovery(failedStage: .history, failureCount: 1, nextGeneration: 2)
    )
  }

  func testINV033UnadmittedNestedTurnFieldFailsSnapshotClosed() throws {
    var transport = try SynchronizationFixture.transportInHistory(
      cursor: SynchronizationFixture.initialCursor
    )

    let effects = transport.send(
      .frame(
        generation: SynchronizationFixture.initialGeneration,
        message: try SynchronizationFixture.queuedTurnWithUnadmittedField()
      )
    )

    XCTAssertFalse(SynchronizationFixture.containsPublishedSnapshot(effects))
    XCTAssertEqual(
      transport.machine.phase,
      SynchronizationFixture.firstRecovery(failedStage: .history)
    )
  }

  func testINV033MalformedNestedEntryFailsSnapshotClosed() throws {
    var transport = try SynchronizationFixture.transportInHistory(
      cursor: SynchronizationFixture.initialCursor
    )

    let effects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.malformedTranscriptEntry()
      )
    )

    XCTAssertFalse(SynchronizationFixture.containsPublishedSnapshot(effects))
    XCTAssertEqual(
      transport.machine.phase,
      .recovery(failedStage: .history, failureCount: 1, nextGeneration: 2)
    )
  }

  func testINV033MalformedNestedTextEntryFailsSnapshotClosed() throws {
    var transport = try SynchronizationFixture.transportInHistory(
      cursor: SynchronizationFixture.initialCursor
    )

    let effects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.malformedTextEntry()
      )
    )

    XCTAssertFalse(SynchronizationFixture.containsPublishedSnapshot(effects))
    XCTAssertEqual(
      transport.machine.phase,
      .recovery(failedStage: .history, failureCount: 1, nextGeneration: 2)
    )
  }

  func testINV033FutureTurnStateFailsAuthoritativeSnapshotClosed() throws {
    var transport = try SynchronizationFixture.transportInHistory(
      cursor: SynchronizationFixture.initialCursor
    )

    let effects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.futureTurnState()
      )
    )

    XCTAssertFalse(SynchronizationFixture.containsPublishedSnapshot(effects))
    XCTAssertEqual(
      transport.machine.phase,
      SynchronizationFixture.firstRecovery(failedStage: .history)
    )
  }

  func testINV033FutureNestedModelCallStateFailsAuthoritativeSnapshotClosed() throws {
    var transport = try SynchronizationFixture.transportInHistory(
      cursor: SynchronizationFixture.initialCursor
    )

    let effects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.futureCurrentModelCallState()
      )
    )

    XCTAssertFalse(SynchronizationFixture.containsPublishedSnapshot(effects))
    XCTAssertEqual(
      transport.machine.phase,
      SynchronizationFixture.firstRecovery(failedStage: .history)
    )
  }

  func testINV033UnadmittedCurrentModelCallFieldFailsSnapshotClosed() throws {
    var transport = try SynchronizationFixture.transportInHistory(
      cursor: SynchronizationFixture.initialCursor
    )

    let effects = transport.send(
      .frame(
        generation: SynchronizationFixture.initialGeneration,
        message: try SynchronizationFixture.currentModelCallWithUnadmittedField()
      )
    )

    XCTAssertFalse(SynchronizationFixture.containsPublishedSnapshot(effects))
    XCTAssertEqual(
      transport.machine.phase,
      SynchronizationFixture.firstRecovery(failedStage: .history)
    )
  }

  func testINV033UnadmittedTerminalModelCallFieldFailsSnapshotClosed() throws {
    var transport = try SynchronizationFixture.transportInHistory(
      cursor: SynchronizationFixture.initialCursor
    )

    let effects = transport.send(
      .frame(
        generation: SynchronizationFixture.initialGeneration,
        message: try SynchronizationFixture.terminalModelCallWithUnadmittedField()
      )
    )

    XCTAssertFalse(SynchronizationFixture.containsPublishedSnapshot(effects))
    XCTAssertEqual(
      transport.machine.phase,
      SynchronizationFixture.firstRecovery(failedStage: .history)
    )
  }

  func testINV033FailedTurnWithCallWithoutAttemptFailsSnapshotClosed() throws {
    var transport = try SynchronizationFixture.transportInHistory(
      cursor: SynchronizationFixture.initialCursor
    )

    let effects = transport.send(
      .frame(
        generation: SynchronizationFixture.initialGeneration,
        message: try SynchronizationFixture.failedTurnWithCallWithoutAttempt()
      )
    )

    XCTAssertFalse(SynchronizationFixture.containsPublishedSnapshot(effects))
    XCTAssertEqual(
      transport.machine.phase,
      SynchronizationFixture.firstRecovery(failedStage: .history)
    )
  }

  func testINV033ActiveRunningRequiresNullableModelCallMember() throws {
    var transport = try SynchronizationFixture.transportInHistory(
      cursor: SynchronizationFixture.initialCursor
    )

    let effects = transport.send(
      .frame(
        generation: SynchronizationFixture.initialGeneration,
        message: try SynchronizationFixture.activeRunningWithoutModelCallMember()
      )
    )

    XCTAssertFalse(SynchronizationFixture.containsPublishedSnapshot(effects))
    XCTAssertEqual(
      transport.machine.phase,
      SynchronizationFixture.firstRecovery(failedStage: .history)
    )
  }

  func testINV033FailedTurnRequiresNullableTerminalMembers() throws {
    var transport = try SynchronizationFixture.transportInHistory(
      cursor: SynchronizationFixture.initialCursor
    )

    let effects = transport.send(
      .frame(
        generation: SynchronizationFixture.initialGeneration,
        message: try SynchronizationFixture.failedTurnWithoutNullableMembers()
      )
    )

    XCTAssertFalse(SynchronizationFixture.containsPublishedSnapshot(effects))
    XCTAssertEqual(
      transport.machine.phase,
      SynchronizationFixture.firstRecovery(failedStage: .history)
    )
  }

  func testINV033CancelledTurnRequiresNullableModelCallMember() throws {
    var transport = try SynchronizationFixture.transportInHistory(
      cursor: SynchronizationFixture.initialCursor
    )

    let effects = transport.send(
      .frame(
        generation: SynchronizationFixture.initialGeneration,
        message: try SynchronizationFixture.cancelledTurnWithoutModelCallMember()
      )
    )

    XCTAssertFalse(SynchronizationFixture.containsPublishedSnapshot(effects))
    XCTAssertEqual(
      transport.machine.phase,
      SynchronizationFixture.firstRecovery(failedStage: .history)
    )
  }

  func testINV033FutureEntryVariantFailsAuthoritativeSnapshotClosed() throws {
    var transport = try SynchronizationFixture.transportInHistory(
      cursor: SynchronizationFixture.initialCursor
    )

    let effects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.futureTranscriptEntry()
      )
    )

    XCTAssertFalse(SynchronizationFixture.containsPublishedSnapshot(effects))
    XCTAssertEqual(
      transport.machine.phase,
      SynchronizationFixture.firstRecovery(failedStage: .history)
    )
  }

  func testOversizedContentFragmentFailsAuthoritativeSnapshotClosed() throws {
    var transport = try SynchronizationFixture.transportInHistory(
      cursor: SynchronizationFixture.initialCursor
    )

    _ = transport.send(
      .frame(generation: 1, message: try SynchronizationFixture.textEntry())
    )
    let effects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.oversizedContent()
      )
    )

    XCTAssertFalse(SynchronizationFixture.containsPublishedSnapshot(effects))
    XCTAssertEqual(
      transport.machine.phase,
      SynchronizationFixture.firstRecovery(failedStage: .history)
    )
  }

  func testSessionEventBeforeSnapshotEndFailsFrameOrderClosed() throws {
    var transport = try SynchronizationFixture.transportInHistory(
      cursor: SynchronizationFixture.initialCursor
    )

    let effects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.inputAcceptedEvent(
          cursor: SynchronizationFixture.unknownCursor
        )
      )
    )

    XCTAssertFalse(SynchronizationFixture.containsPublishedSnapshot(effects))
    XCTAssertEqual(
      transport.machine.phase,
      .recovery(failedStage: .history, failureCount: 1, nextGeneration: 2)
    )
  }

  func testINV033MalformedLiveEventEnvelopeReconnectsForFreshSnapshot() throws {
    var transport = try SynchronizationFixture.synchronizedTransport(
      cursor: SynchronizationFixture.initialCursor
    )

    let effects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.malformedSessionEventEnvelope()
      )
    )

    XCTAssertTrue(SynchronizationFixture.containsRetrySchedule(effects))
    XCTAssertEqual(
      transport.machine.phase,
      .recovery(failedStage: .steady, failureCount: 1, nextGeneration: 2)
    )
  }

  func testINV033UnknownModelCallStateCannotAdvanceReplayCursor() throws {
    var transport = try SynchronizationFixture.transportAtReplay(
      cursor: SynchronizationFixture.initialCursor
    )

    let effects = transport.send(
      .frame(
        generation: SynchronizationFixture.initialGeneration,
        message: try SynchronizationFixture.unknownModelCallStateEvent(
          cursor: SynchronizationFixture.unknownCursor
        )
      )
    )

    XCTAssertFalse(SynchronizationFixture.containsPublishedEvent(effects))
    XCTAssertEqual(
      transport.machine.phase,
      SynchronizationFixture.firstRecovery(failedStage: .replay)
    )
  }

  func testINV033UnknownToolBatchStateCannotAdvanceSteadyCursor() throws {
    var transport = try SynchronizationFixture.synchronizedTransport(
      cursor: SynchronizationFixture.initialCursor
    )

    let effects = transport.send(
      .frame(
        generation: SynchronizationFixture.initialGeneration,
        message: try SynchronizationFixture.unknownToolBatchStateEvent(
          cursor: SynchronizationFixture.unknownCursor
        )
      )
    )

    XCTAssertFalse(SynchronizationFixture.containsPublishedEvent(effects))
    XCTAssertEqual(
      transport.machine.phase,
      SynchronizationFixture.firstRecovery(failedStage: .steady)
    )
  }

  func testRetainedDiagnosticHistoryIsBounded() throws {
    var transport = try SynchronizationFixture.transport()

    _ = transport.send(.start)
    _ = transport.send(.connected(generation: 1))
    try SynchronizationFixture.reportMoreThanRetainedDiagnosticCapacity(to: &transport)

    XCTAssertEqual(
      transport.machine.diagnostics.count,
      SignalboxSessionSynchronizationMachine.maximumRetainedDiagnostics
    )
    XCTAssertEqual(transport.machine.phase, .hello(generation: 1, reconnectAttempt: 0))
  }

  func testUnknownFrameDoesNotDiscardOtherwiseValidSnapshotPage() throws {
    let snapshotCursor = SynchronizationFixture.initialCursor
    var transport = try SynchronizationFixture.transportInHistory(cursor: snapshotCursor)

    let diagnosticEffects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.unknownTopLevelMessage()
      )
    )
    let replayEffects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.snapshotEnd(
          cursor: snapshotCursor,
          turnCount: 0,
          entryCount: 0
        )
      )
    )

    XCTAssertEqual(
      SynchronizationFixture.effectNames(diagnosticEffects),
      ["report_diagnostic"]
    )
    XCTAssertTrue(SynchronizationFixture.containsPublishedSnapshot(replayEffects))
    XCTAssertEqual(
      transport.machine.phase,
      .replay(generation: 1, cursor: SignalboxCanonicalUInt64(rawValue: snapshotCursor))
    )
  }

  func testFreshSideSnapshotMergesBeforeBufferedStreamEvent() throws {
    let bufferedCursor = SynchronizationFixture.sideBufferedCursor
    var transport = try SynchronizationFixture.synchronizedTransport(
      cursor: SynchronizationFixture.initialCursor
    )

    let triggerEffects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.completedEvent(cursor: 20)
      )
    )
    let bufferedEffects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.inputAcceptedEvent(cursor: bufferedCursor)
      )
    )
    let sideStartEffects = transport.send(
      .sideFrame(
        generation: 1,
        refreshID: 1,
        message: try SynchronizationFixture.snapshotStart(cursor: 20)
      )
    )
    let sideEndEffects = transport.send(
      .sideFrame(
        generation: 1,
        refreshID: 1,
        message: try SynchronizationFixture.snapshotEnd(
          cursor: 20,
          turnCount: 0,
          entryCount: 0
        )
      )
    )

    XCTAssertEqual(
      SynchronizationFixture.effectNames(triggerEffects),
      ["publish_event", "request_side_snapshot", "arm_deadline"]
    )
    XCTAssertTrue(bufferedEffects.isEmpty)
    XCTAssertTrue(sideStartEffects.isEmpty)
    XCTAssertEqual(
      SynchronizationFixture.effectNames(sideEndEffects),
      ["cancel_deadline", "merge_side_snapshot", "publish_event"]
    )
    XCTAssertEqual(
      SynchronizationFixture.publishedEventCursors(in: sideEndEffects),
      [bufferedCursor]
    )
    XCTAssertEqual(
      transport.machine.phase,
      .steady(
        generation: 1,
        cursor: SignalboxCanonicalUInt64(rawValue: bufferedCursor),
        refreshID: nil
      )
    )
  }

  func testUnknownEventRequestsBoundedSideSnapshotRefresh() throws {
    var transport = try SynchronizationFixture.synchronizedTransport(
      cursor: SynchronizationFixture.initialCursor
    )

    let effects = transport.send(
      .frame(
        generation: SynchronizationFixture.initialGeneration,
        message: try SynchronizationFixture.unknownEvent(
          cursor: SynchronizationFixture.unknownCursor
        )
      )
    )

    XCTAssertEqual(
      SynchronizationFixture.effectNames(effects),
      ["report_diagnostic", "publish_event", "request_side_snapshot", "arm_deadline"]
    )
    XCTAssertEqual(
      transport.machine.phase,
      SynchronizationFixture.steady(
        cursor: SynchronizationFixture.unknownCursor,
        refreshID: SynchronizationFixture.firstRefreshID
      )
    )
  }

  func testNewerLiveEventCannotSuppressReplayQueuedBehindSideSnapshot() throws {
    let triggerCursor = SynchronizationFixture.sideRefreshTriggerCursor
    let firstReplayCursor = SynchronizationFixture.sideBufferedCursor
    let secondReplayCursor = SynchronizationFixture.secondSideBufferedCursor
    let liveCursor = SynchronizationFixture.liveDuringReplaySideRefreshCursor
    var transport = try SynchronizationFixture.transportAtReplay(
      cursor: SynchronizationFixture.initialCursor
    )

    _ = transport.send(
      .frame(
        generation: SynchronizationFixture.initialGeneration,
        message: try SynchronizationFixture.completedEvent(cursor: triggerCursor)
      )
    )
    _ = transport.send(
      .frame(
        generation: SynchronizationFixture.initialGeneration,
        message: try SynchronizationFixture.inputAcceptedEvent(cursor: firstReplayCursor)
      )
    )
    _ = transport.send(
      .frame(
        generation: SynchronizationFixture.initialGeneration,
        message: try SynchronizationFixture.inputAcceptedEvent(cursor: secondReplayCursor)
      )
    )
    let replayEffects = transport.send(
      .replayCompleted(generation: SynchronizationFixture.initialGeneration)
    )
    let liveEffects = transport.send(
      .frame(
        generation: SynchronizationFixture.initialGeneration,
        message: try SynchronizationFixture.inputAcceptedEvent(cursor: liveCursor)
      )
    )
    _ = transport.send(
      .sideFrame(
        generation: SynchronizationFixture.initialGeneration,
        refreshID: SynchronizationFixture.firstRefreshID,
        message: try SynchronizationFixture.snapshotStart(cursor: liveCursor)
      )
    )
    let sideEndEffects = transport.send(
      .sideFrame(
        generation: SynchronizationFixture.initialGeneration,
        refreshID: SynchronizationFixture.firstRefreshID,
        message: try SynchronizationFixture.snapshotEnd(
          cursor: liveCursor,
          turnCount: 0,
          entryCount: 0
        )
      )
    )

    XCTAssertEqual(
      SynchronizationFixture.publishedEventCursors(in: replayEffects),
      [triggerCursor]
    )
    XCTAssertTrue(liveEffects.isEmpty)
    XCTAssertEqual(
      SynchronizationFixture.publishedEventCursors(in: sideEndEffects),
      [firstReplayCursor, secondReplayCursor, liveCursor]
    )
    XCTAssertEqual(
      transport.machine.phase,
      SynchronizationFixture.steady(cursor: liveCursor)
    )
  }

  func testBufferedEventCountCapacityEntersBoundedRecovery() throws {
    var transport = try SynchronizationFixture.synchronizedTransport(
      cursor: SynchronizationFixture.initialCursor,
      eventBufferCapacity: SynchronizationFixture.eventCountCapacity(maximumEvents: 1)
    )

    _ = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.completedEvent(
          cursor: SynchronizationFixture.sideRefreshTriggerCursor
        )
      )
    )
    let admitted = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.inputAcceptedEvent(
          cursor: SynchronizationFixture.sideBufferedCursor
        )
      )
    )
    let overflow = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.inputAcceptedEvent(
          cursor: SynchronizationFixture.secondSideBufferedCursor
        )
      )
    )

    XCTAssertTrue(admitted.isEmpty)
    XCTAssertTrue(SynchronizationFixture.containsRetrySchedule(overflow))
    XCTAssertEqual(
      transport.machine.phase,
      SynchronizationFixture.firstRecovery(failedStage: .steady)
    )
  }

  func testBufferedEventUTF8CapacityEntersBoundedRecovery() throws {
    var transport = try SynchronizationFixture.synchronizedTransport(
      cursor: SynchronizationFixture.initialCursor,
      eventBufferCapacity: SynchronizationFixture.eventByteCapacity(maximumUTF8Bytes: 0)
    )

    _ = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.completedEvent(
          cursor: SynchronizationFixture.sideRefreshTriggerCursor
        )
      )
    )
    let effects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.inputAcceptedEvent(
          cursor: SynchronizationFixture.sideBufferedCursor
        )
      )
    )

    XCTAssertTrue(SynchronizationFixture.containsRetrySchedule(effects))
    XCTAssertEqual(
      transport.machine.phase,
      SynchronizationFixture.firstRecovery(failedStage: .steady)
    )
  }

  func testFutureEventJSONNodesCountTowardBufferUTF8Capacity() throws {
    var transport = try SynchronizationFixture.synchronizedTransport(
      cursor: SynchronizationFixture.initialCursor,
      eventBufferCapacity: SynchronizationFixture.eventByteCapacity(
        maximumUTF8Bytes: SynchronizationFixture.capacityBelowEncodedFutureEvent
      )
    )

    _ = transport.send(
      .frame(
        generation: SynchronizationFixture.initialGeneration,
        message: try SynchronizationFixture.completedEvent(
          cursor: SynchronizationFixture.sideRefreshTriggerCursor
        )
      )
    )
    let effects = transport.send(
      .frame(
        generation: SynchronizationFixture.initialGeneration,
        message: try SynchronizationFixture.unknownEventWithNullNodes(
          cursor: SynchronizationFixture.sideBufferedCursor
        )
      )
    )

    XCTAssertTrue(SynchronizationFixture.containsRetrySchedule(effects))
    XCTAssertEqual(
      transport.machine.phase,
      SynchronizationFixture.firstRecovery(failedStage: .steady)
    )
  }

  func testStaleSideSnapshotCannotMergeOrClobberStreamState() throws {
    var transport = try SynchronizationFixture.synchronizedTransport(cursor: 10)

    _ = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.completedEvent(cursor: 20)
      )
    )
    _ = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.inputAcceptedEvent(cursor: 21)
      )
    )
    _ = transport.send(
      .sideFrame(
        generation: 1,
        refreshID: 1,
        message: try SynchronizationFixture.snapshotStart(cursor: 19)
      )
    )
    let staleEffects = transport.send(
      .sideFrame(
        generation: 1,
        refreshID: 1,
        message: try SynchronizationFixture.snapshotEnd(
          cursor: 19,
          turnCount: 0,
          entryCount: 0
        )
      )
    )

    XCTAssertFalse(SynchronizationFixture.containsSideMerge(staleEffects))
    XCTAssertEqual(
      transport.machine.phase,
      .recovery(failedStage: .sideHistory, failureCount: 1, nextGeneration: 2)
    )
    XCTAssertEqual(transport.machine.diagnostics.first?.kind, .staleSnapshot)
  }

  func testConnectDeadlineUsesFiniteBackoffAndStopsAtRetryCap() throws {
    var transport = try SynchronizationFixture.transport()

    _ = transport.send(.start)
    let firstFailure = transport.send(
      .deadlineExpired(.connect(generation: 1))
    )
    let secondConnect = transport.send(.retryReady(generation: 2))
    let secondFailure = transport.send(
      .deadlineExpired(.connect(generation: 2))
    )
    let thirdConnect = transport.send(.retryReady(generation: 3))
    let exhausted = transport.send(
      .deadlineExpired(.connect(generation: 3))
    )

    XCTAssertEqual(
      SynchronizationFixture.reconnectDelay(in: firstFailure),
      SynchronizationFixture.policy.retry.delays.first
    )
    XCTAssertEqual(
      secondConnect.first,
      .openFollow(sessionID: try SynchronizationFixture.sessionID(), generation: 2)
    )
    XCTAssertEqual(
      SynchronizationFixture.reconnectDelay(in: secondFailure),
      SynchronizationFixture.policy.retry.delays.last
    )
    XCTAssertEqual(
      thirdConnect.first,
      .openFollow(sessionID: try SynchronizationFixture.sessionID(), generation: 3)
    )
    XCTAssertTrue(SynchronizationFixture.containsRetryLimit(exhausted))
    XCTAssertEqual(
      transport.machine.phase,
      .recovery(failedStage: .connect, failureCount: 3, nextGeneration: nil)
    )
  }

  func testHelloDeadlineUsesTheSameBoundedRecoveryPath() throws {
    var transport = try SynchronizationFixture.transport()

    _ = transport.send(.start)
    _ = transport.send(.connected(generation: 1))
    let effects = transport.send(
      .deadlineExpired(.hello(generation: 1))
    )

    XCTAssertEqual(
      SynchronizationFixture.reconnectDelay(in: effects),
      SynchronizationFixture.policy.retry.delays.first
    )
    XCTAssertEqual(
      transport.machine.phase,
      .recovery(failedStage: .hello, failureCount: 1, nextGeneration: 2)
    )
  }

  func testHistoryDeadlineUsesTheSameBoundedRecoveryPath() throws {
    var transport = try SynchronizationFixture.transport()

    _ = transport.send(.start)
    _ = transport.send(.connected(generation: 1))
    _ = transport.send(
      .frame(generation: 1, message: try SynchronizationFixture.snapshotStart(cursor: 10))
    )
    let effects = transport.send(
      .deadlineExpired(.history(generation: 1))
    )

    XCTAssertEqual(
      SynchronizationFixture.reconnectDelay(in: effects),
      SynchronizationFixture.policy.retry.delays.first
    )
    XCTAssertEqual(
      transport.machine.phase,
      .recovery(failedStage: .history, failureCount: 1, nextGeneration: 2)
    )
  }

  func testReplayDeadlineUsesTheSameBoundedRecoveryPath() throws {
    var transport = try SynchronizationFixture.transportAtReplay(cursor: 10)

    let effects = transport.send(
      .deadlineExpired(.replay(generation: 1))
    )

    XCTAssertEqual(
      SynchronizationFixture.reconnectDelay(in: effects),
      SynchronizationFixture.policy.retry.delays.first
    )
    XCTAssertEqual(
      transport.machine.phase,
      .recovery(failedStage: .replay, failureCount: 1, nextGeneration: 2)
    )
  }

  func testSideHistoryDeadlineUsesTheSameBoundedRecoveryPath() throws {
    var transport = try SynchronizationFixture.synchronizedTransport(cursor: 10)

    _ = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.completedEvent(cursor: 20)
      )
    )
    let effects = transport.send(
      .deadlineExpired(.sideHistory(generation: 1, refreshID: 1))
    )

    XCTAssertEqual(
      SynchronizationFixture.reconnectDelay(in: effects),
      SynchronizationFixture.policy.retry.delays.first
    )
    XCTAssertEqual(
      transport.machine.phase,
      .recovery(failedStage: .sideHistory, failureCount: 1, nextGeneration: 2)
    )
  }

  func testSteadyDisconnectUsesTheSameBoundedRecoveryPath() throws {
    var transport = try SynchronizationFixture.synchronizedTransport(cursor: 10)

    let effects = transport.send(
      .transportEnded(generation: 1, message: "fixture disconnect")
    )

    XCTAssertEqual(
      SynchronizationFixture.reconnectDelay(in: effects),
      SynchronizationFixture.policy.retry.delays.first
    )
    XCTAssertEqual(
      transport.machine.phase,
      .recovery(failedStage: .steady, failureCount: 1, nextGeneration: 2)
    )
  }

  func testSuccessfulReplayResetsRetryBudgetForLaterDisconnect() throws {
    var transport = try SynchronizationFixture.transport()

    _ = transport.send(.start)
    _ = transport.send(
      .transportEnded(generation: 1, message: "fixture connect failure")
    )
    _ = transport.send(.retryReady(generation: 2))
    _ = transport.send(.connected(generation: 2))
    _ = transport.send(
      .frame(generation: 2, message: try SynchronizationFixture.snapshotStart(cursor: 30))
    )
    _ = transport.send(
      .frame(
        generation: 2,
        message: try SynchronizationFixture.snapshotEnd(
          cursor: 30,
          turnCount: 0,
          entryCount: 0
        )
      )
    )
    _ = transport.send(.replayCompleted(generation: 2))
    let effects = transport.send(
      .transportEnded(generation: 2, message: "fixture steady disconnect")
    )

    XCTAssertEqual(
      SynchronizationFixture.reconnectDelay(in: effects),
      SynchronizationFixture.policy.retry.delays.first
    )
    XCTAssertEqual(
      transport.machine.phase,
      .recovery(failedStage: .steady, failureCount: 1, nextGeneration: 3)
    )
  }

  func testDiagnosticSurvivesFallbackAndSuccessfulReconnect() throws {
    let recoveredCursor = SynchronizationFixture.recoveredCursor
    var transport = try SynchronizationFixture.transport()

    _ = transport.send(.start)
    _ = transport.send(.connected(generation: 1))
    _ = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.unknownTopLevelMessage()
      )
    )
    _ = transport.send(.deadlineExpired(.hello(generation: 1)))
    _ = transport.send(.retryReady(generation: 2))
    _ = transport.send(.connected(generation: 2))
    _ = transport.send(
      .frame(
        generation: 2,
        message: try SynchronizationFixture.snapshotStart(cursor: recoveredCursor)
      )
    )
    _ = transport.send(
      .frame(
        generation: 2,
        message: try SynchronizationFixture.snapshotEnd(
          cursor: recoveredCursor,
          turnCount: 0,
          entryCount: 0
        )
      )
    )
    _ = transport.send(.replayCompleted(generation: 2))

    XCTAssertEqual(transport.machine.diagnostics.count, 2)
    XCTAssertEqual(transport.machine.diagnostics.first?.kind, .decoding)
    XCTAssertEqual(
      transport.machine.phase,
      .steady(
        generation: 2,
        cursor: SignalboxCanonicalUInt64(rawValue: recoveredCursor),
        refreshID: nil
      )
    )
  }

  func testStaleCompletionCannotMutateCurrentReconnectAttempt() throws {
    var transport = try SynchronizationFixture.transport()

    _ = transport.send(.start)
    _ = transport.send(
      .transportEnded(generation: 1, message: "fixture disconnect")
    )
    _ = transport.send(.retryReady(generation: 2))
    let staleEffects = transport.send(
      .transportEnded(generation: 1, message: "late fixture completion")
    )

    XCTAssertEqual(
      transport.machine.phase,
      .connect(generation: 2, reconnectAttempt: 1)
    )
    XCTAssertEqual(transport.machine.diagnostics.last?.kind, .staleCompletion)
    XCTAssertEqual(
      SynchronizationFixture.effectNames(staleEffects),
      ["report_diagnostic"]
    )
  }

  func testDuplicateTransportCompletionCannotConsumeAnotherRetry() throws {
    var transport = try SynchronizationFixture.transport()

    _ = transport.send(.start)
    _ = transport.send(
      .transportEnded(generation: 1, message: "fixture disconnect")
    )
    let duplicateEffects = transport.send(
      .transportEnded(generation: 1, message: "duplicate fixture completion")
    )

    XCTAssertEqual(
      transport.machine.phase,
      .recovery(failedStage: .connect, failureCount: 1, nextGeneration: 2)
    )
    XCTAssertEqual(transport.machine.diagnostics.last?.kind, .staleCompletion)
    XCTAssertEqual(
      SynchronizationFixture.effectNames(duplicateEffects),
      ["report_diagnostic"]
    )
  }

  func testStoppingRecoveryCancelsScheduledReconnect() throws {
    var transport = try SynchronizationFixture.transport()

    _ = transport.send(.start)
    _ = transport.send(
      .transportEnded(
        generation: SynchronizationFixture.initialGeneration,
        message: "fixture disconnect"
      )
    )
    let effects = transport.send(.stop)

    XCTAssertEqual(
      SynchronizationFixture.effectNames(effects),
      ["cancel_reconnect", "close_follow"]
    )
    XCTAssertEqual(transport.machine.phase, .stopped)
  }

  func testDuplicateSnapshotEntryFailsClosedIntoRecovery() throws {
    var transport = try SynchronizationFixture.transportInHistory(cursor: 10)

    _ = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.markerEntry(index: 0)
      )
    )
    let effects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.markerEntry(index: 1)
      )
    )

    XCTAssertFalse(SynchronizationFixture.containsPublishedSnapshot(effects))
    XCTAssertEqual(
      transport.machine.phase,
      .recovery(failedStage: .history, failureCount: 1, nextGeneration: 2)
    )
    XCTAssertEqual(transport.machine.diagnostics.first?.kind, .protocolViolation)
  }

  func testInvalidContentFragmentSequenceFailsClosedIntoRecovery() throws {
    var transport = try SynchronizationFixture.transportInHistory(cursor: 10)

    _ = transport.send(
      .frame(generation: 1, message: try SynchronizationFixture.textEntry())
    )
    let effects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.content(fragmentIndex: 1)
      )
    )

    XCTAssertFalse(SynchronizationFixture.containsPublishedSnapshot(effects))
    XCTAssertEqual(
      transport.machine.phase,
      .recovery(failedStage: .history, failureCount: 1, nextGeneration: 2)
    )
  }

  func testMismatchedSnapshotCountsFailClosedIntoRecovery() throws {
    var transport = try SynchronizationFixture.transportInHistory(cursor: 10)

    let effects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.snapshotEnd(
          cursor: 10,
          turnCount: 1,
          entryCount: 0
        )
      )
    )

    XCTAssertFalse(SynchronizationFixture.containsPublishedSnapshot(effects))
    XCTAssertEqual(
      transport.machine.phase,
      .recovery(failedStage: .history, failureCount: 1, nextGeneration: 2)
    )
  }

  func testFrameAfterStopIsIgnoredWithoutRestartingSynchronization() throws {
    var transport = try SynchronizationFixture.transport()

    _ = transport.send(.start)
    _ = transport.send(.stop)
    let effects = transport.send(
      .frame(generation: 1, message: try SynchronizationFixture.unknownTopLevelMessage())
    )

    XCTAssertTrue(effects.isEmpty)
    XCTAssertEqual(transport.machine.phase, .stopped)
    XCTAssertTrue(transport.machine.diagnostics.isEmpty)
  }

  func testLateFrameInRecoveryCannotConsumeAnotherRetry() throws {
    var transport = try SynchronizationFixture.transport()

    _ = transport.send(.start)
    _ = transport.send(
      .transportEnded(generation: 1, message: "fixture disconnect")
    )
    let effects = transport.send(
      .frame(generation: 1, message: try SynchronizationFixture.unknownTopLevelMessage())
    )

    XCTAssertEqual(
      transport.machine.phase,
      .recovery(failedStage: .connect, failureCount: 1, nextGeneration: 2)
    )
    XCTAssertEqual(
      SynchronizationFixture.effectNames(effects),
      ["report_diagnostic"]
    )
    XCTAssertEqual(transport.machine.diagnostics.last?.kind, .staleCompletion)
  }

  func testINV033MalformedKnownSnapshotFrameFailsClosedIntoRecovery() throws {
    var transport = try SynchronizationFixture.transportInHistory(cursor: 10)

    let effects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.malformedKnownSnapshotEntry()
      )
    )

    XCTAssertFalse(SynchronizationFixture.containsPublishedSnapshot(effects))
    XCTAssertEqual(
      transport.machine.phase,
      .recovery(failedStage: .history, failureCount: 1, nextGeneration: 2)
    )
    XCTAssertEqual(transport.machine.diagnostics.first?.kind, .protocolViolation)
  }

  func testSnapshotRecordCapacityFailsClosedWithoutPublishingPartialState() throws {
    var transport = try SynchronizationFixture.transportInHistory(
      cursor: 10,
      snapshotCapacity: .init(maximumRecords: 1, maximumUTF8Bytes: 1_024)
    )

    _ = transport.send(
      .frame(generation: 1, message: try SynchronizationFixture.queuedTurn())
    )
    let effects = transport.send(
      .frame(generation: 1, message: try SynchronizationFixture.markerEntry(index: 0))
    )

    XCTAssertFalse(SynchronizationFixture.containsPublishedSnapshot(effects))
    XCTAssertEqual(
      transport.machine.phase,
      .recovery(failedStage: .history, failureCount: 1, nextGeneration: 2)
    )
    XCTAssertEqual(transport.machine.diagnostics.first?.kind, .protocolViolation)
  }

  func testSnapshotUTF8CapacityFailsClosedWithoutPublishingPartialState() throws {
    var transport = try SynchronizationFixture.transportInHistory(
      cursor: 10,
      snapshotCapacity: .init(maximumRecords: 10, maximumUTF8Bytes: 5)
    )

    let effects = transport.send(
      .frame(generation: 1, message: try SynchronizationFixture.queuedTurn())
    )

    XCTAssertFalse(SynchronizationFixture.containsPublishedSnapshot(effects))
    XCTAssertEqual(
      transport.machine.phase,
      .recovery(failedStage: .history, failureCount: 1, nextGeneration: 2)
    )
    XCTAssertEqual(transport.machine.diagnostics.first?.kind, .protocolViolation)
  }

  func testNonRetriableProtocolErrorReportsTerminalFailure() throws {
    var transport = try SynchronizationFixture.transport()

    _ = transport.send(.start)
    _ = transport.send(.connected(generation: 1))
    let effects = transport.send(
      .frame(generation: 1, message: try SynchronizationFixture.notFoundError())
    )

    XCTAssertTrue(SynchronizationFixture.containsTerminalFailure(effects))
    XCTAssertFalse(SynchronizationFixture.containsRetryLimit(effects))
    XCTAssertEqual(
      transport.machine.phase,
      .recovery(failedStage: .hello, failureCount: 1, nextGeneration: nil)
    )
    XCTAssertEqual(transport.machine.diagnostics.last?.kind, .terminalFailure)
  }

  func testPrimaryDisconnectDuringSideSnapshotIsAttributedToSteadyStage() throws {
    var transport = try SynchronizationFixture.synchronizedTransport(cursor: 10)

    _ = transport.send(
      .frame(generation: 1, message: try SynchronizationFixture.completedEvent(cursor: 20))
    )
    let effects = transport.send(
      .transportEnded(generation: 1, message: "fixture primary disconnect")
    )

    XCTAssertEqual(
      transport.machine.phase,
      .recovery(failedStage: .steady, failureCount: 1, nextGeneration: 2)
    )
    XCTAssertEqual(
      SynchronizationFixture.reconnectDelay(in: effects),
      SynchronizationFixture.policy.retry.delays.first
    )
    XCTAssertTrue(
      effects.contains(
        .cancelSideSnapshot(
          generation: SynchronizationFixture.initialGeneration,
          refreshID: SynchronizationFixture.firstRefreshID
        )
      )
    )
  }

  func testInvalidSideSnapshotCancelsItsSeparateTransport() throws {
    var transport = try SynchronizationFixture.synchronizedTransport(
      cursor: SynchronizationFixture.initialCursor
    )

    _ = transport.send(
      .frame(
        generation: SynchronizationFixture.initialGeneration,
        message: try SynchronizationFixture.completedEvent(
          cursor: SynchronizationFixture.sideRefreshTriggerCursor
        )
      )
    )
    _ = transport.send(
      .sideFrame(
        generation: SynchronizationFixture.initialGeneration,
        refreshID: SynchronizationFixture.firstRefreshID,
        message: try SynchronizationFixture.snapshotStart(
          cursor: SynchronizationFixture.sideRefreshTriggerCursor
        )
      )
    )
    let effects = transport.send(
      .sideFrame(
        generation: SynchronizationFixture.initialGeneration,
        refreshID: SynchronizationFixture.firstRefreshID,
        message: try SynchronizationFixture.content(fragmentIndex: 0)
      )
    )

    XCTAssertTrue(
      effects.contains(
        .cancelSideSnapshot(
          generation: SynchronizationFixture.initialGeneration,
          refreshID: SynchronizationFixture.firstRefreshID
        )
      )
    )
    XCTAssertEqual(
      transport.machine.phase,
      SynchronizationFixture.firstRecovery(failedStage: .sideHistory)
    )
  }
}

private struct ScriptedSynchronizationTransport {
  var machine: SignalboxSessionSynchronizationMachine

  mutating func send(
    _ input: SignalboxSessionSynchronizationInput
  ) -> [SignalboxSessionSynchronizationEffect] {
    machine.receive(input)
  }
}

private enum SynchronizationFixture {
  static let initialGeneration: UInt64 = 1
  static let firstRefreshID: UInt64 = 1
  static let initialCursor: UInt64 = 10
  static let unknownCursor: UInt64 = 11
  static let laterCursor: UInt64 = 13
  static let sideRefreshTriggerCursor: UInt64 = 20
  static let sideBufferedCursor: UInt64 = 21
  static let secondSideBufferedCursor: UInt64 = 22
  static let liveDuringReplaySideRefreshCursor: UInt64 = 23
  static let recoveredCursor: UInt64 = 30
  static let capacityBelowEncodedFutureEvent: UInt = 32
  static let session = "11111111-1111-4111-8111-111111111111"
  static let turn = "22222222-2222-4222-8222-222222222222"
  static let acceptedInput = "33333333-3333-4333-8333-333333333333"
  static let modelCall = "55555555-5555-4555-8555-555555555555"
  static let entry = "66666666-6666-4666-8666-666666666666"
  static let frontier = "77777777-7777-4777-8777-777777777777"

  static let policy = SignalboxSessionSynchronizationPolicy(
    deadlines: SignalboxSynchronizationDeadlines(
      connect: .seconds(1),
      hello: .seconds(2),
      history: .seconds(3),
      replay: .seconds(4),
      sideHistory: .seconds(5)
    ),
    retry: SignalboxSynchronizationRetryPolicy(
      delays: [.milliseconds(100), .milliseconds(200)]
    ),
    snapshotCapacity: .init(maximumRecords: 128, maximumUTF8Bytes: 1_048_576),
    eventBufferCapacity: .init(maximumEvents: 128, maximumUTF8Bytes: 1_048_576)
  )

  static func sessionID() throws -> SignalboxCanonicalUUID {
    try SignalboxCanonicalUUID(validating: session)
  }

  static func firstRecovery(
    failedStage: SignalboxSynchronizationStage
  ) -> SignalboxSessionSynchronizationPhase {
    .recovery(failedStage: failedStage, failureCount: 1, nextGeneration: 2)
  }

  static func steady(
    cursor: UInt64,
    refreshID: UInt64? = nil
  ) -> SignalboxSessionSynchronizationPhase {
    .steady(
      generation: initialGeneration,
      cursor: SignalboxCanonicalUInt64(rawValue: cursor),
      refreshID: refreshID
    )
  }

  static func eventCountCapacity(
    maximumEvents: UInt
  ) -> SignalboxSynchronizationEventBufferCapacity {
    .init(
      maximumEvents: maximumEvents,
      maximumUTF8Bytes: policy.eventBufferCapacity.maximumUTF8Bytes
    )
  }

  static func eventByteCapacity(
    maximumUTF8Bytes: UInt
  ) -> SignalboxSynchronizationEventBufferCapacity {
    .init(
      maximumEvents: policy.eventBufferCapacity.maximumEvents,
      maximumUTF8Bytes: maximumUTF8Bytes
    )
  }

  static func transport(
    snapshotCapacity: SignalboxSynchronizationSnapshotCapacity = policy.snapshotCapacity
  ) throws -> ScriptedSynchronizationTransport {
    try transport(
      snapshotCapacity: snapshotCapacity,
      eventBufferCapacity: policy.eventBufferCapacity
    )
  }

  static func transport(
    eventBufferCapacity: SignalboxSynchronizationEventBufferCapacity
  ) throws -> ScriptedSynchronizationTransport {
    try transport(
      snapshotCapacity: policy.snapshotCapacity,
      eventBufferCapacity: eventBufferCapacity
    )
  }

  private static func transport(
    snapshotCapacity: SignalboxSynchronizationSnapshotCapacity,
    eventBufferCapacity: SignalboxSynchronizationEventBufferCapacity
  ) throws -> ScriptedSynchronizationTransport {
    ScriptedSynchronizationTransport(
      machine: SignalboxSessionSynchronizationMachine(
        sessionID: try sessionID(),
        policy: SignalboxSessionSynchronizationPolicy(
          deadlines: policy.deadlines,
          retry: policy.retry,
          snapshotCapacity: snapshotCapacity,
          eventBufferCapacity: eventBufferCapacity
        )
      )
    )
  }

  static func transportInHistory(
    cursor: UInt64,
    snapshotCapacity: SignalboxSynchronizationSnapshotCapacity = policy.snapshotCapacity
  ) throws -> ScriptedSynchronizationTransport {
    var result = try transport(snapshotCapacity: snapshotCapacity)
    _ = result.send(.start)
    _ = result.send(.connected(generation: 1))
    _ = result.send(
      .frame(
        generation: 1,
        message: try snapshotStart(cursor: cursor)
      )
    )
    return result
  }

  static func transportAtReplay(
    cursor: UInt64
  ) throws -> ScriptedSynchronizationTransport {
    var result = try transportInHistory(cursor: cursor)
    _ = result.send(
      .frame(
        generation: 1,
        message: try snapshotEnd(
          cursor: cursor,
          turnCount: 0,
          entryCount: 0
        )
      )
    )
    return result
  }

  static func synchronizedTransport(
    cursor: UInt64
  ) throws -> ScriptedSynchronizationTransport {
    var result = try transportAtReplay(cursor: cursor)
    _ = result.send(.replayCompleted(generation: 1))
    return result
  }

  static func synchronizedTransport(
    cursor: UInt64,
    eventBufferCapacity: SignalboxSynchronizationEventBufferCapacity
  ) throws -> ScriptedSynchronizationTransport {
    var result = try transport(eventBufferCapacity: eventBufferCapacity)
    _ = result.send(.start)
    _ = result.send(.connected(generation: 1))
    _ = result.send(
      .frame(
        generation: 1,
        message: try snapshotStart(cursor: cursor)
      )
    )
    _ = result.send(
      .frame(
        generation: 1,
        message: try snapshotEnd(
          cursor: cursor,
          turnCount: 0,
          entryCount: 0
        )
      )
    )
    _ = result.send(.replayCompleted(generation: 1))
    return result
  }

  static func snapshotStart(cursor: UInt64) throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"transcript_snapshot_start",
        "session_id":"\(session)",
        "cursor":"\(cursor)"
      }
      """
    )
  }

  static func malformedSnapshotStart() throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"transcript_snapshot_start",
        "session_id":17,
        "cursor":"10"
      }
      """
    )
  }

  static func malformedQueuedTurn() throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"transcript_turn",
        "turn_id":"\(turn)",
        "acceptance_position":"1",
        "state":{
          "type":"queued",
          "accepted_input_id":17,
          "content":"fixture prompt"
        }
      }
      """
    )
  }

  static func queuedTurnWithUnadmittedField() throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"transcript_turn",
        "turn_id":"\(turn)",
        "acceptance_position":"1",
        "state":{
          "type":"queued",
          "accepted_input_id":"\(acceptedInput)",
          "content":"fixture prompt",
          "fixture_unadmitted":true
        }
      }
      """
    )
  }

  static func malformedTranscriptEntry() throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"transcript_entry",
        "entry_index":"0",
        "source_session_id":"\(session)",
        "entry_id":"\(entry)",
        "entry":{
          "type":"assistant_tool_use",
          "turn_id":"\(turn)",
          "model_call_id":"\(modelCall)",
          "tool_request_id":"88888888-8888-4888-8888-888888888888",
          "tool_name":17,
          "arguments":"{}"
        }
      }
      """
    )
  }

  static func malformedTextEntry() throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"transcript_text_entry",
        "entry_index":"0",
        "source_session_id":"\(session)",
        "entry_id":"\(entry)",
        "entry":{
          "type":"user",
          "accepted_input_id":17,
          "turn_id":"\(turn)"
        }
      }
      """
    )
  }

  static func futureTurnState() throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"transcript_turn",
        "turn_id":"\(turn)",
        "acceptance_position":"1",
        "state":{"type":"fixture_future_turn_state"}
      }
      """
    )
  }

  static func futureCurrentModelCallState() throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"transcript_turn",
        "turn_id":"\(turn)",
        "acceptance_position":"1",
        "state":{
          "type":"active_running",
          "current_attempt_id":"44444444-4444-4444-8444-444444444444",
          "current_model_call":{
            "model_call_id":"\(modelCall)",
            "state":{"type":"fixture_future_model_call_state"}
          }
        }
      }
      """
    )
  }

  static func currentModelCallWithUnadmittedField() throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"transcript_turn",
        "turn_id":"\(turn)",
        "acceptance_position":"1",
        "state":{
          "type":"active_running",
          "current_attempt_id":"44444444-4444-4444-8444-444444444444",
          "current_model_call":{
            "model_call_id":"\(modelCall)",
            "state":{"type":"in_flight"},
            "fixture_unadmitted":true
          }
        }
      }
      """
    )
  }

  static func terminalModelCallWithUnadmittedField() throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"transcript_turn",
        "turn_id":"\(turn)",
        "acceptance_position":"1",
        "state":{
          "type":"failed",
          "terminal_frontier_id":"\(frontier)",
          "terminal_attempt_id":"44444444-4444-4444-8444-444444444444",
          "terminal_model_call":{
            "model_call_id":"\(modelCall)",
            "disposition":"known_failed",
            "fixture_unadmitted":true
          }
        }
      }
      """
    )
  }

  static func failedTurnWithCallWithoutAttempt() throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"transcript_turn",
        "turn_id":"\(turn)",
        "acceptance_position":"1",
        "state":{
          "type":"failed",
          "terminal_frontier_id":"\(frontier)",
          "terminal_attempt_id":null,
          "terminal_model_call":{
            "model_call_id":"\(modelCall)",
            "disposition":"known_failed"
          }
        }
      }
      """
    )
  }

  static func activeRunningWithoutModelCallMember() throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"transcript_turn",
        "turn_id":"\(turn)",
        "acceptance_position":"1",
        "state":{
          "type":"active_running",
          "current_attempt_id":"44444444-4444-4444-8444-444444444444"
        }
      }
      """
    )
  }

  static func failedTurnWithoutNullableMembers() throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"transcript_turn",
        "turn_id":"\(turn)",
        "acceptance_position":"1",
        "state":{
          "type":"failed",
          "terminal_frontier_id":"\(frontier)"
        }
      }
      """
    )
  }

  static func cancelledTurnWithoutModelCallMember() throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"transcript_turn",
        "turn_id":"\(turn)",
        "acceptance_position":"1",
        "state":{
          "type":"cancelled",
          "terminal_frontier_id":"\(frontier)",
          "terminal_attempt_id":"44444444-4444-4444-8444-444444444444"
        }
      }
      """
    )
  }

  static func futureTranscriptEntry() throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"transcript_entry",
        "entry_index":"0",
        "source_session_id":"\(session)",
        "entry_id":"\(entry)",
        "entry":{"type":"fixture_future_entry"}
      }
      """
    )
  }

  static func queuedTurn() throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"transcript_turn",
        "turn_id":"\(turn)",
        "acceptance_position":"1",
        "state":{
          "type":"queued",
          "accepted_input_id":"\(acceptedInput)",
          "content":"fixture owner input"
        }
      }
      """
    )
  }

  static func textEntry() throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"transcript_text_entry",
        "entry_index":"0",
        "source_session_id":"\(session)",
        "entry_id":"\(entry)",
        "entry":{
          "type":"user",
          "accepted_input_id":"\(acceptedInput)",
          "turn_id":"\(turn)"
        }
      }
      """
    )
  }

  static func markerEntry(index: UInt64) throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"transcript_entry",
        "entry_index":"\(index)",
        "source_session_id":"\(session)",
        "entry_id":"\(entry)",
        "entry":{"type":"turn_completed","turn_id":"\(turn)"}
      }
      """
    )
  }

  static func content(
    fragmentIndex: UInt64 = 0
  ) throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"transcript_content",
        "entry_index":"0",
        "fragment_index":"\(fragmentIndex)",
        "final_fragment":true,
        "content_fragment":"fixture owner input"
      }
      """
    )
  }

  static func oversizedContent() throws -> SignalboxProcessServerMessage {
    let content = String(
      repeating: "x",
      count: SignalboxProcessProtocol.maximumContentFragmentUTF8Bytes + 1
    )
    return try message(
      """
      {
        "type":"transcript_content",
        "entry_index":"0",
        "fragment_index":"0",
        "final_fragment":true,
        "content_fragment":"\(content)"
      }
      """
    )
  }

  static func snapshotEnd(
    cursor: UInt64,
    turnCount: UInt64,
    entryCount: UInt64
  ) throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"transcript_snapshot_end",
        "session_id":"\(session)",
        "cursor":"\(cursor)",
        "turn_count":"\(turnCount)",
        "entry_count":"\(entryCount)"
      }
      """
    )
  }

  static func inputAcceptedEvent(cursor: UInt64) throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"session_event",
        "cursor":"\(cursor)",
        "session_id":"\(session)",
        "event":{
          "type":"input_accepted",
          "accepted_input_id":"\(acceptedInput)",
          "turn_id":"\(turn)",
          "acceptance_position":"1",
          "content":"fixture owner input"
        }
      }
      """
    )
  }

  static func completedEvent(cursor: UInt64) throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"session_event",
        "cursor":"\(cursor)",
        "session_id":"\(session)",
        "event":{
          "type":"turn_completed",
          "turn_id":"\(turn)",
          "model_call_id":"\(modelCall)",
          "completion_entry_id":"\(entry)",
          "terminal_frontier_id":"\(frontier)"
        }
      }
      """
    )
  }

  static func unknownModelCallStateEvent(
    cursor: UInt64
  ) throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"session_event",
        "cursor":"\(cursor)",
        "session_id":"\(session)",
        "event":{
          "type":"model_call_transition",
          "turn_id":"\(turn)",
          "model_call_id":"\(modelCall)",
          "state":{"type":"fixture_future_model_call_state"}
        }
      }
      """
    )
  }

  static func unknownToolBatchStateEvent(
    cursor: UInt64
  ) throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"session_event",
        "cursor":"\(cursor)",
        "session_id":"\(session)",
        "event":{
          "type":"tool_batch_transition",
          "turn_id":"\(turn)",
          "model_call_id":"\(modelCall)",
          "state":{"type":"fixture_future_tool_batch_state"}
        }
      }
      """
    )
  }

  static func unknownEvent(cursor: UInt64) throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"session_event",
        "cursor":"\(cursor)",
        "session_id":"\(session)",
        "event":{"type":"fixture_future_event","retained":true}
      }
      """
    )
  }

  static func unknownEventWithNullNodes(
    cursor: UInt64
  ) throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"session_event",
        "cursor":"\(cursor)",
        "session_id":"\(session)",
        "event":{
          "type":"fixture_future_event",
          "nodes":[null,null,null,null,null,null,null,null]
        }
      }
      """
    )
  }

  static func malformedKnownEvent(cursor: UInt64) throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"session_event",
        "cursor":"\(cursor)",
        "session_id":"\(session)",
        "event":{"type":"turn_activated","turn_id":17}
      }
      """
    )
  }

  static func malformedSessionEventEnvelope() throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"session_event",
        "cursor":17,
        "session_id":"\(session)",
        "event":{"type":"session_created"}
      }
      """
    )
  }

  static func malformedKnownSnapshotEntry() throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"transcript_entry",
        "entry_index":17,
        "source_session_id":"\(session)",
        "entry_id":"\(entry)",
        "entry":{"type":"turn_completed","turn_id":"\(turn)"}
      }
      """
    )
  }

  static func notFoundError() throws -> SignalboxProcessServerMessage {
    try message(
      """
      {
        "type":"error",
        "code":"not_found",
        "message":"fixture session missing",
        "detail":null
      }
      """
    )
  }

  static func unknownTopLevelMessage() throws -> SignalboxProcessServerMessage {
    try message(
      """
      {"type":"fixture_future_message","retained":true}
      """
    )
  }

  static func reportMoreThanRetainedDiagnosticCapacity(
    to transport: inout ScriptedSynchronizationTransport
  ) throws {
    for _ in 0...SignalboxSessionSynchronizationMachine.maximumRetainedDiagnostics {
      _ = transport.send(
        .frame(generation: 1, message: try unknownTopLevelMessage())
      )
    }
  }

  static func message(_ object: String) throws -> SignalboxProcessServerMessage {
    let data = Data(
      """
      {"version":5,"request_id":"1","message":\(object)}
      """.utf8
    )
    return try SignalboxProcessServerFrame.decode(from: data).message
  }

  static func publishedSnapshot(
    in effects: [SignalboxSessionSynchronizationEffect]
  ) throws -> SignalboxSynchronizationSnapshot {
    for effect in effects {
      if case .publishSnapshot(let snapshot) = effect {
        return snapshot
      }
    }
    throw FixtureFailure.missingEffect
  }

  static func publishedEventCursors(
    in effects: [SignalboxSessionSynchronizationEffect]
  ) -> [UInt64] {
    effects.compactMap { effect in
      if case .publishEvent(let followed) = effect {
        return followed.cursor.rawValue
      }
      return nil
    }
  }

  static func reconnectDelay(
    in effects: [SignalboxSessionSynchronizationEffect]
  ) -> Duration? {
    effects.compactMap { effect in
      if case .scheduleReconnect(_, let delay) = effect {
        return delay
      }
      return nil
    }.first
  }

  static func containsRetryLimit(
    _ effects: [SignalboxSessionSynchronizationEffect]
  ) -> Bool {
    effects.contains(.retryLimitReached)
  }

  static func containsTerminalFailure(
    _ effects: [SignalboxSessionSynchronizationEffect]
  ) -> Bool {
    effects.contains(.terminalFailure)
  }

  static func containsRetrySchedule(
    _ effects: [SignalboxSessionSynchronizationEffect]
  ) -> Bool {
    effects.contains { effect in
      if case .scheduleReconnect = effect {
        return true
      }
      return false
    }
  }

  static func containsSideMerge(
    _ effects: [SignalboxSessionSynchronizationEffect]
  ) -> Bool {
    effects.contains { effect in
      if case .mergeSideSnapshot = effect {
        return true
      }
      return false
    }
  }

  static func containsPublishedSnapshot(
    _ effects: [SignalboxSessionSynchronizationEffect]
  ) -> Bool {
    effects.contains { effect in
      if case .publishSnapshot = effect {
        return true
      }
      return false
    }
  }

  static func containsPublishedEvent(
    _ effects: [SignalboxSessionSynchronizationEffect]
  ) -> Bool {
    effects.contains { effect in
      if case .publishEvent = effect {
        return true
      }
      return false
    }
  }

  static func effectNames(
    _ effects: [SignalboxSessionSynchronizationEffect]
  ) -> [String] {
    effects.map { effect in
      switch effect {
      case .openFollow:
        return "open_follow"
      case .closeFollow:
        return "close_follow"
      case .armDeadline:
        return "arm_deadline"
      case .cancelDeadline:
        return "cancel_deadline"
      case .publishSnapshot:
        return "publish_snapshot"
      case .publishEvent:
        return "publish_event"
      case .requestSideSnapshot:
        return "request_side_snapshot"
      case .cancelSideSnapshot:
        return "cancel_side_snapshot"
      case .mergeSideSnapshot:
        return "merge_side_snapshot"
      case .scheduleReconnect:
        return "schedule_reconnect"
      case .cancelReconnect:
        return "cancel_reconnect"
      case .reportDiagnostic:
        return "report_diagnostic"
      case .retryLimitReached:
        return "retry_limit_reached"
      case .terminalFailure:
        return "terminal_failure"
      }
    }
  }
}

private enum FixtureFailure: Error {
  case missingEffect
}
