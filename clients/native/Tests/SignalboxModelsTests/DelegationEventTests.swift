import Foundation
import XCTest
@testable import SignalboxNative

final class DelegationEventTests: XCTestCase {
  private let parent = "11111111-1111-4111-8111-111111111111"
  private let child = "22222222-2222-4222-8222-222222222222"
  private let request = "33333333-3333-4333-8333-333333333333"

  func testDelegationOutcomeSpellingsPreserveEveryVariant() throws {
    for expected in SignalboxDelegationOutcome.allCases {
      let spelling = expectedSpelling(expected)
      let data = try JSONEncoder().encode(spelling)
      let decoded = try SignalboxJSONCoding.decoder().decode(SignalboxDelegationOutcome.self, from: data)
      XCTAssertEqual(decoded, expected, spelling)
      XCTAssertEqual(decoded.rawValue, spelling)
    }
  }

  func testDelegationReasonSpellingsPreserveEveryVariant() throws {
    for expected in SignalboxDelegationReason.allCases {
      let spelling = expectedSpelling(expected)
      let data = try JSONEncoder().encode(spelling)
      let decoded = try SignalboxJSONCoding.decoder().decode(SignalboxDelegationReason.self, from: data)
      XCTAssertEqual(decoded, expected, spelling)
      XCTAssertEqual(decoded.rawValue, spelling)
    }
  }

  private func expectedSpelling(_ value: SignalboxDelegationOutcome) -> String {
    switch value {
    case .returned: "returned"
    case .failed: "failed"
    case .stopped: "stopped"
    case .cancelled: "cancelled"
    case .continueRunning: "continue_running"
    case .alreadyTerminal: "already_terminal"
    }
  }

  private func expectedSpelling(_ value: SignalboxDelegationReason) -> String {
    switch value {
    case .childCompleted: "child_completed"
    case .childExecutionFailed: "child_execution_failed"
    case .childResultUnavailable: "child_result_unavailable"
    case .childCancelled: "child_cancelled"
    case .parentStopped: "parent_stopped"
    case .parentCancelled: "parent_cancelled"
    }
  }

