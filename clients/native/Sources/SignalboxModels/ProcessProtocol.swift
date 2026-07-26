import Foundation

public enum SignalboxProcessProtocol {
  public static let currentVersion = SignalboxProcessProtocolVersion.five
  public static let maximumFrameBytes = 8 * 1024 * 1024
  public static let maximumContentFragmentUTF8Bytes = 1024 * 1024
  // docs/spec/process-protocol.md owns the version-five metadata projection bounds.
  public static let maximumMetadataTags = 256
  public static let maximumMetadataSummaryUTF8Bytes = 262_144
}

public enum SignalboxProcessProtocolVersion: UInt64, Codable, CaseIterable, Sendable {
  case one = 1
  case two = 2
  case three = 3
  case four = 4
  case five = 5
}

public enum SignalboxCanonicalValueError: LocalizedError, Equatable {
  case uuid
  case decimal
  case requestID
  case commandID

  public var errorDescription: String? {
    switch self {
    case .uuid:
      return "UUID is not canonical lowercase hyphenated text."
    case .decimal:
      return "Unsigned integer is not canonical decimal text."
    case .requestID:
      return "Client request identity must be nonzero."
    case .commandID:
      return "Command identity is a reserved sentinel."
    }
  }
}

public struct SignalboxCanonicalUUID: Codable, Hashable, Sendable {
  public let rawValue: String

  public init(validating rawValue: String) throws {
    guard let value = UUID(uuidString: rawValue),
      value.uuidString.lowercased() == rawValue
    else {
      throw SignalboxCanonicalValueError.uuid
    }
    self.rawValue = rawValue
  }

  public init(from decoder: Decoder) throws {
    try self.init(validating: decoder.singleValueContainer().decode(String.self))
  }

  public func encode(to encoder: Encoder) throws {
    var container = encoder.singleValueContainer()
    try container.encode(rawValue)
  }
}

public struct SignalboxCommandID: Codable, Hashable, Sendable {
  public let rawValue: SignalboxCanonicalUUID

  public init(validating rawValue: String) throws {
    let value = try SignalboxCanonicalUUID(validating: rawValue)
    guard rawValue != "00000000-0000-0000-0000-000000000000",
      rawValue != "ffffffff-ffff-ffff-ffff-ffffffffffff"
    else {
      throw SignalboxCanonicalValueError.commandID
    }
    self.rawValue = value
  }

  public init(from decoder: Decoder) throws {
    try self.init(validating: decoder.singleValueContainer().decode(String.self))
  }

  public func encode(to encoder: Encoder) throws {
    try rawValue.encode(to: encoder)
  }
}

public struct SignalboxCanonicalUInt64: RawRepresentable, Codable, Hashable, Comparable, Sendable {
  public let rawValue: UInt64

  public init(rawValue: UInt64) {
    self.rawValue = rawValue
  }

  public init(from decoder: Decoder) throws {
    let spelling = try decoder.singleValueContainer().decode(String.self)
    guard !spelling.isEmpty,
      spelling == "0" || !spelling.hasPrefix("0"),
      spelling.allSatisfy(\.isASCII),
      spelling.allSatisfy(\.isNumber),
      let value = UInt64(spelling),
      String(value) == spelling
    else {
      throw SignalboxCanonicalValueError.decimal
    }
    self.rawValue = value
  }

  public func encode(to encoder: Encoder) throws {
    var container = encoder.singleValueContainer()
    try container.encode(String(rawValue))
  }

  public static func < (lhs: Self, rhs: Self) -> Bool {
    lhs.rawValue < rhs.rawValue
  }
}

public struct SignalboxRequestID: Codable, Hashable, Sendable {
  public let rawValue: UInt64

  public init(validating rawValue: UInt64) throws {
    guard rawValue != 0 else {
      throw SignalboxCanonicalValueError.requestID
    }
    self.rawValue = rawValue
  }

  public init(from decoder: Decoder) throws {
    let value = try SignalboxCanonicalUInt64(from: decoder).rawValue
    try self.init(validating: value)
  }

  public func encode(to encoder: Encoder) throws {
    try SignalboxCanonicalUInt64(rawValue: rawValue).encode(to: encoder)
  }
}

public enum SignalboxModelSelection: Codable, Equatable, Sendable {
  case direct(selectionID: SignalboxCanonicalUUID)
  case alias(aliasID: SignalboxCanonicalUUID)

  private enum CodingKeys: String, CodingKey {
    case kind
    case selectionID = "selection_id"
    case aliasID = "alias_id"
  }

  public init(from decoder: Decoder) throws {
    let container = try decoder.container(keyedBy: CodingKeys.self)
    switch try container.decode(String.self, forKey: .kind) {
    case "direct":
      self = .direct(
        selectionID: try container.decode(SignalboxCanonicalUUID.self, forKey: .selectionID))
    case "alias":
      self = .alias(aliasID: try container.decode(SignalboxCanonicalUUID.self, forKey: .aliasID))
    default:
      throw DecodingError.dataCorruptedError(
        forKey: .kind, in: container, debugDescription: "Unknown model selection.")
    }
  }

  public func encode(to encoder: Encoder) throws {
    var container = encoder.container(keyedBy: CodingKeys.self)
    switch self {
    case .direct(let selectionID):
      try container.encode("direct", forKey: .kind)
      try container.encode(selectionID, forKey: .selectionID)
    case .alias(let aliasID):
      try container.encode("alias", forKey: .kind)
      try container.encode(aliasID, forKey: .aliasID)
    }
  }
}

public struct SignalboxProcessSessionMetadata: Codable, Equatable, Sendable {
  public let title: String?
  public let tags: [String]
  public let attributes: [String: String]
  public let archived: Bool

  public init(
    title: String?,
    tags: [String],
    attributes: [String: String],
    archived: Bool
  ) {
    self.title = title
    self.tags = tags
    self.attributes = attributes
    self.archived = archived
  }
}

public struct SignalboxMetadataLastWriter: Codable, Equatable, Sendable {
  public let updatedAtUnixMicros: SignalboxCanonicalUInt64
  public let actor: SignalboxMetadataActor

  private enum CodingKeys: String, CodingKey {
    case updatedAtUnixMicros = "updated_at_unix_micros"
    case actor
  }
}

public enum SignalboxMetadataActor: Codable, Equatable, Sendable {
  case owner
  case unknown(kind: String, payload: [String: SignalboxJSONValue])

  public init(from decoder: Decoder) throws {
    let tagged = try SignalboxTaggedPayload(from: decoder)
    self = tagged.kind == "owner" ? .owner : .unknown(kind: tagged.kind, payload: tagged.payload)
  }

