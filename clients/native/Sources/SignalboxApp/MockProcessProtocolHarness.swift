// Release builds must not ship the in-memory process-protocol harness; the
// mock is reachable only through Debug (and therefore test) configurations.
#if DEBUG
import Foundation

#if canImport(SignalboxClient)
  import SignalboxClient
#endif
#if canImport(SignalboxModels)
  import SignalboxModels
#endif

/// The app mock exercises the same process-protocol encoder, decoder, framing,
/// and request identities as the Unix-socket adapter. Only its byte transport is
/// in memory.
struct MockProcessProtocolConnectionFactory: SignalboxProcessConnectionFactory {
  private let state: MockProcessProtocolState

  init(scenario: ScreenshotScenario? = nil) {
    state = MockProcessProtocolState(scenario: scenario)
  }

  func makeConnection() -> any SignalboxProcessConnection {
    MockProcessProtocolConnection(state: state)
  }
}

enum MockProcessProtocolFixtures {
  static let sessionCount = 8
  static let conversationRecordCount = 3
  static let snapshotCursor = "4"
  static let firstAcceptancePosition = "1"
  static let submittedAcceptancePosition = "2"
  static let singleTurnCount = "1"
  static let transcriptEntryCount = "2"
  static let selectionID = "aaaaaaaa-0000-4000-8000-000000000001"
  static let aliasID = "aaaaaaaa-0000-4000-8000-000000000002"
  static let createdSessionID = "aaaaaaaa-0000-4000-8000-000000000003"
  static let continuedSessionID = "aaaaaaaa-0000-4000-8000-000000000004"
  static let importedConversationID = "99999999-0000-4000-8000-000000000001"
  static let importedUserEntryID = "99999999-0000-4000-8000-000000000002"
  static let importedAssistantEntryID = "99999999-0000-4000-8000-000000000003"
  static let importedEntryCount = 2
  static let importedUserPreview = "Summarize the process-protocol boundary."
  static let importedAssistantPreview =
    "The process protocol keeps the daemon boundary explicit."
  static let submittedAcceptedInputID = "bbbbbbbb-0000-4000-8000-000000000001"
  static let submittedTurnID = "cccccccc-0000-4000-8000-000000000001"
  static let activeUserEntryID = "dddddddd-0000-4000-8000-000000000001"
  static let activeAssistantEntryID = "dddddddd-0000-4000-8000-000000000002"
  static let activeAcceptedInputID = "eeeeeeee-0000-4000-8000-000000000001"
  static let activeTurnID = "ffffffff-0000-4000-8000-000000000001"
  static let activeModelCallID = "12121212-0000-4000-8000-000000000001"
  static let completedToolUseEntryID = "dddddddd-0000-4000-8000-000000000007"
  static let completedToolResultEntryID = "dddddddd-0000-4000-8000-000000000008"
  static let completedToolRequestID = "abababab-0000-4000-8000-000000000002"
  static let completedToolRequestEntryIndex = "1"
  static let completedToolAttemptID = "abababab-0000-4000-8000-000000000003"
  static let completedAttemptID = "abababab-0000-4000-8000-000000000004"
  static let completedFrontierID = "abababab-0000-4000-8000-000000000005"
  static let completedToolName = "save_report"
  static let completedToolOutput = "Saved artifact runner-status.md"
  static let approvalUserEntryID = "dddddddd-0000-4000-8000-000000000003"
  static let approvalToolEntryID = "dddddddd-0000-4000-8000-000000000004"
  static let approvalAcceptedInputID = "eeeeeeee-0000-4000-8000-000000000002"
  static let approvalTurnID = "ffffffff-0000-4000-8000-000000000002"
  static let approvalModelCallID = "12121212-0000-4000-8000-000000000002"
  static let failedUserEntryID = "dddddddd-0000-4000-8000-000000000005"
  static let failedMarkerEntryID = "dddddddd-0000-4000-8000-000000000006"
  static let failedAcceptedInputID = "eeeeeeee-0000-4000-8000-000000000003"
  static let failedTurnID = "ffffffff-0000-4000-8000-000000000003"
  static let failedFrontierID = "13131313-0000-4000-8000-000000000001"