  private func decode(_ event: String, recipient: String? = nil) throws -> SignalboxFollowedSessionEvent {
    try SignalboxJSONCoding.decoder().decode(SignalboxFollowedSessionEvent.self,
      from: Data(#"{"cursor":"2","session_id":"\#(recipient ?? parent)","event":\#(event)}"#.utf8))
  }

  func testRetiredGoalTurnPreservesItsIdentity() throws {
    let decoded = try decode(#"{"type":"goal_turn_retired","turn_id":"\#(request)"}"#)
    XCTAssertEqual(decoded.event, .goalTurnRetired(turnID: try .init(validating: request)))
  }

  func testSpawnedChildPreservesBoundPolicy() throws {
    let decoded = try decode(#"{"type":"child_spawned","spawning_request_id":"\#(request)","child_session_id":"\#(child)","relationship":{"type":"bound","on_parent_stopped":"keep_running","on_parent_cancelled":"cancel"}}"#)
    XCTAssertEqual(decoded.event, .childSpawned(spawningRequestID: try .init(validating: request),
      childSessionID: try .init(validating: child), relationship: .bound(onParentStopped: .keepRunning, onParentCancelled: .cancel)))
  }

  func testChildWaitPreservesForegroundMode() throws {
    let decoded = try decode(#"{"type":"child_waiting","await_request_id":"\#(parent)","spawning_request_id":"\#(request)","child_session_id":"\#(child)","mode":"foreground"}"#)
    XCTAssertEqual(decoded.event, .childWaiting(awaitRequestID: try .init(validating: parent),
      spawningRequestID: try .init(validating: request), childSessionID: try .init(validating: child), mode: .foreground))
  }

  func testSessionMessagePreservesDeliveryIdentityAndText() throws {
    let decoded = try decode(message)
    XCTAssertEqual(decoded.event, .sessionMessage(spawningRequestID: try .init(validating: request),
      messageID: try .init(validating: request), senderSessionID: try .init(validating: child),
      recipientSessionID: try .init(validating: parent), ordinal: .init(rawValue: 1),
      deliverySequence: .init(rawValue: 4), content: "Child update"))
  }

  func testSessionMessageRejectsWrongStreamRecipient() throws {
    XCTAssertThrowsError(try decode(message, recipient: child))
  }

  func testChildResultPreservesCompletionContent() throws {
    let decoded = try decode(#"{"type":"child_result","spawning_request_id":"\#(request)","child_session_id":"\#(child)","outcome":"returned","content":"Child answer","reason":"child_completed","provenance":{"type":"child_turn","child_session_id":"\#(child)","child_turn_id":"\#(request)"}}"#)
    XCTAssertEqual(decoded.event, .childResult(spawningRequestID: try .init(validating: request),
      childSessionID: try .init(validating: child), outcome: .returned, content: "Child answer",
      reason: .childCompleted, provenance: .childTurn(childSessionID: try .init(validating: child), childTurnID: try .init(validating: request))))
  }

  func testChildTerminalCascadeIsAdmittedOnBothStreams() throws {
    let expected = SignalboxProcessSessionEvent.childLifecycleDisposition(
      spawningRequestID: try .init(validating: request), childSessionID: try .init(validating: child),
      outcome: .cancelled, reason: .parentCancelled,
      provenance: .parentTurnCommand(parentSessionID: try .init(validating: parent),
        parentTurnID: try .init(validating: request), commandID: try .init(validating: request), descendantScope: .parentAndDescendants))
    XCTAssertEqual(try decode(cascade).event, expected)
    XCTAssertEqual(try decode(cascade, recipient: child).event, expected)
  }

  func testLifecycleCommandCascadeDecodesOnParentAndChildStreams() throws {
    let event = #"{"type":"child_lifecycle_disposition","spawning_request_id":"\#(request)","child_session_id":"\#(child)","outcome":"cancelled","reason":"parent_cancelled","provenance":\#(lifecycleProvenance)}"#
    let expected = SignalboxProcessSessionEvent.childLifecycleDisposition(
      spawningRequestID: try .init(validating: request), childSessionID: try .init(validating: child),
      outcome: .cancelled, reason: .parentCancelled,
      provenance: .parentLifecycleCommand(parentSessionID: try .init(validating: parent),
        commandID: try .init(validating: request), descendantScope: .parentAndDescendants))
    XCTAssertEqual(try decode(event).event, expected)
    XCTAssertEqual(try decode(event, recipient: child).event, expected)
  }

  func testLifecycleCommandResultDecodesItsCascadeProvenance() throws {
    let event = #"{"type":"child_result","spawning_request_id":"\#(request)","child_session_id":"\#(child)","outcome":"cancelled","content":null,"reason":"parent_cancelled","provenance":\#(lifecycleProvenance)}"#
    XCTAssertEqual(try decode(event).event, .childResult(
      spawningRequestID: try .init(validating: request), childSessionID: try .init(validating: child),
      outcome: .cancelled, content: nil, reason: .parentCancelled,
      provenance: .parentLifecycleCommand(parentSessionID: try .init(validating: parent),
        commandID: try .init(validating: request), descendantScope: .parentAndDescendants)))
    XCTAssertThrowsError(try decode(event.replacingOccurrences(of: "parent_and_descendants", with: "parent_alone")))
  }

  func testGoalCommandCascadesRequireAPositiveGeneration() throws {
    for kind in ["child_result", "child_lifecycle_disposition"] {
      let content = kind == "child_result" ? #","content":null"# : ""
      func event(generation: UInt64) -> String {
        #"{"type":"\#(kind)","spawning_request_id":"\#(request)","child_session_id":"\#(child)","outcome":"cancelled"\#(content),"reason":"parent_cancelled","provenance":{"type":"parent_goal_command","parent_session_id":"\#(parent)","goal_generation":"\#(generation)","command_id":"\#(request)","descendant_scope":"parent_and_descendants"}}"#
      }
      XCTAssertThrowsError(try decode(event(generation: 0)))
      XCTAssertNoThrow(try decode(event(generation: 1)))
      if kind == "child_lifecycle_disposition" {
        XCTAssertThrowsError(try decode(event(generation: 0), recipient: child))
        XCTAssertNoThrow(try decode(event(generation: 1), recipient: child))
      }
    }
  }

  private var lifecycleProvenance: String {
    #"{"type":"parent_lifecycle_command","parent_session_id":"\#(parent)","command_id":"\#(request)","descendant_scope":"parent_and_descendants"}"#
  }

  private var message: String {
    #"{"type":"session_message","spawning_request_id":"\#(request)","message_id":"\#(request)","sender_session_id":"\#(child)","recipient_session_id":"\#(parent)","ordinal":"1","delivery_sequence":"4","content":"Child update"}"#
  }

  private var cascade: String {
    #"{"type":"child_lifecycle_disposition","spawning_request_id":"\#(request)","child_session_id":"\#(child)","outcome":"cancelled","reason":"parent_cancelled","provenance":{"type":"parent_turn_command","parent_session_id":"\#(parent)","parent_turn_id":"\#(request)","command_id":"\#(request)","descendant_scope":"parent_and_descendants"}}"#
  }
}