  public func encode(to encoder: Encoder) throws {
    switch self {
    case .owner:
      try ["type": SignalboxJSONValue.string("owner")].encode(to: encoder)
    case .unknown(let kind, var payload):
      payload["type"] = .string(kind)
      try payload.encode(to: encoder)
    }
  }
}

public enum SignalboxProcessClientRequest: Encodable, Equatable, Sendable {
  case createSession(commandID: SignalboxCommandID, initialModelSelection: SignalboxModelSelection)
  case listSessions
  case submitInput(
    commandID: SignalboxCommandID,
    sessionID: SignalboxCanonicalUUID,
    content: String,
    expectedDefaultsVersion: SignalboxCanonicalUInt64
  )
  case readTranscript(sessionID: SignalboxCanonicalUUID)
  case followSession(sessionID: SignalboxCanonicalUUID)
  case listSessionMetadata(
    requiredTags: [String],
    titleContains: String?,
    includeArchived: Bool,
    pageSize: SignalboxCanonicalUInt64,
    afterSessionID: SignalboxCanonicalUUID?
  )
  case readSessionMetadata(sessionID: SignalboxCanonicalUUID)
  case replaceSessionMetadata(
    commandID: SignalboxCommandID,
    sessionID: SignalboxCanonicalUUID,
    metadata: SignalboxProcessSessionMetadata
  )
  case importConversation(format: SignalboxConversationImportFormat, source: Data)

  public func encode(to encoder: Encoder) throws {
    var container = encoder.container(keyedBy: SignalboxDynamicCodingKey.self)
    switch self {
    case .createSession(let commandID, let selection):
      try container.encode("create_session", forKey: "type")
      try container.encode(commandID, forKey: "command_id")
      try container.encode(selection, forKey: "initial_model_selection")
    case .listSessions:
      try container.encode("list_sessions", forKey: "type")
    case .submitInput(let commandID, let sessionID, let content, let expectedVersion):
      try container.encode("submit_input", forKey: "type")
      try container.encode(commandID, forKey: "command_id")
      try container.encode(sessionID, forKey: "session_id")
      try container.encode(content, forKey: "content")
      try container.encode(expectedVersion, forKey: "expected_defaults_version")
    case .readTranscript(let sessionID):
      try container.encode("read_transcript", forKey: "type")
      try container.encode(sessionID, forKey: "session_id")
    case .followSession(let sessionID):
      try container.encode("follow_session", forKey: "type")
      try container.encode(sessionID, forKey: "session_id")
    case .listSessionMetadata(let tags, let title, let archived, let pageSize, let after):
      try container.encode("list_session_metadata", forKey: "type")
      try container.encode(tags, forKey: "required_tags")
      try container.encode(title, forKey: "title_contains")
      try container.encode(archived, forKey: "include_archived")
      try container.encode(pageSize, forKey: "page_size")
      try container.encode(after, forKey: "after_session_id")
    case .readSessionMetadata(let sessionID):
      try container.encode("read_session_metadata", forKey: "type")
      try container.encode(sessionID, forKey: "session_id")
    case .replaceSessionMetadata(let commandID, let sessionID, let metadata):
      try container.encode("replace_session_metadata", forKey: "type")
      try container.encode(commandID, forKey: "command_id")
      try container.encode(sessionID, forKey: "session_id")
      try container.encode(metadata, forKey: "metadata")
    case .importConversation(let format, let source):
      try container.encode("import_conversation", forKey: "type")
      try container.encode(format, forKey: "format")
      try container.encode(source.base64EncodedString(), forKey: "source")
    }
  }
}

public enum SignalboxConversationImportFormat: String, Codable, Equatable, Sendable {
  case claudeCodeSessionJSONLV2 = "claude_code_session_jsonl_v2"
  case codexRolloutJSONLV1 = "codex_rollout_jsonl_v1"
}

public struct SignalboxProcessClientFrame: Encodable, Equatable, Sendable {
  public let version: SignalboxProcessProtocolVersion
  public let requestID: SignalboxRequestID
  public let request: SignalboxProcessClientRequest

  public init(
    version: SignalboxProcessProtocolVersion = SignalboxProcessProtocol.currentVersion,
    requestID: SignalboxRequestID,
    request: SignalboxProcessClientRequest
  ) {
    self.version = version
    self.requestID = requestID
    self.request = request
  }

  private enum CodingKeys: String, CodingKey {
    case version
    case requestID = "request_id"
    case request
  }
}

public struct SignalboxProcessServerFrame: Decodable, Equatable, Sendable {
  public let version: SignalboxProcessProtocolVersion
  public let requestID: SignalboxCanonicalUInt64
  public let message: SignalboxProcessServerMessage

  private enum CodingKeys: String, CodingKey {
    case version
    case requestID = "request_id"
    case message
  }
}

public enum SignalboxProcessServerMessage: Decodable, Equatable, Sendable {
  case sessionCreated(sessionID: SignalboxCanonicalUUID)
  case inputSubmitted(SignalboxInputSubmitted)
  case sessionsStart
  case sessionSummary(SignalboxProcessSessionSummary)
  case sessionsEnd(sessionCount: SignalboxCanonicalUInt64)
  case sessionMetadataPageStart
  case sessionMetadataSummary(SignalboxProcessSessionMetadataSummary)
  case sessionMetadataPageEnd(SignalboxProcessSessionMetadataPageEnd)
  case sessionMetadata(SignalboxProcessSessionMetadataRead)
  case sessionMetadataReplaced(SignalboxProcessSessionMetadataRead)
  case conversationImportInserted(importedConversationID: SignalboxCanonicalUUID)
  case conversationImportAlreadyImported(importedConversationID: SignalboxCanonicalUUID)
  case transcriptSnapshotStart(SignalboxTranscriptSnapshotBoundary)
  case transcriptTurn(SignalboxTranscriptTurn)
  case transcriptEntry(SignalboxTranscriptEntryMessage)
  case transcriptTextEntry(SignalboxTranscriptTextEntryMessage)
  case transcriptContent(SignalboxTranscriptContent)
  case transcriptSnapshotEnd(SignalboxTranscriptSnapshotEnd)
  case sessionEvent(SignalboxFollowedSessionEvent)
  case protocolError(SignalboxProcessError)
  case unknown(
    kind: String,
    payload: [String: SignalboxJSONValue],
    decodingDiagnostic: SignalboxDecodingDiagnostic?
  )

