import Foundation
import XCTest

@testable import SignalboxNative

final class ProcessProtocolTests: XCTestCase {
  private let sessionID = "11111111-1111-4111-8111-111111111111"
  private let turnID = "22222222-2222-4222-8222-222222222222"

  func testClientFrameUsesVersionOneAndCanonicalStringScalars() throws {
    let frame = SignalboxProcessClientFrame(
      requestID: try SignalboxRequestID(validating: 7),
      request: .readTranscript(
        sessionID: try SignalboxCanonicalUUID(validating: sessionID)
      )
    )

    let encoded = try SignalboxJSONCoding.encoder().encode(frame)

    XCTAssertEqual(
      String(decoding: encoded, as: UTF8.self),
      #"{"request":{"session_id":"\#(sessionID)","type":"read_transcript"},"request_id":"7","version":1}"#
    )
  }

  /// INV-033: imported continuation requests retain their closed version-one shape.
  func testImportedContinuationRequestUsesTheVersionOneFrontierShape() throws {
    let importedConversationID = "33333333-3333-4333-8333-333333333333"
    let aliasID = "44444444-4444-4444-8444-444444444444"
    let frame = SignalboxProcessClientFrame(
      requestID: try SignalboxRequestID(validating: 10),
      request: .createSessionFromImportedFrontier(
        commandID: try SignalboxCommandID(validating: turnID),
        importedConversationID: try SignalboxCanonicalUUID(
          validating: importedConversationID
        ),
        throughPosition: SignalboxCanonicalUInt64(rawValue: 2),
        relationship: .resume,
        initialModelSelection: .alias(
          aliasID: try SignalboxCanonicalUUID(validating: aliasID)
        )
      )
    )

    let encoded = try SignalboxJSONCoding.encoder().encode(frame)

    XCTAssertEqual(
      String(decoding: encoded, as: UTF8.self),
      #"{"request":{"command_id":"\#(turnID)","imported_conversation_id":"\#(importedConversationID)","initial_model_selection":{"alias_id":"\#(aliasID)","kind":"alias"},"relationship":"resume","through_position":"2","type":"create_session_from_imported_frontier"},"request_id":"10","version":1}"#
    )
  }

  /// INV-033: admitted imported-entry members decode without weakening the closed shape.
  func testImportedConversationEntryDecodesItsAttestedTextPreview() throws {
    let importedEntryID = "33333333-3333-4333-8333-333333333333"
    let position = SignalboxCanonicalUInt64(rawValue: 1)
    let preview = "User fixture text"
    let encoded = Data(
      """
      {
        "version":1,
        "request_id":"10",
        "message":{
          "type":"imported_conversation_entry",
          "position":"\(position.rawValue)",
          "imported_entry_id":"\(importedEntryID)",
          "source_speaker":{"type":"attested","speaker":"user"},
          "content_kind":"text",
          "text_preview":{"preview":"\(preview)","truncated":false}
        }
      }
      """.utf8
    )

    let frame = try SignalboxProcessServerFrame.decode(from: encoded)
    let entry = try ProcessProtocolFixture.importedEntry(in: frame.message)

    XCTAssertEqual(entry.position, position)
    XCTAssertEqual(entry.importedEntryID.rawValue, importedEntryID)
    XCTAssertEqual(entry.sourceSpeakerLabel, "User")
    XCTAssertEqual(entry.contentKind, .text)
    XCTAssertEqual(entry.textPreview?.preview, preview)
    XCTAssertEqual(entry.textPreview?.truncated, false)
  }

  /// INV-033: omitting a required nullable imported-entry member fails explicitly.
  func testImportedConversationEntryRequiresExplicitNullablePreview() throws {
    let encoded = Data(
      """
      {
        "version":1,
        "request_id":"10",
        "message":{
          "type":"imported_conversation_entry",
          "position":"1",
          "imported_entry_id":"33333333-3333-4333-8333-333333333333",
          "source_speaker":{"type":"not_attested"},
          "content_kind":"source_event"
        }
      }
      """.utf8
    )

    let frame = try SignalboxProcessServerFrame.decode(from: encoded)
    let diagnostic = try ProcessProtocolFixture.unknownDiagnostic(in: frame.message)

    XCTAssertTrue(diagnostic.message.contains("text_preview"))
  }

