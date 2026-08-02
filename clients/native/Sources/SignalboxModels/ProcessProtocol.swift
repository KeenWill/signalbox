import Foundation

public enum SignalboxProcessProtocol {
  public static let currentVersion = SignalboxProcessProtocolVersion.one
  public static let maximumFrameBytes = 8 * 1024 * 1024
  public static let maximumContentFragmentUTF8Bytes = 1024 * 1024
  // docs/spec/process-protocol.md owns the metadata and conversation-list bounds.
  public static let maximumMetadataTags = 256
  public static let maximumMetadataAttributes = 256
  public static let maximumIndexedMetadataUTF8Bytes = 1_024
  public static let maximumMetadataUTF8Bytes = 262_144
  public static let maximumMetadataSummaryUTF8Bytes = maximumMetadataUTF8Bytes
  public static let maximumConversationPageSize: UInt64 = 100
  public static let maximumConversationTitleUTF8Bytes = 262_144
  public static let maximumImportedConversationTitleScalars = 256
  public static let maximumImportedTextPreviewUTF8Bytes = 256
  public static let maximumModelAliasCatalogEntries = 10_000
  public static let maximumStreamedTextUTF8Bytes = 8 * 1024 * 1024
}

public enum SignalboxProcessProtocolVersion: UInt64, Codable, CaseIterable, Sendable {
  case one = 1
}

public enum SignalboxCanonicalValueError: LocalizedError, Equatable {
  case uuid
  case decimal
  case dollarAmount
  case rateVersion
  case requestID
  case commandID