  public init(from decoder: Decoder) throws {
    let tagged = try SignalboxTaggedPayload(from: decoder)
    do {
      switch tagged.kind {
      case "session_created":
        self = .sessionCreated(sessionID: try decoder.decode("session_id"))
      case "input_submitted":
        self = .inputSubmitted(try SignalboxInputSubmitted(from: decoder))
      case "sessions_start":
        self = .sessionsStart
      case "session_summary":
        self = .sessionSummary(try SignalboxProcessSessionSummary(from: decoder))
      case "sessions_end":
        self = .sessionsEnd(sessionCount: try decoder.decode("session_count"))
      case "session_metadata_page_start":
        self = .sessionMetadataPageStart
      case "session_metadata_summary":
        self = .sessionMetadataSummary(try SignalboxProcessSessionMetadataSummary(from: decoder))
      case "session_metadata_page_end":
        self = .sessionMetadataPageEnd(try SignalboxProcessSessionMetadataPageEnd(from: decoder))
      case "session_metadata":
        self = .sessionMetadata(try SignalboxProcessSessionMetadataRead(from: decoder))
      case "session_metadata_replaced":
        self = .sessionMetadataReplaced(try SignalboxProcessSessionMetadataRead(from: decoder))
      case "conversation_import_inserted":
        self = .conversationImportInserted(
          importedConversationID: try decoder.decode("imported_conversation_id"))
      case "conversation_import_already_imported":
        self = .conversationImportAlreadyImported(
          importedConversationID: try decoder.decode("imported_conversation_id"))
      case "transcript_snapshot_start":
        try tagged.rejectUnadmittedFields(
          ["type", "session_id", "cursor"],
          decoder: decoder
        )
        self = .transcriptSnapshotStart(try SignalboxTranscriptSnapshotBoundary(from: decoder))
      case "transcript_turn":
        try tagged.rejectUnadmittedFields(
          ["type", "turn_id", "acceptance_position", "state"],
          decoder: decoder
        )
        self = .transcriptTurn(try SignalboxTranscriptTurn(from: decoder))
      case "transcript_entry":
        try tagged.rejectUnadmittedFields(
          ["type", "entry_index", "source_session_id", "entry_id", "entry"],
          decoder: decoder
        )
        self = .transcriptEntry(try SignalboxTranscriptEntryMessage(from: decoder))
      case "transcript_text_entry":
        try tagged.rejectUnadmittedFields(
          ["type", "entry_index", "source_session_id", "entry_id", "entry"],
          decoder: decoder
        )
        self = .transcriptTextEntry(try SignalboxTranscriptTextEntryMessage(from: decoder))
      case "transcript_content":
        try tagged.rejectUnadmittedFields(
          ["type", "entry_index", "fragment_index", "final_fragment", "content_fragment"],
          decoder: decoder
        )
        self = .transcriptContent(try SignalboxTranscriptContent(from: decoder))
      case "transcript_snapshot_end":
        try tagged.rejectUnadmittedFields(
          ["type", "session_id", "cursor", "turn_count", "entry_count"],
          decoder: decoder
        )
        self = .transcriptSnapshotEnd(try SignalboxTranscriptSnapshotEnd(from: decoder))
      case "session_event":
        try tagged.rejectUnadmittedFields(
          ["type", "cursor", "session_id", "event"],
          decoder: decoder
        )
        self = .sessionEvent(try SignalboxFollowedSessionEvent(from: decoder))
      case "error":
        self = .protocolError(try SignalboxProcessError(from: decoder))
      default:
        self = .unknown(kind: tagged.kind, payload: tagged.payload, decodingDiagnostic: nil)
      }
    } catch {
      self = .unknown(
        kind: tagged.kind,
        payload: tagged.payload,
        decodingDiagnostic: SignalboxDecodingDiagnostic(error: error)
      )
    }
  }
}

public struct SignalboxInputSubmitted: Decodable, Equatable, Sendable {
  public let sessionID: SignalboxCanonicalUUID
  public let acceptedInputID: SignalboxCanonicalUUID
  public let acceptancePosition: SignalboxCanonicalUInt64
  public let turnID: SignalboxCanonicalUUID

  private enum CodingKeys: String, CodingKey {
    case sessionID = "session_id"
    case acceptedInputID = "accepted_input_id"
    case acceptancePosition = "acceptance_position"
    case turnID = "turn_id"
  }
}

public struct SignalboxProcessSessionSummary: Decodable, Equatable, Sendable {
  public let sessionID: SignalboxCanonicalUUID
  public let defaultsVersion: SignalboxCanonicalUInt64
  public let modelSelection: SignalboxModelSelection

  private enum CodingKeys: String, CodingKey {
    case sessionID = "session_id"
    case defaultsVersion = "defaults_version"
    case modelSelection = "model_selection"
  }
}

public struct SignalboxProcessSessionMetadataSummary: Decodable, Equatable, Sendable {
  public let sessionID: SignalboxCanonicalUUID
  public let defaultsVersion: SignalboxCanonicalUInt64
  public let modelSelection: SignalboxModelSelection
  public let dangerousToolAutoApproval: Bool
  public let title: String?
  public let tags: [String]
  public let archived: Bool
  public let lastWriter: SignalboxMetadataLastWriter?

  private enum CodingKeys: String, CodingKey {
    case sessionID = "session_id"
    case defaultsVersion = "defaults_version"
    case modelSelection = "model_selection"
    case dangerousToolAutoApproval = "dangerous_tool_auto_approval"
    case title
    case tags
    case archived
    case lastWriter = "last_writer"
  }
}

public struct SignalboxProcessSessionMetadataPageEnd: Decodable, Equatable, Sendable {
  public let sessionCount: SignalboxCanonicalUInt64
  public let nextAfterSessionID: SignalboxCanonicalUUID?

  private enum CodingKeys: String, CodingKey {
    case sessionCount = "session_count"
    case nextAfterSessionID = "next_after_session_id"
  }
}

public struct SignalboxProcessSessionMetadataRead: Decodable, Equatable, Sendable {
  public let sessionID: SignalboxCanonicalUUID
  public let metadata: SignalboxProcessSessionMetadata
  public let lastWriter: SignalboxMetadataLastWriter?

  private enum CodingKeys: String, CodingKey {
    case sessionID = "session_id"
    case metadata
    case lastWriter = "last_writer"
  }
}

public struct SignalboxTranscriptSnapshotBoundary: Decodable, Equatable, Sendable {
  public let sessionID: SignalboxCanonicalUUID
  public let cursor: SignalboxCanonicalUInt64

  private enum CodingKeys: String, CodingKey {
    case sessionID = "session_id"
    case cursor
  }
}

