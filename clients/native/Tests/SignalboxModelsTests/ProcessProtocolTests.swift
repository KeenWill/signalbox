import Foundation
import XCTest

@testable import SignalboxNative

final class ProcessProtocolTests: XCTestCase {
  private let sessionID = "11111111-1111-4111-8111-111111111111"
  private let turnID = "22222222-2222-4222-8222-222222222222"

  func testClientFrameUsesVersionEighteenAndCanonicalStringScalars() throws {
    let frame = SignalboxProcessClientFrame(
      requestID: try SignalboxRequestID(validating: 7),
      request: .readTranscript(
        sessionID: try SignalboxCanonicalUUID(validating: sessionID)
      )
    )

    let encoded = try SignalboxJSONCoding.encoder().encode(frame)

    XCTAssertEqual(
      String(decoding: encoded, as: UTF8.self),
      #"{"request":{"session_id":"\#(sessionID)","type":"read_transcript"},"request_id":"7","version":18}"#
    )
  }

  func testModelAliasCatalogRequestAndSummaryUseClosedVersionEighteenShapes() throws {
    let requestFrame = SignalboxProcessClientFrame(
      requestID: try SignalboxRequestID(validating: 8),
      request: .listModelAliases
    )
    let encodedRequest = try SignalboxJSONCoding.encoder().encode(requestFrame)
    XCTAssertEqual(
      String(decoding: encodedRequest, as: UTF8.self),
      #"{"request":{"type":"list_model_aliases"},"request_id":"8","version":18}"#
    )

    let encodedSummary = Data(
      """
      {
        "version":18,
        "request_id":"8",
        "message":{
          "type":"model_alias_summary",
          "alias_id":"\(sessionID)",
          "selection_id":"\(turnID)"
        }
      }
      """.utf8
    )
    let summaryFrame = try SignalboxProcessServerFrame.decode(from: encodedSummary)
    XCTAssertEqual(
      summaryFrame.message,
      .modelAliasSummary(
        SignalboxModelAliasSummary(
          aliasID: try SignalboxCanonicalUUID(validating: sessionID),
          selectionID: try SignalboxCanonicalUUID(validating: turnID)
        )
      )
    )
  }