  public var errorDescription: String? {
    switch self {
    case .uuid:
      return "UUID is not canonical lowercase hyphenated text."
    case .decimal:
      return "Unsigned integer is not canonical decimal text."
    case .dollarAmount:
      return "Dollar amount is not canonical nonnegative decimal text."
    case .rateVersion:
      return "Billing rate version is not a bounded unpadded string."
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

  public init(from decoder: Decoder) throws {
    let container = try decoder.container(keyedBy: CodingKeys.self)
    guard container.contains(.title) else {
      throw DecodingError.keyNotFound(
        CodingKeys.title,
        .init(
          codingPath: decoder.codingPath,
          debugDescription: "Session metadata requires the nullable title member."
        )
      )
    }
    title = try container.decodeIfPresent(String.self, forKey: .title)
    tags = try container.decode([String].self, forKey: .tags)
    attributes = try container.decode([String: String].self, forKey: .attributes)
    archived = try container.decode(Bool.self, forKey: .archived)
  }

  public func encode(to encoder: Encoder) throws {
    var container = encoder.container(keyedBy: CodingKeys.self)
    if let title {
      try container.encode(title, forKey: .title)
    } else {
      try container.encodeNil(forKey: .title)
    }
    try container.encode(tags, forKey: .tags)
    try container.encode(attributes, forKey: .attributes)
    try container.encode(archived, forKey: .archived)
  }

  private enum CodingKeys: String, CodingKey {
    case title
    case tags
    case attributes
    case archived
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
  case user
  case unknown(kind: String, payload: [String: SignalboxJSONValue])

  public init(from decoder: Decoder) throws {
    let tagged = try SignalboxTaggedPayload(from: decoder)
    self = tagged.kind == "user" ? .user : .unknown(kind: tagged.kind, payload: tagged.payload)
  }

  public func encode(to encoder: Encoder) throws {
    switch self {
    case .user:
      try ["type": SignalboxJSONValue.string("user")].encode(to: encoder)
    case .unknown(let kind, var payload):
      payload["type"] = .string(kind)
      try payload.encode(to: encoder)
    }
  }
}

public enum SignalboxProcessClientRequest: Encodable, Equatable, Sendable {
  case createSession(
    commandID: SignalboxCommandID,
    initialModelSelection: SignalboxModelSelection,
    systemPrompt: String?
  )
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
  case readImportedConversation(importedConversationID: SignalboxCanonicalUUID)
  case createSessionFromImportedFrontier(
    commandID: SignalboxCommandID,
    importedConversationID: SignalboxCanonicalUUID,
    throughPosition: SignalboxCanonicalUInt64,
    relationship: SignalboxImportedSessionRelationship,
    initialModelSelection: SignalboxModelSelection
  )
  case stopTurn(
    commandID: SignalboxCommandID,
    sessionID: SignalboxCanonicalUUID,
    expectedActiveTurnID: SignalboxCanonicalUUID,
    content: String,
    expectedDefaultsVersion: SignalboxCanonicalUInt64
  )
  case decideToolRequest(
    commandID: SignalboxCommandID,
    sessionID: SignalboxCanonicalUUID,
    toolRequestID: SignalboxCanonicalUUID,
    decision: SignalboxProcessToolDecision
  )
  case readSessionDefaults(
    sessionID: SignalboxCanonicalUUID,
    defaultsVersion: SignalboxCanonicalUInt64?
  )
  case listConversations(
    titleContains: String?,
    origin: SignalboxConversationOriginFilter,
    includeArchived: Bool,
    pageSize: SignalboxCanonicalUInt64,
    after: SignalboxConversationCursor?
  )
  case listModelAliases

  public func encode(to encoder: Encoder) throws {
    var container = encoder.container(keyedBy: SignalboxDynamicCodingKey.self)
    switch self {
    case .createSession(let commandID, let selection, let systemPrompt):
      try container.encode("create_session", forKey: "type")
      try container.encode(commandID, forKey: "command_id")
      try container.encode(selection, forKey: "initial_model_selection")
      try container.encode(systemPrompt, forKey: "system_prompt")
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
    case .readImportedConversation(let importedConversationID):
      try container.encode("read_imported_conversation", forKey: "type")
      try container.encode(importedConversationID, forKey: "imported_conversation_id")
    case .createSessionFromImportedFrontier(
      let commandID,
      let importedConversationID,
      let throughPosition,
      let relationship,
      let selection
    ):
      try container.encode("create_session_from_imported_frontier", forKey: "type")
      try container.encode(commandID, forKey: "command_id")
      try container.encode(importedConversationID, forKey: "imported_conversation_id")
      try container.encode(throughPosition, forKey: "through_position")
      try container.encode(relationship, forKey: "relationship")
      try container.encode(selection, forKey: "initial_model_selection")
    case .stopTurn(
      let commandID,
      let sessionID,
      let activeTurnID,
      let content,
      let expectedDefaultsVersion
    ):
      try container.encode("stop_turn", forKey: "type")
      try container.encode(commandID, forKey: "command_id")
      try container.encode(sessionID, forKey: "session_id")
      try container.encode(activeTurnID, forKey: "expected_active_turn_id")
      try container.encode(content, forKey: "content")
      try container.encode(expectedDefaultsVersion, forKey: "expected_defaults_version")
    case .decideToolRequest(let commandID, let sessionID, let toolRequestID, let decision):
      try container.encode("decide_tool_request", forKey: "type")
      try container.encode(commandID, forKey: "command_id")
      try container.encode(sessionID, forKey: "session_id")
      try container.encode(toolRequestID, forKey: "tool_request_id")
      try container.encode(decision, forKey: "decision")
    case .readSessionDefaults(let sessionID, let defaultsVersion):
      try container.encode("read_session_defaults", forKey: "type")
      try container.encode(sessionID, forKey: "session_id")
      try container.encode(defaultsVersion, forKey: "defaults_version")
    case .listConversations(let title, let origin, let archived, let pageSize, let after):
      try container.encode("list_conversations", forKey: "type")
      try container.encode(title, forKey: "title_contains")
      try container.encode(origin, forKey: "origin")
      try container.encode(archived, forKey: "include_archived")
      try container.encode(pageSize, forKey: "page_size")
      try container.encode(after, forKey: "after")
    case .listModelAliases:
      try container.encode("list_model_aliases", forKey: "type")
    }
  }
}

public enum SignalboxProcessToolDecision: Codable, Equatable, Sendable {
  case approve
  case deny(reason: String)

  public init(from decoder: Decoder) throws {
    let tagged = try SignalboxTaggedPayload(from: decoder)
    switch tagged.kind {
    case "approve":
      try tagged.rejectUnadmittedFields(["type"], decoder: decoder)
      self = .approve
    case "deny":
      try tagged.rejectUnadmittedFields(["type", "reason"], decoder: decoder)
      self = .deny(reason: try decoder.decode("reason"))
    default:
      throw DecodingError.dataCorrupted(
        .init(
          codingPath: decoder.codingPath,
          debugDescription: "Unknown tool-decision type."
        )
      )
    }
  }

  public func encode(to encoder: Encoder) throws {
    var container = encoder.container(keyedBy: SignalboxDynamicCodingKey.self)
    switch self {
    case .approve:
      try container.encode("approve", forKey: "type")
    case .deny(let reason):
      try container.encode("deny", forKey: "type")
      try container.encode(reason, forKey: "reason")
    }
  }
}

public enum SignalboxConversationOriginFilter: String, Codable, Equatable, Sendable {
  case native
  case imported
  case all
}

public enum SignalboxConversationCursorOrigin: String, Codable, Equatable, Sendable {
  case nativeSession = "native_session"
  case importedConversation = "imported_conversation"
}

public struct SignalboxConversationCursor: Codable, Equatable, Sendable {
  public let origin: SignalboxConversationCursorOrigin
  public let conversationID: SignalboxCanonicalUUID

  public init(
    origin: SignalboxConversationCursorOrigin,
    conversationID: SignalboxCanonicalUUID
  ) {
    self.origin = origin
    self.conversationID = conversationID
  }

  public init(from decoder: Decoder) throws {
    let payload = try SignalboxUntaggedPayload(from: decoder)
    try payload.rejectUnadmittedFields(
      ["origin", "conversation_id"],
      decoder: decoder
    )
    let container = try decoder.container(keyedBy: CodingKeys.self)
    origin = try container.decode(SignalboxConversationCursorOrigin.self, forKey: .origin)
    conversationID = try container.decode(SignalboxCanonicalUUID.self, forKey: .conversationID)
  }

  private enum CodingKeys: String, CodingKey {
    case origin
    case conversationID = "conversation_id"
  }
}

public enum SignalboxConversationImportFormat: String, Codable, Equatable, Sendable {
  case claudeCodeSessionJSONLV2 = "claude_code_session_jsonl_v2"
  case codexRolloutJSONLV1 = "codex_rollout_jsonl_v1"
}

public enum SignalboxImportedSessionRelationship: String, Codable, Equatable, Sendable {
  case resume
  case fork
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

public enum SignalboxProcessFrameDecodingError: Error, Equatable {
  case oversizedFrame
}

public struct SignalboxProcessServerFrame: Equatable, Sendable {
  public let version: SignalboxProcessProtocolVersion
  public let requestID: SignalboxCanonicalUInt64
  public let message: SignalboxProcessServerMessage

  private init(
    version: SignalboxProcessProtocolVersion,
    requestID: SignalboxCanonicalUInt64,
    message: SignalboxProcessServerMessage
  ) {
    self.version = version
    self.requestID = requestID
    self.message = message
  }

  public static func decode(from data: Data) throws -> Self {
    guard data.count <= SignalboxProcessProtocol.maximumFrameBytes else {
      throw SignalboxProcessFrameDecodingError.oversizedFrame
    }
    var scanner = SignalboxJSONDuplicateMemberScanner(data: data)
    let duplicateObjectPaths = try scanner.scan()
    let decoder = SignalboxJSONCoding.decoder()
    decoder.userInfo[.signalboxDuplicateObjectPaths] = duplicateObjectPaths
    let wire = try decoder.decode(SignalboxProcessServerWireFrame.self, from: data)
    return Self(version: wire.version, requestID: wire.requestID, message: wire.message)
  }
}

private struct SignalboxProcessServerWireFrame: Decodable {
  let version: SignalboxProcessProtocolVersion
  let requestID: SignalboxCanonicalUInt64
  let message: SignalboxProcessServerMessage

  init(from decoder: Decoder) throws {
    let payload = try SignalboxUntaggedPayload(from: decoder)
    try payload.rejectUnadmittedFields(
      ["version", "request_id", "message"],
      decoder: decoder
    )
    let container = try decoder.container(keyedBy: CodingKeys.self)
    version = try container.decode(SignalboxProcessProtocolVersion.self, forKey: .version)
    requestID = try container.decode(SignalboxCanonicalUInt64.self, forKey: .requestID)
    message = try container.decode(SignalboxProcessServerMessage.self, forKey: .message)
  }

  private enum CodingKeys: String, CodingKey {
    case version
    case requestID = "request_id"
    case message
  }
}

public enum SignalboxProcessServerMessage: Decodable, Equatable, Sendable {
  case sessionCreated(sessionID: SignalboxCanonicalUUID)
  case inputSubmitted(SignalboxInputSubmitted)
  case toolRequestDecided(SignalboxToolRequestDecided)
  case sessionDefaults(SignalboxSessionDefaultsRead)
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
  case conversationPageStart
  case conversationSummary(SignalboxConversationSummary)
  case conversationPageEnd(SignalboxConversationPageEnd)
  case importedConversationStart(importedConversationID: SignalboxCanonicalUUID)
  case importedConversationEntry(SignalboxImportedConversationEntry)
  case importedConversationEnd(SignalboxImportedConversationEnd)
  case modelAliasesStart
  case modelAliasSummary(SignalboxModelAliasSummary)
  case modelAliasesEnd(aliasCount: SignalboxCanonicalUInt64)
  case transcriptSnapshotStart(SignalboxTranscriptSnapshotBoundary)
  case transcriptTurn(SignalboxTranscriptTurn)
  case transcriptModelCallUsage(SignalboxTranscriptModelCallUsage)
  case transcriptModelCallsEnd(modelCallCount: SignalboxCanonicalUInt64)
  case transcriptEntry(SignalboxTranscriptEntryMessage)
  case transcriptTextEntry(SignalboxTranscriptTextEntryMessage)
  case transcriptContent(SignalboxTranscriptContent)
  case transcriptSnapshotEnd(SignalboxTranscriptSnapshotEnd)
  case sessionEvent(SignalboxFollowedSessionEvent)
  case providerTextDelta(SignalboxProviderTextDelta)
  case protocolError(SignalboxProcessError)
  case unknown(
    kind: String,
    payload: [String: SignalboxJSONValue],
    decodingDiagnostic: SignalboxDecodingDiagnostic?
  )

  public init(from decoder: Decoder) throws {
    if decoder.containsDuplicateObjectMembers {
      let payload =
        try decoder.singleValueContainer().decode([String: SignalboxJSONValue].self)
      guard case .string(let kind) = payload["type"] else {
        throw DecodingError.keyNotFound(
          SignalboxDynamicCodingKey("type"),
          .init(
            codingPath: decoder.codingPath,
            debugDescription: "Tagged object is missing its type."
          )
        )
      }
      self = .unknown(
        kind: kind,
        payload: payload,
        decodingDiagnostic: SignalboxDecodingDiagnostic(
          error: decoder.duplicateObjectMembersError()
        )
      )
      return
    }
    let tagged = try SignalboxTaggedPayload(from: decoder)
    do {
      switch tagged.kind {
      case "session_created":
        self = .sessionCreated(sessionID: try decoder.decode("session_id"))
      case "input_submitted":
        self = .inputSubmitted(try SignalboxInputSubmitted(from: decoder))
      case "tool_request_decided":
        self = .toolRequestDecided(try SignalboxToolRequestDecided(from: decoder))
      case "session_defaults":
        self = .sessionDefaults(try SignalboxSessionDefaultsRead(from: decoder))
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
      case "conversation_page_start":
        try tagged.rejectUnadmittedFields(["type"], decoder: decoder)
        self = .conversationPageStart
      case "conversation_summary":
        try tagged.rejectUnadmittedFields(
          ["type", "conversation"],
          decoder: decoder
        )
        self = .conversationSummary(try decoder.decode("conversation"))
      case "conversation_page_end":
        self = .conversationPageEnd(try SignalboxConversationPageEnd(from: decoder))
      case "imported_conversation_start":
        try tagged.rejectUnadmittedFields(
          ["type", "imported_conversation_id"],
          decoder: decoder
        )
        self = .importedConversationStart(
          importedConversationID: try decoder.decode("imported_conversation_id")
        )
      case "imported_conversation_entry":
        self = .importedConversationEntry(
          try SignalboxImportedConversationEntry(from: decoder)
        )
      case "imported_conversation_end":
        try tagged.rejectUnadmittedFields(
          ["type", "imported_conversation_id", "entry_count"],
          decoder: decoder
        )
        self = .importedConversationEnd(
          try SignalboxImportedConversationEnd(from: decoder)
        )
      case "model_aliases_start":
        try tagged.rejectUnadmittedFields(["type"], decoder: decoder)
        self = .modelAliasesStart
      case "model_alias_summary":
        self = .modelAliasSummary(try SignalboxModelAliasSummary(from: decoder))
      case "model_aliases_end":
        try tagged.rejectUnadmittedFields(["type", "alias_count"], decoder: decoder)
        self = .modelAliasesEnd(aliasCount: try decoder.decode("alias_count"))
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
      case "transcript_model_call_usage":
        try tagged.rejectUnadmittedFields(
          [
            "type", "model_call_index", "turn_id", "model_call_id", "usage_provenance",
            "usage", "cost",
          ],
          decoder: decoder
        )
        try tagged.requireFields(["cost"], decoder: decoder)
        self = .transcriptModelCallUsage(
          try SignalboxTranscriptModelCallUsage(from: decoder)
        )
      case "transcript_model_calls_end":
        try tagged.rejectUnadmittedFields(
          ["type", "model_call_count"],
          decoder: decoder
        )
        self = .transcriptModelCallsEnd(
          modelCallCount: try decoder.decode("model_call_count")
        )
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
      case "provider_text_delta":
        self = .providerTextDelta(try SignalboxProviderTextDelta(from: decoder))
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

public struct SignalboxToolRequestDecided: Decodable, Equatable, Sendable {
  public let toolRequestID: SignalboxCanonicalUUID
  public let decision: SignalboxProcessToolDecision

  public init(
    toolRequestID: SignalboxCanonicalUUID,
    decision: SignalboxProcessToolDecision
  ) {
    self.toolRequestID = toolRequestID
    self.decision = decision
  }

  public init(from decoder: Decoder) throws {
    let tagged = try SignalboxTaggedPayload(from: decoder)
    try tagged.rejectUnadmittedFields(
      ["type", "tool_request_id", "decision"],
      decoder: decoder
    )
    let container = try decoder.container(keyedBy: CodingKeys.self)
    toolRequestID = try container.decode(SignalboxCanonicalUUID.self, forKey: .toolRequestID)
    decision = try container.decode(SignalboxProcessToolDecision.self, forKey: .decision)
  }

  private enum CodingKeys: String, CodingKey {
    case toolRequestID = "tool_request_id"
    case decision
  }
}

public struct SignalboxSessionDefaultsRead: Decodable, Equatable, Sendable {
  public let sessionID: SignalboxCanonicalUUID
  public let defaultsVersion: SignalboxCanonicalUInt64
  public let modelSelection: SignalboxModelSelection
  public let dangerousToolAutoApproval: Bool
  public let systemPrompt: String?

  public init(from decoder: Decoder) throws {
    let tagged = try SignalboxTaggedPayload(from: decoder)
    try tagged.rejectUnadmittedFields(
      [
        "type", "session_id", "defaults_version", "model_selection",
        "dangerous_tool_auto_approval", "system_prompt",
      ],
      decoder: decoder
    )
    let container = try decoder.container(keyedBy: CodingKeys.self)
    sessionID = try container.decode(SignalboxCanonicalUUID.self, forKey: .sessionID)
    defaultsVersion = try container.decode(SignalboxCanonicalUInt64.self, forKey: .defaultsVersion)
    modelSelection = try container.decode(SignalboxModelSelection.self, forKey: .modelSelection)
    dangerousToolAutoApproval = try container.decode(
      Bool.self,
      forKey: .dangerousToolAutoApproval
    )
    guard container.contains(.systemPrompt) else {
      throw DecodingError.keyNotFound(
        CodingKeys.systemPrompt,
        .init(
          codingPath: decoder.codingPath,
          debugDescription: "Session defaults require the system-prompt member."
        )
      )
    }
    systemPrompt = try container.decodeIfPresent(String.self, forKey: .systemPrompt)
  }

  private enum CodingKeys: String, CodingKey {
    case sessionID = "session_id"
    case defaultsVersion = "defaults_version"
    case modelSelection = "model_selection"
    case dangerousToolAutoApproval = "dangerous_tool_auto_approval"
    case systemPrompt = "system_prompt"
  }
}

public struct SignalboxModelAliasSummary: Decodable, Equatable, Identifiable, Sendable {
  public let aliasID: SignalboxCanonicalUUID
  public let selectionID: SignalboxCanonicalUUID

  public var id: SignalboxCanonicalUUID {
    aliasID
  }

  public init(aliasID: SignalboxCanonicalUUID, selectionID: SignalboxCanonicalUUID) {
    self.aliasID = aliasID
    self.selectionID = selectionID
  }

  public init(from decoder: Decoder) throws {
    let tagged = try SignalboxTaggedPayload(from: decoder)
    try tagged.rejectUnadmittedFields(
      ["type", "alias_id", "selection_id"],
      decoder: decoder
    )
    let container = try decoder.container(keyedBy: CodingKeys.self)
    aliasID = try container.decode(SignalboxCanonicalUUID.self, forKey: .aliasID)
    selectionID = try container.decode(SignalboxCanonicalUUID.self, forKey: .selectionID)
  }

  private enum CodingKeys: String, CodingKey {
    case aliasID = "alias_id"
    case selectionID = "selection_id"
  }
}

public enum SignalboxImportedConversationSourceFormat: String, Decodable, Equatable, Sendable {
  case claudeCodeSessionJSONLV1 = "claude_code_session_jsonl_v1"
  case claudeCodeSessionJSONLV2 = "claude_code_session_jsonl_v2"
  case codexRolloutJSONLV1 = "codex_rollout_jsonl_v1"
}

public struct SignalboxImportedTextPreview: Decodable, Equatable, Sendable {
  public let preview: String
  public let truncated: Bool

  public init(from decoder: Decoder) throws {
    let payload = try SignalboxUntaggedPayload(from: decoder)
    try payload.rejectUnadmittedFields(["preview", "truncated"], decoder: decoder)
    try payload.requireFields(["preview", "truncated"], decoder: decoder)
    let container = try decoder.container(keyedBy: CodingKeys.self)
    preview = try container.decode(String.self, forKey: .preview)
    truncated = try container.decode(Bool.self, forKey: .truncated)
    guard
      preview.utf8.count <= SignalboxProcessProtocol.maximumImportedTextPreviewUTF8Bytes,
      !truncated || !preview.isEmpty
    else {
      throw DecodingError.dataCorrupted(
        .init(
          codingPath: decoder.codingPath,
          debugDescription: "Imported text preview violates its bounded prefix shape."
        )
      )
    }
  }

  private enum CodingKeys: String, CodingKey {
    case preview
    case truncated
  }
}

public struct SignalboxImportedConversationEntry: Decodable, Equatable, Sendable {
  public let position: SignalboxCanonicalUInt64
  public let importedEntryID: SignalboxCanonicalUUID
  public let sourceSpeaker: SignalboxImportedSourceSpeaker
  public let contentKind: SignalboxImportedContentKind
  public let textPreview: SignalboxImportedTextPreview?

  public init(from decoder: Decoder) throws {
    let tagged = try SignalboxTaggedPayload(from: decoder)
    try tagged.rejectUnadmittedFields(
      [
        "type", "position", "imported_entry_id", "source_speaker",
        "content_kind", "text_preview",
      ],
      decoder: decoder
    )
    try tagged.requireFields(
      [
        "position", "imported_entry_id", "source_speaker", "content_kind",
        "text_preview",
      ],
      decoder: decoder
    )
    let container = try decoder.container(keyedBy: CodingKeys.self)
    position = try container.decode(SignalboxCanonicalUInt64.self, forKey: .position)
    importedEntryID = try container.decode(
      SignalboxCanonicalUUID.self,
      forKey: .importedEntryID
    )
    sourceSpeaker = try container.decode(
      SignalboxImportedSourceSpeaker.self,
      forKey: .sourceSpeaker
    )
    contentKind = try container.decode(SignalboxImportedContentKind.self, forKey: .contentKind)
    textPreview = try container.decodeIfPresent(
      SignalboxImportedTextPreview.self,
      forKey: .textPreview
    )
    switch sourceSpeaker {
    case .notAttested, .attestedAbsent, .attested:
      break
    case .unknown(let kind, _):
      throw DecodingError.dataCorrupted(
        .init(
          codingPath: decoder.codingPath + [CodingKeys.sourceSpeaker],
          debugDescription:
            "Imported conversation entry source_speaker variant \(kind) is not admitted."
        )
      )
    }
    let admitsTextPreview: Bool
    switch contentKind {
    case .text:
      admitsTextPreview = true
    case .sourceEvent, .sourceMessageBlock, .toolCall, .toolResult, .thinking,
      .redactedThinking, .document, .messageContentAbsent:
      admitsTextPreview = false
    }
    guard textPreview == nil || admitsTextPreview else {
      throw DecodingError.dataCorrupted(
        .init(
          codingPath: decoder.codingPath + [CodingKeys.textPreview],
          debugDescription:
            "Imported conversation entry text_preview requires text content."
        )
      )
    }
    guard position.rawValue > 0 else {
      throw DecodingError.dataCorrupted(
        .init(
          codingPath: decoder.codingPath + [CodingKeys.position],
          debugDescription: "Imported conversation entry position must be positive."
        )
      )
    }
  }

  private enum CodingKeys: String, CodingKey {
    case position
    case importedEntryID = "imported_entry_id"
    case sourceSpeaker = "source_speaker"
    case contentKind = "content_kind"
    case textPreview = "text_preview"
  }
}

public struct SignalboxImportedConversationEnd: Decodable, Equatable, Sendable {
  public let importedConversationID: SignalboxCanonicalUUID
  public let entryCount: SignalboxCanonicalUInt64

  private enum CodingKeys: String, CodingKey {
    case importedConversationID = "imported_conversation_id"
    case entryCount = "entry_count"
  }
}

public struct SignalboxNativeConversationSummary: Equatable, Sendable {
  public let sessionID: SignalboxCanonicalUUID
  public let title: String?
  public let archived: Bool
  public let defaultsVersion: SignalboxCanonicalUInt64

  public init(
    sessionID: SignalboxCanonicalUUID,
    title: String?,
    archived: Bool,
    defaultsVersion: SignalboxCanonicalUInt64
  ) {
    self.sessionID = sessionID
    self.title = title
    self.archived = archived
    self.defaultsVersion = defaultsVersion
  }
}

public struct SignalboxImportedConversationSummary: Equatable, Sendable {
  public let importedConversationID: SignalboxCanonicalUUID
  public let title: String?
  public let entryCount: SignalboxCanonicalUInt64
  public let sourceFormat: SignalboxImportedConversationSourceFormat
}

public enum SignalboxConversationSummary: Decodable, Equatable, Sendable {
  case native(SignalboxNativeConversationSummary)
  case imported(SignalboxImportedConversationSummary)

  public init(from decoder: Decoder) throws {
    let payload = try SignalboxUntaggedPayload(from: decoder)
    guard case .string(let origin) = payload.payload["origin"] else {
      throw DecodingError.keyNotFound(
        SignalboxDynamicCodingKey("origin"),
        .init(
          codingPath: decoder.codingPath,
          debugDescription: "Conversation summary is missing its origin."
        )
      )
    }
    switch origin {
    case "native_session":
      try payload.rejectUnadmittedFields(
        ["origin", "session_id", "title", "archived", "defaults_version"],
        decoder: decoder
      )
      let container = try decoder.container(keyedBy: NativeCodingKeys.self)
      guard container.contains(.title) else {
        throw DecodingError.keyNotFound(
          NativeCodingKeys.title,
          .init(
            codingPath: decoder.codingPath,
            debugDescription: "Native conversation summary is missing its title."
          )
        )
      }
      self = .native(
        SignalboxNativeConversationSummary(
          sessionID: try container.decode(SignalboxCanonicalUUID.self, forKey: .sessionID),
          title: try container.decodeIfPresent(String.self, forKey: .title),
          archived: try container.decode(Bool.self, forKey: .archived),
          defaultsVersion: try container.decode(
            SignalboxCanonicalUInt64.self,
            forKey: .defaultsVersion
          )
        )
      )
    case "imported_conversation":
      try payload.rejectUnadmittedFields(
        [
          "origin", "imported_conversation_id", "title", "entry_count",
          "source_format",
        ],
        decoder: decoder
      )
      let container = try decoder.container(keyedBy: ImportedCodingKeys.self)
      guard container.contains(.title) else {
        throw DecodingError.keyNotFound(
          ImportedCodingKeys.title,
          .init(
            codingPath: decoder.codingPath,
            debugDescription: "Imported conversation summary is missing its title."
          )
        )
      }
      self = .imported(
        SignalboxImportedConversationSummary(
          importedConversationID: try container.decode(
            SignalboxCanonicalUUID.self,
            forKey: .importedConversationID
          ),
          title: try container.decodeIfPresent(String.self, forKey: .title),
          entryCount: try container.decode(SignalboxCanonicalUInt64.self, forKey: .entryCount),
          sourceFormat: try container.decode(
            SignalboxImportedConversationSourceFormat.self,
            forKey: .sourceFormat
          )
        )
      )
    default:
      throw DecodingError.dataCorrupted(
        .init(
          codingPath: decoder.codingPath,
          debugDescription: "Unknown conversation-summary origin."
        )
      )
    }
  }

  private enum NativeCodingKeys: String, CodingKey {
    case sessionID = "session_id"
    case title
    case archived
    case defaultsVersion = "defaults_version"
  }

  private enum ImportedCodingKeys: String, CodingKey {
    case importedConversationID = "imported_conversation_id"
    case title
    case entryCount = "entry_count"
    case sourceFormat = "source_format"
  }
}

public struct SignalboxConversationPageEnd: Decodable, Equatable, Sendable {
  public let conversationCount: SignalboxCanonicalUInt64
  public let nextAfter: SignalboxConversationCursor?

  public init(from decoder: Decoder) throws {
    let tagged = try SignalboxTaggedPayload(from: decoder)
    try tagged.rejectUnadmittedFields(
      ["type", "conversation_count", "next_after"],
      decoder: decoder
    )
    let container = try decoder.container(keyedBy: CodingKeys.self)
    conversationCount = try container.decode(
      SignalboxCanonicalUInt64.self,
      forKey: .conversationCount
    )
    guard conversationCount.rawValue <= SignalboxProcessProtocol.maximumConversationPageSize else {
      throw DecodingError.dataCorruptedError(
        forKey: .conversationCount,
        in: container,
        debugDescription: "Conversation page count exceeded the protocol bound."
      )
    }
    guard container.contains(.nextAfter) else {
      throw DecodingError.keyNotFound(
        CodingKeys.nextAfter,
        .init(
          codingPath: decoder.codingPath,
          debugDescription: "Conversation page end requires its cursor member."
        )
      )
    }
    nextAfter = try container.decodeIfPresent(SignalboxConversationCursor.self, forKey: .nextAfter)
  }

  private enum CodingKeys: String, CodingKey {
    case conversationCount = "conversation_count"
    case nextAfter = "next_after"
  }
}

public struct SignalboxProviderTextDelta: Decodable, Equatable, Sendable {
  public let sessionID: SignalboxCanonicalUUID
  public let turnID: SignalboxCanonicalUUID
  public let modelCallID: SignalboxCanonicalUUID
  public let partIndex: SignalboxCanonicalUInt64
  public let content: String

  public init(
    sessionID: SignalboxCanonicalUUID,
    turnID: SignalboxCanonicalUUID,
    modelCallID: SignalboxCanonicalUUID,
    partIndex: SignalboxCanonicalUInt64,
    content: String
  ) {
    self.sessionID = sessionID
    self.turnID = turnID
    self.modelCallID = modelCallID
    self.partIndex = partIndex
    self.content = content
  }

  public init(from decoder: Decoder) throws {
    let tagged = try SignalboxTaggedPayload(from: decoder)
    try tagged.rejectUnadmittedFields(
      ["type", "session_id", "turn_id", "model_call_id", "part_index", "content"],
      decoder: decoder
    )
    let container = try decoder.container(keyedBy: CodingKeys.self)
    sessionID = try container.decode(SignalboxCanonicalUUID.self, forKey: .sessionID)
    turnID = try container.decode(SignalboxCanonicalUUID.self, forKey: .turnID)
    modelCallID = try container.decode(SignalboxCanonicalUUID.self, forKey: .modelCallID)
    partIndex = try container.decode(SignalboxCanonicalUInt64.self, forKey: .partIndex)
    content = try container.decode(String.self, forKey: .content)
    guard content.utf8.count <= SignalboxProcessProtocol.maximumContentFragmentUTF8Bytes else {
      throw DecodingError.dataCorruptedError(
        forKey: .content,
        in: container,
        debugDescription: "Provider text delta exceeded the fragment bound."
      )
    }
  }

  private enum CodingKeys: String, CodingKey {
    case sessionID = "session_id"
    case turnID = "turn_id"
    case modelCallID = "model_call_id"
    case partIndex = "part_index"
    case content
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

public struct SignalboxTranscriptModelCallUsage: Decodable, Equatable, Sendable {
  public let modelCallIndex: SignalboxCanonicalUInt64
  public let turnID: SignalboxCanonicalUUID
  public let modelCallID: SignalboxCanonicalUUID
  public let usageProvenance: SignalboxUsageProvenance
  public let usage: SignalboxModelCallTokenUsage
  public let cost: SignalboxModelCallDollarCost?

  public init(from decoder: Decoder) throws {
    modelCallIndex = try decoder.decode("model_call_index")
    turnID = try decoder.decode("turn_id")
    modelCallID = try decoder.decode("model_call_id")
    usageProvenance = try decoder.decode("usage_provenance")
    usage = try decoder.decode("usage")
    cost = try decoder.decodeIfPresent("cost")
    guard cost == nil
      || usage.inputTokens != nil
      || usage.outputTokens != nil
      || usage.cacheCreationInputTokens != nil
      || usage.cacheReadInputTokens != nil
    else {
      throw DecodingError.dataCorrupted(
        .init(
          codingPath: decoder.codingPath,
          debugDescription: "Model-call cost requires at least one present usage axis."
        )
      )
    }
  }
}

public enum SignalboxUsageProvenance: String, Decodable, Equatable, Sendable {
  case reported
  case estimated
}

public enum SignalboxModelCallCostLabel: String, Decodable, Equatable, Sendable {
  case real
  case meteredEquivalent = "metered_equivalent"
}

public struct SignalboxCanonicalDollarAmount: Decodable, Equatable, Sendable {
  public let rawValue: String

  public init(from decoder: Decoder) throws {
    let spelling = try decoder.singleValueContainer().decode(String.self)
    let components = spelling.split(separator: ".", omittingEmptySubsequences: false)
    let integer = components.first.map(String.init) ?? ""
    let fraction = components.count == 2 ? String(components[1]) : nil
    let integerIsCanonical = integer == "0"
      || (!integer.hasPrefix("0") && integer.allSatisfy(\.isASCII)
        && integer.allSatisfy(\.isNumber))
    let fractionIsCanonical = fraction.map {
      !$0.isEmpty && $0.utf8.count <= 28 && $0.allSatisfy(\.isASCII)
        && $0.allSatisfy(\.isNumber) && !$0.hasSuffix("0")
    } ?? true
    let coefficient = String(spelling.filter { $0 != "." }.drop(while: { $0 == "0" }))
    let maximumCoefficient = "79228162514264337593543950335"
    let coefficientFits = coefficient.count < maximumCoefficient.count
      || (coefficient.count == maximumCoefficient.count
        && (coefficient == maximumCoefficient
          || coefficient.lexicographicallyPrecedes(maximumCoefficient)))
    guard !spelling.isEmpty,
      spelling.utf8.count <= 30,
      components.count <= 2,
      integerIsCanonical,
      fractionIsCanonical,
      coefficientFits
    else {
      throw SignalboxCanonicalValueError.dollarAmount
    }
    rawValue = spelling
  }
}

public struct SignalboxBillingRateVersion: Decodable, Equatable, Sendable {
  public let rawValue: String

  public init(from decoder: Decoder) throws {
    let spelling = try decoder.singleValueContainer().decode(String.self)
    let beginsWithProtocolTrimWhitespace = spelling.unicodeScalars.first.map {
      signalboxIsProtocolTrimWhitespace($0)
    } ?? false
    let endsWithProtocolTrimWhitespace = spelling.unicodeScalars.last.map {
      signalboxIsProtocolTrimWhitespace($0)
    } ?? false
    guard !spelling.isEmpty,
      spelling.utf8.count <= 128,
      !beginsWithProtocolTrimWhitespace,
      !endsWithProtocolTrimWhitespace,
      !spelling.contains("\0")
    else {
      throw SignalboxCanonicalValueError.rateVersion
    }
    rawValue = spelling
  }
}

private func signalboxIsProtocolTrimWhitespace(_ scalar: Unicode.Scalar) -> Bool {
  switch scalar.value {
  case 0x0009...0x000D, 0x0020, 0x0085, 0x00A0, 0x1680,
    0x2000...0x200A, 0x2028, 0x2029, 0x202F, 0x205F, 0x3000:
    return true
  default:
    return false
  }
}

public struct SignalboxModelCallDollarCost: Decodable, Equatable, Sendable {
  public let amountUSD: SignalboxCanonicalDollarAmount
  public let rateVersion: SignalboxBillingRateVersion
  public let label: SignalboxModelCallCostLabel

  public init(from decoder: Decoder) throws {
    let payload = try SignalboxUntaggedPayload(from: decoder)
    try payload.rejectUnadmittedFields(
      ["amount_usd", "rate_version", "label"], decoder: decoder)
    try payload.requireFields(
      ["amount_usd", "rate_version", "label"], decoder: decoder)
    amountUSD = try decoder.decode("amount_usd")
    rateVersion = try decoder.decode("rate_version")
    label = try decoder.decode("label")
  }
}

public struct SignalboxModelCallTokenUsage: Decodable, Equatable, Sendable {
  public let inputTokens: SignalboxCanonicalUInt64?
  public let outputTokens: SignalboxCanonicalUInt64?
  public let cacheCreationInputTokens: SignalboxCanonicalUInt64?
  public let cacheReadInputTokens: SignalboxCanonicalUInt64?

  public init(from decoder: Decoder) throws {
    let payload = try SignalboxUntaggedPayload(from: decoder)
    try payload.rejectUnadmittedFields(
      [
        "input_tokens", "output_tokens", "cache_creation_input_tokens",
        "cache_read_input_tokens",
      ],
      decoder: decoder
    )
    try payload.requireFields(
      [
        "input_tokens", "output_tokens", "cache_creation_input_tokens",
        "cache_read_input_tokens",
      ],
      decoder: decoder
    )
    inputTokens = try decoder.decodeIfPresent("input_tokens")
    outputTokens = try decoder.decodeIfPresent("output_tokens")
    cacheCreationInputTokens = try decoder.decodeIfPresent("cache_creation_input_tokens")
    cacheReadInputTokens = try decoder.decodeIfPresent("cache_read_input_tokens")
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
        try tagged.requireFields(["current_model_call"], decoder: decoder)
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
        try tagged.requireFields(
          ["terminal_attempt_id", "terminal_model_call"],
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
        try tagged.requireFields(["terminal_model_call_id"], decoder: decoder)
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
  public let cause: SignalboxFailedModelCallCause?

  public init(from decoder: Decoder) throws {
    let payload = try SignalboxUntaggedPayload(from: decoder)
    try payload.rejectUnadmittedFields(
      ["model_call_id", "disposition", "cause"],
      decoder: decoder
    )
    // The wire contract uses omission for an unclassified failure. Accepting
    // explicit null here would erase the distinction before callers can reject
    // the malformed known frame.
    guard payload.payload["cause"] != .null else {
      throw DecodingError.valueNotFound(
        SignalboxFailedModelCallCause.self,
        .init(
          codingPath: decoder.codingPath + [SignalboxDynamicCodingKey("cause")],
          debugDescription: "A provider-failure cause must be absent or a closed token."
        )
      )
    }
    modelCallID = try decoder.decode("model_call_id")
    disposition = try decoder.decode("disposition")
    cause = try decoder.decodeIfPresent("cause")
    guard disposition != .cancelled || cause == nil else {
      throw DecodingError.dataCorrupted(
        .init(
          codingPath: decoder.codingPath,
          debugDescription: "A cancelled model call cannot carry a provider-failure cause."
        )
      )
    }
  }
}

public enum SignalboxFailedModelCallDisposition: String, Decodable, Equatable, Sendable {
  case knownFailed = "known_failed"
  case cancelled
}

public enum SignalboxFailedModelCallCause: String, Decodable, Equatable, Sendable {
  case credentialRejected = "credential_rejected"
  case permissionDenied = "permission_denied"
  case invalidRequest = "invalid_request"
  case targetNotFound = "target_not_found"
  case requestTooLarge = "request_too_large"
  case rateLimited = "rate_limited"
  case quotaExhausted = "quota_exhausted"
  case overloaded
  case providerInternal = "provider_internal"
  case unrecognized
}

public struct SignalboxCurrentModelCall: Decodable, Equatable, Sendable {
  public let modelCallID: SignalboxCanonicalUUID
  public let state: SignalboxCurrentModelCallState

  public init(from decoder: Decoder) throws {
    try SignalboxUntaggedPayload(from: decoder).rejectUnadmittedFields(
      ["model_call_id", "state"],
      decoder: decoder
    )
    modelCallID = try decoder.decode("model_call_id")
    state = try decoder.decode("state")
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
  case contextSummary(
    modelCallID: SignalboxCanonicalUUID,
    firstSourceSessionID: SignalboxCanonicalUUID,
    firstEntryID: SignalboxCanonicalUUID,
    throughSourceSessionID: SignalboxCanonicalUUID,
    throughEntryID: SignalboxCanonicalUUID
  )
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
      case "context_summary":
        try tagged.rejectUnadmittedFields(
          [
            "type", "model_call_id", "first_source_session_id", "first_entry_id",
            "through_source_session_id", "through_entry_id",
          ],
          decoder: decoder
        )
        self = .contextSummary(
          modelCallID: try decoder.decode("model_call_id"),
          firstSourceSessionID: try decoder.decode("first_source_session_id"),
          firstEntryID: try decoder.decode("first_entry_id"),
          throughSourceSessionID: try decoder.decode("through_source_session_id"),
          throughEntryID: try decoder.decode("through_entry_id")
        )
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
  case contextCompacted(
    contextCompactionID: SignalboxCanonicalUUID,
    modelCallID: SignalboxCanonicalUUID,
    throughPosition: SignalboxCanonicalUInt64,
    summaryEntryID: SignalboxCanonicalUUID,
    resultFrontierID: SignalboxCanonicalUUID
  )
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
      case "context_compacted":
        try tagged.rejectUnadmittedFields(
          [
            "type", "context_compaction_id", "model_call_id", "through_position",
            "summary_entry_id", "result_frontier_id",
          ],
          decoder: decoder
        )
        self = .contextCompacted(
          contextCompactionID: try decoder.decode("context_compaction_id"),
          modelCallID: try decoder.decode("model_call_id"),
          throughPosition: try decoder.decode("through_position"),
          summaryEntryID: try decoder.decode("summary_entry_id"),
          resultFrontierID: try decoder.decode("result_frontier_id")
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

  public init(from decoder: Decoder) throws {
    let tagged = try SignalboxTaggedPayload(from: decoder)
    try tagged.rejectUnadmittedFields(
      ["type", "code", "message", "detail"],
      decoder: decoder
    )
    code = try decoder.decode("code")
    message = try decoder.decode("message")
    let container = try decoder.container(keyedBy: CodingKeys.self)
    switch code {
    case .rejected:
      guard container.contains(.detail) else {
        throw DecodingError.keyNotFound(
          CodingKeys.detail,
          .init(
            codingPath: decoder.codingPath,
            debugDescription: "A rejected error requires detail."
          )
        )
      }
      guard try !container.decodeNil(forKey: .detail) else {
        throw DecodingError.valueNotFound(
          SignalboxRejectionDetail.self,
          .init(
            codingPath: decoder.codingPath + [CodingKeys.detail],
            debugDescription: "A rejected error requires non-null detail."
          )
        )
      }
      let rejectionDetail = try container.decode(
        SignalboxRejectionDetail.self,
        forKey: .detail
      )
      guard case .unknown(let kind, _) = rejectionDetail else {
        detail = rejectionDetail
        return
      }
      throw DecodingError.dataCorrupted(
        .init(
          codingPath: decoder.codingPath + [CodingKeys.detail],
          debugDescription: "Unrecognized rejection detail type: \(kind)."
        )
      )
    default:
      guard !container.contains(.detail) else {
        throw DecodingError.dataCorrupted(
          .init(
            codingPath: decoder.codingPath + [CodingKeys.detail],
            debugDescription: "Only rejected errors admit detail."
          )
        )
      }
      detail = nil
    }
  }

  private enum CodingKeys: String, CodingKey {
    case detail
  }
}

public enum SignalboxRejectionDetail: Decodable, Equatable, Sendable {
  case sessionNotFound(sessionID: SignalboxCanonicalUUID)
  case activeTurnPresent(sessionID: SignalboxCanonicalUUID, activeTurnID: SignalboxCanonicalUUID)
  case activeTurnMismatch(
    sessionID: SignalboxCanonicalUUID,
    expectedActiveTurnID: SignalboxCanonicalUUID,
    activeTurnID: SignalboxCanonicalUUID
  )
  case noActiveTurn(
    sessionID: SignalboxCanonicalUUID,
    expectedActiveTurnID: SignalboxCanonicalUUID
  )
  case turnNotAwaitingReconciliation(
    sessionID: SignalboxCanonicalUUID,
    turnID: SignalboxCanonicalUUID
  )
  case interruptAlreadyApplied(
    sessionID: SignalboxCanonicalUUID,
    activeTurnID: SignalboxCanonicalUUID,
    existingCommandID: SignalboxCanonicalUUID
  )
  case interruptUnavailableWhileAwaitingApproval(
    sessionID: SignalboxCanonicalUUID,
    activeTurnID: SignalboxCanonicalUUID
  )
  case safePointUnavailableWhileStopping(
    sessionID: SignalboxCanonicalUUID,
    activeTurnID: SignalboxCanonicalUUID,
    existingCommandID: SignalboxCanonicalUUID
  )
  case toolRequestNotFound(toolRequestID: SignalboxCanonicalUUID)
  case toolRequestAlreadyResolved(toolRequestID: SignalboxCanonicalUUID)
  case toolRequestNotEarliestUndecided(
    toolRequestID: SignalboxCanonicalUUID,
    earliestToolRequestID: SignalboxCanonicalUUID
  )
  case toolRequestNotInSession(
    sessionID: SignalboxCanonicalUUID,
    toolRequestID: SignalboxCanonicalUUID
  )
  case defaultsVersionMismatch(
    sessionID: SignalboxCanonicalUUID, expected: SignalboxCanonicalUInt64,
    current: SignalboxCanonicalUInt64)
  case unknownModelAlias(sessionID: SignalboxCanonicalUUID, aliasID: SignalboxCanonicalUUID)
  case acceptancePositionExhausted(
    sessionID: SignalboxCanonicalUUID, last: SignalboxCanonicalUInt64)
  case defaultsVersionExhausted(
    sessionID: SignalboxCanonicalUUID,
    current: SignalboxCanonicalUInt64
  )
  case importedConversationNotFound(importedConversationID: SignalboxCanonicalUUID)
  case importedFrontierPositionOutOfRange(
    importedConversationID: SignalboxCanonicalUUID,
    requestedPosition: SignalboxCanonicalUInt64,
    lastPosition: SignalboxCanonicalUInt64
  )
  case unknown(kind: String, payload: [String: SignalboxJSONValue])

  public init(from decoder: Decoder) throws {
    let tagged = try SignalboxTaggedPayload(from: decoder)
    switch tagged.kind {
    case "session_not_found":
      try tagged.rejectUnadmittedFields(["type", "session_id"], decoder: decoder)
      self = .sessionNotFound(sessionID: try decoder.decode("session_id"))
    case "active_turn_present":
      try tagged.rejectUnadmittedFields(
        ["type", "session_id", "active_turn_id"],
        decoder: decoder
      )
      self = .activeTurnPresent(
        sessionID: try decoder.decode("session_id"),
        activeTurnID: try decoder.decode("active_turn_id"))
    case "active_turn_mismatch":
      try tagged.rejectUnadmittedFields(
        ["type", "session_id", "expected_active_turn_id", "active_turn_id"],
        decoder: decoder
      )
      self = .activeTurnMismatch(
        sessionID: try decoder.decode("session_id"),
        expectedActiveTurnID: try decoder.decode("expected_active_turn_id"),
        activeTurnID: try decoder.decode("active_turn_id")
      )
    case "no_active_turn":
      try tagged.rejectUnadmittedFields(
        ["type", "session_id", "expected_active_turn_id"],
        decoder: decoder
      )
      self = .noActiveTurn(
        sessionID: try decoder.decode("session_id"),
        expectedActiveTurnID: try decoder.decode("expected_active_turn_id")
      )
    case "turn_not_awaiting_reconciliation":
      try tagged.rejectUnadmittedFields(
        ["type", "session_id", "turn_id"],
        decoder: decoder
      )
      self = .turnNotAwaitingReconciliation(
        sessionID: try decoder.decode("session_id"),
        turnID: try decoder.decode("turn_id")
      )
    case "interrupt_already_applied":
      try tagged.rejectUnadmittedFields(
        ["type", "session_id", "active_turn_id", "existing_command_id"],
        decoder: decoder
      )
      self = .interruptAlreadyApplied(
        sessionID: try decoder.decode("session_id"),
        activeTurnID: try decoder.decode("active_turn_id"),
        existingCommandID: try decoder.decode("existing_command_id")
      )
    case "interrupt_unavailable_while_awaiting_approval":
      try tagged.rejectUnadmittedFields(
        ["type", "session_id", "active_turn_id"],
        decoder: decoder
      )
      self = .interruptUnavailableWhileAwaitingApproval(
        sessionID: try decoder.decode("session_id"),
        activeTurnID: try decoder.decode("active_turn_id")
      )
    case "safe_point_unavailable_while_stopping":
      try tagged.rejectUnadmittedFields(
        ["type", "session_id", "active_turn_id", "existing_command_id"],
        decoder: decoder
      )
      self = .safePointUnavailableWhileStopping(
        sessionID: try decoder.decode("session_id"),
        activeTurnID: try decoder.decode("active_turn_id"),
        existingCommandID: try decoder.decode("existing_command_id")
      )
    case "tool_request_not_found":
      try tagged.rejectUnadmittedFields(
        ["type", "tool_request_id"],
        decoder: decoder
      )
      self = .toolRequestNotFound(toolRequestID: try decoder.decode("tool_request_id"))
    case "tool_request_already_resolved":
      try tagged.rejectUnadmittedFields(
        ["type", "tool_request_id"],
        decoder: decoder
      )
      self = .toolRequestAlreadyResolved(
        toolRequestID: try decoder.decode("tool_request_id"))
    case "tool_request_not_earliest_undecided":
      try tagged.rejectUnadmittedFields(
        ["type", "tool_request_id", "earliest_tool_request_id"],
        decoder: decoder
      )
      self = .toolRequestNotEarliestUndecided(
        toolRequestID: try decoder.decode("tool_request_id"),
        earliestToolRequestID: try decoder.decode("earliest_tool_request_id")
      )
    case "tool_request_not_in_session":
      try tagged.rejectUnadmittedFields(
        ["type", "session_id", "tool_request_id"],
        decoder: decoder
      )
      self = .toolRequestNotInSession(
        sessionID: try decoder.decode("session_id"),
        toolRequestID: try decoder.decode("tool_request_id")
      )
    case "defaults_version_mismatch":
      try tagged.rejectUnadmittedFields(
        ["type", "session_id", "expected", "current"],
        decoder: decoder
      )
      self = .defaultsVersionMismatch(
        sessionID: try decoder.decode("session_id"),
        expected: try decoder.decode("expected"),
        current: try decoder.decode("current")
      )
    case "unknown_model_alias":
      try tagged.rejectUnadmittedFields(
        ["type", "session_id", "alias_id"],
        decoder: decoder
      )
      self = .unknownModelAlias(
        sessionID: try decoder.decode("session_id"), aliasID: try decoder.decode("alias_id"))
    case "acceptance_position_exhausted":
      try tagged.rejectUnadmittedFields(
        ["type", "session_id", "last"],
        decoder: decoder
      )
      self = .acceptancePositionExhausted(
        sessionID: try decoder.decode("session_id"), last: try decoder.decode("last"))
    case "defaults_version_exhausted":
      try tagged.rejectUnadmittedFields(
        ["type", "session_id", "current"],
        decoder: decoder
      )
      self = .defaultsVersionExhausted(
        sessionID: try decoder.decode("session_id"),
        current: try decoder.decode("current")
      )
    case "imported_conversation_not_found":
      try tagged.rejectUnadmittedFields(
        ["type", "imported_conversation_id"],
        decoder: decoder
      )
      self = .importedConversationNotFound(
        importedConversationID: try decoder.decode("imported_conversation_id"))
    case "imported_frontier_position_out_of_range":
      try tagged.rejectUnadmittedFields(
        [
          "type",
          "imported_conversation_id",
          "requested_position",
          "last_position",
        ],
        decoder: decoder
      )
      self = .importedFrontierPositionOutOfRange(
        importedConversationID: try decoder.decode("imported_conversation_id"),
        requestedPosition: try decoder.decode("requested_position"),
        lastPosition: try decoder.decode("last_position")
      )
    default:
      self = .unknown(kind: tagged.kind, payload: tagged.payload)
    }
  }
}

private struct SignalboxTaggedPayload: Decodable {
  let kind: String
  let payload: [String: SignalboxJSONValue]

  init(from decoder: Decoder) throws {
    try decoder.rejectDuplicateObjectMembers()
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

  func requireFields(
    _ requiredFields: Set<String>,
    decoder: Decoder
  ) throws {
    guard
      let field = requiredFields.sorted().first(where: { payload[$0] == nil })
    else {
      return
    }
    throw DecodingError.keyNotFound(
      SignalboxDynamicCodingKey(field),
      .init(
        codingPath: decoder.codingPath,
        debugDescription: "Tagged object is missing a required field."
      )
    )
  }
}

/// The decoded members of a closed object that carries no `type` discriminator.
///
/// A tagged variant names its admitted fields through ``SignalboxTaggedPayload``
/// once its discriminator selects the shape. A nested record such as
/// `current_model_call` or `terminal_model_call` has one shape and no
/// discriminator, so it names its admitted fields directly and rejects every
/// other member rather than letting a synthesized decoder discard it.
private struct SignalboxUntaggedPayload: Decodable {
  let payload: [String: SignalboxJSONValue]

  init(from decoder: Decoder) throws {
    try decoder.rejectDuplicateObjectMembers()
    payload = try decoder.singleValueContainer().decode([String: SignalboxJSONValue].self)
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
        debugDescription: "Closed object contains an unadmitted field."
      )
    )
  }

  func requireFields(
    _ requiredFields: Set<String>,
    decoder: Decoder
  ) throws {
    guard
      let field = requiredFields.sorted().first(where: { payload[$0] == nil })
    else {
      return
    }
    throw DecodingError.keyNotFound(
      SignalboxDynamicCodingKey(field),
      .init(
        codingPath: decoder.codingPath,
        debugDescription: "Closed object is missing a required field."
      )
    )
  }
}

extension CodingUserInfoKey {
  fileprivate static let signalboxDuplicateObjectPaths = CodingUserInfoKey(
    rawValue: "org.signalbox.process-protocol.duplicate-object-paths"
  )!
}

extension Decoder {
  fileprivate var containsDuplicateObjectMembers: Bool {
    guard
      let duplicateObjectPaths =
        userInfo[.signalboxDuplicateObjectPaths] as? Set<[String]>
    else {
      return false
    }
    return duplicateObjectPaths.contains(decodedObjectPath)
  }

  fileprivate func rejectDuplicateObjectMembers() throws {
    guard containsDuplicateObjectMembers else {
      return
    }
    throw duplicateObjectMembersError()
  }

  fileprivate func duplicateObjectMembersError() -> DecodingError {
    .dataCorrupted(
      .init(
        codingPath: codingPath,
        debugDescription: "Object contains a repeated decoded member name."
      )
    )
  }

  private var decodedObjectPath: [String] {
    codingPath.map { key in
      key.intValue.map { "[\($0)]" } ?? key.stringValue
    }
  }
}

private struct SignalboxJSONDuplicateMemberScanner {
  private let bytes: [UInt8]
  private let stringDecoder = SignalboxJSONCoding.decoder()
  private var index = 0
  private var duplicateObjectPaths: Set<[String]> = []

  init(data: Data) {
    bytes = Array(data)
  }

  mutating func scan() throws -> Set<[String]> {
    skipWhitespace()
    try scanValue(path: [], containerDepth: 0)
    skipWhitespace()
    guard index == bytes.count else {
      throw malformedJSON()
    }
    return duplicateObjectPaths
  }

  private mutating func scanValue(
    path: [String],
    containerDepth: Int
  ) throws {
    guard let byte = currentByte else {
      throw malformedJSON()
    }
    switch byte {
    case UInt8(ascii: "{"):
      guard containerDepth < Self.maximumContainerDepth else {
        throw excessiveContainerDepth()
      }
      try scanObject(path: path, containerDepth: containerDepth + 1)
    case UInt8(ascii: "["):
      guard containerDepth < Self.maximumContainerDepth else {
        throw excessiveContainerDepth()
      }
      try scanArray(path: path, containerDepth: containerDepth + 1)
    case UInt8(ascii: "\""):
      _ = try skipString()
    default:
      try scanPrimitive()
    }
  }

  private mutating func scanObject(
    path: [String],
    containerDepth: Int
  ) throws {
    index += 1
    skipWhitespace()
    if consume(UInt8(ascii: "}")) {
      return
    }
    var members: Set<String> = []
    while true {
      let member = try scanString()
      if !members.insert(member).inserted {
        recordDuplicateObject(at: path)
      }
      skipWhitespace()
      guard consume(UInt8(ascii: ":")) else {
        throw malformedJSON()
      }
      skipWhitespace()
      try scanValue(path: path + [member], containerDepth: containerDepth)
      skipWhitespace()
      if consume(UInt8(ascii: "}")) {
        return
      }
      guard consume(UInt8(ascii: ",")) else {
        throw malformedJSON()
      }
      skipWhitespace()
    }
  }

  private mutating func recordDuplicateObject(at path: [String]) {
    duplicateObjectPaths.insert(path)
    if path.first == "message" {
      duplicateObjectPaths.insert(["message"])
    }
  }

  private mutating func scanArray(
    path: [String],
    containerDepth: Int
  ) throws {
    index += 1
    skipWhitespace()
    if consume(UInt8(ascii: "]")) {
      return
    }
    var elementIndex = 0
    while true {
      try scanValue(
        path: path + ["[\(elementIndex)]"],
        containerDepth: containerDepth
      )
      elementIndex += 1
      skipWhitespace()
      if consume(UInt8(ascii: "]")) {
        return
      }
      guard consume(UInt8(ascii: ",")) else {
        throw malformedJSON()
      }
      skipWhitespace()
    }
  }

  private mutating func scanString() throws -> String {
    let encoded = Data(bytes[try skipString()])
    return try stringDecoder.decode(String.self, from: encoded)
  }

  private mutating func skipString() throws -> Range<Int> {
    guard currentByte == UInt8(ascii: "\"") else {
      throw malformedJSON()
    }
    let start = index
    index += 1
    var escaped = false
    while let byte = currentByte {
      index += 1
      if escaped {
        escaped = false
      } else if byte == UInt8(ascii: "\\") {
        escaped = true
      } else if byte == UInt8(ascii: "\"") {
        return start..<index
      }
    }
    throw malformedJSON()
  }

  private mutating func scanPrimitive() throws {
    let start = index
    while let byte = currentByte,
      !Self.primitiveDelimiters.contains(byte)
    {
      index += 1
    }
    guard index > start else {
      throw malformedJSON()
    }
  }

  private mutating func skipWhitespace() {
    while let byte = currentByte,
      Self.whitespace.contains(byte)
    {
      index += 1
    }
  }

  private mutating func consume(_ byte: UInt8) -> Bool {
    guard currentByte == byte else {
      return false
    }
    index += 1
    return true
  }

  private var currentByte: UInt8? {
    bytes.indices.contains(index) ? bytes[index] : nil
  }

  private static let whitespace: Set<UInt8> = [
    UInt8(ascii: " "),
    UInt8(ascii: "\t"),
    UInt8(ascii: "\n"),
    UInt8(ascii: "\r"),
  ]

  private static let primitiveDelimiters = whitespace.union([
    UInt8(ascii: ","),
    UInt8(ascii: "]"),
    UInt8(ascii: "}"),
  ])

  private static let maximumContainerDepth = 127

  private func malformedJSON() -> DecodingError {
    .dataCorrupted(
      .init(
        codingPath: [],
        debugDescription: "Process frame was not one complete JSON value."
      )
    )
  }

  private func excessiveContainerDepth() -> DecodingError {
    .dataCorrupted(
      .init(
        codingPath: [],
        debugDescription: "Process frame exceeded 127 simultaneously open JSON containers."
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