public struct SignalboxTranscriptSnapshotEnd: Decodable, Equatable, Sendable {
  public let sessionID: SignalboxCanonicalUUID
  public let cursor: SignalboxCanonicalUInt64
  public let turnCount: SignalboxCanonicalUInt64
  public let entryCount: SignalboxCanonicalUInt64

  private enum CodingKeys: String, CodingKey {
    case sessionID = "session_id"
    case cursor
    case turnCount = "turn_count"
    case entryCount = "entry_count"
  }
}

public struct SignalboxTranscriptTurn: Decodable, Equatable, Sendable {
  public let turnID: SignalboxCanonicalUUID
  public let acceptancePosition: SignalboxCanonicalUInt64
  public let state: SignalboxTranscriptTurnState

  private enum CodingKeys: String, CodingKey {
    case turnID = "turn_id"
    case acceptancePosition = "acceptance_position"
    case state
  }
}

public enum SignalboxTranscriptTurnState: Decodable, Equatable, Sendable {
  case queued(acceptedInputID: SignalboxCanonicalUUID, content: String)
  case activeRunning(
    currentAttemptID: SignalboxCanonicalUUID, currentModelCall: SignalboxCurrentModelCall?)
  case activeAwaitingModelCallRecovery(
    endedAttemptID: SignalboxCanonicalUUID, recoveryModelCallID: SignalboxCanonicalUUID)
  case activeAwaitingToolApproval(toolRequestID: SignalboxCanonicalUUID)
  case activeAwaitingToolRecovery(
    endedAttemptID: SignalboxCanonicalUUID, recoveryToolAttemptID: SignalboxCanonicalUUID)
  case failed(
    terminalFrontierID: SignalboxCanonicalUUID,
    terminalAttemptID: SignalboxCanonicalUUID?,
    terminalModelCall: SignalboxFailedTerminalModelCall?
  )
  case completed(
    terminalFrontierID: SignalboxCanonicalUUID,
    terminalAttemptID: SignalboxCanonicalUUID,
    terminalModelCallID: SignalboxCanonicalUUID
  )
  case refused(
    terminalFrontierID: SignalboxCanonicalUUID,
    terminalAttemptID: SignalboxCanonicalUUID,
    terminalModelCallID: SignalboxCanonicalUUID
  )
  case cancelled(
    terminalFrontierID: SignalboxCanonicalUUID,
    terminalAttemptID: SignalboxCanonicalUUID,
    terminalModelCallID: SignalboxCanonicalUUID?
  )
  case reconciliationRequired(
    terminalFrontierID: SignalboxCanonicalUUID,
    terminalAttemptID: SignalboxCanonicalUUID,
    terminalModelCallID: SignalboxCanonicalUUID
  )
  case toolReconciliationRequired(
    terminalFrontierID: SignalboxCanonicalUUID,
    terminalAttemptID: SignalboxCanonicalUUID,
    terminalToolAttemptID: SignalboxCanonicalUUID
  )
  case unknown(
    kind: String, payload: [String: SignalboxJSONValue],
    decodingDiagnostic: SignalboxDecodingDiagnostic?)

  public init(from decoder: Decoder) throws {
    let tagged = try SignalboxTaggedPayload(from: decoder)
    do {
      switch tagged.kind {
      case "queued":
        try tagged.rejectUnadmittedFields(
          ["type", "accepted_input_id", "content"],
          decoder: decoder
        )
        self = .queued(
          acceptedInputID: try decoder.decode("accepted_input_id"),
          content: try decoder.decode("content"))
      case "active_running":
        try tagged.rejectUnadmittedFields(
          ["type", "current_attempt_id", "current_model_call"],
          decoder: decoder
        )
        self = .activeRunning(
          currentAttemptID: try decoder.decode("current_attempt_id"),
          currentModelCall: try decoder.decodeIfPresent("current_model_call")
        )
      case "active_awaiting_model_call_recovery":
        try tagged.rejectUnadmittedFields(
          ["type", "ended_attempt_id", "recovery_model_call_id"],
          decoder: decoder
        )
        self = .activeAwaitingModelCallRecovery(
          endedAttemptID: try decoder.decode("ended_attempt_id"),
          recoveryModelCallID: try decoder.decode("recovery_model_call_id")
        )
      case "active_awaiting_tool_approval":
        try tagged.rejectUnadmittedFields(
          ["type", "tool_request_id"],
          decoder: decoder
        )
        self = .activeAwaitingToolApproval(toolRequestID: try decoder.decode("tool_request_id"))
      case "active_awaiting_tool_recovery":
        try tagged.rejectUnadmittedFields(
          ["type", "ended_attempt_id", "recovery_tool_attempt_id"],
          decoder: decoder
        )
        self = .activeAwaitingToolRecovery(
          endedAttemptID: try decoder.decode("ended_attempt_id"),
          recoveryToolAttemptID: try decoder.decode("recovery_tool_attempt_id")
        )
      case "failed":
        try tagged.rejectUnadmittedFields(
          ["type", "terminal_frontier_id", "terminal_attempt_id", "terminal_model_call"],
          decoder: decoder
        )
        self = .failed(
          terminalFrontierID: try decoder.decode("terminal_frontier_id"),
          terminalAttemptID: try decoder.decodeIfPresent("terminal_attempt_id"),
          terminalModelCall: try decoder.decodeIfPresent("terminal_model_call")
        )
      case "completed":
        try tagged.rejectUnadmittedFields(
          ["type", "terminal_frontier_id", "terminal_attempt_id", "terminal_model_call_id"],
          decoder: decoder
        )
        self = .completed(
          terminalFrontierID: try decoder.decode("terminal_frontier_id"),
          terminalAttemptID: try decoder.decode("terminal_attempt_id"),
          terminalModelCallID: try decoder.decode("terminal_model_call_id")
        )
      case "refused":
        try tagged.rejectUnadmittedFields(
          ["type", "terminal_frontier_id", "terminal_attempt_id", "terminal_model_call_id"],
          decoder: decoder
        )
        self = .refused(
          terminalFrontierID: try decoder.decode("terminal_frontier_id"),
          terminalAttemptID: try decoder.decode("terminal_attempt_id"),
          terminalModelCallID: try decoder.decode("terminal_model_call_id")
        )
      case "cancelled":
        try tagged.rejectUnadmittedFields(
          ["type", "terminal_frontier_id", "terminal_attempt_id", "terminal_model_call_id"],
          decoder: decoder
        )
        self = .cancelled(
          terminalFrontierID: try decoder.decode("terminal_frontier_id"),
          terminalAttemptID: try decoder.decode("terminal_attempt_id"),
          terminalModelCallID: try decoder.decodeIfPresent("terminal_model_call_id")
        )
      case "reconciliation_required":
        try tagged.rejectUnadmittedFields(
          ["type", "terminal_frontier_id", "terminal_attempt_id", "terminal_model_call_id"],
          decoder: decoder
        )
        self = .reconciliationRequired(
          terminalFrontierID: try decoder.decode("terminal_frontier_id"),
          terminalAttemptID: try decoder.decode("terminal_attempt_id"),
          terminalModelCallID: try decoder.decode("terminal_model_call_id")
        )
      case "tool_reconciliation_required":
        try tagged.rejectUnadmittedFields(
          ["type", "terminal_frontier_id", "terminal_attempt_id", "terminal_tool_attempt_id"],
          decoder: decoder
        )
        self = .toolReconciliationRequired(
          terminalFrontierID: try decoder.decode("terminal_frontier_id"),
          terminalAttemptID: try decoder.decode("terminal_attempt_id"),
          terminalToolAttemptID: try decoder.decode("terminal_tool_attempt_id")
        )
      default:
        self = .unknown(kind: tagged.kind, payload: tagged.payload, decodingDiagnostic: nil)
      }
    } catch {
      self = .unknown(
        kind: tagged.kind,
        payload: tagged.payload,
        decodingDiagnostic: SignalboxDecodingDiagnostic(error: error)
      )
    }
  }
}

