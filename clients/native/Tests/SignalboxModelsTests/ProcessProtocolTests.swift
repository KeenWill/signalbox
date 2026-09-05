import Foundation
import XCTest

@testable import SignalboxNative

final class ProcessProtocolTests: XCTestCase {
  private let sessionID = "11111111-1111-4111-8111-111111111111"
  private let turnID = "22222222-2222-4222-8222-222222222222"
  private let toolRequestID = "33333333-3333-4333-8333-333333333333"
  private let blobDigest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"

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

  /// every metadata last-writer actor the daemon can send decodes into
  /// its own typed variant carrying the reference that actor object states, so
  /// a tool-written or model-written snapshot is readable rather than opaque.
  func testMetadataLastWriterDecodesEveryActor() throws {
    let userFrame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.metadataReadFrame(
        sessionID: sessionID,
        actorJSON: #"{"type":"user"}"#
      )
    )
    XCTAssertEqual(try ProcessProtocolFixture.metadataActor(in: userFrame.message), .user)

    let modelFrame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.metadataReadFrame(
        sessionID: sessionID,
        actorJSON: #"{"type":"model","turn_id":"\#(turnID)"}"#
      )
    )
    XCTAssertEqual(
      try ProcessProtocolFixture.metadataActor(in: modelFrame.message),
      .model(turnID: try SignalboxCanonicalUUID(validating: turnID))
    )

    let recoveryFrame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.metadataReadFrame(
        sessionID: sessionID,
        actorJSON: #"{"type":"recovery"}"#
      )
    )
    XCTAssertEqual(try ProcessProtocolFixture.metadataActor(in: recoveryFrame.message), .recovery)

    let toolFrame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.metadataReadFrame(
        sessionID: sessionID,
        actorJSON: #"{"type":"tool","tool_request_id":"\#(toolRequestID)"}"#
      )
    )
    XCTAssertEqual(
      try ProcessProtocolFixture.metadataActor(in: toolFrame.message),
      .tool(toolRequestID: try SignalboxCanonicalUUID(validating: toolRequestID))
    )
  }

  /// every metadata last-writer actor encodes to its exact wire bytes
  /// and decodes back to the same value. The two arms are hand-written and
  /// separate, so an encoder that dropped a carried reference, or spelled a
  /// member differently from the decoder, would otherwise ship unseen.
  func testMetadataLastWriterActorRoundTripsToExactBytes() throws {
    try assertMetadataActorRoundTrips(.user, #"{"type":"user"}"#)
    try assertMetadataActorRoundTrips(
      .model(turnID: try SignalboxCanonicalUUID(validating: turnID)),
      #"{"turn_id":"\#(turnID)","type":"model"}"#
    )
    try assertMetadataActorRoundTrips(.recovery, #"{"type":"recovery"}"#)
    try assertMetadataActorRoundTrips(
      .tool(toolRequestID: try SignalboxCanonicalUUID(validating: toolRequestID)),
      #"{"tool_request_id":"\#(toolRequestID)","type":"tool"}"#
    )
  }

  /// Pins one actor's exact encoded bytes, then decodes those same bytes back,
  /// so neither arm can drift without the other. A failure reports the call
  /// site's actor rather than this helper.
  private func assertMetadataActorRoundTrips(
    _ actor: SignalboxMetadataActor,
    _ expectedJSON: String,
    file: StaticString = #filePath,
    line: UInt = #line
  ) throws {
    let encoded = try SignalboxJSONCoding.encoder().encode(actor)

    XCTAssertEqual(
      String(decoding: encoded, as: UTF8.self),
      expectedJSON,
      file: file,
      line: line
    )
    XCTAssertEqual(
      try SignalboxJSONCoding.decoder().decode(SignalboxMetadataActor.self, from: encoded),
      actor,
      file: file,
      line: line
    )
  }

  /// turn stops encode the required descendant scope in version one.
  func testTurnStopRequestEncodesItsDescendantScope() throws {
    let frame = SignalboxProcessClientFrame(
      requestID: try SignalboxRequestID(validating: 9),
      request: .stopTurn(
        commandID: try SignalboxCommandID(validating: turnID),
        sessionID: try SignalboxCanonicalUUID(validating: sessionID),
        expectedActiveTurnID: try SignalboxCanonicalUUID(validating: turnID),
        content: "Stop and continue",
        expectedDefaultsVersion: SignalboxCanonicalUInt64(rawValue: 3),
        descendantScope: .parentAndDescendants
      )
    )

    let encoded = try SignalboxJSONCoding.encoder().encode(frame)

    XCTAssertEqual(
      String(decoding: encoded, as: UTF8.self),
      #"{"request":{"command_id":"\#(turnID)","content":[{"text":"Stop and continue","type":"text"}],"descendant_scope":"parent_and_descendants","expected_active_turn_id":"\#(turnID)","expected_defaults_version":"3","model_settings":{"fast_mode":{"kind":"inherit"},"reasoning_level":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},"session_id":"\#(sessionID)","type":"stop_turn"},"request_id":"9","version":1}"#
    )
  }

  /// multipart decoding preserves ordered attachment
  /// metadata and structural replay equality.
  func testUserInputContentPreservesOrderedAttachmentMetadata() throws {
    let content = try SignalboxUserInputContent(validating: [
      .text("before"),
      .attachment(
        digest: try SignalboxCanonicalBlobDigest(validating: blobDigest),
        kind: .document,
        mediaType: "application/pdf",
        displayFilename: "brief.pdf"
      ),
      .text("after"),
    ])

    let encoded = try SignalboxJSONCoding.encoder().encode(content)
    let decoded = try SignalboxJSONCoding.decoder().decode(
      SignalboxUserInputContent.self,
      from: encoded
    )

    XCTAssertEqual(decoded, content)
  }

  func testTranscriptUserEntryDecodesTypedMultipartContent() throws {
    let entryID = "44444444-4444-4444-8444-444444444444"
    let message = try SignalboxJSONCoding.decoder().decode(
      SignalboxProcessServerMessage.self,
      from: Data(
        """
        {
          "type":"transcript_user_entry",
          "entry_index":"0",
          "source_session_id":"\(sessionID)",
          "entry_id":"\(entryID)",
          "accepted_input_id":"\(toolRequestID)",
          "turn_id":"\(turnID)",
          "content":[
            {"type":"text","text":"before"},
            {
              "type":"attachment",
              "digest":"\(blobDigest)",
              "kind":"document",
              "media_type":"application/pdf",
              "display_filename":"brief.pdf"
            },
            {"type":"text","text":"after"}
          ]
        }
        """.utf8
      )
    )

    guard case .transcriptUserEntry(let entry) = message else {
      return XCTFail("Expected a typed transcript user entry.")
    }
    XCTAssertEqual(entry.entryIndex, SignalboxCanonicalUInt64(rawValue: 0))
    XCTAssertEqual(entry.sourceSessionID.rawValue, sessionID)
    XCTAssertEqual(entry.entryID.rawValue, entryID)
    XCTAssertEqual(entry.acceptedInputID.rawValue, toolRequestID)
    XCTAssertEqual(entry.turnID.rawValue, turnID)
    XCTAssertEqual(
      entry.content,
      try SignalboxUserInputContent(validating: [
        .text("before"),
        .attachment(
          digest: SignalboxCanonicalBlobDigest(validating: blobDigest),
          kind: .document,
          mediaType: "application/pdf",
          displayFilename: "brief.pdf"
        ),
        .text("after"),
      ])
    )
  }

  func testUserInputContentRejectsAdjacentTextParts() {
    XCTAssertThrowsError(
      try SignalboxUserInputContent(validating: [.text("first"), .text("second")])
    )
  }

  /// native multipart decoding stops at the retained-parts bound
  /// without decoding an unbounded remainder.
  func testUserInputContentDecodingStopsAtThePartLimit() throws {
    let retained = Array(
      repeating: #"{"type":"text","text":"x"}"#,
      count: SignalboxProcessProtocol.maximumUserInputParts
    )
    let encoded = Data(("[" + (retained + ["false"]).joined(separator: ",") + "]").utf8)

    XCTAssertThrowsError(
      try SignalboxJSONCoding.decoder().decode(SignalboxUserInputContent.self, from: encoded)
    ) { error in
      guard case DecodingError.dataCorrupted(let context) = error else {
        return XCTFail("Expected the multipart count error, got \(error).")
      }
      XCTAssertEqual(context.debugDescription, "User input part count is invalid.")
    }
  }

  func testUserInputContentDisplayTextEscapesFilenameLineBreaks() throws {
    let content = try SignalboxUserInputContent(validating: [
      .attachment(
        digest: try SignalboxCanonicalBlobDigest(validating: blobDigest),
        kind: .document,
        mediaType: "application/pdf",
        displayFilename: "brief.pdf\n[trusted-looking transcript line]"
      )
    ])

    XCTAssertEqual(
      content.displayText,
      "[attachment document \"brief.pdf\\n[trusted-looking transcript line]\" \(blobDigest)]"
    )
  }

  func testNewerProtocolVersionRemainsClassifiable() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.newerVersionFrame()
    )

    XCTAssertEqual(frame.version, .unknown(ProcessProtocolFixture.newerVersion))
    XCTAssertEqual(frame.message, .sessionsStart)
  }

  func testSessionDefaultsRequireTheModelSettingsSnapshot() {
    let encoded = Data(
      """
      {
        "type":"session_defaults",
        "session_id":"\(sessionID)",
        "defaults_version":"1",
        "model_selection":{"kind":"direct","selection_id":"\(turnID)"},
        "dangerous_tool_auto_approval":false,
        "system_prompt":null
      }
      """.utf8
    )

    XCTAssertThrowsError(
      try SignalboxJSONCoding.decoder().decode(SignalboxSessionDefaultsRead.self, from: encoded)
    )
  }

  func testSessionDefaultsRejectMalformedModelSettings() {
    let encoded = Data(
      """
      {
        "type":"session_defaults",
        "session_id":"\(sessionID)",
        "defaults_version":"1",
        "model_selection":{"kind":"direct","selection_id":"\(turnID)"},
        "model_settings":{},
        "dangerous_tool_auto_approval":false,
        "system_prompt":null
      }
      """.utf8
    )

    XCTAssertThrowsError(
      try SignalboxJSONCoding.decoder().decode(SignalboxSessionDefaultsRead.self, from: encoded)
    )
  }

  func testSessionDefaultsRejectSettingsValidatedForAnotherDirectModel() {
    let modelSettings = String(
      decoding: ProcessProtocolFixture.modelSettingsSnapshot(
        validatedForSelectionID: "\"\(sessionID)\""
      ),
      as: UTF8.self
    )
    let encoded = Data(
      """
      {
        "type":"session_defaults",
        "session_id":"\(sessionID)",
        "defaults_version":"1",
        "model_selection":{"kind":"direct","selection_id":"\(turnID)"},
        "model_settings":\(modelSettings),
        "dangerous_tool_auto_approval":false,
        "system_prompt":null
      }
      """.utf8
    )

    XCTAssertThrowsError(
      try SignalboxJSONCoding.decoder().decode(SignalboxSessionDefaultsRead.self, from: encoded)
    )
  }

  func testSessionDefaultsRejectPerCallModelSettings() {
    let modelSettings = String(
      decoding: ProcessProtocolFixture.modelSettingsSnapshot(
        perCallReasoning: #"{"kind":"value","value":"high"}"#,
        effectiveReasoning: "\"high\"",
        reasoningSource: "\"per_call\"",
        validatedForSelectionID: "\"\(turnID)\""
      ),
      as: UTF8.self
    )
    let encoded = Data(
      """
      {
        "type":"session_defaults",
        "session_id":"\(sessionID)",
        "defaults_version":"1",
        "model_selection":{"kind":"direct","selection_id":"\(turnID)"},
        "model_settings":\(modelSettings),
        "dangerous_tool_auto_approval":false,
        "system_prompt":null
      }
      """.utf8
    )

    XCTAssertThrowsError(
      try SignalboxJSONCoding.decoder().decode(SignalboxSessionDefaultsRead.self, from: encoded)
    )
  }

  func testSessionCreatedRequiresTheModelSettingsSnapshot() throws {
    let encoded = Data(
      """
      {
        "type":"session_created",
        "session_id":"\(sessionID)"
      }
      """.utf8
    )

    let message = try SignalboxJSONCoding.decoder().decode(
      SignalboxProcessServerMessage.self,
      from: encoded
    )

    XCTAssertNotNil(ProcessProtocolFixture.decodingDiagnostic(in: message))
  }

  func testSessionCreatedRejectsMalformedModelSettings() throws {
    let encoded = Data(
      """
      {
        "type":"session_created",
        "session_id":"\(sessionID)",
        "model_settings":{}
      }
      """.utf8
    )

    let message = try SignalboxJSONCoding.decoder().decode(
      SignalboxProcessServerMessage.self,
      from: encoded
    )

    XCTAssertNotNil(ProcessProtocolFixture.decodingDiagnostic(in: message))
  }

  func testSessionCreatedRejectsPerCallModelSettings() throws {
    let modelSettings = String(
      decoding: ProcessProtocolFixture.modelSettingsSnapshot(
        perCallReasoning: #"{"kind":"provider_default"}"#,
        reasoningSource: "\"per_call\"",
        validatedForSelectionID: "\"\(turnID)\""
      ),
      as: UTF8.self
    )
    let encoded = Data(
      """
      {
        "type":"session_created",
        "session_id":"\(sessionID)",
        "model_settings":\(modelSettings)
      }
      """.utf8
    )

    let message = try SignalboxJSONCoding.decoder().decode(
      SignalboxProcessServerMessage.self,
      from: encoded
    )

    XCTAssertNotNil(ProcessProtocolFixture.decodingDiagnostic(in: message))
  }

  func testModelSettingsSnapshotRejectsContradictoryEffectiveValue() {
    let encoded = ProcessProtocolFixture.modelSettingsSnapshot(
      effectiveReasoning: "\"high\"",
      reasoningSource: "null",
      validatedForSelectionID: "null"
    )

    XCTAssertThrowsError(
      try SignalboxJSONCoding.decoder().decode(
        SignalboxModelSettingsSnapshot.self,
        from: encoded
      )
    )
  }

  func testModelSettingsSnapshotRequiresValidationForNondefaultValue() {
    let encoded = ProcessProtocolFixture.modelSettingsSnapshot(
      sessionReasoning: #"{"kind":"value","value":"high"}"#,
      effectiveReasoning: "\"high\"",
      reasoningSource: "\"session\"",
      validatedForSelectionID: "null"
    )

    XCTAssertThrowsError(
      try SignalboxJSONCoding.decoder().decode(
        SignalboxModelSettingsSnapshot.self,
        from: encoded
      )
    )
  }

  func testInputSubmittedRequiresTheModelSettingsSnapshot() {
    let encoded = Data(
      """
      {
        "type":"input_submitted",
        "session_id":"\(sessionID)",
        "accepted_input_id":"\(turnID)",
        "acceptance_position":"1",
        "turn_id":"\(turnID)"
      }
      """.utf8
    )

    XCTAssertThrowsError(
      try SignalboxJSONCoding.decoder().decode(SignalboxInputSubmitted.self, from: encoded)
    )
  }

  func testInputSubmittedRejectsMalformedModelSettings() {
    let encoded = Data(
      """
      {
        "type":"input_submitted",
        "session_id":"\(sessionID)",
        "accepted_input_id":"\(turnID)",
        "acceptance_position":"1",
        "turn_id":"\(turnID)",
        "model_settings":{}
      }
      """.utf8
    )

    XCTAssertThrowsError(
      try SignalboxJSONCoding.decoder().decode(SignalboxInputSubmitted.self, from: encoded)
    )
  }

  /// imported continuation requests retain their closed version-one shape.
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
      #"{"request":{"command_id":"\#(turnID)","imported_conversation_id":"\#(importedConversationID)","initial_model_selection":{"alias_id":"\#(aliasID)","kind":"alias"},"model_settings":{"fast_mode":{"kind":"inherit"},"reasoning_level":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},"relationship":"resume","through_position":"2","type":"create_session_from_imported_frontier"},"request_id":"10","version":1}"#
    )
  }

  /// admitted imported-entry members decode without weakening the closed shape.
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

  /// omitting a required nullable imported-entry member fails explicitly.
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

  func testImportedConversationEntryPreservesUnknownSourceSpeaker() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.importedEntryWithUnknownSourceSpeakerFrame()
    )
    let entry = try ProcessProtocolFixture.importedEntry(in: frame.message)

    XCTAssertEqual(
      entry.sourceSpeaker,
      .unknown(
        kind: ProcessProtocolFixture.unknownImportedSourceSpeakerKind,
        payload: [
          "type": .string(ProcessProtocolFixture.unknownImportedSourceSpeakerKind)
        ]
      )
    )
    XCTAssertEqual(
      entry.sourceSpeakerLabel,
      "Unknown speaker (\(ProcessProtocolFixture.unknownImportedSourceSpeakerKind))"
    )
  }

  func testUnknownImportedEntryPresentationLabelsAreBounded() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.importedEntryWithUnknownSourceSpeakerFrame(
        sourceSpeakerKind: ProcessProtocolFixture.oversizedUnknownPresentationToken,
        contentKind: ProcessProtocolFixture.oversizedUnknownPresentationToken
      )
    )
    let entry = try ProcessProtocolFixture.importedEntry(in: frame.message)

    XCTAssertEqual(
      entry.sourceSpeakerLabel.utf8.count,
      SignalboxProcessPresentation.maximumLabelUTF8Bytes
    )
    XCTAssertEqual(
      entry.contentKindLabel.utf8.count,
      SignalboxProcessPresentation.maximumLabelUTF8Bytes
    )
  }

  /// explicit null preview admission stays distinct from an omitted member.
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

  /// session summaries retain the same complete runner
  /// projection as transcript snapshot boundaries.
  func testSessionSummaryDecodesCompleteRunnerProjection() throws {
    let runnerID = "44444444-4444-4444-8444-444444444444"
    let frame = try SignalboxProcessServerFrame.decode(
      from: Data(
        """
        {
          "version":1,
          "request_id":"8",
          "message":{
            "type":"session_summary",
            "session_id":"\(sessionID)",
            "defaults_version":"1",
            "model_selection":{"kind":"alias","alias_id":"\(turnID)"},
            "placement_version":"1",
            "placement":{"kind":"scoped","path":"workspace.project"},
            "runner":{
              "selector":{"type":"capability_class","name":"linux.workspace"},
              "runner_id":"\(runnerID)",
              "placement_revision":"3",
              "sandbox_profile":"workspace-restricted",
              "credential_profile":"readonly",
              "repository":"primary",
              "working_directory":"workspace/project",
              "connection_health":null,
              "state":"runner_lost"
            }
          }
        }
        """.utf8
      )
    )
    let projection = try SignalboxRunnerProjection(
      selector: .capabilityClass(
        name: SignalboxRunnerCapabilityClass(validating: "linux.workspace")
      ),
      runnerID: SignalboxCanonicalUUID(validating: runnerID),
      placementRevision: SignalboxCanonicalUInt64(rawValue: 3),
      sandboxProfile: .workspaceRestricted,
      credentialProfile: SignalboxRunnerCredentialProfileName(validating: "readonly"),
      repository: SignalboxRunnerRepositoryKey(validating: "primary"),
      workingDirectory: SignalboxRunnerWorkingDirectory(validating: "workspace/project"),
      connectionHealth: nil,
      state: .runnerLost
    )
    let expected = SignalboxProcessSessionSummary(
      sessionID: try SignalboxCanonicalUUID(validating: sessionID),
      defaultsVersion: SignalboxCanonicalUInt64(rawValue: 1),
      modelSelection: .alias(aliasID: try SignalboxCanonicalUUID(validating: turnID)),
      placementVersion: SignalboxCanonicalUInt64(rawValue: 1),
      placement: .scoped(path: "workspace.project"),
      runner: projection
    )

    XCTAssertEqual(frame.message, .sessionSummary(expected))
  }

  /// the session-summary runner member is required even when null.
  func testSessionSummaryMissingRunnerDegradesWithDiagnostic() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: Data(
        """
        {
          "version":1,
          "request_id":"8",
          "message":{
            "type":"session_summary",
            "session_id":"\(sessionID)",
            "defaults_version":"1",
            "model_selection":{"kind":"alias","alias_id":"\(turnID)"},
            "placement_version":"1",
            "placement":{"kind":"pathless"}
          }
        }
        """.utf8
      )
    )

    XCTAssertNotNil(ProcessProtocolFixture.decodingDiagnostic(in: frame.message))
  }

  func testSessionSummaryDecodesPathlessPlacement() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: Data(
        """
        {
          "version":1,
          "request_id":"8",
          "message":{
            "type":"session_summary",
            "session_id":"\(sessionID)",
            "defaults_version":"1",
            "model_selection":{"kind":"alias","alias_id":"\(turnID)"},
            "placement_version":"1",
            "placement":{"kind":"pathless"},
            "runner":null
          }
        }
        """.utf8
      )
    )
    let expected = SignalboxProcessSessionSummary(
      sessionID: try SignalboxCanonicalUUID(validating: sessionID),
      defaultsVersion: SignalboxCanonicalUInt64(rawValue: 1),
      modelSelection: .alias(aliasID: try SignalboxCanonicalUUID(validating: turnID)),
      placementVersion: SignalboxCanonicalUInt64(rawValue: 1),
      placement: .pathless,
      runner: nil
    )

    XCTAssertEqual(frame.message, .sessionSummary(expected))
  }

  func testSessionSummaryDecodesRootGlobalReadPlacement() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: Data(
        """
        {
          "version":1,
          "request_id":"8",
          "message":{
            "type":"session_summary",
            "session_id":"\(sessionID)",
            "defaults_version":"1",
            "model_selection":{"kind":"alias","alias_id":"\(turnID)"},
            "placement_version":"2",
            "placement":{
              "kind":"root_global_read",
              "path":"workspace",
              "intent":"acknowledged"
            },
            "runner":null
          }
        }
        """.utf8
      )
    )
    let expected = SignalboxProcessSessionSummary(
      sessionID: try SignalboxCanonicalUUID(validating: sessionID),
      defaultsVersion: SignalboxCanonicalUInt64(rawValue: 1),
      modelSelection: .alias(aliasID: try SignalboxCanonicalUUID(validating: turnID)),
      placementVersion: SignalboxCanonicalUInt64(rawValue: 2),
      placement: .rootGlobalRead(path: "workspace", intent: .acknowledged),
      runner: nil
    )

    XCTAssertEqual(frame.message, .sessionSummary(expected))
  }

  func testSessionSummaryRejectsZeroPlacementVersion() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: Data(
        """
        {
          "version":1,
          "request_id":"8",
          "message":{
            "type":"session_summary",
            "session_id":"\(sessionID)",
            "defaults_version":"1",
            "model_selection":{"kind":"alias","alias_id":"\(turnID)"},
            "placement_version":"0",
            "placement":{"kind":"pathless"},
            "runner":null
          }
        }
        """.utf8
      )
    )

    XCTAssertNotNil(ProcessProtocolFixture.decodingDiagnostic(in: frame.message))
  }

  func testSessionSummaryRejectsMalformedScopedPlacement() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: Data(
        """
        {
          "version":1,
          "request_id":"8",
          "message":{
            "type":"session_summary",
            "session_id":"\(sessionID)",
            "defaults_version":"1",
            "model_selection":{"kind":"alias","alias_id":"\(turnID)"},
            "placement_version":"1",
            "placement":{"kind":"scoped","path":"root"},
            "runner":null
          }
        }
        """.utf8
      )
    )

    XCTAssertNotNil(ProcessProtocolFixture.decodingDiagnostic(in: frame.message))
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

  /// runner transitions remain typed native session events.
  func testRunnerStateTransitionDecodesItsClosedPayload() throws {
    let runnerID = "44444444-4444-4444-8444-444444444444"
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
            "type":"runner_state_transition",
            "runner_id":"\(runnerID)",
            "placement_revision":"3",
            "sandbox_profile":"workspace-restricted",
            "working_directory":"workspace/project",
            "state":"working_directory_changed"
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
          event: .runnerStateTransition(
            runnerID: try SignalboxCanonicalUUID(validating: runnerID),
            placementRevision: SignalboxCanonicalUInt64(rawValue: 3),
            sandboxProfile: .workspaceRestricted,
            workingDirectory: try SignalboxRunnerWorkingDirectory(
              validating: "workspace/project"),
            state: .workingDirectoryChanged
          )
        )
      )
    )
  }

  func testSettingsChangeSessionEventDecodesAsKnown() throws {
    let settings = String(
      decoding: ProcessProtocolFixture.modelSettingsSnapshot(),
      as: UTF8.self
    )
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
            "type":"session_model_settings_changed",
            "command_id":"33333333-3333-4333-8333-333333333333",
            "prior_defaults_version":"1",
            "installed_defaults_version":"2",
            "prior_model":{"kind":"direct","selection_id":"\(turnID)"},
            "installed_model":{"kind":"alias","alias_id":"\(sessionID)"},
            "prior_settings":\(settings),
            "installed_settings":\(settings),
            "caller_override":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},
            "adjustments":[]
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
          event: .sessionModelSettingsChanged
        )
      )
    )
  }

  func testSettingsChangeSessionEventRejectsUnrelatedInstalledSessionLayer() throws {
    let priorSettings = String(
      decoding: ProcessProtocolFixture.modelSettingsSnapshot(
        sessionReasoning: #"{"kind":"value","value":"high"}"#,
        effectiveReasoning: "\"high\"",
        reasoningSource: "\"session\"",
        validatedForSelectionID: "\"(turnID)\""
      ),
      as: UTF8.self
    )
    let installedSettings = String(
      decoding: ProcessProtocolFixture.modelSettingsSnapshot(
        sessionReasoning: #"{"kind":"value","value":"low"}"#,
        effectiveReasoning: "\"low\"",
        reasoningSource: "\"session\"",
        validatedForSelectionID: "\"(turnID)\""
      ),
      as: UTF8.self
    )
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
            "type":"session_model_settings_changed",
            "command_id":"33333333-3333-4333-8333-333333333333",
            "prior_defaults_version":"1",
            "installed_defaults_version":"2",
            "prior_model":{"kind":"direct","selection_id":"\(turnID)"},
            "installed_model":{"kind":"direct","selection_id":"\(turnID)"},
            "prior_settings":\(priorSettings),
            "installed_settings":\(installedSettings),
            "caller_override":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},
            "adjustments":[]
          }
        }
      }
      """.utf8
    )

    let frame = try SignalboxProcessServerFrame.decode(from: encoded)

    XCTAssertNotNil(ProcessProtocolFixture.sessionEventDecodingDiagnostic(in: frame))
  }

  func testTurnSettingsSessionEventDecodesAsKnown() throws {
    let settings = String(
      decoding: ProcessProtocolFixture.modelSettingsSnapshot(),
      as: UTF8.self
    )
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
            "type":"turn_model_settings_resolved",
            "accepted_input_id":"33333333-3333-4333-8333-333333333333",
            "turn_id":"\(turnID)",
            "defaults_version":"1",
            "requested_model":{"kind":"direct","selection_id":"\(turnID)"},
            "selected_direct_id":"\(turnID)",
            "per_call_override":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},
            "settings":\(settings),
            "adjusted_from_selection_id":null,
            "adjustments":[]
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
          event: .turnModelSettingsResolved
        )
      )
    )
  }

  func testTurnSettingsSessionEventRejectsDirectSelectionMismatch() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.turnModelSettingsEventFrame(
        sessionID: sessionID,
        turnID: turnID,
        requestedModel: #"{"kind":"direct","selection_id":"\#(sessionID)"}"#,
        selectedDirectID: turnID
      )
    )

    XCTAssertNotNil(ProcessProtocolFixture.sessionEventDecodingDiagnostic(in: frame))
  }

  func testTurnSettingsSessionEventRejectsPerCallProvenanceMismatch() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.turnModelSettingsEventFrame(
        sessionID: sessionID,
        turnID: turnID,
        requestedModel: #"{"kind":"direct","selection_id":"\#(turnID)"}"#,
        selectedDirectID: turnID,
        perCallOverride:
          #"{"reasoning_level":{"kind":"provider_default"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}}"#
      )
    )

    XCTAssertNotNil(ProcessProtocolFixture.sessionEventDecodingDiagnostic(in: frame))
  }

  func testTurnSettingsSessionEventRejectsUnchangedAdjustmentSource() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.turnModelSettingsEventFrame(
        sessionID: sessionID,
        turnID: turnID,
        requestedModel: #"{"kind":"direct","selection_id":"\#(turnID)"}"#,
        selectedDirectID: turnID,
        adjustedFromSelectionID: "\"\(turnID)\"",
        adjustments: #"[{"type":"reasoning_level_cleared","from":"high"}]"#
      )
    )

    XCTAssertNotNil(ProcessProtocolFixture.sessionEventDecodingDiagnostic(in: frame))
  }

  func testTurnSettingsSessionEventAcceptsDistinctAdjustmentSource() throws {
    let settings = ProcessProtocolFixture.modelSettingsSnapshot(
      sessionReasoning: #"{"kind":"value","value":"low"}"#,
      effectiveReasoning: "\"low\"",
      reasoningSource: "\"session\"",
      validatedForSelectionID: "\"\(turnID)\""
    )
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.turnModelSettingsEventFrame(
        sessionID: sessionID,
        turnID: turnID,
        requestedModel: #"{"kind":"direct","selection_id":"\#(turnID)"}"#,
        selectedDirectID: turnID,
        settings: settings,
        adjustedFromSelectionID: "\"\(sessionID)\"",
        adjustments:
          #"[{"type":"reasoning_level_clamped","from":"high","to":"low"}]"#
      )
    )

    XCTAssertNil(ProcessProtocolFixture.sessionEventDecodingDiagnostic(in: frame))
  }

  func testSettingsChangeSessionEventRejectsUnknownMembers() throws {
    let settings = String(
      decoding: ProcessProtocolFixture.modelSettingsSnapshot(),
      as: UTF8.self
    )
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
            "type":"session_model_settings_changed",
            "command_id":"33333333-3333-4333-8333-333333333333",
            "prior_defaults_version":"1",
            "installed_defaults_version":"2",
            "prior_model":{"kind":"direct","selection_id":"\(turnID)"},
            "installed_model":{"kind":"alias","alias_id":"\(sessionID)"},
            "prior_settings":\(settings),
            "installed_settings":\(settings),
            "caller_override":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},
            "adjustments":[],
            "unexpected":true
          }
        }
      }
      """.utf8
    )

    let frame = try SignalboxProcessServerFrame.decode(from: encoded)

    XCTAssertNotNil(ProcessProtocolFixture.sessionEventDecodingDiagnostic(in: frame))
  }

  func testTurnSettingsSessionEventRejectsMalformedAdjustments() throws {
    let settings = String(
      decoding: ProcessProtocolFixture.modelSettingsSnapshot(),
      as: UTF8.self
    )
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
            "type":"turn_model_settings_resolved",
            "accepted_input_id":"33333333-3333-4333-8333-333333333333",
            "turn_id":"\(turnID)",
            "defaults_version":"1",
            "requested_model":{"kind":"direct","selection_id":"\(turnID)"},
            "selected_direct_id":"\(turnID)",
            "per_call_override":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},
            "settings":\(settings),
            "adjusted_from_selection_id":"\(sessionID)",
            "adjustments":[{"type":"reasoning_level_clamped","from":"high"}]
          }
        }
      }
      """.utf8
    )

    let frame = try SignalboxProcessServerFrame.decode(from: encoded)

    XCTAssertNotNil(ProcessProtocolFixture.sessionEventDecodingDiagnostic(in: frame))
  }

  func testToolApprovalDecisionDecodesTypedDelegateProvenance() throws {
    let requestID = "44444444-4444-4444-8444-444444444444"
    let selectionID = "55555555-5555-4555-8555-555555555555"
    let callID = "66666666-6666-4666-8666-666666666666"
    let rationale = "The requested effect exceeds the delegated scope."
    let encoded = Data(
      """
      {
        "version":1,
        "request_id":"9",
        "message":{
          "type":"session_event",
          "cursor":"13",
          "session_id":"\(sessionID)",
          "event":{
            "type":"tool_approval_decided",
            "turn_id":"\(turnID)",
            "tool_request_id":"\(requestID)",
            "decision":{"type":"deny","reason":null},
            "decider":{
              "type":"delegate",
              "model_selection_id":"\(selectionID)",
              "model_call_id":"\(callID)"
            },
            "rationale":"\(rationale)"
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
          cursor: SignalboxCanonicalUInt64(rawValue: 13),
          sessionID: try SignalboxCanonicalUUID(validating: sessionID),
          event: .toolApprovalDecided(
            turnID: try SignalboxCanonicalUUID(validating: turnID),
            toolRequestID: try SignalboxCanonicalUUID(validating: requestID),
            decision: .deny(reason: nil),
            decider: .delegate(
              modelSelectionID: try SignalboxCanonicalUUID(validating: selectionID),
              modelCallID: try SignalboxCanonicalUUID(validating: callID)
            ),
            rationale: rationale
          )
        )
      )
    )
  }

  func testDelegateRationaleRejectsNUL() throws {
    let encoded = Data(
      """
      {
        "version":1,
        "request_id":"9",
        "message":{
          "type":"session_event",
          "cursor":"13",
          "session_id":"\(sessionID)",
          "event":{
            "type":"tool_approval_decided",
            "turn_id":"\(turnID)",
            "tool_request_id":"44444444-4444-4444-8444-444444444444",
            "decision":{"type":"approve"},
            "decider":{
              "type":"delegate",
              "model_selection_id":"55555555-5555-4555-8555-555555555555",
              "model_call_id":"66666666-6666-4666-8666-666666666666"
            },
            "rationale":"unsafe\\u0000rationale"
          }
        }
      }
      """.utf8
    )

    let frame = try SignalboxProcessServerFrame.decode(from: encoded)

    XCTAssertNotNil(ProcessProtocolFixture.toolApprovalDecisionDiagnostic(in: frame.message))
  }

  func testUserDenialReasonAllowsNonPOSIXUnicodeWhitespaceAtEdges() throws {
    let nonbreakingSpace = "\u{00A0}"
    let reason = "\(nonbreakingSpace)kept verbatim\(nonbreakingSpace)"
    let encoded = Data(
      """
      {
        "version":1,
        "request_id":"9",
        "message":{
          "type":"session_event",
          "cursor":"13",
          "session_id":"\(sessionID)",
          "event":{
            "type":"tool_approval_decided",
            "turn_id":"\(turnID)",
            "tool_request_id":"44444444-4444-4444-8444-444444444444",
            "decision":{"type":"deny","reason":"\(reason)"},
            "decider":{
              "type":"user",
              "command_id":"55555555-5555-4555-8555-555555555555"
            },
            "rationale":null
          }
        }
      }
      """.utf8
    )

    let frame = try SignalboxProcessServerFrame.decode(from: encoded)

    XCTAssertEqual(ProcessProtocolFixture.userDenialReason(in: frame.message), reason)
  }

  func testUserDenialReasonAllowsUnicodeFormatScalar() throws {
    let formatScalar = "\u{200D}"
    let reason = "kept\(formatScalar)verbatim"
    let encoded = Data(
      """
      {
        "version":1,
        "request_id":"9",
        "message":{
          "type":"session_event",
          "cursor":"13",
          "session_id":"\(sessionID)",
          "event":{
            "type":"tool_approval_decided",
            "turn_id":"\(turnID)",
            "tool_request_id":"44444444-4444-4444-8444-444444444444",
            "decision":{"type":"deny","reason":"\(reason)"},
            "decider":{
              "type":"user",
              "command_id":"55555555-5555-4555-8555-555555555555"
            },
            "rationale":null
          }
        }
      }
      """.utf8
    )

    let frame = try SignalboxProcessServerFrame.decode(from: encoded)

    XCTAssertEqual(ProcessProtocolFixture.userDenialReason(in: frame.message), reason)
  }

  func testLegacyUserTextEntryFailsClosed() throws {
    let encoded = Data(
      """
      {
        "version":1,
        "request_id":"9",
        "message":{
          "type":"transcript_text_entry",
          "entry_index":"0",
          "source_session_id":"\(sessionID)",
          "entry_id":"33333333-3333-4333-8333-333333333333",
          "entry":{
            "type":"user",
            "accepted_input_id":"44444444-4444-4444-8444-444444444444",
            "turn_id":"\(turnID)"
          }
        }
      }
      """.utf8
    )

    let frame = try SignalboxProcessServerFrame.decode(from: encoded)

    XCTAssertNotNil(ProcessProtocolFixture.textEntryDecodingDiagnostic(in: frame.message))
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

  func testModelIdentityChangedEntryDecodesAsKnownEntry() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.modelIdentityChangedFrame(
        sessionID: sessionID,
        turnID: turnID
      )
    )
    let entry = try ProcessProtocolFixture.transcriptEntry(in: frame.message)

    XCTAssertEqual(entry.entryIndex.rawValue, ProcessProtocolFixture.firstEntryIndex)
    XCTAssertEqual(entry.sourceSessionID.rawValue, sessionID)
    XCTAssertEqual(
      entry.entry,
      .modelIdentityChanged(
        turnID: try SignalboxCanonicalUUID(validating: turnID),
        defaultsVersion: SignalboxCanonicalUInt64(
          rawValue: ProcessProtocolFixture.modelIdentityDefaultsVersion
        ),
        selectedModelID: try SignalboxCanonicalUUID(
          validating: ProcessProtocolFixture.selectedModelID
        )
      )
    )
  }

  func testModelIdentityChangedRejectsZeroDefaultsVersion() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.modelIdentityChangedFrame(
        sessionID: sessionID,
        turnID: turnID,
        defaultsVersion: 0
      )
    )
    let diagnostic = try ProcessProtocolFixture.transcriptEntryDiagnostic(in: frame.message)

    XCTAssertTrue(diagnostic.message.contains(ProcessProtocolFixture.defaultsVersionField))
  }

  func testDelegatedTaskEntryDecodesExactProvenanceAndContent() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.delegatedTaskEntryFrame(sessionID: sessionID)
    )
    let entry = try ProcessProtocolFixture.transcriptEntry(in: frame.message)

    XCTAssertEqual(
      entry.entry,
      .delegatedTask(
        spawningRequestID: try SignalboxCanonicalUUID(
          validating: ProcessProtocolFixture.spawningRequestID),
        parentSessionID: try SignalboxCanonicalUUID(
          validating: ProcessProtocolFixture.parentSessionID),
        parentTurnID: try SignalboxCanonicalUUID(
          validating: ProcessProtocolFixture.parentTurnID),
        content: ProcessProtocolFixture.delegatedTaskContent
      )
    )
  }

  func testDelegatedTaskEntryRejectsEmptyContent() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.delegatedTaskEntryFrame(sessionID: sessionID, content: "")
    )
    let diagnostic = try ProcessProtocolFixture.transcriptEntryDiagnostic(in: frame.message)

    XCTAssertFalse(diagnostic.message.isEmpty)
  }

  func testDelegationMessageEntryDecodesExactDelivery() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.delegationMessageEntryFrame(sessionID: sessionID)
    )
    let entry = try ProcessProtocolFixture.transcriptEntry(in: frame.message)

    XCTAssertEqual(
      entry.entry,
      .delegationMessage(
        spawningRequestID: try SignalboxCanonicalUUID(
          validating: ProcessProtocolFixture.spawningRequestID),
        messageID: try SignalboxCanonicalUUID(validating: ProcessProtocolFixture.messageID),
        senderSessionID: try SignalboxCanonicalUUID(
          validating: ProcessProtocolFixture.parentSessionID),
        recipientSessionID: try SignalboxCanonicalUUID(validating: sessionID),
        ordinal: SignalboxCanonicalUInt64(
          rawValue: ProcessProtocolFixture.delegationMessageOrdinal),
        deliverySequence: SignalboxCanonicalUInt64(
          rawValue: ProcessProtocolFixture.delegationMessageDeliverySequence),
        content: ProcessProtocolFixture.delegationMessageContent
      )
    )
  }

  func testDelegationMessageEntryRejectsNULContent() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.delegationMessageEntryFrame(
        sessionID: sessionID,
        content: #"invalid\u0000message"#
      )
    )
    let diagnostic = try ProcessProtocolFixture.transcriptEntryDiagnostic(in: frame.message)

    XCTAssertFalse(diagnostic.message.isEmpty)
  }

  func testForegroundDelegationResultEntryDecodesExactLifecycleProof() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.delegationResultEntryFrame(
        sessionID: sessionID,
        mode: "foreground",
        deliverySequence: "null"
      )
    )
    let entry = try ProcessProtocolFixture.transcriptEntry(in: frame.message)

    XCTAssertEqual(
      entry.entry,
      .delegationResult(
        awaitRequestID: try SignalboxCanonicalUUID(
          validating: ProcessProtocolFixture.awaitRequestID),
        spawningRequestID: try SignalboxCanonicalUUID(
          validating: ProcessProtocolFixture.spawningRequestID),
        childSessionID: try SignalboxCanonicalUUID(
          validating: ProcessProtocolFixture.childSessionID),
        mode: .foreground,
        deliverySequence: nil,
        outcome: .returned,
        content: ProcessProtocolFixture.delegationResultContent,
        reason: .childCompleted,
        provenance: .childTurn(
          childSessionID: try SignalboxCanonicalUUID(
            validating: ProcessProtocolFixture.childSessionID),
          childTurnID: try SignalboxCanonicalUUID(
            validating: ProcessProtocolFixture.childTurnID)
        )
      )
    )
  }

  func testBackgroundDelegationResultEntryDecodesWakeSequence() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.delegationResultEntryFrame(
        sessionID: sessionID,
        mode: "background",
        deliverySequence: "\"7\""
      )
    )
    let entry = try ProcessProtocolFixture.transcriptEntry(in: frame.message)

    XCTAssertEqual(
      entry.entry,
      .delegationResult(
        awaitRequestID: try SignalboxCanonicalUUID(
          validating: ProcessProtocolFixture.awaitRequestID),
        spawningRequestID: try SignalboxCanonicalUUID(
          validating: ProcessProtocolFixture.spawningRequestID),
        childSessionID: try SignalboxCanonicalUUID(
          validating: ProcessProtocolFixture.childSessionID),
        mode: .background,
        deliverySequence: SignalboxCanonicalUInt64(rawValue: 7),
        outcome: .returned,
        content: ProcessProtocolFixture.delegationResultContent,
        reason: .childCompleted,
        provenance: .childTurn(
          childSessionID: try SignalboxCanonicalUUID(
            validating: ProcessProtocolFixture.childSessionID),
          childTurnID: try SignalboxCanonicalUUID(
            validating: ProcessProtocolFixture.childTurnID)
        )
      )
    )
  }

  func testDelegationResultRejectsContradictoryOutcomeReasonTuple() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.delegationResultEntryFrame(
        sessionID: sessionID,
        mode: "foreground",
        deliverySequence: "null",
        reason: "child_cancelled"
      )
    )
    let diagnostic = try ProcessProtocolFixture.transcriptEntryDiagnostic(in: frame.message)

    XCTAssertFalse(diagnostic.message.isEmpty)
  }

  func testDelegationResultRejectsOversizedContent() throws {
    let oversized = String(
      repeating: "x",
      count: SignalboxProcessProtocol.maximumContentFragmentUTF8Bytes + 1
    )
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.delegationResultEntryFrame(
        sessionID: sessionID,
        mode: "foreground",
        deliverySequence: "null",
        content: oversized
      )
    )
    let diagnostic = try ProcessProtocolFixture.transcriptEntryDiagnostic(in: frame.message)

    XCTAssertFalse(diagnostic.message.isEmpty)
  }

  func testTranscriptToolApprovalRejectsExplicitNull() throws {
    let encoded = Data(
      """
      {
        "version":1,
        "request_id":"9",
        "message":{
          "type":"transcript_entry",
          "entry_index":"0",
          "source_session_id":"\(sessionID)",
          "entry_id":"33333333-3333-4333-8333-333333333333",
          "entry":{
            "type":"assistant_tool_use",
            "turn_id":"\(turnID)",
            "model_call_id":"44444444-4444-4444-8444-444444444444",
            "tool_request_id":"55555555-5555-4555-8555-555555555555",
            "tool_name":"publish",
            "arguments":"{}",
            "approval":null
          }
        }
      }
      """.utf8
    )
    let frame = try SignalboxProcessServerFrame.decode(from: encoded)

    XCTAssertNotNil(try ProcessProtocolFixture.transcriptEntryDiagnostic(in: frame.message))
  }

  func testFutureProviderFailureCauseRemainsClassifiable() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.failedTurnFrame(
        turnID: turnID,
        cause: ProcessProtocolFixture.futureProviderFailureCause
      )
    )

    XCTAssertEqual(
      try ProcessProtocolFixture.failedModelCallCause(in: frame.message),
      .unknown(ProcessProtocolFixture.futureProviderFailureCause)
    )
  }

  func testQueuedDelegatedTurnDecodesExactOriginProvenance() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.queuedDelegatedTurnFrame(turnID: turnID)
    )
    let origin = try ProcessProtocolFixture.queuedDelegatedOrigin(in: frame.message)

    XCTAssertEqual(origin.spawningRequestID.rawValue, ProcessProtocolFixture.spawningRequestID)
    XCTAssertEqual(origin.parentSessionID.rawValue, ProcessProtocolFixture.parentSessionID)
    XCTAssertEqual(origin.parentTurnID.rawValue, ProcessProtocolFixture.parentTurnID)
    XCTAssertEqual(origin.content, ProcessProtocolFixture.delegatedTaskContent)
  }

  func testQueuedDelegationWakeDecodesExactDeliveryRange() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.queuedDelegationWakeTurnFrame(turnID: turnID)
    )
    let range = try ProcessProtocolFixture.queuedDelegationWakeRange(in: frame.message)

    XCTAssertEqual(range.first.rawValue, ProcessProtocolFixture.wakeFirstDeliverySequence)
    XCTAssertEqual(range.through.rawValue, ProcessProtocolFixture.wakeThroughDeliverySequence)
  }

  func testDelegationTerminalTurnDecodesParentGoalAuthority() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.delegationTerminalTurnFrame(turnID: turnID)
    )
    let terminal = try ProcessProtocolFixture.delegationTerminal(in: frame.message)

    XCTAssertEqual(terminal.spawningRequestID.rawValue, ProcessProtocolFixture.spawningRequestID)
    XCTAssertEqual(terminal.outcome, .stopped)
    XCTAssertEqual(terminal.reason, .parentStopped)
    XCTAssertEqual(
      terminal.provenance,
      .parentGoalCommand(
        parentSessionID: try SignalboxCanonicalUUID(
          validating: ProcessProtocolFixture.parentSessionID),
        goalGeneration: SignalboxCanonicalUInt64(rawValue: 1),
        commandID: try SignalboxCanonicalUUID(validating: ProcessProtocolFixture.parentTurnID),
        descendantScope: .parentAndDescendants
      )
    )
  }

  func testDelegationTerminalTurnAdmitsCrossedParentPolicy() throws {
    // A bound relationship maps the parent verb through its own policy, so a
    // parent cancellation may stop the child and a parent stop may cancel it.
    let stoppedByCancellation = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.delegationTerminalTurnFrame(
        turnID: turnID, outcome: "stopped", reason: "parent_cancelled")
    )
    let cancelledByStop = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.delegationTerminalTurnFrame(
        turnID: turnID, outcome: "cancelled", reason: "parent_stopped")
    )

    let stoppedTerminal = try ProcessProtocolFixture.delegationTerminal(
      in: stoppedByCancellation.message)
    XCTAssertEqual(stoppedTerminal.outcome, .stopped)
    XCTAssertEqual(stoppedTerminal.reason, .parentCancelled)
    let cancelledTerminal = try ProcessProtocolFixture.delegationTerminal(
      in: cancelledByStop.message)
    XCTAssertEqual(cancelledTerminal.outcome, .cancelled)
    XCTAssertEqual(cancelledTerminal.reason, .parentStopped)
  }

  func testDelegationTerminalTurnRejectsANonTerminalOutcome() throws {
    // `continue_running` reports an edge the cascade did not terminalize, so it
    // is never a terminal turn state.
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.delegationTerminalTurnFrame(
        turnID: turnID, outcome: "continue_running", reason: "parent_stopped")
    )

    XCTAssertNotNil(ProcessProtocolFixture.turnStateDecodingDiagnostic(in: frame.message))
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

  func testFailedTerminalModelCallCauseDecodesAsAClosedClassification() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.failedTurnFrame(
        turnID: turnID, cause: "quota_exhausted"
      )
    )

    XCTAssertEqual(
      try ProcessProtocolFixture.failedModelCallCause(in: frame.message),
      .quotaExhausted
    )
  }

  func testAttachmentTooLargeCauseDecodesAsAClosedClassification() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.failedTurnFrame(
        turnID: turnID, cause: "attachment_too_large"
      )
    )

    XCTAssertEqual(
      try ProcessProtocolFixture.failedModelCallCause(in: frame.message),
      .attachmentTooLarge
    )
  }

  func testAttachmentMissingCauseDecodesAsAClosedClassification() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.failedTurnFrame(
        turnID: turnID, cause: "attachment_missing"
      )
    )

    XCTAssertEqual(
      try ProcessProtocolFixture.failedModelCallCause(in: frame.message),
      .attachmentMissing
    )
  }

  func testAttachmentCorruptCauseDecodesAsAClosedClassification() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.failedTurnFrame(
        turnID: turnID, cause: "attachment_corrupt"
      )
    )

    XCTAssertEqual(
      try ProcessProtocolFixture.failedModelCallCause(in: frame.message),
      .attachmentCorrupt
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
          message: "Missing required field at message.model_settings."
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

  /// a daemon-only transcript snapshot carries its nullable
  /// runner member without becoming an unknown message.
  func testTranscriptSnapshotStartDecodesAbsentRunnerProjection() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: Data(
        """
        {
          "version":1,
          "request_id":"9",
          "message":{
            "type":"transcript_snapshot_start",
            "session_id":"\(sessionID)",
            "cursor":"12",
            "runner":null
          }
        }
        """.utf8
      )
    )
    let expected = SignalboxTranscriptSnapshotBoundary(
      sessionID: try SignalboxCanonicalUUID(validating: sessionID),
      cursor: SignalboxCanonicalUInt64(rawValue: 12),
      runner: nil
    )

    XCTAssertEqual(frame.message, .transcriptSnapshotStart(expected))
  }

  /// the native boundary retains every axis of one complete
  /// runner projection rather than silently discarding the new wire member.
  func testTranscriptSnapshotStartDecodesCompleteRunnerProjection() throws {
    let runnerID = "44444444-4444-4444-8444-444444444444"
    let frame = try SignalboxProcessServerFrame.decode(
      from: Data(
        """
        {
          "version":1,
          "request_id":"9",
          "message":{
            "type":"transcript_snapshot_start",
            "session_id":"\(sessionID)",
            "cursor":"12",
            "runner":{
              "selector":{"type":"capability_class","name":"linux.workspace"},
              "runner_id":"\(runnerID)",
              "placement_revision":"3",
              "sandbox_profile":"workspace-restricted",
              "credential_profile":"readonly",
              "repository":"primary",
              "working_directory":"workspace/project",
              "connection_health":null,
              "state":"runner_lost"
            }
          }
        }
        """.utf8
      )
    )
    let expectedProjection = try SignalboxRunnerProjection(
      selector: .capabilityClass(
        name: SignalboxRunnerCapabilityClass(validating: "linux.workspace")
      ),
      runnerID: SignalboxCanonicalUUID(validating: runnerID),
      placementRevision: SignalboxCanonicalUInt64(rawValue: 3),
      sandboxProfile: .workspaceRestricted,
      credentialProfile: SignalboxRunnerCredentialProfileName(validating: "readonly"),
      repository: SignalboxRunnerRepositoryKey(validating: "primary"),
      workingDirectory: SignalboxRunnerWorkingDirectory(validating: "workspace/project"),
      connectionHealth: nil,
      state: .runnerLost
    )
    let expected = SignalboxTranscriptSnapshotBoundary(
      sessionID: try SignalboxCanonicalUUID(validating: sessionID),
      cursor: SignalboxCanonicalUInt64(rawValue: 12),
      runner: expectedProjection
    )

    XCTAssertEqual(frame.message, .transcriptSnapshotStart(expected))
  }

  /// the required nullable runner member cannot be omitted from a
  /// known transcript snapshot boundary.
  func testTranscriptSnapshotStartMissingRunnerDegradesWithDiagnostic() throws {
    let encoded = Data(
      """
      {
        "version":1,
        "request_id":"9",
        "message":{
          "type":"transcript_snapshot_start",
          "session_id":"\(sessionID)",
          "cursor":"12"
        }
      }
      """.utf8
    )

    let frame = try SignalboxProcessServerFrame.decode(from: encoded)

    XCTAssertNotNil(ProcessProtocolFixture.decodingDiagnostic(in: frame.message))
  }

  /// capability names retain the portable runner-name grammar at the
  /// native protocol boundary.
  func testTranscriptSnapshotStartRejectsInvalidRunnerCapabilityName() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.runnerSnapshotStartFrame(
        sessionID: sessionID,
        capabilityNameJSON: #""linux/workspace""#,
        credentialProfileJSON: "null",
        repositoryJSON: "null",
        workingDirectoryJSON: "null"
      )
    )

    XCTAssertNotNil(ProcessProtocolFixture.decodingDiagnostic(in: frame.message))
  }

  /// credential profiles retain the portable runner-name grammar at
  /// the native protocol boundary.
  func testTranscriptSnapshotStartRejectsInvalidRunnerCredentialProfile() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.runnerSnapshotStartFrame(
        sessionID: sessionID,
        capabilityNameJSON: #""linux.workspace""#,
        credentialProfileJSON: #""read/only""#,
        repositoryJSON: "null",
        workingDirectoryJSON: "null"
      )
    )

    XCTAssertNotNil(ProcessProtocolFixture.decodingDiagnostic(in: frame.message))
  }

  /// repository keys retain the portable runner-name grammar at the
  /// native protocol boundary.
  func testTranscriptSnapshotStartRejectsInvalidRunnerRepositoryKey() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.runnerSnapshotStartFrame(
        sessionID: sessionID,
        capabilityNameJSON: #""linux.workspace""#,
        credentialProfileJSON: "null",
        repositoryJSON: "\"\"",
        workingDirectoryJSON: "null"
      )
    )

    XCTAssertNotNil(ProcessProtocolFixture.decodingDiagnostic(in: frame.message))
  }

  /// runner working-directory text retains its exact byte bound at
  /// the native protocol boundary.
  func testTranscriptSnapshotStartRejectsOversizedRunnerWorkingDirectory() throws {
    let oversizedDirectory = String(repeating: "x", count: 4_097)
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.runnerSnapshotStartFrame(
        sessionID: sessionID,
        capabilityNameJSON: #""linux.workspace""#,
        credentialProfileJSON: "null",
        repositoryJSON: "null",
        workingDirectoryJSON: "\"\(oversizedDirectory)\""
      )
    )

    XCTAssertNotNil(ProcessProtocolFixture.decodingDiagnostic(in: frame.message))
  }

  /// runner working-directory text rejects NUL at the native protocol
  /// boundary.
  func testTranscriptSnapshotStartRejectsNULBearingRunnerWorkingDirectory() throws {
    let frame = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.runnerSnapshotStartFrame(
        sessionID: sessionID,
        capabilityNameJSON: #""linux.workspace""#,
        credentialProfileJSON: "null",
        repositoryJSON: "null",
        workingDirectoryJSON: #""workspace\u0000project""#
      )
    )

    XCTAssertNotNil(ProcessProtocolFixture.decodingDiagnostic(in: frame.message))
  }

  func testUnadmittedFrameMemberFailsClosed() {
    let encoded = ProcessProtocolFixture.frameWithAddedMember()

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

  func testAttachmentRejectionsDecodeTypedDetails() throws {
    let missing = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.attachmentBlobNotFoundFrame(digest: blobDigest)
    )
    let oversized = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.attachmentByteBudgetExceededFrame(maximumBytes: "4096")
    )

    XCTAssertEqual(
      try ProcessProtocolFixture.rejectionDetail(in: missing.message),
      .attachmentBlobNotFound(
        digest: try SignalboxCanonicalBlobDigest(validating: blobDigest)
      )
    )
    XCTAssertEqual(
      try ProcessProtocolFixture.rejectionDetail(in: oversized.message),
      .attachmentByteBudgetExceeded(
        maximumBytes: SignalboxCanonicalUInt64(rawValue: 4096)
      )
    )
  }

  func testAttachmentRejectionsRejectMalformedKnownDetails() throws {
    let invalidDigest = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.attachmentBlobNotFoundFrame(digest: "sha256:AA")
    )
    let zeroMaximum = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.attachmentByteBudgetExceededFrame(maximumBytes: "0")
    )
    let noncanonicalMaximum = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.attachmentByteBudgetExceededFrame(maximumBytes: "04096")
    )
    let expanded = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.expandedAttachmentBlobNotFoundFrame(digest: blobDigest)
    )

    XCTAssertNotNil(ProcessProtocolFixture.decodingDiagnostic(in: invalidDigest.message))
    XCTAssertNotNil(ProcessProtocolFixture.decodingDiagnostic(in: zeroMaximum.message))
    XCTAssertNotNil(ProcessProtocolFixture.decodingDiagnostic(in: noncanonicalMaximum.message))
    XCTAssertNotNil(ProcessProtocolFixture.decodingDiagnostic(in: expanded.message))
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

  func testModelSettingRejectionsDecodeTypedDetails() throws {
    let reasoning = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.unsupportedReasoningLevelFrame(
        selectionID: sessionID,
        requested: "xhigh"
      )
    )
    let fastMode = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.unsupportedFastModeFrame(selectionID: sessionID)
    )
    let serviceTier = try SignalboxProcessServerFrame.decode(
      from: ProcessProtocolFixture.unsupportedServiceTierFrame(selectionID: sessionID)
    )
    let selection = try SignalboxCanonicalUUID(validating: sessionID)

    XCTAssertEqual(
      try ProcessProtocolFixture.rejectionDetail(in: reasoning.message),
      .unsupportedReasoningLevel(selectionID: selection, requested: "xhigh")
    )
    XCTAssertEqual(
      try ProcessProtocolFixture.rejectionDetail(in: fastMode.message),
      .unsupportedFastMode(selectionID: selection)
    )
    XCTAssertEqual(
      try ProcessProtocolFixture.rejectionDetail(in: serviceTier.message),
      .unsupportedServiceTier(
        selectionID: selection,
        provider: "open_ai",
        requested: "priority"
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

  func testUnderivableImportedTitleUsesAVisibleListFallback() throws {
    let summary = try SignalboxJSONCoding.decoder().decode(
      SignalboxConversationSummary.self,
      from: ProcessProtocolFixture.importedConversationWithNullTitle()
    )

    let conversation = SignalboxProcessConversation(summary: summary)

    XCTAssertEqual(
      conversation.displayTitle,
      ProcessProtocolFixture.untitledImportedConversationLabel
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

  /// Both recovery turn states carry the daemon's complete four-member
  /// serialization, so a transcript parked on either wait decodes rather than
  /// failing session synchronization on an unadmitted field.
  func testModelCallRecoveryTurnDecodesItsAutomaticReconciliationStatus() throws {
    let attemptID = "55555555-5555-4555-8555-555555555555"
    let modelCallID = "66666666-6666-4666-8666-666666666666"
    let frame = try SignalboxProcessServerFrame.decode(
      from: Data(
        """
        {
          "version":1,
          "request_id":"9",
          "message":{
            "type":"transcript_turn",
            "turn_id":"\(turnID)",
            "acceptance_position":"1",
            "state":{
              "type":"active_awaiting_model_call_recovery",
              "ended_attempt_id":"\(attemptID)",
              "recovery_model_call_id":"\(modelCallID)",
              "automatic_reconciliation_attempts":"2",
              "operator_action_required":false
            }
          }
        }
        """.utf8
      )
    )
    let expected = SignalboxTranscriptTurnState.activeAwaitingModelCallRecovery(
      endedAttemptID: try SignalboxCanonicalUUID(validating: attemptID),
      recoveryModelCallID: try SignalboxCanonicalUUID(validating: modelCallID),
      automaticReconciliationAttempts: SignalboxCanonicalUInt64(rawValue: 2),
      operatorActionRequired: false
    )

    XCTAssertEqual(
      ProcessProtocolFixture.transcriptTurnState(in: frame.message),
      expected
    )
  }

  func testToolRecoveryTurnDecodesItsAutomaticReconciliationStatus() throws {
    let attemptID = "55555555-5555-4555-8555-555555555555"
    let toolAttemptID = "77777777-7777-4777-8777-777777777777"
    let frame = try SignalboxProcessServerFrame.decode(
      from: Data(
        """
        {
          "version":1,
          "request_id":"9",
          "message":{
            "type":"transcript_turn",
            "turn_id":"\(turnID)",
            "acceptance_position":"1",
            "state":{
              "type":"active_awaiting_tool_recovery",
              "ended_attempt_id":"\(attemptID)",
              "recovery_tool_attempt_id":"\(toolAttemptID)",
              "automatic_reconciliation_attempts":"5",
              "operator_action_required":true
            }
          }
        }
        """.utf8
      )
    )
    let expected = SignalboxTranscriptTurnState.activeAwaitingToolRecovery(
      endedAttemptID: try SignalboxCanonicalUUID(validating: attemptID),
      recoveryToolAttemptID: try SignalboxCanonicalUUID(validating: toolAttemptID),
      automaticReconciliationAttempts: SignalboxCanonicalUInt64(rawValue: 5),
      operatorActionRequired: true
    )

    XCTAssertEqual(
      ProcessProtocolFixture.transcriptTurnState(in: frame.message),
      expected
    )
  }
}

private enum ProcessProtocolFixture {
  static let untitledImportedConversationLabel =
    "Untitled imported conversation 33333333"
  static let requestID: UInt64 = 9
  static let firstEntryIndex: UInt64 = 0
  static let newerVersion: UInt64 = 2
  static let modelIdentityDefaultsVersion: UInt64 = 7
  static let defaultsVersionField = "defaults_version"
  static let selectedModelID = "88888888-8888-4888-8888-888888888888"
  static let spawningRequestID = "33333333-3333-4333-8333-333333333333"
  static let parentSessionID = "44444444-4444-4444-8444-444444444444"
  static let parentTurnID = "12121212-1212-4212-8212-121212121212"
  static let delegatedTaskContent = "fixture delegated task"
  static let wakeFirstDeliverySequence: UInt64 = 3
  static let wakeThroughDeliverySequence: UInt64 = 5
  static let delegationMessageContent = "fixture delegation message"
  static let delegationMessageOrdinal: UInt64 = 1
  static let delegationMessageDeliverySequence: UInt64 = 2
  static let delegationResultContent = "fixture delegation result"
  static let messageID = "55555555-5555-4555-8555-555555555555"
  static let awaitRequestID = "66666666-6666-4666-8666-666666666666"
  static let childSessionID = "77777777-7777-4777-8777-777777777777"
  static let childTurnID = "99999999-9999-4999-8999-999999999999"
  static let futureProviderFailureCause = "future_provider_failure"
  static let expandedErrorMessage = "fixture error"
  static let expandedRejectionMessage = "fixture rejection"
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
  static let oversizedUnknownPresentationToken = String(
    repeating: "x",
    count: SignalboxProcessPresentation.maximumLabelUTF8Bytes + 1
  )

  static func frameWithAddedMember() -> Data {
    Data(
      """
      {
        "version":1,
        "request_id":"\(requestID)",
        "unexpected":true,
        "message":{"type":"sessions_start"}
      }
      """.utf8
    )
  }

  static func newerVersionFrame() -> Data {
    Data(
      """
      {
        "version":\(newerVersion),
        "request_id":"\(requestID)",
        "message":{"type":"sessions_start"}
      }
      """.utf8
    )
  }

  static func modelIdentityChangedFrame(
    sessionID: String,
    turnID: String,
    defaultsVersion: UInt64 = modelIdentityDefaultsVersion
  ) -> Data {
    Data(
      """
      {
        "version":1,
        "request_id":"\(requestID)",
        "message":{
          "type":"transcript_entry",
          "entry_index":"\(firstEntryIndex)",
          "source_session_id":"\(sessionID)",
          "entry_id":"33333333-3333-4333-8333-333333333333",
          "entry":{
            "type":"model_identity_changed",
            "turn_id":"\(turnID)",
            "defaults_version":"\(defaultsVersion)",
            "selected_model_id":"\(selectedModelID)"
          }
        }
      }
      """.utf8
    )
  }

  static func delegatedTaskEntryFrame(
    sessionID: String,
    content: String = delegatedTaskContent
  ) -> Data {
    Data(
      """
      {
        "version":1,
        "request_id":"\(requestID)",
        "message":{
          "type":"transcript_entry",
          "entry_index":"\(firstEntryIndex)",
          "source_session_id":"\(sessionID)",
          "entry_id":"22222222-2222-4222-8222-222222222222",
          "entry":{
            "type":"delegated_task",
            "spawning_request_id":"\(spawningRequestID)",
            "parent_session_id":"\(parentSessionID)",
            "parent_turn_id":"\(parentTurnID)",
            "content":"\(content)"
          }
        }
      }
      """.utf8
    )
  }

  static func delegationMessageEntryFrame(
    sessionID: String,
    content: String = delegationMessageContent
  ) -> Data {
    Data(
      """
      {
        "version":1,
        "request_id":"\(requestID)",
        "message":{
          "type":"transcript_entry",
          "entry_index":"\(firstEntryIndex)",
          "source_session_id":"\(sessionID)",
          "entry_id":"22222222-2222-4222-8222-222222222222",
          "entry":{
            "type":"delegation_message",
            "spawning_request_id":"\(spawningRequestID)",
            "message_id":"\(messageID)",
            "sender_session_id":"\(parentSessionID)",
            "recipient_session_id":"\(sessionID)",
            "ordinal":"\(delegationMessageOrdinal)",
            "delivery_sequence":"\(delegationMessageDeliverySequence)",
            "content":"\(content)"
          }
        }
      }
      """.utf8
    )
  }

  static func delegationResultEntryFrame(
    sessionID: String,
    mode: String,
    deliverySequence: String,
    reason: String = "child_completed",
    content: String = delegationResultContent
  ) -> Data {
    Data(
      """
      {
        "version":1,
        "request_id":"\(requestID)",
        "message":{
          "type":"transcript_entry",
          "entry_index":"\(firstEntryIndex)",
          "source_session_id":"\(sessionID)",
          "entry_id":"22222222-2222-4222-8222-222222222222",
          "entry":{
            "type":"delegation_result",
            "await_request_id":"\(awaitRequestID)",
            "spawning_request_id":"\(spawningRequestID)",
            "child_session_id":"\(childSessionID)",
            "mode":"\(mode)",
            "delivery_sequence":\(deliverySequence),
            "outcome":"returned",
            "content":"\(content)",
            "reason":"\(reason)",
            "provenance":{
              "type":"child_turn",
              "child_session_id":"\(childSessionID)",
              "child_turn_id":"\(childTurnID)"
            }
          }
        }
      }
      """.utf8
    )
  }

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

  static func runnerSnapshotStartFrame(
    sessionID: String,
    capabilityNameJSON: String,
    credentialProfileJSON: String,
    repositoryJSON: String,
    workingDirectoryJSON: String
  ) -> Data {
    Data(
      """
      {
        "version":1,
        "request_id":"9",
        "message":{
          "type":"transcript_snapshot_start",
          "session_id":"\(sessionID)",
          "cursor":"12",
          "runner":{
            "selector":{"type":"capability_class","name":\(capabilityNameJSON)},
            "runner_id":"44444444-4444-4444-8444-444444444444",
            "placement_revision":"3",
            "sandbox_profile":"workspace-restricted",
            "credential_profile":\(credentialProfileJSON),
            "repository":\(repositoryJSON),
            "working_directory":\(workingDirectoryJSON),
            "connection_health":null,
            "state":"runner_lost"
          }
        }
      }
      """.utf8
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
        "request_id":"\(requestID)",
        "message":{
          "type":"error",
          "code":"not_found",
          "message":"\(expandedErrorMessage)",
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
        "request_id":"\(requestID)",
        "message":{
          "type":"error",
          "code":"rejected",
          "message":"\(expandedRejectionMessage)",
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

  static func attachmentBlobNotFoundFrame(digest: String) -> Data {
    rejectedFrame(
      detail:
        """
        {
          "type":"attachment_blob_not_found",
          "digest":"\(digest)"
        }
        """
    )
  }

  static func attachmentByteBudgetExceededFrame(maximumBytes: String) -> Data {
    rejectedFrame(
      detail:
        """
        {
          "type":"attachment_byte_budget_exceeded",
          "maximum_bytes":"\(maximumBytes)"
        }
        """
    )
  }

  static func expandedAttachmentBlobNotFoundFrame(digest: String) -> Data {
    rejectedFrame(
      detail:
        """
        {
          "type":"attachment_blob_not_found",
          "digest":"\(digest)",
          "future_field":true
        }
        """
    )
  }

  static func unsupportedReasoningLevelFrame(
    selectionID: String,
    requested: String
  ) -> Data {
    rejectedFrame(
      detail:
        """
        {
          "type":"unsupported_reasoning_level",
          "selection_id":"\(selectionID)",
          "requested":"\(requested)"
        }
        """
    )
  }

  static func unsupportedFastModeFrame(selectionID: String) -> Data {
    rejectedFrame(
      detail:
        """
        {
          "type":"unsupported_fast_mode",
          "selection_id":"\(selectionID)"
        }
        """
    )
  }

  static func unsupportedServiceTierFrame(selectionID: String) -> Data {
    rejectedFrame(
      detail:
        """
        {
          "type":"unsupported_service_tier",
          "selection_id":"\(selectionID)",
          "requested":{"provider":"open_ai","value":"priority"}
        }
        """
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

  static func importedConversationWithNullTitle() -> Data {
    Data(
      """
      {
        "origin":"imported_conversation",
        "imported_conversation_id":"33333333-3333-4333-8333-333333333333",
        "title":null,
        "entry_count":"1",
        "source_format":"codex_rollout_jsonl_v1"
      }
      """.utf8
    )
  }

  static func importedEntryWithUnknownSourceSpeakerFrame(
    sourceSpeakerKind: String = unknownImportedSourceSpeakerKind,
    contentKind: String = "source_event"
  ) -> Data {
    Data(
      """
      {
        "version":1,
        "request_id":"10",
        "message":{
          "type":"imported_conversation_entry",
          "position":"1",
          "imported_entry_id":"33333333-3333-4333-8333-333333333333",
          "source_speaker":{"type":"\(sourceSpeakerKind)"},
          "content_kind":"\(contentKind)",
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

  static func processError(
    in message: SignalboxProcessServerMessage
  ) throws -> SignalboxProcessError {
    guard case .protocolError(let error) = message else {
      throw ProcessProtocolFixtureError.missingProcessError
    }
    return error
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

  static func queuedDelegatedTurnFrame(turnID: String) -> Data {
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
            "type":"queued_delegated",
            "spawning_request_id":"\(spawningRequestID)",
            "parent_session_id":"\(parentSessionID)",
            "parent_turn_id":"\(parentTurnID)",
            "content":"\(delegatedTaskContent)"
          }
        }
      }
      """.utf8
    )
  }

  static func queuedDelegationWakeTurnFrame(turnID: String) -> Data {
    Data(
      """
      {
        "version":1,
        "request_id":"9",
        "message":{
          "type":"transcript_turn",
          "turn_id":"\(turnID)",
          "acceptance_position":"2",
          "state":{
            "type":"queued_delegation_wake",
            "first_delivery_sequence":"\(wakeFirstDeliverySequence)",
            "through_delivery_sequence":"\(wakeThroughDeliverySequence)"
          }
        }
      }
      """.utf8
    )
  }

  static func delegationTerminalTurnFrame(
    turnID: String, outcome: String = "stopped", reason: String = "parent_stopped"
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
            "type":"delegation_terminated",
            "spawning_request_id":"\(spawningRequestID)",
            "outcome":"\(outcome)",
            "reason":"\(reason)",
            "provenance":{
              "type":"parent_goal_command",
              "parent_session_id":"\(parentSessionID)",
              "goal_generation":"1",
              "command_id":"\(parentTurnID)",
              "descendant_scope":"parent_and_descendants"
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

  /// One `session_metadata` frame whose last writer carries the supplied actor
  /// object. The metadata is the initial shape: only the actor varies.
  static func metadataReadFrame(sessionID: String, actorJSON: String) -> Data {
    Data(
      """
      {"version":1,"request_id":"1","message":{"type":"session_metadata",\
      "session_id":"\(sessionID)","metadata":{"title":null,"tags":[],\
      "attributes":{},"archived":false},"last_writer":\
      {"updated_at_unix_micros":"1","actor":\(actorJSON)}}}
      """.utf8
    )
  }

  static func metadataActor(
    in message: SignalboxProcessServerMessage
  ) throws -> SignalboxMetadataActor {
    guard case .sessionMetadata(let read) = message, let writer = read.lastWriter else {
      throw ProcessProtocolFixtureError.missingMetadataWriter
    }
    return writer.actor
  }

  static func transcriptEntry(
    in message: SignalboxProcessServerMessage
  ) throws -> SignalboxTranscriptEntryMessage {
    guard case .transcriptEntry(let entry) = message else {
      throw ProcessProtocolFixtureError.missingTranscriptEntry
    }
    return entry
  }

  static func transcriptEntryDiagnostic(
    in message: SignalboxProcessServerMessage
  ) throws -> SignalboxDecodingDiagnostic {
    let message = try transcriptEntry(in: message)
    guard case .unknown(_, _, let diagnostic) = message.entry, let diagnostic else {
      throw ProcessProtocolFixtureError.missingUnknownDiagnostic
    }
    return diagnostic
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

  static func failedModelCallCause(
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

  static func queuedDelegatedOrigin(
    in message: SignalboxProcessServerMessage
  ) throws -> (
    spawningRequestID: SignalboxCanonicalUUID,
    parentSessionID: SignalboxCanonicalUUID,
    parentTurnID: SignalboxCanonicalUUID,
    content: String
  ) {
    guard
      case .transcriptTurn(let turn) = message,
      case .queuedDelegated(
        let spawningRequestID,
        let parentSessionID,
        let parentTurnID,
        let content
      ) = turn.state
    else {
      throw ProcessProtocolFixtureError.missingDelegatedOrigin
    }
    return (spawningRequestID, parentSessionID, parentTurnID, content)
  }

  static func queuedDelegationWakeRange(
    in message: SignalboxProcessServerMessage
  ) throws -> (first: SignalboxCanonicalUInt64, through: SignalboxCanonicalUInt64) {
    guard
      case .transcriptTurn(let turn) = message,
      case .queuedDelegationWake(let first, let through) = turn.state
    else {
      throw ProcessProtocolFixtureError.missingDelegatedOrigin
    }
    return (first, through)
  }

  static func delegationTerminal(
    in message: SignalboxProcessServerMessage
  ) throws -> (
    spawningRequestID: SignalboxCanonicalUUID,
    outcome: SignalboxDelegationOutcome,
    reason: SignalboxDelegationReason,
    provenance: SignalboxDelegationProvenance
  ) {
    guard
      case .transcriptTurn(let turn) = message,
      case .delegationTerminated(
        let spawningRequestID, let outcome, let reason, let provenance) = turn.state
    else {
      throw ProcessProtocolFixtureError.missingDelegatedOrigin
    }
    return (spawningRequestID, outcome, reason, provenance)
  }

  static func oversizedFrame() -> Data {
    Data(
      repeating: 0x20,
      count: SignalboxProcessProtocol.maximumFrameBytes + 1
    )
  }

  static func modelSettingsSnapshot(
    perCallReasoning: String = #"{"kind":"inherit"}"#,
    sessionReasoning: String = #"{"kind":"inherit"}"#,
    effectiveReasoning: String = "null",
    reasoningSource: String = "null",
    validatedForSelectionID: String = "null"
  ) -> Data {
    Data(
      """
      {
        "precedence":{
          "per_call":{"reasoning_level":\(perCallReasoning),"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},
          "session":{"reasoning_level":\(sessionReasoning),"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},
          "profile":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},
          "global_default":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}}
        },
        "effective":{"reasoning_level":\(effectiveReasoning),"fast_mode":"disabled","service_tier":null},
        "reasoning_source":\(reasoningSource),
        "fast_mode_source":null,
        "service_tier_source":null,
        "validated_for_selection_id":\(validatedForSelectionID)
      }
      """.utf8
    )
  }

  static func turnModelSettingsEventFrame(
    sessionID: String,
    turnID: String,
    requestedModel: String,
    selectedDirectID: String,
    settings: Data? = nil,
    perCallOverride: String =
      #"{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}}"#,
    adjustedFromSelectionID: String = "null",
    adjustments: String = "[]"
  ) -> Data {
    let settings = String(decoding: settings ?? modelSettingsSnapshot(), as: UTF8.self)
    return Data(
      """
      {
        "version":1,
        "request_id":"9",
        "message":{
          "type":"session_event",
          "cursor":"12",
          "session_id":"\(sessionID)",
          "event":{
            "type":"turn_model_settings_resolved",
            "accepted_input_id":"33333333-3333-4333-8333-333333333333",
            "turn_id":"\(turnID)",
            "defaults_version":"1",
            "requested_model":\(requestedModel),
            "selected_direct_id":"\(selectedDirectID)",
            "per_call_override":\(perCallOverride),
            "settings":\(settings),
            "adjusted_from_selection_id":\(adjustedFromSelectionID),
            "adjustments":\(adjustments)
          }
        }
      }
      """.utf8
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

  static func sessionEventDecodingDiagnostic(
    in frame: SignalboxProcessServerFrame
  ) -> SignalboxDecodingDiagnostic? {
    guard
      case .sessionEvent(let followed) = frame.message,
      case .unknown(_, _, let diagnostic) = followed.event
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

  static func textEntryDecodingDiagnostic(
    in message: SignalboxProcessServerMessage
  ) -> SignalboxDecodingDiagnostic? {
    guard case .transcriptTextEntry(let textEntry) = message,
      case .unknown("user", _, let diagnostic) = textEntry.entry
    else {
      return nil
    }
    return diagnostic
  }

  static func userDenialReason(in message: SignalboxProcessServerMessage) -> String? {
    guard case .sessionEvent(let followed) = message,
      case .toolApprovalDecided(_, _, .deny(let reason), .user, nil) = followed.event
    else {
      return nil
    }
    return reason
  }

  static func transcriptTurnState(
    in message: SignalboxProcessServerMessage
  ) -> SignalboxTranscriptTurnState? {
    guard case .transcriptTurn(let turn) = message else {
      return nil
    }
    return turn.state
  }

  static func toolApprovalDecisionDiagnostic(
    in message: SignalboxProcessServerMessage
  ) -> SignalboxDecodingDiagnostic? {
    guard case .sessionEvent(let followed) = message,
      case .unknown("tool_approval_decided", _, let diagnostic) = followed.event
    else {
      return nil
    }
    return diagnostic
  }
}

private enum ProcessProtocolFixtureError: Error {
  case missingDelegatedOrigin
  case missingImportedEntry
  case missingRejectionDetail
  case missingProcessError
  case missingMetadataWriter
  case missingTranscriptEntry
  case missingModelCallUsage
  case missingModelCallsEnd
  case missingProviderFailureCause
  case missingUnknownDiagnostic
}