  func testCanonicalDecimalRejectsLeadingZeroes() {
    let encoded = Data(#""01""#.utf8)

    XCTAssertThrowsError(
      try SignalboxJSONCoding.decoder().decode(
        SignalboxCanonicalUInt64.self,
        from: encoded
      )
    )
  }

  func testClientRequestIdentityRejectsReservedZero() {
    let encoded = Data(#""0""#.utf8)

    XCTAssertThrowsError(
      try SignalboxJSONCoding.decoder().decode(
        SignalboxRequestID.self,
        from: encoded
      )
    )
  }

  func testKnownSessionEventDecodesItsTypedPayload() throws {
    let encoded = Data(
      """
      {
        "version":18,
        "request_id":"9",
        "message":{
          "type":"session_event",
          "cursor":"12",
          "session_id":"\(sessionID)",
          "event":{
            "type":"turn_activated",
            "turn_id":"\(turnID)",
            "current_attempt_id":"33333333-3333-4333-8333-333333333333"
          }
        }
      }
      """.utf8
    )

    let frame = try SignalboxProcessServerFrame.decode(from: encoded)

    XCTAssertEqual(
      frame.message,
      .sessionEvent(
        SignalboxFollowedSessionEvent(
          cursor: SignalboxCanonicalUInt64(rawValue: 12),
          sessionID: try SignalboxCanonicalUUID(validating: sessionID),
          event: .turnActivated(
            turnID: try SignalboxCanonicalUUID(validating: turnID),
            currentAttemptID: try SignalboxCanonicalUUID(
              validating: "33333333-3333-4333-8333-333333333333"
            )
          )
        )
      )
    )
  }

  func testUnknownSessionEventDoesNotDiscardItsFrame() throws {
    let encoded = Data(
      """
      {
        "version":18,
        "request_id":"9",
        "message":{
          "type":"session_event",
          "cursor":"12",
          "session_id":"\(sessionID)",
          "event":{"type":"future_transition","field":"retained"}
        }
      }
      """.utf8
    )

    let frame = try SignalboxProcessServerFrame.decode(from: encoded)

    XCTAssertEqual(
      frame.message,
      .sessionEvent(
        SignalboxFollowedSessionEvent(
          cursor: SignalboxCanonicalUInt64(rawValue: 12),
          sessionID: try SignalboxCanonicalUUID(validating: sessionID),
          event: .unknown(
            kind: "future_transition",
            payload: [
              "type": .string("future_transition"),
              "field": .string("retained"),
            ],
            decodingDiagnostic: nil
          )
        )
      )
    )
  }

  func testMalformedKnownMessageDegradesWithDiagnostic() throws {
    let encoded = Data(
      """
      {
        "version":18,
        "request_id":"9",
        "message":{"type":"session_created","session_id":17}
      }
      """.utf8
    )

    let frame = try SignalboxProcessServerFrame.decode(from: encoded)

    XCTAssertEqual(
      frame.message,
      .unknown(
        kind: "session_created",
        payload: [
          "type": .string("session_created"),
          "session_id": .number(17),
        ],
        decodingDiagnostic: SignalboxDecodingDiagnostic(
          message: "Unexpected field type at message.session_id."
        )
      )
    )
  }

  func testDuplicateDecodedMemberDegradesBeforeTypedProjection() throws {
    let encoded = Data(
      """
      {
        "version":18,
        "request_id":"9",
        "message":{
          "type":"transcript_snapshot_start",
          "session_id":"\(sessionID)",
          "cursor":"12",
          "\\u0063ursor":"12"
        }
      }
      """.utf8
    )

    let frame = try SignalboxProcessServerFrame.decode(from: encoded)

    XCTAssertEqual(
      frame.message,
      ProcessProtocolFixture.duplicateSnapshotBoundaryMessage(sessionID: sessionID)
    )
  }

  func testUnadmittedFrameMemberFailsClosed() {
    let encoded = Data(
      """
      {
        "version":18,
        "request_id":"9",
        "unexpected":true,
        "message":{"type":"sessions_start"}
      }
      """.utf8
    )

    XCTAssertThrowsError(try SignalboxProcessServerFrame.decode(from: encoded))
  }

  func testNestedDuplicateMemberDegradesEnclosingMessage() throws {
    let encoded = Data(
      """
      {
        "version":18,
        "request_id":"9",
        "message":{
          "type":"session_event",
          "cursor":"12",
          "session_id":"\(sessionID)",
          "event":{
            "type":"future_transition",
            "nested":{"value":1,"value":2}
          }
        }
      }
      """.utf8
    )

    let frame = try SignalboxProcessServerFrame.decode(from: encoded)

    ProcessProtocolFixture.assertNestedDuplicateMessage(frame.message)
  }

  func testExcessiveContainerDepthFailsBeforeTypedDecoding() {
    let encoded = ProcessProtocolFixture.excessivelyNestedFrame()

    XCTAssertThrowsError(try SignalboxProcessServerFrame.decode(from: encoded))
  }

  func testExpandedErrorMessageDegradesBeforeProtocolProjection() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.expandedErrorFrame()
    )

    XCTAssertEqual(
      ProcessProtocolFixture.decodingDiagnostic(in: frame.message),
      ProcessProtocolFixture.unadmittedErrorFieldDiagnostic
    )
  }