public struct SignalboxFailedTerminalModelCall: Decodable, Equatable, Sendable {
  public let modelCallID: SignalboxCanonicalUUID
  public let disposition: SignalboxFailedModelCallDisposition

  private enum CodingKeys: String, CodingKey {
    case modelCallID = "model_call_id"
    case disposition
  }
}

public enum SignalboxFailedModelCallDisposition: String, Decodable, Equatable, Sendable {
  case knownFailed = "known_failed"
  case cancelled
}

public struct SignalboxCurrentModelCall: Decodable, Equatable, Sendable {
  public let modelCallID: SignalboxCanonicalUUID
  public let state: SignalboxCurrentModelCallState

  private enum CodingKeys: String, CodingKey {
    case modelCallID = "model_call_id"
    case state
  }
}

public enum SignalboxCurrentModelCallState: Decodable, Equatable, Sendable {
  case prepared
  case inFlight
  case cancellationRequested
  case unknown(kind: String, payload: [String: SignalboxJSONValue])

  public init(from decoder: Decoder) throws {
    let tagged = try SignalboxTaggedPayload(from: decoder)
    switch tagged.kind {
    case "prepared":
      try tagged.rejectUnadmittedFields(["type"], decoder: decoder)
      self = .prepared
    case "in_flight":
      try tagged.rejectUnadmittedFields(["type"], decoder: decoder)
      self = .inFlight
    case "cancellation_requested":
      try tagged.rejectUnadmittedFields(["type"], decoder: decoder)
      self = .cancellationRequested
    default:
      self = .unknown(kind: tagged.kind, payload: tagged.payload)
    }
  }
}

public struct SignalboxTranscriptEntryMessage: Decodable, Equatable, Sendable {
  public let entryIndex: SignalboxCanonicalUInt64
  public let sourceSessionID: SignalboxCanonicalUUID
  public let entryID: SignalboxCanonicalUUID
  public let entry: SignalboxTranscriptEntry

  private enum CodingKeys: String, CodingKey {
    case entryIndex = "entry_index"
    case sourceSessionID = "source_session_id"
    case entryID = "entry_id"
    case entry
  }
}

public enum SignalboxTranscriptEntry: Decodable, Equatable, Sendable {
  case assistantToolUse(
    turnID: SignalboxCanonicalUUID, modelCallID: SignalboxCanonicalUUID,
    toolRequestID: SignalboxCanonicalUUID, toolName: String, arguments: String)
  case toolExecutionResult(
    toolRequestID: SignalboxCanonicalUUID, toolAttemptID: SignalboxCanonicalUUID, content: String)
  case toolDenied(toolRequestID: SignalboxCanonicalUUID, content: String)
  case toolClosed(toolRequestID: SignalboxCanonicalUUID, content: String)
  case turnCompleted(turnID: SignalboxCanonicalUUID)
  case turnFailed(turnID: SignalboxCanonicalUUID)
  case turnCancelled(turnID: SignalboxCanonicalUUID)
  case imported(
    importedConversationID: SignalboxCanonicalUUID,
    importedEntryID: SignalboxCanonicalUUID,
    sourceSpeaker: SignalboxImportedSourceSpeaker,
    contentKind: SignalboxImportedContentKind
  )
  case unknown(
    kind: String, payload: [String: SignalboxJSONValue],
    decodingDiagnostic: SignalboxDecodingDiagnostic?)

  public init(from decoder: Decoder) throws {
    let tagged = try SignalboxTaggedPayload(from: decoder)
    do {
      switch tagged.kind {
      case "assistant_tool_use":
        try tagged.rejectUnadmittedFields(
          ["type", "turn_id", "model_call_id", "tool_request_id", "tool_name", "arguments"],
          decoder: decoder
        )
        self = .assistantToolUse(
          turnID: try decoder.decode("turn_id"),
          modelCallID: try decoder.decode("model_call_id"),
          toolRequestID: try decoder.decode("tool_request_id"),
          toolName: try decoder.decode("tool_name"),
          arguments: try decoder.decode("arguments")
        )
      case "tool_execution_result":
        try tagged.rejectUnadmittedFields(
          ["type", "tool_request_id", "tool_attempt_id", "content"],
          decoder: decoder
        )
        self = .toolExecutionResult(
          toolRequestID: try decoder.decode("tool_request_id"),
          toolAttemptID: try decoder.decode("tool_attempt_id"),
          content: try decoder.decode("content")
        )
      case "tool_denied":
        try tagged.rejectUnadmittedFields(
          ["type", "tool_request_id", "content"],
          decoder: decoder
        )
        self = .toolDenied(
          toolRequestID: try decoder.decode("tool_request_id"),
          content: try decoder.decode("content"))
      case "tool_closed":
        try tagged.rejectUnadmittedFields(
          ["type", "tool_request_id", "content"],
          decoder: decoder
        )
        self = .toolClosed(
          toolRequestID: try decoder.decode("tool_request_id"),
          content: try decoder.decode("content"))
      case "turn_completed":
        try tagged.rejectUnadmittedFields(["type", "turn_id"], decoder: decoder)
        self = .turnCompleted(turnID: try decoder.decode("turn_id"))
      case "turn_failed":
        try tagged.rejectUnadmittedFields(["type", "turn_id"], decoder: decoder)
        self = .turnFailed(turnID: try decoder.decode("turn_id"))
      case "turn_cancelled":
        try tagged.rejectUnadmittedFields(["type", "turn_id"], decoder: decoder)
        self = .turnCancelled(turnID: try decoder.decode("turn_id"))
      case "imported":
        try tagged.rejectUnadmittedFields(
          [
            "type", "imported_conversation_id", "imported_entry_id", "source_speaker",
            "content_kind",
          ],
          decoder: decoder
        )
        self = .imported(
          importedConversationID: try decoder.decode("imported_conversation_id"),
          importedEntryID: try decoder.decode("imported_entry_id"),
          sourceSpeaker: try decoder.decode("source_speaker"),
          contentKind: try decoder.decode("content_kind")
        )
      default:
        self = .unknown(kind: tagged.kind, payload: tagged.payload, decodingDiagnostic: nil)
      }
    } catch {
      self = .unknown(
        kind: tagged.kind,
        payload: tagged.payload,
        decodingDiagnostic: SignalboxDecodingDiagnostic(error: error)
      )
    }
  }
}

