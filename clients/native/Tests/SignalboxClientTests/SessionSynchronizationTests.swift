import Foundation
import XCTest

@testable import SignalboxNative

final class SessionSynchronizationTests: XCTestCase {
  func testScriptedTransportTraversesEverySynchronizationPhase() throws {
    var transport = try SynchronizationFixture.transport()

    let connectEffects = transport.send(.start)
    let helloEffects = transport.send(.connected(generation: 1))
    let historyEffects = transport.send(
      .frame(generation: 1, message: try SynchronizationFixture.snapshotStart(cursor: 10))
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
          cursor: 10,
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
    XCTAssertEqual(snapshot.cursor.rawValue, 10)
    XCTAssertEqual(snapshot.records.count, 3)
    XCTAssertEqual(
      steadyEffects,
      [.cancelDeadline(.replay(generation: 1))]
    )
    XCTAssertEqual(
      transport.machine.phase,
      .steady(
        generation: 1,
        cursor: SignalboxCanonicalUInt64(rawValue: 10),
        refreshID: nil
      )
    )
  }

  func testReplayDeduplicatesSnapshotCursorAndPreservesUnknownEvent() throws {
    var transport = try SynchronizationFixture.transportAtReplay(cursor: 10)

    let duplicateEffects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.inputAcceptedEvent(cursor: 10)
      )
    )
    let unknownEffects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.unknownEvent(cursor: 11)
      )
    )
    let laterEffects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.inputAcceptedEvent(cursor: 13)
      )
    )
    let replayEffects = transport.send(.replayCompleted(generation: 1))

    XCTAssertTrue(duplicateEffects.isEmpty)
    XCTAssertEqual(unknownEffects.count, 1)
    XCTAssertEqual(laterEffects.count, 0)
    XCTAssertEqual(
      SynchronizationFixture.publishedEventCursors(in: replayEffects),
      [11, 13]
    )
    XCTAssertEqual(transport.machine.diagnostics.count, 1)
    XCTAssertEqual(
      transport.machine.phase,
      .steady(
        generation: 1,
        cursor: SignalboxCanonicalUInt64(rawValue: 13),
        refreshID: nil
      )
    )
  }

  func testMalformedKnownEventPublishesDiagnosticWithoutKillingSteadyStream() throws {
    var transport = try SynchronizationFixture.synchronizedTransport(cursor: 10)

    let effects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.malformedKnownEvent(cursor: 11)
      )
    )

    XCTAssertEqual(
      SynchronizationFixture.effectNames(effects),
      ["report_diagnostic", "publish_event"]
    )
    XCTAssertEqual(transport.machine.diagnostics.last?.kind, .decoding)
    XCTAssertEqual(
      transport.machine.phase,
      .steady(
        generation: 1,
        cursor: SignalboxCanonicalUInt64(rawValue: 11),
        refreshID: nil
      )
    )
  }

  func testUnknownFrameDoesNotDiscardOtherwiseValidSnapshotPage() throws {
    var transport = try SynchronizationFixture.transportInHistory(cursor: 10)

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
          cursor: 10,
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
      .replay(generation: 1, cursor: SignalboxCanonicalUInt64(rawValue: 10))
    )
  }

  func testFreshSideSnapshotMergesBeforeBufferedStreamEvent() throws {
    var transport = try SynchronizationFixture.synchronizedTransport(cursor: 10)

    let triggerEffects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.completedEvent(cursor: 20)
      )
    )
    let bufferedEffects = transport.send(
      .frame(
        generation: 1,
        message: try SynchronizationFixture.inputAcceptedEvent(cursor: 21)
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
      [21]
    )
    XCTAssertEqual(
      transport.machine.phase,
      .steady(
        generation: 1,
        cursor: SignalboxCanonicalUInt64(rawValue: 21),
        refreshID: nil
      )
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
      .milliseconds(100)
    )
    XCTAssertEqual(
      secondConnect.first,
      .openFollow(sessionID: try SynchronizationFixture.sessionID(), generation: 2)
    )
    XCTAssertEqual(
      SynchronizationFixture.reconnectDelay(in: secondFailure),
      .milliseconds(200)
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
      .milliseconds(100)
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
      .milliseconds(100)
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
      .milliseconds(100)
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
      .milliseconds(100)
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
      .milliseconds(100)
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
      .milliseconds(100)
    )
    XCTAssertEqual(
      transport.machine.phase,
      .recovery(failedStage: .steady, failureCount: 1, nextGeneration: 3)
    )
  }

  func testDiagnosticSurvivesFallbackAndSuccessfulReconnect() throws {
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

    XCTAssertEqual(transport.machine.diagnostics.count, 2)
    XCTAssertEqual(transport.machine.diagnostics.first?.kind, .decoding)
    XCTAssertEqual(
      transport.machine.phase,
      .steady(
        generation: 2,
        cursor: SignalboxCanonicalUInt64(rawValue: 30),
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
    )
  )

  static func sessionID() throws -> SignalboxCanonicalUUID {
    try SignalboxCanonicalUUID(validating: session)
  }

  static func transport() throws -> ScriptedSynchronizationTransport {
    ScriptedSynchronizationTransport(
      machine: SignalboxSessionSynchronizationMachine(
        sessionID: try sessionID(),
        policy: policy
      )
    )
  }

  static func transportInHistory(
    cursor: UInt64
  ) throws -> ScriptedSynchronizationTransport {
    var result = try transport()
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

  static func unknownTopLevelMessage() throws -> SignalboxProcessServerMessage {
    try message(
      """
      {"type":"fixture_future_message","retained":true}
      """
    )
  }

  static func message(_ object: String) throws -> SignalboxProcessServerMessage {
    let data = Data(
      """
      {"version":5,"request_id":"1","message":\(object)}
      """.utf8
    )
    return try SignalboxJSONCoding.decoder().decode(
      SignalboxProcessServerFrame.self,
      from: data
    ).message
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
      case .mergeSideSnapshot:
        return "merge_side_snapshot"
      case .scheduleReconnect:
        return "schedule_reconnect"
      case .reportDiagnostic:
        return "report_diagnostic"
      case .retryLimitReached:
        return "retry_limit_reached"
      }
    }
  }
}

private enum FixtureFailure: Error {
  case missingEffect
}
