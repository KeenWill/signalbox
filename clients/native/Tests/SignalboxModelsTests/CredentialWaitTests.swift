import Foundation
@testable import SignalboxNative
import XCTest

final class CredentialWaitTests: XCTestCase {
  func testCredentialWaitDecodesBothClosedCausesAndRejectsAnUnknownCause() throws {
    for cause in ["contended", "exhausted"] {
      let data = Data(#"{"type":"active_awaiting_credential_availability","wait_attempt_id":"11111111-1111-4111-8111-111111111111","cause":"\#(cause)"}"#.utf8)
      let state = try SignalboxJSONCoding.decoder().decode(SignalboxTranscriptTurnState.self, from: data)
      guard case .activeAwaitingCredentialAvailability(_, let actual) = state else {
        return XCTFail("A parked wait must retain its active typed state")
      }
      XCTAssertEqual(actual.rawValue, cause)
    }
    let unknown = Data(#"{"type":"active_awaiting_credential_availability","wait_attempt_id":"11111111-1111-4111-8111-111111111111","cause":"unknown"}"#.utf8)
    let state = try SignalboxJSONCoding.decoder().decode(SignalboxTranscriptTurnState.self, from: unknown)
    guard case .unknown(_, _, let diagnostic) = state else {
      return XCTFail("An unknown wait cause requires a diagnostic")
    }
    XCTAssertNotNil(diagnostic)
  }

  func testTerminalCredentialWaitPreservesPredecessorAndRejectsNonProviderCauses() throws {
    let data = terminalWait(cause: "quota_exhausted")
    let state = try SignalboxJSONCoding.decoder().decode(SignalboxTranscriptTurnState.self, from: data)
    guard case .failedAfterCredentialWait(_, let terminalAttempt, let predecessor) = state else {
      return XCTFail("A terminal wait release requires its predecessor failure")
    }
    XCTAssertEqual(terminalAttempt.rawValue, "22222222-2222-4222-8222-222222222222")
    XCTAssertEqual(predecessor.modelCallID.rawValue, "33333333-3333-4333-8333-333333333333")
    XCTAssertEqual(predecessor.cause, .quotaExhausted)
    let malformed = try SignalboxJSONCoding.decoder().decode(
      SignalboxTranscriptTurnState.self, from: terminalWait(cause: "attachment_missing"))
    guard case .unknown(_, _, let diagnostic) = malformed else {
      return XCTFail("A local attachment failure cannot supply a wait predecessor cause")
    }
    XCTAssertNotNil(diagnostic)
  }

  // Distinct synthetic UUIDs make the terminal attempt and predecessor independently observable.
  private func terminalWait(cause: String) -> Data {
    Data(#"{"type":"failed_after_credential_wait","terminal_frontier_id":"11111111-1111-4111-8111-111111111111","terminal_attempt_id":"22222222-2222-4222-8222-222222222222","predecessor_model_call":{"model_call_id":"33333333-3333-4333-8333-333333333333","disposition":"known_failed","cause":"\#(cause)"}}"#.utf8)
  }
}