public enum SignalboxImportedContentKind: String, Decodable, Equatable, Sendable {
  case sourceEvent = "source_event"
  case sourceMessageBlock = "source_message_block"
  case text
  case toolCall = "tool_call"
  case toolResult = "tool_result"
  case thinking
  case redactedThinking = "redacted_thinking"
  case document
  case messageContentAbsent = "message_content_absent"
}

public enum SignalboxImportedSourceSpeaker: Decodable, Equatable, Sendable {
  case notAttested
  case attestedAbsent
  case attested(speaker: SignalboxImportedSpeaker)
  case unknown(kind: String, payload: [String: SignalboxJSONValue])

  public init(from decoder: Decoder) throws {
    let tagged = try SignalboxTaggedPayload(from: decoder)
    switch tagged.kind {
    case "not_attested":
      try tagged.rejectUnadmittedFields(["type"], decoder: decoder)
      self = .notAttested
    case "attested_absent":
      try tagged.rejectUnadmittedFields(["type"], decoder: decoder)
      self = .attestedAbsent
    case "attested":
      try tagged.rejectUnadmittedFields(["type", "speaker"], decoder: decoder)
      self = .attested(speaker: try decoder.decode("speaker"))
    default:
      self = .unknown(kind: tagged.kind, payload: tagged.payload)
    }
  }
}

public enum SignalboxImportedSpeaker: String, Decodable, Equatable, Sendable {
  case user
  case assistant
}

public struct SignalboxTranscriptTextEntryMessage: Decodable, Equatable, Sendable {
  public let entryIndex: SignalboxCanonicalUInt64
  public let sourceSessionID: SignalboxCanonicalUUID
  public let entryID: SignalboxCanonicalUUID
  public let entry: SignalboxTranscriptTextEntry

  private enum CodingKeys: String, CodingKey {
    case entryIndex = "entry_index"
    case sourceSessionID = "source_session_id"
    case entryID = "entry_id"
    case entry
  }
}

public enum SignalboxTranscriptTextEntry: Decodable, Equatable, Sendable {
  case user(acceptedInputID: SignalboxCanonicalUUID, turnID: SignalboxCanonicalUUID)
  case assistant(turnID: SignalboxCanonicalUUID, modelCallID: SignalboxCanonicalUUID)
  case imported(
    importedConversationID: SignalboxCanonicalUUID,
    importedEntryID: SignalboxCanonicalUUID,
    sourceSpeaker: SignalboxImportedSourceSpeaker
  )
  case unknown(
    kind: String, payload: [String: SignalboxJSONValue],
    decodingDiagnostic: SignalboxDecodingDiagnostic?)

  public init(from decoder: Decoder) throws {
    let tagged = try SignalboxTaggedPayload(from: decoder)
    do {
      switch tagged.kind {
      case "user":
        try tagged.rejectUnadmittedFields(
          ["type", "accepted_input_id", "turn_id"],
          decoder: decoder
        )
        self = .user(
          acceptedInputID: try decoder.decode("accepted_input_id"),
          turnID: try decoder.decode("turn_id"))
      case "assistant":
        try tagged.rejectUnadmittedFields(
          ["type", "turn_id", "model_call_id"],
          decoder: decoder
        )
        self = .assistant(
          turnID: try decoder.decode("turn_id"), modelCallID: try decoder.decode("model_call_id"))
      case "imported":
        try tagged.rejectUnadmittedFields(
          ["type", "imported_conversation_id", "imported_entry_id", "source_speaker"],
          decoder: decoder
        )
        self = .imported(
          importedConversationID: try decoder.decode("imported_conversation_id"),
          importedEntryID: try decoder.decode("imported_entry_id"),
          sourceSpeaker: try decoder.decode("source_speaker")
        )
      default:
        self = .unknown(kind: tagged.kind, payload: tagged.payload, decodingDiagnostic: nil)
      }
    } catch {
      self = .unknown(
        kind: tagged.kind,
        payload: tagged.payload,
        decodingDiagnostic: SignalboxDecodingDiagnostic(error: error)
      )
    }
  }
}

public struct SignalboxTranscriptContent: Decodable, Equatable, Sendable {
  public let entryIndex: SignalboxCanonicalUInt64
  public let fragmentIndex: SignalboxCanonicalUInt64
  public let finalFragment: Bool
  public let contentFragment: String

  private enum CodingKeys: String, CodingKey {
    case entryIndex = "entry_index"
    case fragmentIndex = "fragment_index"
    case finalFragment = "final_fragment"
    case contentFragment = "content_fragment"
  }
}

public struct SignalboxFollowedSessionEvent: Decodable, Equatable, Sendable {
  public let cursor: SignalboxCanonicalUInt64
  public let sessionID: SignalboxCanonicalUUID
  public let event: SignalboxProcessSessionEvent

  private enum CodingKeys: String, CodingKey {
    case cursor
    case sessionID = "session_id"
    case event
  }
}