  func testExpandedKnownErrorDetailDegradesBeforeProtocolProjection() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.expandedErrorDetailFrame(sessionID: sessionID)
    )

    XCTAssertEqual(
      ProcessProtocolFixture.decodingDiagnostic(in: frame.message),
      ProcessProtocolFixture.unadmittedDetailFieldDiagnostic
    )
  }

  func testRejectedErrorRequiresDetailMember() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.rejectedErrorWithoutDetailFrame()
    )

    XCTAssertEqual(
      ProcessProtocolFixture.decodingDiagnostic(in: frame.message),
      ProcessProtocolFixture.missingErrorDetailDiagnostic
    )
  }

  func testRejectedErrorRequiresNonNullDetail() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.rejectedErrorWithNullDetailFrame()
    )

    XCTAssertEqual(
      ProcessProtocolFixture.decodingDiagnostic(in: frame.message),
      ProcessProtocolFixture.nullErrorDetailDiagnostic
    )
  }

  func testNonRejectedErrorForbidsDetailMember() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.nonRejectedErrorWithDetailFrame()
    )

    XCTAssertEqual(
      ProcessProtocolFixture.decodingDiagnostic(in: frame.message),
      ProcessProtocolFixture.forbiddenErrorDetailDiagnostic
    )
  }

  func testUnknownRejectionDetailDegradesKnownError() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.unknownRejectionDetailFrame()
    )

    XCTAssertEqual(
      ProcessProtocolFixture.decodingDiagnostic(in: frame.message),
      ProcessProtocolFixture.unknownRejectionDetailDiagnostic
    )
  }

  func testTurnControlRejectionsDecodeTypedDetails() throws {
    let notFound = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.toolRequestNotFoundFrame(toolRequestID: turnID)
    )
    let alreadyResolved = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.toolRequestAlreadyResolvedFrame(toolRequestID: turnID)
    )
    let notEarliest = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.toolRequestNotEarliestFrame(
        toolRequestID: turnID,
        earliestToolRequestID: sessionID
      )
    )
    let notInSession = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.toolRequestNotInSessionFrame(
        sessionID: sessionID,
        toolRequestID: turnID
      )
    )

    XCTAssertEqual(
      try ProcessProtocolFixture.rejectionDetail(in: notFound.message),
      .toolRequestNotFound(toolRequestID: try SignalboxCanonicalUUID(validating: turnID))
    )
    XCTAssertEqual(
      try ProcessProtocolFixture.rejectionDetail(in: alreadyResolved.message),
      .toolRequestAlreadyResolved(
        toolRequestID: try SignalboxCanonicalUUID(validating: turnID))
    )
    XCTAssertEqual(
      try ProcessProtocolFixture.rejectionDetail(in: notEarliest.message),
      .toolRequestNotEarliestUndecided(
        toolRequestID: try SignalboxCanonicalUUID(validating: turnID),
        earliestToolRequestID: try SignalboxCanonicalUUID(validating: sessionID)
      )
    )
    XCTAssertEqual(
      try ProcessProtocolFixture.rejectionDetail(in: notInSession.message),
      .toolRequestNotInSession(
        sessionID: try SignalboxCanonicalUUID(validating: sessionID),
        toolRequestID: try SignalboxCanonicalUUID(validating: turnID)
      )
    )
  }

  func testConversationSummaryRequiresNullableTitleMember() throws {
    let native = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.nativeConversationWithoutTitleFrame(sessionID: sessionID)
    )
    let imported = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.importedConversationWithoutTitleFrame(
        importedConversationID: sessionID
      )
    )

    XCTAssertEqual(
      ProcessProtocolFixture.decodingDiagnostic(in: native.message),
      ProcessProtocolFixture.missingConversationTitleDiagnostic
    )
    XCTAssertEqual(
      ProcessProtocolFixture.decodingDiagnostic(in: imported.message),
      ProcessProtocolFixture.missingConversationTitleDiagnostic
    )
  }

  func testPublicFrameDecoderRejectsOversizedInputBeforeScanning() {
    XCTAssertThrowsError(
      try SignalboxProcessServerFrame.decode(
        from: ProcessProtocolFixture.oversizedFrame()
      )
    ) {
      XCTAssertEqual(
        $0 as? SignalboxProcessFrameDecodingError,
        .oversizedFrame
      )
    }
  }
}

private enum ProcessProtocolFixture {
  static let unadmittedErrorFieldDiagnostic = SignalboxDecodingDiagnostic(
    message: "Invalid field value at message.extra."
  )
  static let unadmittedDetailFieldDiagnostic = SignalboxDecodingDiagnostic(
    message: "Invalid field value at message.detail.extra."
  )
  static let missingErrorDetailDiagnostic = SignalboxDecodingDiagnostic(
    message: "Missing required field at message.detail."
  )
  static let nullErrorDetailDiagnostic = SignalboxDecodingDiagnostic(
    message: "Missing required value at message.detail."
  )
  static let forbiddenErrorDetailDiagnostic = SignalboxDecodingDiagnostic(
    message: "Invalid field value at message.detail."
  )
  static let unknownRejectionDetailDiagnostic = SignalboxDecodingDiagnostic(
    message: "Invalid field value at message.detail."
  )
  static let missingConversationTitleDiagnostic = SignalboxDecodingDiagnostic(
    message: "Missing required field at message.conversation.title."
  )

  static func duplicateSnapshotBoundaryMessage(
    sessionID: String
  ) -> SignalboxProcessServerMessage {
    .unknown(
      kind: "transcript_snapshot_start",
      payload: [
        "type": .string("transcript_snapshot_start"),
        "session_id": .string(sessionID),
        "cursor": .string("12"),
      ],
      decodingDiagnostic: SignalboxDecodingDiagnostic(
        message: "Invalid field value at message."
      )
    )
  }

  static func assertNestedDuplicateMessage(
    _ message: SignalboxProcessServerMessage,
    file: StaticString = #filePath,
    line: UInt = #line
  ) {
    guard case .unknown(let kind, _, let diagnostic) = message else {
      XCTFail("Expected a diagnostic unknown message.", file: file, line: line)
      return
    }
    XCTAssertEqual(kind, "session_event", file: file, line: line)
    XCTAssertEqual(
      diagnostic,
      SignalboxDecodingDiagnostic(message: "Invalid field value at message."),
      file: file,
      line: line
    )
  }

  static func excessivelyNestedFrame() -> Data {
    Data(
      (String(repeating: "[", count: 128)
        + "null"
        + String(repeating: "]", count: 128)).utf8
    )
  }

  static func expandedErrorFrame() -> Data {
    Data(
      """
      {
        "version":18,
        "request_id":"9",
        "message":{
          "type":"error",
          "code":"not_found",
          "message":"fixture error",
          "extra":true
        }
      }
      """.utf8
    )
  }