  static func providerDefaultModelSettings() -> [String: Any] {
    let inherit: [String: Any] = ["kind": "inherit"]
    let overlay: [String: Any] = [
      "reasoning_level": inherit,
      "fast_mode": inherit,
      "service_tier": inherit,
    ]
    return [
      "precedence": [
        "per_call": overlay,
        "session": overlay,
        "profile": overlay,
        "global_default": overlay,
      ],
      "effective": [
        "reasoning_level": NSNull(),
        "fast_mode": "disabled",
        "service_tier": NSNull(),
      ],
      "reasoning_source": NSNull(),
      "fast_mode_source": NSNull(),
      "service_tier_source": NSNull(),
      "validated_for_selection_id": NSNull(),
    ]
  }
}

private actor MockProcessProtocolConnection: SignalboxProcessConnection {
  private let state: MockProcessProtocolState
  private var chunks: [Data] = []
  private var waiter: CheckedContinuation<Data?, Never>?
  private var keepsFollowOpen = false
  private var isClosed = false

  init(state: MockProcessProtocolState) {
    self.state = state
  }

  func start() async throws {}

  func send(_ data: Data) async throws {
    let response = try await state.response(to: data)
    chunks = response.frames
    keepsFollowOpen = response.keepsConnectionOpen
  }

  func receive() async throws -> Data? {
    guard !isClosed else {
      return nil
    }
    if !chunks.isEmpty {
      return chunks.removeFirst()
    }
    guard keepsFollowOpen else {
      return nil
    }
    return await withCheckedContinuation { continuation in
      waiter = continuation
    }
  }

  func close() async {
    isClosed = true
    let pending = waiter
    waiter = nil
    pending?.resume(returning: nil)
  }
}

private struct MockProcessProtocolResponse: Sendable {
  let frames: [Data]
  let keepsConnectionOpen: Bool
}