public enum SignalboxProcessSessionEvent: Decodable, Equatable, Sendable {
  case sessionCreated
  case inputAccepted(
    acceptedInputID: SignalboxCanonicalUUID, turnID: SignalboxCanonicalUUID,
    acceptancePosition: SignalboxCanonicalUInt64, content: String)
  case turnActivated(turnID: SignalboxCanonicalUUID, currentAttemptID: SignalboxCanonicalUUID)
  case modelCallTransition(
    turnID: SignalboxCanonicalUUID, modelCallID: SignalboxCanonicalUUID,
    state: SignalboxModelCallState)
  case toolBatchTransition(
    turnID: SignalboxCanonicalUUID, modelCallID: SignalboxCanonicalUUID,
    state: SignalboxToolBatchState)
  case turnCompleted(
    turnID: SignalboxCanonicalUUID, modelCallID: SignalboxCanonicalUUID,
    completionEntryID: SignalboxCanonicalUUID, terminalFrontierID: SignalboxCanonicalUUID)
  case turnFailed(
    turnID: SignalboxCanonicalUUID, failureEntryID: SignalboxCanonicalUUID,
    terminalFrontierID: SignalboxCanonicalUUID)
  case turnRefused(
    turnID: SignalboxCanonicalUUID, modelCallID: SignalboxCanonicalUUID,
    terminalFrontierID: SignalboxCanonicalUUID)
  case turnCancelled(
    turnID: SignalboxCanonicalUUID, cancellationEntryID: SignalboxCanonicalUUID,
    terminalFrontierID: SignalboxCanonicalUUID)
  case turnReconciliationRequired(
    turnID: SignalboxCanonicalUUID, modelCallID: SignalboxCanonicalUUID,
    terminalFrontierID: SignalboxCanonicalUUID)
  case turnToolReconciliationRequired(
    turnID: SignalboxCanonicalUUID, toolAttemptID: SignalboxCanonicalUUID,
    terminalFrontierID: SignalboxCanonicalUUID)
  case unknown(
    kind: String, payload: [String: SignalboxJSONValue],
    decodingDiagnostic: SignalboxDecodingDiagnostic?)

  public init(from decoder: Decoder) throws {
    let tagged = try SignalboxTaggedPayload(from: decoder)
    do {
      switch tagged.kind {
      case "session_created":
        try tagged.rejectUnadmittedFields(["type"], decoder: decoder)
        self = .sessionCreated
      case "input_accepted":
        try tagged.rejectUnadmittedFields(
          ["type", "accepted_input_id", "turn_id", "acceptance_position", "content"],
          decoder: decoder
        )
        self = .inputAccepted(
          acceptedInputID: try decoder.decode("accepted_input_id"),
          turnID: try decoder.decode("turn_id"),
          acceptancePosition: try decoder.decode("acceptance_position"),
          content: try decoder.decode("content")
        )
      case "turn_activated":
        try tagged.rejectUnadmittedFields(
          ["type", "turn_id", "current_attempt_id"],
          decoder: decoder
        )
        self = .turnActivated(
          turnID: try decoder.decode("turn_id"),
          currentAttemptID: try decoder.decode("current_attempt_id"))
      case "model_call_transition":
        try tagged.rejectUnadmittedFields(
          ["type", "turn_id", "model_call_id", "state"],
          decoder: decoder
        )
        self = .modelCallTransition(
          turnID: try decoder.decode("turn_id"),
          modelCallID: try decoder.decode("model_call_id"),
          state: try decoder.decode("state")
        )
      case "tool_batch_transition":
        try tagged.rejectUnadmittedFields(
          ["type", "turn_id", "model_call_id", "state"],
          decoder: decoder
        )
        self = .toolBatchTransition(
          turnID: try decoder.decode("turn_id"),
          modelCallID: try decoder.decode("model_call_id"),
          state: try decoder.decode("state")
        )
      case "turn_completed":
        try tagged.rejectUnadmittedFields(
          ["type", "turn_id", "model_call_id", "completion_entry_id", "terminal_frontier_id"],
          decoder: decoder
        )
        self = .turnCompleted(
          turnID: try decoder.decode("turn_id"),
          modelCallID: try decoder.decode("model_call_id"),
          completionEntryID: try decoder.decode("completion_entry_id"),
          terminalFrontierID: try decoder.decode("terminal_frontier_id")
        )
      case "turn_failed":
        try tagged.rejectUnadmittedFields(
          ["type", "turn_id", "failure_entry_id", "terminal_frontier_id"],
          decoder: decoder
        )
        self = .turnFailed(
          turnID: try decoder.decode("turn_id"),
          failureEntryID: try decoder.decode("failure_entry_id"),
          terminalFrontierID: try decoder.decode("terminal_frontier_id")
        )
      case "turn_refused":
        try tagged.rejectUnadmittedFields(
          ["type", "turn_id", "model_call_id", "terminal_frontier_id"],
          decoder: decoder
        )
        self = .turnRefused(
          turnID: try decoder.decode("turn_id"),
          modelCallID: try decoder.decode("model_call_id"),
          terminalFrontierID: try decoder.decode("terminal_frontier_id")
        )
      case "turn_cancelled":
        try tagged.rejectUnadmittedFields(
          ["type", "turn_id", "cancellation_entry_id", "terminal_frontier_id"],
          decoder: decoder
        )
        self = .turnCancelled(
          turnID: try decoder.decode("turn_id"),
          cancellationEntryID: try decoder.decode("cancellation_entry_id"),
          terminalFrontierID: try decoder.decode("terminal_frontier_id")
        )
      case "turn_reconciliation_required":
        try tagged.rejectUnadmittedFields(
          ["type", "turn_id", "model_call_id", "terminal_frontier_id"],
          decoder: decoder
        )
        self = .turnReconciliationRequired(
          turnID: try decoder.decode("turn_id"),
          modelCallID: try decoder.decode("model_call_id"),
          terminalFrontierID: try decoder.decode("terminal_frontier_id")
        )
      case "turn_tool_reconciliation_required":
        try tagged.rejectUnadmittedFields(
          ["type", "turn_id", "tool_attempt_id", "terminal_frontier_id"],
          decoder: decoder
        )
        self = .turnToolReconciliationRequired(
          turnID: try decoder.decode("turn_id"),
          toolAttemptID: try decoder.decode("tool_attempt_id"),
          terminalFrontierID: try decoder.decode("terminal_frontier_id")
        )
      default:
        self = .unknown(kind: tagged.kind, payload: tagged.payload, decodingDiagnostic: nil)
      }
    } catch {
      self = .unknown(
        kind: tagged.kind,
        payload: tagged.payload,
        decodingDiagnostic: SignalboxDecodingDiagnostic(error: error)
      )
    }
  }
}

public enum SignalboxModelCallState: Decodable, Equatable, Sendable {
  case prepared
  case inFlight
  case cancellationRequested
  case terminal(disposition: SignalboxModelCallDisposition)
  case unknown(kind: String, payload: [String: SignalboxJSONValue])