  static func expandedErrorDetailFrame(
    sessionID: String
  ) -> Data {
    Data(
      """
      {
        "version":18,
        "request_id":"9",
        "message":{
          "type":"error",
          "code":"rejected",
          "message":"fixture rejection",
          "detail":{
            "type":"session_not_found",
            "session_id":"\(sessionID)",
            "extra":true
          }
        }
      }
      """.utf8
    )
  }

  static func rejectedErrorWithoutDetailFrame() -> Data {
    Data(
      """
      {
        "version":18,
        "request_id":"9",
        "message":{
          "type":"error",
          "code":"rejected",
          "message":"fixture rejection"
        }
      }
      """.utf8
    )
  }

  static func rejectedErrorWithNullDetailFrame() -> Data {
    Data(
      """
      {
        "version":18,
        "request_id":"9",
        "message":{
          "type":"error",
          "code":"rejected",
          "message":"fixture rejection",
          "detail":null
        }
      }
      """.utf8
    )
  }

  static func nonRejectedErrorWithDetailFrame() -> Data {
    Data(
      """
      {
        "version":18,
        "request_id":"9",
        "message":{
          "type":"error",
          "code":"not_found",
          "message":"fixture error",
          "detail":null
        }
      }
      """.utf8
    )
  }

  static func unknownRejectionDetailFrame() -> Data {
    Data(
      """
      {
        "version":18,
        "request_id":"9",
        "message":{
          "type":"error",
          "code":"rejected",
          "message":"fixture rejection",
          "detail":{"type":"future_rejection"}
        }
      }
      """.utf8
    )
  }

  static func toolRequestNotFoundFrame(toolRequestID: String) -> Data {
    rejectedFrame(
      detail:
        """
        {
          "type":"tool_request_not_found",
          "tool_request_id":"\(toolRequestID)"
        }
        """
    )
  }

  static func toolRequestAlreadyResolvedFrame(toolRequestID: String) -> Data {
    rejectedFrame(
      detail:
        """
        {
          "type":"tool_request_already_resolved",
          "tool_request_id":"\(toolRequestID)"
        }
        """
    )
  }

  static func toolRequestNotEarliestFrame(
    toolRequestID: String,
    earliestToolRequestID: String
  ) -> Data {
    rejectedFrame(
      detail:
        """
        {
          "type":"tool_request_not_earliest_undecided",
          "tool_request_id":"\(toolRequestID)",
          "earliest_tool_request_id":"\(earliestToolRequestID)"
        }
        """
    )
  }

  static func toolRequestNotInSessionFrame(
    sessionID: String,
    toolRequestID: String
  ) -> Data {
    rejectedFrame(
      detail:
        """
        {
          "type":"tool_request_not_in_session",
          "session_id":"\(sessionID)",
          "tool_request_id":"\(toolRequestID)"
        }
        """
    )
  }

  static func nativeConversationWithoutTitleFrame(sessionID: String) -> Data {
    conversationFrame(
      conversation:
        """
        {
          "origin":"native_session",
          "session_id":"\(sessionID)",
          "archived":false,
          "defaults_version":"1"
        }
        """
    )
  }

  static func importedConversationWithoutTitleFrame(
    importedConversationID: String
  ) -> Data {
    conversationFrame(
      conversation:
        """
        {
          "origin":"imported_conversation",
          "imported_conversation_id":"\(importedConversationID)",
          "entry_count":"1",
          "source_format":"codex_rollout_jsonl_v1"
        }
        """
    )
  }

  static func rejectionDetail(
    in message: SignalboxProcessServerMessage
  ) throws -> SignalboxRejectionDetail {
    guard case .protocolError(let error) = message, let detail = error.detail else {
      throw ProcessProtocolFixtureError.missingRejectionDetail
    }
    return detail
  }

  private static func rejectedFrame(detail: String) -> Data {
    Data(
      """
      {
        "version":18,
        "request_id":"9",
        "message":{
          "type":"error",
          "code":"rejected",
          "message":"fixture rejection",
          "detail":\(detail)
        }
      }
      """.utf8
    )
  }

  private static func conversationFrame(conversation: String) -> Data {
    Data(
      """
      {
        "version":18,
        "request_id":"9",
        "message":{
          "type":"conversation_summary",
          "conversation":\(conversation)
        }
      }
      """.utf8
    )
  }

  static func oversizedFrame() -> Data {
    Data(
      repeating: 0x20,
      count: SignalboxProcessProtocol.maximumFrameBytes + 1
    )
  }

  static func decodingDiagnostic(
    in message: SignalboxProcessServerMessage
  ) -> SignalboxDecodingDiagnostic? {
    guard case .unknown(_, _, let diagnostic) = message else {
      return nil
    }
    return diagnostic
  }
}

private enum ProcessProtocolFixtureError: Error {
  case missingRejectionDetail
}