private actor MockProcessProtocolState {
  private struct Session: Sendable {
    let id: String
    let defaultsVersion: String
    let title: String
    let tags: [String]
    var archived: Bool
  }

  private let scenario: ScreenshotScenario?
  private var sessions = [
    Session(
      id: MockSignalboxFixtures.approvalSessionID,
      defaultsVersion: "3",
      title: "Tool decision unavailable",
      tags: ["approval", "local"],
      archived: false
    ),
    Session(
      id: MockSignalboxFixtures.activeSessionID,
      defaultsVersion: "7",
      title: "Research process protocol",
      tags: ["native", "v5"],
      archived: false
    ),
    Session(
      id: MockSignalboxFixtures.markdownBasicsSessionID,
      defaultsVersion: "1",
      title: "Markdown headings and lists",
      tags: ["markdown"],
      archived: false
    ),
    Session(
      id: MockSignalboxFixtures.markdownTableSessionID,
      defaultsVersion: "1",
      title: "Markdown table",
      tags: ["markdown"],
      archived: false
    ),
    Session(
      id: MockSignalboxFixtures.markdownCodeSessionID,
      defaultsVersion: "1",
      title: "Markdown code blocks",
      tags: ["markdown"],
      archived: false
    ),
    Session(
      id: MockSignalboxFixtures.markdownSessionID,
      defaultsVersion: "1",
      title: "Markdown mixed message",
      tags: ["markdown"],
      archived: false
    ),
    Session(
      id: MockSignalboxFixtures.failedSessionID,
      defaultsVersion: "2",
      title: "Failed local turn",
      tags: ["diagnostic"],
      archived: false
    ),
    Session(
      id: MockSignalboxFixtures.archivedSessionID,
      defaultsVersion: "1",
      title: "Archived smoke run",
      tags: ["archive"],
      archived: true
    ),
  ]

  init(scenario: ScreenshotScenario?) {
    self.scenario = scenario
  }

  func response(to data: Data) throws -> MockProcessProtocolResponse {
    let object = try JSONSerialization.jsonObject(with: data)
    guard
      let envelope = object as? [String: Any],
      let requestID = envelope["request_id"] as? String,
      let request = envelope["request"] as? [String: Any],
      let type = request["type"] as? String
    else {
      throw SignalboxProcessClientError.unterminatedFrame
    }
    switch type {
    case "list_model_aliases":
      return try response(
        requestID: requestID,
        messages: [
          ["type": "model_aliases_start"],
          [
            "type": "model_alias_summary",
            "alias_id": MockProcessProtocolFixtures.aliasID,
            "selection_id": MockProcessProtocolFixtures.selectionID,
          ],
          ["type": "model_aliases_end", "alias_count": "1"],
        ],
        keepsConnectionOpen: true
      )
    case "create_session":
      let selection = request["initial_model_selection"] as? [String: Any]
      guard
        request["command_id"] is String,
        selection?["kind"] as? String == "alias",
        selection?["alias_id"] as? String == MockProcessProtocolFixtures.aliasID
      else {
        throw MockProcessProtocolError.invalidRequest
      }
      if !sessions.contains(where: { $0.id == MockProcessProtocolFixtures.createdSessionID }) {
        sessions.append(
          Session(
            id: MockProcessProtocolFixtures.createdSessionID,
            defaultsVersion: "1",
            title: "New native session",
            tags: [],
            archived: false
          )
        )
      }
      return try response(
        requestID: requestID,
        messages: [
          [
            "type": "session_created",
            "session_id": MockProcessProtocolFixtures.createdSessionID,
            "model_settings": MockProcessProtocolFixtures.providerDefaultModelSettings(),
          ]
        ]
      )
    case "read_imported_conversation":
      guard
        request["imported_conversation_id"] as? String
          == MockProcessProtocolFixtures.importedConversationID
      else {
        throw MockProcessProtocolError.invalidRequest
      }
      return try response(
        requestID: requestID,
        messages: [
          [
            "type": "imported_conversation_start",
            "imported_conversation_id": MockProcessProtocolFixtures.importedConversationID,
          ],
          [
            "type": "imported_conversation_entry",
            "position": "1",
            "imported_entry_id": MockProcessProtocolFixtures.importedUserEntryID,
            "source_speaker": ["type": "attested", "speaker": "user"],
            "content_kind": "text",
            "text_preview": [
              "preview": MockProcessProtocolFixtures.importedUserPreview,
              "truncated": false,
            ],
          ],
          [
            "type": "imported_conversation_entry",
            "position": "2",
            "imported_entry_id": MockProcessProtocolFixtures.importedAssistantEntryID,
            "source_speaker": ["type": "attested", "speaker": "assistant"],
            "content_kind": "text",
            "text_preview": [
              "preview": MockProcessProtocolFixtures.importedAssistantPreview,
              "truncated": false,
            ],
          ],
          [
            "type": "imported_conversation_end",
            "imported_conversation_id": MockProcessProtocolFixtures.importedConversationID,
            "entry_count": String(MockProcessProtocolFixtures.importedEntryCount),
          ],
        ]
      )
    case "create_session_from_imported_frontier":
      let selection = request["initial_model_selection"] as? [String: Any]
      let throughPosition = request["through_position"] as? String
      let relationship = request["relationship"] as? String
      guard
        request["command_id"] is String,
        request["imported_conversation_id"] as? String
          == MockProcessProtocolFixtures.importedConversationID,
        throughPosition.map({ ["1", "2"].contains($0) }) == true,
        relationship.map({ ["resume", "fork"].contains($0) }) == true,
        selection?["kind"] as? String == "alias",
        selection?["alias_id"] as? String == MockProcessProtocolFixtures.aliasID
      else {
        throw MockProcessProtocolError.invalidRequest
      }
      if !sessions.contains(where: { $0.id == MockProcessProtocolFixtures.continuedSessionID }) {
        sessions.append(
          Session(
            id: MockProcessProtocolFixtures.continuedSessionID,
            defaultsVersion: "1",
            title: "Continued imported session",
            tags: [],
            archived: false
          )
        )
      }
      return try response(
        requestID: requestID,
        messages: [
          [
            "type": "session_created",
            "session_id": MockProcessProtocolFixtures.continuedSessionID,
            "model_settings": MockProcessProtocolFixtures.providerDefaultModelSettings(),
          ]
        ]
      )
    case "list_conversations":
      let includeArchived = request["include_archived"] as? Bool ?? false
      var page = sessions
        .filter { includeArchived || !$0.archived }
        .map { session in
          (
            session.id,
            [
              "type": "conversation_summary",
              "conversation": [
                "origin": "native_session",
                "session_id": session.id,
                "title": session.title,
                "archived": session.archived,
                "defaults_version": session.defaultsVersion,
              ],
            ] as [String: Any]
          )
        }
      if scenario == nil {
        page.append(
          (
            MockProcessProtocolFixtures.importedConversationID,
            [
              "type": "conversation_summary",
              "conversation": [
                "origin": "imported_conversation",
                "imported_conversation_id": MockProcessProtocolFixtures.importedConversationID,
                "title": "Imported process-protocol notes",
                "entry_count": String(MockProcessProtocolFixtures.importedEntryCount),
                "source_format": "codex_rollout_jsonl_v1",
              ],
            ] as [String: Any]
          )
        )
      }
      page.sort { $0.0 < $1.0 }
      var messages: [[String: Any]] = [["type": "conversation_page_start"]]
      messages.append(contentsOf: page.map(\.1))
      messages.append([
        "type": "conversation_page_end",
        "conversation_count": String(page.count),
        "next_after": NSNull(),
      ])
      return try response(requestID: requestID, messages: messages)
    case "list_session_metadata":
      let page = try metadataPage(request)
      var messages: [[String: Any]] = [["type": "session_metadata_page_start"]]
      messages.append(contentsOf: page.sessions.map(metadataSummary))
      let nextAfterSessionID: Any =
        if let next = page.nextAfterSessionID {
          next
        } else {
          NSNull()
        }
      messages.append([
        "type": "session_metadata_page_end",
        "session_count": String(page.sessions.count),
        "next_after_session_id": nextAfterSessionID,
      ])
      return try response(requestID: requestID, messages: messages)
    case "read_session_metadata":
      let session = try requiredSession(request)
      return try response(
        requestID: requestID,
        messages: [metadataRead(type: "session_metadata", session: session)]
      )
    case "read_session_defaults":
      let session = try requiredSession(request)
      return try response(
        requestID: requestID,
        messages: [
          [
            "type": "session_defaults",
            "session_id": session.id,
            "defaults_version": session.defaultsVersion,
            "model_selection": [
              "kind": "direct",
              "selection_id": MockProcessProtocolFixtures.selectionID,
            ],
            "model_settings": MockProcessProtocolFixtures.providerDefaultModelSettings(),
            "dangerous_tool_auto_approval": false,
            "system_prompt": NSNull(),
          ]
        ]
      )
    case "replace_session_metadata":
      let session = try requiredSession(request)
      let archived = try replacementArchive(request, session: session)
      try replaceArchived(archived, sessionID: session.id)
      let replacement = sessions.first { $0.id == session.id } ?? session
      return try response(
        requestID: requestID,
        messages: [metadataRead(type: "session_metadata_replaced", session: replacement)]
      )
    case "submit_input":
      let session = try requiredSession(request)
      try validateSubmission(request, session: session)
      return try response(
        requestID: requestID,
        messages: [
          [
            "type": "input_submitted",
            "session_id": session.id,
            "accepted_input_id": MockProcessProtocolFixtures.submittedAcceptedInputID,
            "acceptance_position": MockProcessProtocolFixtures.submittedAcceptancePosition,
            "turn_id": MockProcessProtocolFixtures.submittedTurnID,
            "model_settings": MockProcessProtocolFixtures.providerDefaultModelSettings(),
          ]
        ]
      )
    case "follow_session":
      let session = try requiredSession(request)
      return try response(
        requestID: requestID,
        messages: transcript(session),
        keepsConnectionOpen: true
      )
    case "read_transcript":
      let session = try requiredSession(request)
      return try response(requestID: requestID, messages: transcript(session))
    default:
      return try response(
        requestID: requestID,
        messages: [
          [
            "type": "error",
            "code": "unsupported_operation",
            "message": "The mock harness does not implement \(type).",
          ]
        ]
      )
    }
  }

  private func metadataPage(
    _ request: [String: Any]
  ) throws -> (sessions: [Session], nextAfterSessionID: String?) {
    guard
      let pageSizeText = request["page_size"] as? String,
      let pageSize = Int(pageSizeText),
      pageSize > 0
    else {
      throw MockProcessProtocolError.invalidRequest
    }
    let afterSessionID: String?
    if request["after_session_id"] is NSNull {
      afterSessionID = nil
    } else if let after = request["after_session_id"] as? String {
      afterSessionID = after
    } else {
      throw MockProcessProtocolError.invalidRequest
    }
    let includeArchived = request["include_archived"] as? Bool ?? false
    let candidates =
      sessions
      .filter { includeArchived || !$0.archived }
      .filter { session in
        afterSessionID.map { after in after < session.id } ?? true
      }
      .sorted { $0.id < $1.id }
    let page = Array(candidates.prefix(pageSize))
    let nextAfterSessionID: String? =
      if candidates.count > page.count {
        page.last?.id
      } else {
        nil
      }
    return (page, nextAfterSessionID)
  }

  private func requiredSession(_ request: [String: Any]) throws -> Session {
    guard
      let sessionID = request["session_id"] as? String,
      let session = sessions.first(where: { $0.id == sessionID })
    else {
      throw SignalboxProcessClientError.connectionClosed
    }
    return session
  }

  private func replaceArchived(_ archived: Bool, sessionID: String) throws {
    guard let index = sessions.firstIndex(where: { $0.id == sessionID }) else {
      throw SignalboxProcessClientError.connectionClosed
    }
    sessions[index].archived = archived
  }

  private func replacementArchive(
    _ request: [String: Any],
    session: Session
  ) throws -> Bool {
    guard
      request["command_id"] is String,
      let metadata = request["metadata"] as? [String: Any],
      metadata["title"] as? String == session.title,
      metadata["tags"] as? [String] == session.tags,
      let attributes = metadata["attributes"] as? [String: Any],
      attributes.isEmpty,
      let archived = metadata["archived"] as? Bool
    else {
      throw MockProcessProtocolError.invalidRequest
    }
    return archived
  }

  private func validateSubmission(
    _ request: [String: Any],
    session: Session
  ) throws {
    guard
      request["command_id"] is String,
      let content = request["content"] as? [[String: Any]],
      content.count == 1,
      content[0]["type"] as? String == "text",
      content[0]["text"] is String,
      request["expected_defaults_version"] as? String == session.defaultsVersion
    else {
      throw MockProcessProtocolError.invalidRequest
    }
  }

  private func metadataSummary(_ session: Session) -> [String: Any] {
    [
      "type": "session_metadata_summary",
      "session_id": session.id,
      "defaults_version": session.defaultsVersion,
      "model_selection": [
        "kind": "direct",
        "selection_id": MockProcessProtocolFixtures.selectionID,
      ],
      "dangerous_tool_auto_approval": false,
      "title": session.title,
      "tags": session.tags,
      "archived": session.archived,
      "last_writer": NSNull(),
    ]
  }

  private func metadataRead(type: String, session: Session) -> [String: Any] {
    [
      "type": type,
      "session_id": session.id,
      "metadata": [
        "title": session.title,
        "tags": session.tags,
        "attributes": [:],
        "archived": session.archived,
      ],
      "last_writer": NSNull(),
    ]
  }

  private func transcript(_ session: Session) -> [[String: Any]] {
    let cursor = MockProcessProtocolFixtures.snapshotCursor
    let fixture = transcriptFixture(session)
    var messages: [[String: Any]] = [
      [
        "type": "transcript_snapshot_start",
        "session_id": session.id,
        "cursor": cursor,
        "runner": NSNull(),
      ]
    ]
    messages.append(contentsOf: fixture.records.prefix(1))
    messages.append(contentsOf: fixture.modelCallUsage)
    messages.append([
      "type": "transcript_model_calls_end",
      "model_call_count": String(fixture.modelCallUsage.count),
    ])
    messages.append(contentsOf: fixture.records.dropFirst())
    messages.append([
      "type": "transcript_snapshot_end",
      "session_id": session.id,
      "cursor": cursor,
      "turn_count": fixture.turnCount,
      "entry_count": fixture.entryCount,
    ])
    return messages
  }

  private struct TranscriptFixture {
    let records: [[String: Any]]
    let modelCallUsage: [[String: Any]]
    let turnCount: String
    let entryCount: String
  }

  private func transcriptFixture(_ session: Session) -> TranscriptFixture {
    if session.id == MockSignalboxFixtures.approvalSessionID {
      return TranscriptFixture(
        records: approvalTranscript(sessionID: session.id),
        modelCallUsage: [
          modelCallUsage(
            turnID: MockProcessProtocolFixtures.approvalTurnID,
            modelCallID: MockProcessProtocolFixtures.approvalModelCallID
          )
        ],
        turnCount: MockProcessProtocolFixtures.singleTurnCount,
        entryCount: MockProcessProtocolFixtures.transcriptEntryCount
      )
    }
    if session.id == MockSignalboxFixtures.failedSessionID {
      return TranscriptFixture(
        records: failedTranscript(sessionID: session.id),
        modelCallUsage: [],
        turnCount: MockProcessProtocolFixtures.singleTurnCount,
        entryCount: MockProcessProtocolFixtures.transcriptEntryCount
      )
    }
    if session.id == MockSignalboxFixtures.activeSessionID, scenario == .completedTool {
      return TranscriptFixture(
        records: completedToolTranscript(sessionID: session.id),
        modelCallUsage: [
          modelCallUsage(
            turnID: MockProcessProtocolFixtures.activeTurnID,
            modelCallID: MockProcessProtocolFixtures.activeModelCallID
          )
        ],
        turnCount: MockProcessProtocolFixtures.singleTurnCount,
        entryCount: "3"
      )
    }
    let conversation = conversationContent(sessionID: session.id)
    return TranscriptFixture(
      records: conversationTranscript(
        sessionID: session.id,
        userText: conversation.user,
        assistantText: conversation.assistant
      ),
      modelCallUsage: [
        modelCallUsage(
          turnID: MockProcessProtocolFixtures.activeTurnID,
          modelCallID: MockProcessProtocolFixtures.activeModelCallID
        )
      ],
      turnCount: MockProcessProtocolFixtures.singleTurnCount,
      entryCount: MockProcessProtocolFixtures.transcriptEntryCount
    )
  }

  private func modelCallUsage(
    turnID: String,
    modelCallID: String
  ) -> [String: Any] {
    [
      "type": "transcript_model_call_usage",
      "model_call_index": "0",
      "turn_id": turnID,
      "model_call_id": modelCallID,
      "usage_provenance": "reported",
      "usage": [
        "input_tokens": NSNull(),
        "output_tokens": NSNull(),
        "cache_creation_input_tokens": NSNull(),
        "cache_read_input_tokens": NSNull(),
      ],
      "cost": NSNull(),
    ]
  }

  private func conversationTranscript(
    sessionID: String,
    userText: String,
    assistantText: String
  ) -> [[String: Any]] {
    [
      [
        "type": "transcript_turn",
        "turn_id": MockProcessProtocolFixtures.activeTurnID,
        "acceptance_position": MockProcessProtocolFixtures.firstAcceptancePosition,
        "state": [
          "type": "completed",
          "terminal_frontier_id": MockProcessProtocolFixtures.completedFrontierID,
          "terminal_attempt_id": MockProcessProtocolFixtures.completedAttemptID,
          "terminal_model_call_id": MockProcessProtocolFixtures.activeModelCallID,
        ],
      ],
      textEntry(
        index: "0",
        sessionID: sessionID,
        entryID: MockProcessProtocolFixtures.activeUserEntryID,
        entry: [
          "type": "user",
          "accepted_input_id": MockProcessProtocolFixtures.activeAcceptedInputID,
          "turn_id": MockProcessProtocolFixtures.activeTurnID,
        ]
      ),
      content(index: "0", text: userText),
      textEntry(
        index: "1",
        sessionID: sessionID,
        entryID: MockProcessProtocolFixtures.activeAssistantEntryID,
        entry: [
          "type": "assistant",
          "turn_id": MockProcessProtocolFixtures.activeTurnID,
          "model_call_id": MockProcessProtocolFixtures.activeModelCallID,
        ]
      ),
      content(index: "1", text: assistantText),
    ]
  }

  private func conversationContent(
    sessionID: String
  ) -> (user: String, assistant: String) {
    switch sessionID {
    case MockSignalboxFixtures.markdownBasicsSessionID:
      return (
        MockSignalboxFixtures.markdownBasicsUserText,
        MockSignalboxFixtures.markdownBasicsAssistantText
      )
    case MockSignalboxFixtures.markdownTableSessionID:
      return (
        MockSignalboxFixtures.markdownTableUserText,
        MockSignalboxFixtures.markdownTableAssistantText
      )
    case MockSignalboxFixtures.markdownCodeSessionID:
      return (
        MockSignalboxFixtures.markdownCodeUserText,
        MockSignalboxFixtures.markdownCodeAssistantText
      )
    case MockSignalboxFixtures.markdownSessionID:
      return (
        MockSignalboxFixtures.markdownUserText,
        MockSignalboxFixtures.markdownAssistantText
      )
    default:
      return (
        "Show the native client speaking the real process protocol.",
        "The view is projected from a process-protocol JSONL snapshot over the transport abstraction."
      )
    }
  }

  private func completedToolTranscript(sessionID: String) -> [[String: Any]] {
    [
      [
        "type": "transcript_turn",
        "turn_id": MockProcessProtocolFixtures.activeTurnID,
        "acceptance_position": MockProcessProtocolFixtures.firstAcceptancePosition,
        "state": [
          "type": "completed",
          "terminal_frontier_id": MockProcessProtocolFixtures.completedFrontierID,
          "terminal_attempt_id": MockProcessProtocolFixtures.completedAttemptID,
          "terminal_model_call_id": MockProcessProtocolFixtures.activeModelCallID,
        ],
      ],
      textEntry(
        index: "0",
        sessionID: sessionID,
        entryID: MockProcessProtocolFixtures.activeUserEntryID,
        entry: [
          "type": "user",
          "accepted_input_id": MockProcessProtocolFixtures.activeAcceptedInputID,
          "turn_id": MockProcessProtocolFixtures.activeTurnID,
        ]
      ),
      content(index: "0", text: "Save the runner status report."),
      [
        "type": "transcript_entry",
        "entry_index": MockProcessProtocolFixtures.completedToolRequestEntryIndex,
        "source_session_id": sessionID,
        "entry_id": MockProcessProtocolFixtures.completedToolUseEntryID,
        "entry": [
          "type": "assistant_tool_use",
          "turn_id": MockProcessProtocolFixtures.activeTurnID,
          "model_call_id": MockProcessProtocolFixtures.activeModelCallID,
          "tool_request_id": MockProcessProtocolFixtures.completedToolRequestID,
          "tool_name": MockProcessProtocolFixtures.completedToolName,
          "arguments": #"{"title":"runner-status.md"}"#,
        ],
      ],
      [
        "type": "transcript_entry",
        "entry_index": "2",
        "source_session_id": sessionID,
        "entry_id": MockProcessProtocolFixtures.completedToolResultEntryID,
        "entry": [
          "type": "tool_execution_result",
          "tool_request_id": MockProcessProtocolFixtures.completedToolRequestID,
          "tool_attempt_id": MockProcessProtocolFixtures.completedToolAttemptID,
          "content": MockProcessProtocolFixtures.completedToolOutput,
        ],
      ],
    ]
  }

  private func approvalTranscript(sessionID: String) -> [[String: Any]] {
    [
      [
        "type": "transcript_turn",
        "turn_id": MockProcessProtocolFixtures.approvalTurnID,
        "acceptance_position": MockProcessProtocolFixtures.firstAcceptancePosition,
        "state": [
          "type": "active_awaiting_tool_approval",
          "tool_request_id": MockSignalboxFixtures.invocationID,
        ],
      ],
      textEntry(
        index: "0",
        sessionID: sessionID,
        entryID: MockProcessProtocolFixtures.approvalUserEntryID,
        entry: [
          "type": "user",
          "accepted_input_id": MockProcessProtocolFixtures.approvalAcceptedInputID,
          "turn_id": MockProcessProtocolFixtures.approvalTurnID,
        ]
      ),
      content(index: "0", text: "Apply the proposed local patch."),
      [
        "type": "transcript_entry",
        "entry_index": "1",
        "source_session_id": sessionID,
        "entry_id": MockProcessProtocolFixtures.approvalToolEntryID,
        "entry": [
          "type": "assistant_tool_use",
          "turn_id": MockProcessProtocolFixtures.approvalTurnID,
          "model_call_id": MockProcessProtocolFixtures.approvalModelCallID,
          "tool_request_id": MockSignalboxFixtures.invocationID,
          "tool_name": "apply_patch",
          "arguments": #"{"path":"README.md"}"#,
        ],
      ],
    ]
  }

  private func failedTranscript(sessionID: String) -> [[String: Any]] {
    [
      [
        "type": "transcript_turn",
        "turn_id": MockProcessProtocolFixtures.failedTurnID,
        "acceptance_position": MockProcessProtocolFixtures.firstAcceptancePosition,
        "state": [
          "type": "failed",
          "terminal_frontier_id": MockProcessProtocolFixtures.failedFrontierID,
          "terminal_attempt_id": NSNull(),
          "terminal_model_call": NSNull(),
        ],
      ],
      textEntry(
        index: "0",
        sessionID: sessionID,
        entryID: MockProcessProtocolFixtures.failedUserEntryID,
        entry: [
          "type": "user",
          "accepted_input_id": MockProcessProtocolFixtures.failedAcceptedInputID,
          "turn_id": MockProcessProtocolFixtures.failedTurnID,
        ]
      ),
      content(index: "0", text: "Run the local operation."),
      [
        "type": "transcript_entry",
        "entry_index": "1",
        "source_session_id": sessionID,
        "entry_id": MockProcessProtocolFixtures.failedMarkerEntryID,
        "entry": [
          "type": "turn_failed",
          "turn_id": MockProcessProtocolFixtures.failedTurnID,
        ],
      ],
    ]
  }

  private func textEntry(
    index: String,
    sessionID: String,
    entryID: String,
    entry: [String: Any]
  ) -> [String: Any] {
    [
      "type": "transcript_text_entry",
      "entry_index": index,
      "source_session_id": sessionID,
      "entry_id": entryID,
      "entry": entry,
    ]
  }

  private func content(index: String, text: String) -> [String: Any] {
    [
      "type": "transcript_content",
      "entry_index": index,
      "fragment_index": "0",
      "final_fragment": true,
      "content_fragment": text,
    ]
  }

  private func response(
    requestID: String,
    messages: [[String: Any]],
    keepsConnectionOpen: Bool = false
  ) throws -> MockProcessProtocolResponse {
    let frames = try messages.map { message in
      let envelope: [String: Any] = [
        "version": SignalboxProcessProtocol.currentVersion.rawValue,
        "request_id": requestID,
        "message": message,
      ]
      var data = try JSONSerialization.data(withJSONObject: envelope, options: [.sortedKeys])
      data.append(0x0A)
      return data
    }
    return MockProcessProtocolResponse(
      frames: frames,
      keepsConnectionOpen: keepsConnectionOpen
    )
  }
}

private enum MockProcessProtocolError: Error {
  case invalidRequest
}
#endif