  public init(from decoder: Decoder) throws {
    let tagged = try SignalboxTaggedPayload(from: decoder)
    switch tagged.kind {
    case "prepared":
      try tagged.rejectUnadmittedFields(["type"], decoder: decoder)
      self = .prepared
    case "in_flight":
      try tagged.rejectUnadmittedFields(["type"], decoder: decoder)
      self = .inFlight
    case "cancellation_requested":
      try tagged.rejectUnadmittedFields(["type"], decoder: decoder)
      self = .cancellationRequested
    case "terminal":
      try tagged.rejectUnadmittedFields(["type", "disposition"], decoder: decoder)
      self = .terminal(disposition: try decoder.decode("disposition"))
    default:
      self = .unknown(kind: tagged.kind, payload: tagged.payload)
    }
  }
}

public enum SignalboxModelCallDisposition: String, Decodable, Equatable, Sendable {
  case completed
  case knownFailed = "known_failed"
  case refused
  case cancelled
  case ambiguous
}

public enum SignalboxToolBatchState: Decodable, Equatable, Sendable {
  case proposed(frontierID: SignalboxCanonicalUUID)
  case resultsProjected(frontierID: SignalboxCanonicalUUID)
  case recoveryRequired(toolAttemptID: SignalboxCanonicalUUID)
  case unknown(kind: String, payload: [String: SignalboxJSONValue])

  public init(from decoder: Decoder) throws {
    let tagged = try SignalboxTaggedPayload(from: decoder)
    switch tagged.kind {
    case "proposed":
      try tagged.rejectUnadmittedFields(["type", "frontier_id"], decoder: decoder)
      self = .proposed(frontierID: try decoder.decode("frontier_id"))
    case "results_projected":
      try tagged.rejectUnadmittedFields(["type", "frontier_id"], decoder: decoder)
      self = .resultsProjected(frontierID: try decoder.decode("frontier_id"))
    case "recovery_required":
      try tagged.rejectUnadmittedFields(["type", "tool_attempt_id"], decoder: decoder)
      self = .recoveryRequired(toolAttemptID: try decoder.decode("tool_attempt_id"))
    default:
      self = .unknown(kind: tagged.kind, payload: tagged.payload)
    }
  }
}

public enum SignalboxProcessErrorCode: String, Decodable, Equatable, Sendable {
  case malformedFrame = "malformed_frame"
  case unsupportedVersion = "unsupported_version"
  case invalidRequest = "invalid_request"
  case notFound = "not_found"
  case conflictingReuse = "conflicting_reuse"
  case rejected
  case resyncRequired = "resync_required"
  case unavailable
  case commitAmbiguous = "commit_ambiguous"
  case `internal`
}

public struct SignalboxProcessError: Decodable, Equatable, Sendable {
  public let code: SignalboxProcessErrorCode
  public let message: String
  public let detail: SignalboxRejectionDetail?
}

public enum SignalboxRejectionDetail: Decodable, Equatable, Sendable {
  case sessionNotFound(sessionID: SignalboxCanonicalUUID)
  case activeTurnPresent(sessionID: SignalboxCanonicalUUID, activeTurnID: SignalboxCanonicalUUID)
  case defaultsVersionMismatch(
    sessionID: SignalboxCanonicalUUID, expected: SignalboxCanonicalUInt64,
    current: SignalboxCanonicalUInt64)
  case unknownModelAlias(sessionID: SignalboxCanonicalUUID, aliasID: SignalboxCanonicalUUID)
  case acceptancePositionExhausted(
    sessionID: SignalboxCanonicalUUID, last: SignalboxCanonicalUInt64)
  case unknown(kind: String, payload: [String: SignalboxJSONValue])

  public init(from decoder: Decoder) throws {
    let tagged = try SignalboxTaggedPayload(from: decoder)
    switch tagged.kind {
    case "session_not_found":
      self = .sessionNotFound(sessionID: try decoder.decode("session_id"))
    case "active_turn_present":
      self = .activeTurnPresent(
        sessionID: try decoder.decode("session_id"),
        activeTurnID: try decoder.decode("active_turn_id"))
    case "defaults_version_mismatch":
      self = .defaultsVersionMismatch(
        sessionID: try decoder.decode("session_id"),
        expected: try decoder.decode("expected"),
        current: try decoder.decode("current")
      )
    case "unknown_model_alias":
      self = .unknownModelAlias(
        sessionID: try decoder.decode("session_id"), aliasID: try decoder.decode("alias_id"))
    case "acceptance_position_exhausted":
      self = .acceptancePositionExhausted(
        sessionID: try decoder.decode("session_id"), last: try decoder.decode("last"))
    default:
      self = .unknown(kind: tagged.kind, payload: tagged.payload)
    }
  }
}

private struct SignalboxTaggedPayload: Decodable {
  let kind: String
  let payload: [String: SignalboxJSONValue]

  init(from decoder: Decoder) throws {
    payload = try decoder.singleValueContainer().decode([String: SignalboxJSONValue].self)
    guard case .string(let kind) = payload["type"] else {
      throw DecodingError.keyNotFound(
        SignalboxDynamicCodingKey("type"),
        .init(
          codingPath: decoder.codingPath, debugDescription: "Tagged object is missing its type.")
      )
    }
    self.kind = kind
  }

  func rejectUnadmittedFields(
    _ admittedFields: Set<String>,
    decoder: Decoder
  ) throws {
    guard
      let field = payload.keys.sorted().first(where: { !admittedFields.contains($0) })
    else {
      return
    }
    throw DecodingError.dataCorrupted(
      .init(
        codingPath: decoder.codingPath + [SignalboxDynamicCodingKey(field)],
        debugDescription: "Tagged object contains an unadmitted field."
      )
    )
  }
}

private struct SignalboxDynamicCodingKey: CodingKey {
  let stringValue: String
  let intValue: Int? = nil

  init(_ stringValue: String) {
    self.stringValue = stringValue
  }

  init?(stringValue: String) {
    self.stringValue = stringValue
  }

  init?(intValue: Int) {
    nil
  }
}

extension KeyedEncodingContainer where Key == SignalboxDynamicCodingKey {
  fileprivate mutating func encode<Value: Encodable>(_ value: Value, forKey key: String) throws {
    try encode(value, forKey: SignalboxDynamicCodingKey(key))
  }
}

extension Decoder {
  fileprivate func decode<Value: Decodable>(_ key: String) throws -> Value {
    try container(keyedBy: SignalboxDynamicCodingKey.self).decode(
      Value.self,
      forKey: SignalboxDynamicCodingKey(key)
    )
  }

  fileprivate func decodeIfPresent<Value: Decodable>(_ key: String) throws -> Value? {
    try container(keyedBy: SignalboxDynamicCodingKey.self).decodeIfPresent(
      Value.self,
      forKey: SignalboxDynamicCodingKey(key)
    )
  }
}