  /// INV-033: an unknown imported source-speaker variant fails explicitly.
  func testImportedConversationEntryRejectsUnknownSourceSpeaker() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.importedEntryWithUnknownSourceSpeakerFrame()
    )
    let diagnostic = try ProcessProtocolFixture.unknownDiagnostic(in: frame.message)

    XCTAssertEqual(diagnostic, ProcessProtocolFixture.unknownImportedSourceSpeakerDiagnostic)
  }

  /// INV-033: explicit null preview admission stays distinct from an omitted member.
  func testImportedConversationEntryAcceptsAttestedSpeakerWithoutTextPreview() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.attestedSpeakerWithoutTextPreviewFrame()
    )
    let entry = try ProcessProtocolFixture.importedEntry(in: frame.message)

    XCTAssertEqual(entry.sourceSpeakerLabel, "User")
    XCTAssertEqual(entry.contentKind, .text)
    XCTAssertNil(entry.textPreview)
  }

  func testModelAliasCatalogRequestAndSummaryUseClosedVersionOneShapes() throws {
    let requestFrame = SignalboxProcessClientFrame(
      requestID: try SignalboxRequestID(validating: 8),
      request: .listModelAliases
    )
    let encodedRequest = try SignalboxJSONCoding.encoder().encode(requestFrame)
    XCTAssertEqual(
      String(decoding: encodedRequest, as: UTF8.self),
      #"{"request":{"type":"list_model_aliases"},"request_id":"8","version":1}"#
    )

    let encodedSummary = Data(
      """
      {
        "version":1,
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
        "version":1,
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

  func testContextCompactionFramesDecodeTheirCurrentShapes() throws {
    let contextCompactionID = "33333333-3333-4333-8333-333333333333"
    let modelCallID = "44444444-4444-4444-8444-444444444444"
    let firstEntryID = "55555555-5555-4555-8555-555555555555"
    let summaryEntryID = "66666666-6666-4666-8666-666666666666"
    let frontierID = "77777777-7777-4777-8777-777777777777"
    let summaryFrame = try SignalboxProcessServerFrame.decode(
      from: Data(
        """
        {
          "version":1,
          "request_id":"9",
          "message":{
            "type":"transcript_text_entry",
            "entry_index":"0",
            "source_session_id":"\(sessionID)",
            "entry_id":"\(summaryEntryID)",
            "entry":{
              "type":"context_summary",
              "model_call_id":"\(modelCallID)",
              "first_source_session_id":"\(sessionID)",
              "first_entry_id":"\(firstEntryID)",
              "through_source_session_id":"\(sessionID)",
              "through_entry_id":"\(firstEntryID)"
            }
          }
        }
        """.utf8
      )
    )
    let compactedFrame = try SignalboxProcessServerFrame.decode(
      from: Data(
        """
        {
          "version":1,
          "request_id":"9",
          "message":{
            "type":"session_event",
            "cursor":"12",
            "session_id":"\(sessionID)",
            "event":{
              "type":"context_compacted",
              "context_compaction_id":"\(contextCompactionID)",
              "model_call_id":"\(modelCallID)",
              "through_position":"11",
              "summary_entry_id":"\(summaryEntryID)",
              "result_frontier_id":"\(frontierID)"
            }
          }
        }
        """.utf8
      )
    )

    XCTAssertEqual(
      summaryFrame.message,
      .transcriptTextEntry(
        SignalboxTranscriptTextEntryMessage(
          entryIndex: SignalboxCanonicalUInt64(rawValue: 0),
          sourceSessionID: try SignalboxCanonicalUUID(validating: sessionID),
          entryID: try SignalboxCanonicalUUID(validating: summaryEntryID),
          entry: .contextSummary(
            modelCallID: try SignalboxCanonicalUUID(validating: modelCallID),
            firstSourceSessionID: try SignalboxCanonicalUUID(validating: sessionID),
            firstEntryID: try SignalboxCanonicalUUID(validating: firstEntryID),
            throughSourceSessionID: try SignalboxCanonicalUUID(validating: sessionID),
            throughEntryID: try SignalboxCanonicalUUID(validating: firstEntryID)
          )
        )
      )
    )
    XCTAssertEqual(
      compactedFrame.message,
      .sessionEvent(
        SignalboxFollowedSessionEvent(
          cursor: SignalboxCanonicalUInt64(rawValue: 12),
          sessionID: try SignalboxCanonicalUUID(validating: sessionID),
          event: .contextCompacted(
            contextCompactionID: try SignalboxCanonicalUUID(validating: contextCompactionID),
            modelCallID: try SignalboxCanonicalUUID(validating: modelCallID),
            throughPosition: SignalboxCanonicalUInt64(rawValue: 11),
            summaryEntryID: try SignalboxCanonicalUUID(validating: summaryEntryID),
            resultFrontierID: try SignalboxCanonicalUUID(validating: frontierID)
          )
        )
      )
    )
  }

  func testUnknownSessionEventDoesNotDiscardItsFrame() throws {
    let encoded = Data(
      """
      {
        "version":1,
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

  func testTranscriptModelCallEvidenceFramesDecodeAsKnownMessages() throws {
    let usageFrame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.modelCallUsageFrame(turnID: turnID)
    )
    let endFrame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.modelCallsEndFrame()
    )
    let evidence = try ProcessProtocolFixture.modelCallUsage(in: usageFrame.message)

    XCTAssertEqual(evidence.modelCallIndex.rawValue, 0)
    XCTAssertEqual(evidence.turnID.rawValue, turnID)
    XCTAssertEqual(evidence.modelCallID.rawValue, ProcessProtocolFixture.modelCallID)
    XCTAssertEqual(evidence.usageProvenance, .reported)
    XCTAssertEqual(evidence.usage.inputTokens?.rawValue, 10)
    XCTAssertEqual(evidence.usage.outputTokens?.rawValue, 0)
    XCTAssertNil(evidence.usage.cacheCreationInputTokens)
    XCTAssertEqual(evidence.usage.cacheReadInputTokens?.rawValue, 4)
    XCTAssertEqual(evidence.cost?.amountUSD.rawValue, "0.125")
    XCTAssertEqual(evidence.cost?.rateVersion.rawValue, "rates-v7")
    XCTAssertEqual(evidence.cost?.label, .meteredEquivalent)
    XCTAssertEqual(try ProcessProtocolFixture.modelCallCount(in: endFrame.message), 1)
  }

  func testTranscriptModelCallCostWithoutAUsageAxisDegradesWithDiagnostic() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.modelCallCostWithoutUsageAxisFrame(turnID: turnID)
    )

    XCTAssertNotNil(ProcessProtocolFixture.decodingDiagnostic(in: frame.message))
  }

  func testBillingRateVersionAdmitsZeroWidthSpaceOutsideProtocolTrimSet() throws {
    let spelling = "\u{200B}rates-v7"
    let encoded = try SignalboxJSONCoding.encoder().encode(spelling)

    let version = try SignalboxJSONCoding.decoder().decode(
      SignalboxBillingRateVersion.self,
      from: encoded
    )

    XCTAssertEqual(version.rawValue, spelling)
  }

  func testFailedTerminalProviderCauseDecodesAsAClosedClassification() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.failedTurnFrame(
        turnID: turnID, cause: "quota_exhausted"
      )
    )

    XCTAssertEqual(
      try ProcessProtocolFixture.failedProviderCause(in: frame.message),
      .quotaExhausted
    )
  }

  func testCancelledTerminalModelCallRejectsProviderFailureCause() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.failedTurnFrame(
        turnID: turnID, cause: "quota_exhausted", disposition: "cancelled"
      )
    )

    XCTAssertNotNil(ProcessProtocolFixture.turnStateDecodingDiagnostic(in: frame.message))
  }

  func testFailedTerminalModelCallRejectsExplicitNullCause() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.failedTurnWithNullCauseFrame(turnID: turnID)
    )

    XCTAssertNotNil(ProcessProtocolFixture.turnStateDecodingDiagnostic(in: frame.message))
  }

  func testMalformedKnownMessageDegradesWithDiagnostic() throws {
    let encoded = Data(
      """
      {
        "version":1,
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
        "version":1,
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
        "version":1,
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
        "version":1,
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
  static let unknownImportedSourceSpeakerKind = "future_speaker"
  static let unknownImportedSourceSpeakerDiagnostic = SignalboxDecodingDiagnostic(
    message: "Invalid field value at message.source_speaker."
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
        "version":1,
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
        "version":1,
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
        "version":1,
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
        "version":1,
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
        "version":1,
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
        "version":1,
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

  static func importedEntryWithUnknownSourceSpeakerFrame() -> Data {
    Data(
      """
      {
        "version":1,
        "request_id":"10",
        "message":{
          "type":"imported_conversation_entry",
          "position":"1",
          "imported_entry_id":"33333333-3333-4333-8333-333333333333",
          "source_speaker":{"type":"\(unknownImportedSourceSpeakerKind)"},
          "content_kind":"source_event",
          "text_preview":null
        }
      }
      """.utf8
    )
  }

  static func attestedSpeakerWithoutTextPreviewFrame() -> Data {
    Data(
      """
      {
        "version":1,
        "request_id":"10",
        "message":{
          "type":"imported_conversation_entry",
          "position":"1",
          "imported_entry_id":"33333333-3333-4333-8333-333333333333",
          "source_speaker":{"type":"attested","speaker":"user"},
          "content_kind":"text",
          "text_preview":null
        }
      }
      """.utf8
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
        "version":1,
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
        "version":1,
        "request_id":"9",
        "message":{
          "type":"conversation_summary",
          "conversation":\(conversation)
        }
      }
      """.utf8
    )
  }

  static let modelCallID = "55555555-5555-4555-8555-555555555555"
  private static let attemptID = "66666666-6666-4666-8666-666666666666"
  private static let frontierID = "77777777-7777-4777-8777-777777777777"

  static func modelCallUsageFrame(turnID: String) -> Data {
    Data(
      """
      {
        "version":1,
        "request_id":"9",
        "message":{
          "type":"transcript_model_call_usage",
          "model_call_index":"0",
          "turn_id":"\(turnID)",
          "model_call_id":"\(modelCallID)",
          "usage_provenance":"reported",
          "usage":{
            "input_tokens":"10",
            "output_tokens":"0",
            "cache_creation_input_tokens":null,
            "cache_read_input_tokens":"4"
          },
          "cost":{
            "amount_usd":"0.125",
            "rate_version":"rates-v7",
            "label":"metered_equivalent"
          }
        }
      }
      """.utf8
    )
  }

  static func modelCallCostWithoutUsageAxisFrame(turnID: String) -> Data {
    Data(
      """
      {
        "version":1,
        "request_id":"9",
        "message":{
          "type":"transcript_model_call_usage",
          "model_call_index":"0",
          "turn_id":"\(turnID)",
          "model_call_id":"\(modelCallID)",
          "usage_provenance":"reported",
          "usage":{
            "input_tokens":null,
            "output_tokens":null,
            "cache_creation_input_tokens":null,
            "cache_read_input_tokens":null
          },
          "cost":{
            "amount_usd":"0",
            "rate_version":"rates-v7",
            "label":"real"
          }
        }
      }
      """.utf8
    )
  }

  static func modelCallsEndFrame() -> Data {
    Data(
      """
      {
        "version":1,
        "request_id":"9",
        "message":{
          "type":"transcript_model_calls_end",
          "model_call_count":"1"
        }
      }
      """.utf8
    )
  }

  static func failedTurnFrame(
    turnID: String, cause: String, disposition: String = "known_failed"
  ) -> Data {
    Data(
      """
      {
        "version":1,
        "request_id":"9",
        "message":{
          "type":"transcript_turn",
          "turn_id":"\(turnID)",
          "acceptance_position":"1",
          "state":{
            "type":"failed",
            "terminal_frontier_id":"\(frontierID)",
            "terminal_attempt_id":"\(attemptID)",
            "terminal_model_call":{
              "model_call_id":"\(modelCallID)",
              "disposition":"\(disposition)",
              "cause":"\(cause)"
            }
          }
        }
      }
      """.utf8
    )
  }

  static func failedTurnWithNullCauseFrame(turnID: String) -> Data {
    Data(
      """
      {
        "version":1,
        "request_id":"9",
        "message":{
          "type":"transcript_turn",
          "turn_id":"\(turnID)",
          "acceptance_position":"1",
          "state":{
            "type":"failed",
            "terminal_frontier_id":"\(frontierID)",
            "terminal_attempt_id":"\(attemptID)",
            "terminal_model_call":{
              "model_call_id":"\(modelCallID)",
              "disposition":"known_failed",
              "cause":null
            }
          }
        }
      }
      """.utf8
    )
  }

  static func modelCallUsage(
    in message: SignalboxProcessServerMessage
  ) throws -> SignalboxTranscriptModelCallUsage {
    guard case .transcriptModelCallUsage(let evidence) = message else {
      throw ProcessProtocolFixtureError.missingModelCallUsage
    }
    return evidence
  }

  static func importedEntry(
    in message: SignalboxProcessServerMessage
  ) throws -> SignalboxImportedConversationEntry {
    guard case .importedConversationEntry(let entry) = message else {
      throw ProcessProtocolFixtureError.missingImportedEntry
    }
    return entry
  }

  static func unknownDiagnostic(
    in message: SignalboxProcessServerMessage
  ) throws -> SignalboxDecodingDiagnostic {
    guard case .unknown(_, _, let diagnostic) = message, let diagnostic else {
      throw ProcessProtocolFixtureError.missingUnknownDiagnostic
    }
    return diagnostic
  }

  static func modelCallCount(
    in message: SignalboxProcessServerMessage
  ) throws -> UInt64 {
    guard case .transcriptModelCallsEnd(let count) = message else {
      throw ProcessProtocolFixtureError.missingModelCallsEnd
    }
    return count.rawValue
  }

  static func failedProviderCause(
    in message: SignalboxProcessServerMessage
  ) throws -> SignalboxFailedModelCallCause {
    guard
      case .transcriptTurn(let turn) = message,
      case .failed(_, _, let terminalModelCall) = turn.state,
      let cause = terminalModelCall?.cause
    else {
      throw ProcessProtocolFixtureError.missingProviderFailureCause
    }
    return cause
  }

  static func oversizedFrame() -> Data {
    Data(
      repeating: 0x20,
      count: SignalboxProcessProtocol.maximumFrameBytes + 1
    )
  }

  static func turnStateDecodingDiagnostic(
    in message: SignalboxProcessServerMessage
  ) -> SignalboxDecodingDiagnostic? {
    guard
      case .transcriptTurn(let turn) = message,
      case .unknown(_, _, let diagnostic) = turn.state
    else {
      return nil
    }
    return diagnostic
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
  case missingImportedEntry
  case missingRejectionDetail
  case missingModelCallUsage
  case missingModelCallsEnd
  case missingProviderFailureCause
  case missingUnknownDiagnostic
}
