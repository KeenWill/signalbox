import Foundation

public enum SignalboxProcessPresentation {
  public static let maximumLabelUTF8Bytes = 4 * 1_024

  /// Bounds protocol-derived labels so unbounded retained tokens cannot exhaust memory or
  /// stall SwiftUI layout.
  public static func retainedLabel(_ label: String) -> String {
    retainedLabel(label, maximumUTF8Bytes: maximumLabelUTF8Bytes)
  }

  /// Bounds protocol-derived labels while preserving a diagnostic suffix so retained
  /// tokens cannot exhaust memory or stall SwiftUI layout.
  public static func retainedLabel(_ label: String, preserving suffix: String) -> String {
    let retainedSuffix = retainedLabel(suffix)
    let maximumPrefixUTF8Bytes = maximumLabelUTF8Bytes - retainedSuffix.utf8.count
    return retainedLabel(label, maximumUTF8Bytes: maximumPrefixUTF8Bytes) + retainedSuffix
  }

  private static func retainedLabel(_ label: String, maximumUTF8Bytes: Int) -> String {
    let scalars = label.unicodeScalars
    var retainedEnd = scalars.startIndex
    var retainedBytes = 0
    while retainedEnd != scalars.endIndex {
      let scalarBytes = scalars[retainedEnd].utf8.count
      guard retainedBytes + scalarBytes <= maximumUTF8Bytes else {
        break
      }
      retainedBytes += scalarBytes
      retainedEnd = scalars.index(after: retainedEnd)
    }
    return String(scalars[..<retainedEnd])
  }
}

public struct SignalboxProcessSession: Identifiable, Equatable, Sendable {
  public let id: SignalboxCanonicalUUID
  public let defaultsVersion: SignalboxCanonicalUInt64
  public let modelSelection: SignalboxModelSelection
  public let dangerousToolAutoApproval: Bool
  public let title: String?
  public let tags: [String]
  public let archived: Bool

  public init(summary: SignalboxProcessSessionMetadataSummary) {
    self.id = summary.sessionID
    self.defaultsVersion = summary.defaultsVersion
    self.modelSelection = summary.modelSelection
    self.dangerousToolAutoApproval = summary.dangerousToolAutoApproval
    self.title = summary.title
    self.tags = summary.tags
    self.archived = summary.archived
  }

  public init(
    id: SignalboxCanonicalUUID,
    defaults: SignalboxSessionDefaultsRead,
    metadata: SignalboxProcessSessionMetadata
  ) {
    self.id = id
    self.defaultsVersion = defaults.defaultsVersion
    self.modelSelection = defaults.modelSelection
    self.dangerousToolAutoApproval = defaults.dangerousToolAutoApproval
    self.title = metadata.title
    self.tags = metadata.tags
    self.archived = metadata.archived
  }

  public var displayTitle: String {
    guard let title, !title.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
      return "Session \(id.rawValue.prefix(8))"
    }
    return title
  }

  public var modelSelectionLabel: String {
    switch modelSelection {
    case .direct(let selectionID):
      return "Direct \(selectionID.rawValue.prefix(8))"
    case .alias(let aliasID):
      return "Alias \(aliasID.rawValue.prefix(8))"
    }
  }
}

public enum SignalboxProcessConversationOrigin: String, Equatable, Sendable {
  case native
  case imported
}

public struct SignalboxProcessConversation: Identifiable, Equatable, Sendable {
  public enum Record: Equatable, Sendable {
    case native(SignalboxNativeConversationSummary)
    case imported(SignalboxImportedConversationSummary)
  }

  public let record: Record

  public init(summary: SignalboxConversationSummary) {
    switch summary {
    case .native(let native):
      record = .native(native)
    case .imported(let imported):
      record = .imported(imported)
    }
  }

  public var id: String {
    // Native and imported records carry UUIDs from independent namespaces.
    // SwiftUI selection therefore needs the origin prefix to prevent aliasing.
    switch record {
    case .native(let native):
      return "native-\(native.sessionID.rawValue)"
    case .imported(let imported):
      return "imported-\(imported.importedConversationID.rawValue)"
    }
  }

  public var origin: SignalboxProcessConversationOrigin {
    switch record {
    case .native:
      return .native
    case .imported:
      return .imported
    }
  }

  public var conversationID: SignalboxCanonicalUUID {
    switch record {
    case .native(let native):
      return native.sessionID
    case .imported(let imported):
      return imported.importedConversationID
    }
  }

  public var title: String? {
    switch record {
    case .native(let native):
      return native.title
    case .imported(let imported):
      return imported.title
    }
  }

  public var displayTitle: String {
    guard let title, !title.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
      switch origin {
      case .native:
        return "Session \(conversationID.rawValue.prefix(8))"
      case .imported:
        return "Untitled imported conversation \(conversationID.rawValue.prefix(8))"
      }
    }
    return title
  }

  public var archived: Bool {
    guard case .native(let native) = record else {
      return false
    }
    return native.archived
  }

  public var defaultsVersion: SignalboxCanonicalUInt64? {
    guard case .native(let native) = record else {
      return nil
    }
    return native.defaultsVersion
  }

  public var importedEntryCount: SignalboxCanonicalUInt64? {
    guard case .imported(let imported) = record else {
      return nil
    }
    return imported.entryCount
  }

  public var importedSourceFormat: SignalboxImportedConversationSourceFormat? {
    guard case .imported(let imported) = record else {
      return nil
    }
    return imported.sourceFormat
  }
}

extension SignalboxImportedConversationEntry: Identifiable {
  public var id: String {
    importedEntryID.rawValue
  }

  public var sourceSpeakerLabel: String {
    switch sourceSpeaker {
    case .notAttested:
      return "Speaker not attested"
    case .attestedAbsent:
      return "Speaker absent"
    case .attested(speaker: .user):
      return "User"
    case .attested(speaker: .assistant):
      return "Assistant"
    case .attested(speaker: .unknown(let value)):
      return SignalboxProcessPresentation.retainedLabel(
        "Unrecognized speaker (\(value))"
      )
    case .unknown(let kind, _):
      return SignalboxProcessPresentation.retainedLabel(
        "Unknown speaker (\(kind))"
      )
    }
  }

  public var contentKindLabel: String {
    switch contentKind {
    case .sourceEvent:
      return "Source event"
    case .sourceMessageBlock:
      return "Message block"
    case .text:
      return "Text"
    case .toolCall:
      return "Tool call"
    case .toolResult:
      return "Tool result"
    case .thinking:
      return "Thinking"
    case .redactedThinking:
      return "Redacted thinking"
    case .document:
      return "Document"
    case .messageContentAbsent:
      return "Message content absent"
    case .unknown(let value):
      return SignalboxProcessPresentation.retainedLabel(
        "Unrecognized content (\(value))"
      )
    }
  }
}

public struct SignalboxImportedConversationTranscript: Equatable, Sendable {
  public let importedConversationID: SignalboxCanonicalUUID
  public let entries: [SignalboxImportedConversationEntry]

  public init(
    importedConversationID: SignalboxCanonicalUUID,
    entries: [SignalboxImportedConversationEntry]
  ) {
    self.importedConversationID = importedConversationID
    self.entries = entries
  }

  public var entryCount: SignalboxCanonicalUInt64 {
    SignalboxCanonicalUInt64(rawValue: UInt64(entries.count))
  }
}

public struct SignalboxProcessStreamedText: Identifiable, Equatable, Sendable {
  public let sessionID: SignalboxCanonicalUUID
  public let turnID: SignalboxCanonicalUUID
  public let modelCallID: SignalboxCanonicalUUID
  public private(set) var text: String

  public init(delta: SignalboxProviderTextDelta) {
    sessionID = delta.sessionID
    turnID = delta.turnID
    modelCallID = delta.modelCallID
    text = delta.content
  }

  public var id: String {
    "\(turnID.rawValue)-\(modelCallID.rawValue)"
  }

  @discardableResult
  public mutating func append(_ delta: SignalboxProviderTextDelta) -> Bool {
    // Provider deltas are not retained by the snapshot capacity guard, so the
    // presentation accumulator enforces its own independent heap bound.
    let (retainedBytes, overflowed) = text.utf8.count.addingReportingOverflow(
      delta.content.utf8.count
    )
    guard
      !overflowed,
      retainedBytes <= SignalboxProcessProtocol.maximumStreamedTextUTF8Bytes
    else {
      return false
    }
    text += delta.content
    return true
  }
}

public struct SignalboxProcessPendingInput: Identifiable, Equatable, Sendable {
  public let id: SignalboxCanonicalUUID
  public let turnID: SignalboxCanonicalUUID
  public let acceptancePosition: SignalboxCanonicalUInt64
  public let content: String

  public init(
    id: SignalboxCanonicalUUID,
    turnID: SignalboxCanonicalUUID,
    acceptancePosition: SignalboxCanonicalUInt64,
    content: String
  ) {
    self.id = id
    self.turnID = turnID
    self.acceptancePosition = acceptancePosition
    self.content = content
  }
}

public enum SignalboxProcessActivityState: String, Equatable, Sendable {
  case unavailable
  case queued
  case running
  case waitingForToolDecision
  case recoveryRequired
  case failed
  case completed
  case refused
  case cancelled
}

public struct SignalboxProcessActivity: Equatable, Sendable {
  public let state: SignalboxProcessActivityState
  public let label: String

  public init(state: SignalboxProcessActivityState, label: String) {
    self.state = state
    self.label = label
  }

  public static let unavailable = Self(
    state: .unavailable,
    label: "Open for current state"
  )
}

public enum SignalboxProcessMessageSourceAttribution: String, Codable, Equatable, Sendable {
  case importedUserRole = "imported_user_role"
  case importedAssistantRole = "imported_assistant_role"
  case importedSpeakerNotAttested = "imported_speaker_not_attested"
  case importedSpeakerAbsent = "imported_speaker_absent"

  public var presentationLabel: String {
    switch self {
    case .importedUserRole:
      return "User role"
    case .importedAssistantRole:
      return "Assistant role"
    case .importedSpeakerNotAttested:
      return "Speaker not attested"
    case .importedSpeakerAbsent:
      return "Speaker absent"
    }
  }

  public var role: SignalboxMessageRole {
    switch self {
    case .importedUserRole:
      return .user
    case .importedAssistantRole:
      return .assistant
    case .importedSpeakerNotAttested, .importedSpeakerAbsent:
      return .unknown
    }
  }
}

public struct SignalboxProcessMessageEvent: Codable, Equatable, Sendable {
  private enum CodingKeys: String, CodingKey {
    case kind
    case role
    case text
    case unrecognizedKind = "unrecognized_kind"
    case sourceAttribution = "source_attribution"
  }

  public let kind: String
  public let role: SignalboxMessageRole
  public let text: String
  public let unrecognizedKind: String?
  public let sourceAttribution: SignalboxProcessMessageSourceAttribution?

  public init(
    role: SignalboxMessageRole,
    text: String,
    unrecognizedKind: String? = nil,
    sourceAttribution: SignalboxProcessMessageSourceAttribution? = nil
  ) {
    let retainedUnrecognizedKind = sourceAttribution == nil
      ? unrecognizedKind.map { SignalboxProcessPresentation.retainedLabel($0) }
      : nil
    self.kind = "process_message"
    self.role = retainedUnrecognizedKind == nil ? sourceAttribution?.role ?? role : .unknown
    self.text = text
    self.unrecognizedKind = retainedUnrecognizedKind
    self.sourceAttribution = sourceAttribution
  }

  public init(from decoder: Decoder) throws {
    let container = try decoder.container(keyedBy: CodingKeys.self)
    self.kind = try container.decode(String.self, forKey: .kind)
    let role = try container.decode(SignalboxMessageRole.self, forKey: .role)
    self.text = try container.decode(String.self, forKey: .text)
    self.unrecognizedKind = try container.decodeIfPresent(
      String.self,
      forKey: .unrecognizedKind
    ).map {
      SignalboxProcessPresentation.retainedLabel($0)
    }
    let sourceAttribution = try container.decodeIfPresent(
      SignalboxProcessMessageSourceAttribution.self,
      forKey: .sourceAttribution
    )
    guard sourceAttribution?.role == role || sourceAttribution == nil else {
      throw DecodingError.dataCorruptedError(
        forKey: .sourceAttribution,
        in: container,
        debugDescription: "Imported source attribution contradicts the message role."
      )
    }
    guard unrecognizedKind == nil || sourceAttribution == nil else {
      throw DecodingError.dataCorruptedError(
        forKey: .sourceAttribution,
        in: container,
        debugDescription: "Imported source attribution contradicts unrecognized speaker evidence."
      )
    }
    guard unrecognizedKind == nil || role == .unknown else {
      throw DecodingError.dataCorruptedError(
        forKey: .role,
        in: container,
        debugDescription: "Unrecognized speaker evidence requires the unknown message role."
      )
    }
    self.role = role
    self.sourceAttribution = sourceAttribution
  }

  public func encode(to encoder: Encoder) throws {
    var container = encoder.container(keyedBy: CodingKeys.self)
    try container.encode(kind, forKey: .kind)
    try container.encode(role, forKey: .role)
    try container.encode(text, forKey: .text)
    try container.encodeIfPresent(unrecognizedKind, forKey: .unrecognizedKind)
    try container.encodeIfPresent(sourceAttribution, forKey: .sourceAttribution)
  }
}

public struct SignalboxProcessContextSummaryEvent: Codable, Equatable, Sendable {
  public let kind: String
  public let text: String

  public init(text: String) {
    self.kind = "process_context_summary"
    self.text = text
  }

  private enum CodingKeys: String, CodingKey {
    case kind
    case text
  }

  public init(from decoder: Decoder) throws {
    let container = try decoder.container(keyedBy: CodingKeys.self)
    let kind = try container.decode(String.self, forKey: .kind)
    guard kind == "process_context_summary" else {
      throw DecodingError.dataCorruptedError(
        forKey: .kind,
        in: container,
        debugDescription: "The value is not context-summary evidence."
      )
    }
    self.kind = kind
    self.text = try container.decode(String.self, forKey: .text)
  }

  init(closedFrom decoder: Decoder) throws {
    let payload = try SignalboxUntaggedPayload(from: decoder)
    try payload.rejectUnadmittedFields(["kind", "text"], decoder: decoder)
    self = try Self(from: decoder)
  }
}

public struct SignalboxProcessModelIdentityEvent: Codable, Equatable, Sendable {
  public let kind: String
  public let turnID: SignalboxCanonicalUUID
  public let defaultsVersion: SignalboxCanonicalUInt64
  public let selectedModelID: SignalboxCanonicalUUID

  public init(
    turnID: SignalboxCanonicalUUID,
    defaultsVersion: SignalboxCanonicalUInt64,
    selectedModelID: SignalboxCanonicalUUID
  ) throws {
    guard defaultsVersion.rawValue > 0 else {
      throw DecodingError.dataCorrupted(
        .init(
          codingPath: [],
          debugDescription: "Model-identity defaults version must be positive."
        )
      )
    }
    self.kind = "process_model_identity"
    self.turnID = turnID
    self.defaultsVersion = defaultsVersion
    self.selectedModelID = selectedModelID
  }

  private enum CodingKeys: String, CodingKey {
    case kind
    case turnID = "turn_id"
    case defaultsVersion = "defaults_version"
    case selectedModelID = "selected_model_id"
  }

  public init(from decoder: Decoder) throws {
    let container = try decoder.container(keyedBy: CodingKeys.self)
    let kind = try container.decode(String.self, forKey: .kind)
    guard kind == "process_model_identity" else {
      throw DecodingError.dataCorruptedError(
        forKey: .kind,
        in: container,
        debugDescription: "The value is not model-identity evidence."
      )
    }
    try self.init(
      turnID: container.decode(SignalboxCanonicalUUID.self, forKey: .turnID),
      defaultsVersion: container.decode(
        SignalboxCanonicalUInt64.self,
        forKey: .defaultsVersion
      ),
      selectedModelID: container.decode(SignalboxCanonicalUUID.self, forKey: .selectedModelID)
    )
  }

  init(closedFrom decoder: Decoder) throws {
    let payload = try SignalboxUntaggedPayload(from: decoder)
    try payload.rejectUnadmittedFields(
      ["kind", "turn_id", "defaults_version", "selected_model_id"],
      decoder: decoder
    )
    self = try Self(from: decoder)
  }
}

public struct SignalboxProcessRunnerPlacementEvent: Codable, Equatable, Sendable {
  public let kind: String
  public let priorRunnerID: SignalboxCanonicalUUID
  public let newRunnerID: SignalboxCanonicalUUID
  public let placementRevision: SignalboxCanonicalUInt64
  public let sandboxProfile: SignalboxRunnerSandboxProfile

  public init(
    priorRunnerID: SignalboxCanonicalUUID,
    newRunnerID: SignalboxCanonicalUUID,
    placementRevision: SignalboxCanonicalUInt64,
    sandboxProfile: SignalboxRunnerSandboxProfile
  ) throws {
    guard placementRevision.rawValue > 0 else {
      throw DecodingError.dataCorrupted(
        .init(
          codingPath: [],
          debugDescription: "Runner-placement revision must be positive."
        )
      )
    }
    self.kind = "process_runner_placement"
    self.priorRunnerID = priorRunnerID
    self.newRunnerID = newRunnerID
    self.placementRevision = placementRevision
    self.sandboxProfile = sandboxProfile
  }

  private enum CodingKeys: String, CodingKey {
    case kind
    case priorRunnerID = "prior_runner_id"
    case newRunnerID = "new_runner_id"
    case placementRevision = "placement_revision"
    case sandboxProfile = "sandbox_profile"
  }

  public init(from decoder: Decoder) throws {
    let container = try decoder.container(keyedBy: CodingKeys.self)
    let kind = try container.decode(String.self, forKey: .kind)
    guard kind == "process_runner_placement" else {
      throw DecodingError.dataCorruptedError(
        forKey: .kind,
        in: container,
        debugDescription: "The value is not runner-placement evidence."
      )
    }
    try self.init(
      priorRunnerID: container.decode(SignalboxCanonicalUUID.self, forKey: .priorRunnerID),
      newRunnerID: container.decode(SignalboxCanonicalUUID.self, forKey: .newRunnerID),
      placementRevision: container.decode(
        SignalboxCanonicalUInt64.self,
        forKey: .placementRevision
      ),
      sandboxProfile: container.decode(
        SignalboxRunnerSandboxProfile.self,
        forKey: .sandboxProfile
      )
    )
  }

  init(closedFrom decoder: Decoder) throws {
    let payload = try SignalboxUntaggedPayload(from: decoder)
    try payload.rejectUnadmittedFields(
      [
        "kind", "prior_runner_id", "new_runner_id", "placement_revision",
        "sandbox_profile",
      ],
      decoder: decoder
    )
    self = try Self(from: decoder)
  }
}

public struct SignalboxProcessModelCallUsageEvent: Codable, Equatable, Sendable {
  public let kind: String
  public let turnID: SignalboxCanonicalUUID
  public let modelCallID: SignalboxCanonicalUUID
  public let usageProvenance: String
  public let inputTokens: SignalboxCanonicalUInt64?
  public let outputTokens: SignalboxCanonicalUInt64?
  public let cacheCreationInputTokens: SignalboxCanonicalUInt64?
  public let cacheReadInputTokens: SignalboxCanonicalUInt64?
  public let costAmountUSD: String?
  public let costRateVersion: String?
  public let costLabel: String?

  public init(evidence: SignalboxTranscriptModelCallUsage) {
    self.kind = "process_model_call_usage"
    self.turnID = evidence.turnID
    self.modelCallID = evidence.modelCallID
    self.usageProvenance = evidence.usageProvenance.rawValue
    self.inputTokens = evidence.usage.inputTokens
    self.outputTokens = evidence.usage.outputTokens
    self.cacheCreationInputTokens = evidence.usage.cacheCreationInputTokens
    self.cacheReadInputTokens = evidence.usage.cacheReadInputTokens
    self.costAmountUSD = evidence.cost?.amountUSD.rawValue
    self.costRateVersion = evidence.cost?.rateVersion.rawValue
    self.costLabel = evidence.cost?.label.rawValue
  }

  var hasAtomicCostFields: Bool {
    let count = [costAmountUSD, costRateVersion, costLabel].compactMap { $0 }.count
    return count == 0 || count == 3
  }

  private enum CodingKeys: String, CodingKey {
    case kind
    case turnID = "turn_id"
    case modelCallID = "model_call_id"
    case usageProvenance = "usage_provenance"
    case inputTokens = "input_tokens"
    case outputTokens = "output_tokens"
    case cacheCreationInputTokens = "cache_creation_input_tokens"
    case cacheReadInputTokens = "cache_read_input_tokens"
    case costAmountUSD = "cost_amount_usd"
    case costRateVersion = "cost_rate_version"
    case costLabel = "cost_label"
  }

  public init(from decoder: Decoder) throws {
    let container = try decoder.container(keyedBy: CodingKeys.self)
    let kind = try container.decode(String.self, forKey: .kind)
    guard kind == "process_model_call_usage" else {
      throw DecodingError.dataCorruptedError(
        forKey: .kind,
        in: container,
        debugDescription: "The value is not model-usage evidence."
      )
    }
    let provenance = try container.decode(
      SignalboxUsageProvenance.self,
      forKey: .usageProvenance
    )
    let amount = try container.decodeIfPresent(
      SignalboxCanonicalDollarAmount.self,
      forKey: .costAmountUSD
    )
    let rateVersion = try container.decodeIfPresent(
      SignalboxBillingRateVersion.self,
      forKey: .costRateVersion
    )
    let label = try container.decodeIfPresent(
      SignalboxModelCallCostLabel.self,
      forKey: .costLabel
    )
    self.kind = kind
    self.turnID = try container.decode(SignalboxCanonicalUUID.self, forKey: .turnID)
    self.modelCallID = try container.decode(
      SignalboxCanonicalUUID.self,
      forKey: .modelCallID
    )
    self.usageProvenance = provenance.rawValue
    self.inputTokens = try container.decodeIfPresent(
      SignalboxCanonicalUInt64.self,
      forKey: .inputTokens
    )
    self.outputTokens = try container.decodeIfPresent(
      SignalboxCanonicalUInt64.self,
      forKey: .outputTokens
    )
    self.cacheCreationInputTokens = try container.decodeIfPresent(
      SignalboxCanonicalUInt64.self,
      forKey: .cacheCreationInputTokens
    )
    self.cacheReadInputTokens = try container.decodeIfPresent(
      SignalboxCanonicalUInt64.self,
      forKey: .cacheReadInputTokens
    )
    self.costAmountUSD = amount?.rawValue
    self.costRateVersion = rateVersion?.rawValue
    self.costLabel = label?.rawValue
    let hasUsage = inputTokens != nil || outputTokens != nil
      || cacheCreationInputTokens != nil || cacheReadInputTokens != nil
    guard hasAtomicCostFields, amount == nil || hasUsage else {
      throw DecodingError.dataCorrupted(
        .init(
          codingPath: decoder.codingPath,
          debugDescription: "Model-usage evidence contains invalid scalar relationships."
        )
      )
    }
  }

  init(closedFrom decoder: Decoder) throws {
    let payload = try SignalboxUntaggedPayload(from: decoder)
    try payload.rejectUnadmittedFields(
      [
        "kind", "turn_id", "model_call_id", "usage_provenance", "input_tokens",
        "output_tokens", "cache_creation_input_tokens", "cache_read_input_tokens",
        "cost_amount_usd", "cost_rate_version", "cost_label",
      ],
      decoder: decoder
    )
    self = try Self(from: decoder)
  }
}

public enum SignalboxProcessImportedContentKind: String, Codable, Equatable, Sendable {
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

public struct SignalboxProcessImportedContentEvent: Codable, Equatable, Sendable {
  public let kind: String
  public let contentKind: SignalboxProcessImportedContentKind
  public let sourceSpeaker: String

  public init(
    contentKind: SignalboxProcessImportedContentKind,
    sourceSpeaker: String
  ) {
    kind = "process_imported_content"
    self.contentKind = contentKind
    self.sourceSpeaker = SignalboxProcessPresentation.retainedLabel(sourceSpeaker)
  }

  private enum CodingKeys: String, CodingKey {
    case kind
    case contentKind = "content_kind"
    case sourceSpeaker = "source_speaker"
  }

  public init(from decoder: Decoder) throws {
    let container = try decoder.container(keyedBy: CodingKeys.self)
    let kind = try container.decode(String.self, forKey: .kind)
    guard kind == "process_imported_content" else {
      throw DecodingError.dataCorruptedError(
        forKey: .kind,
        in: container,
        debugDescription: "The value is not imported-content evidence."
      )
    }
    self.init(
      contentKind: try container.decode(
        SignalboxProcessImportedContentKind.self,
        forKey: .contentKind
      ),
      sourceSpeaker: try container.decode(String.self, forKey: .sourceSpeaker)
    )
  }

  init(closedFrom decoder: Decoder) throws {
    let payload = try SignalboxUntaggedPayload(from: decoder)
    try payload.rejectUnadmittedFields(
      ["kind", "content_kind", "source_speaker"],
      decoder: decoder
    )
    self = try Self(from: decoder)
  }
}

public enum SignalboxProcessToolStatus: String, Codable, Equatable, Sendable {
  case proposed
  case awaitingDecision = "awaiting_decision"
  case completed
  case denied
  case closed
  case recoveryRequired = "recovery_required"
}

public struct SignalboxProcessToolRequestPosition: Codable, Equatable, Sendable {
  public let turnID: SignalboxCanonicalUUID
  public let entryIndex: SignalboxCanonicalUInt64
  public let toolName: String
  public let toolAttemptID: SignalboxCanonicalUUID?
  public let toolOutput: String?

  public init(
    turnID: SignalboxCanonicalUUID,
    entryIndex: SignalboxCanonicalUInt64,
    toolName: String,
    toolAttemptID: SignalboxCanonicalUUID?,
    toolOutput: String?
  ) {
    self.turnID = turnID
    self.entryIndex = entryIndex
    self.toolName = toolName
    self.toolAttemptID = toolAttemptID
    self.toolOutput = toolOutput
  }
}

public struct SignalboxProcessToolEvent: Codable, Equatable, Sendable {
  public let kind: String
  public let toolRequestID: SignalboxToolInvocationID
  public let turnID: SignalboxCanonicalUUID?
  public let sessionTurnAcceptancePositions:
    [SignalboxCanonicalUUID: SignalboxCanonicalUInt64]?
  public let sessionToolRequestPositions:
    [SignalboxCanonicalUUID: SignalboxProcessToolRequestPosition]?
  public let toolAttemptID: SignalboxCanonicalUUID?
  public let toolName: String
  public let arguments: String?
  public let output: String?
  public let status: SignalboxProcessToolStatus

  public init(
    toolRequestID: SignalboxToolInvocationID,
    turnID: SignalboxCanonicalUUID? = nil,
    sessionTurnAcceptancePositions:
      [SignalboxCanonicalUUID: SignalboxCanonicalUInt64]? = nil,
    sessionToolRequestPositions:
      [SignalboxCanonicalUUID: SignalboxProcessToolRequestPosition]? = nil,
    toolAttemptID: SignalboxCanonicalUUID? = nil,
    toolName: String,
    arguments: String?,
    output: String?,
    status: SignalboxProcessToolStatus
  ) {
    self.kind = "process_tool"
    self.toolRequestID = toolRequestID
    self.turnID = turnID
    self.sessionTurnAcceptancePositions = sessionTurnAcceptancePositions
    self.sessionToolRequestPositions = sessionToolRequestPositions
    self.toolAttemptID = toolAttemptID
    self.toolName = toolName
    self.arguments = arguments
    self.output = output
    self.status = status
  }

  init(closedFrom decoder: Decoder) throws {
    let payload = try SignalboxUntaggedPayload(from: decoder)
    try payload.rejectUnadmittedFields(
      [
        "kind", "toolRequestID", "turnID", "sessionTurnAcceptancePositions", "toolAttemptID",
        "sessionToolRequestPositions", "toolName", "arguments", "output", "status",
      ],
      decoder: decoder
    )
    self = try Self(from: decoder)
  }
}

public struct SignalboxProcessTurnFailureEvent: Codable, Equatable, Sendable {
  public let kind: String
  public let reason: String

  public init(reason: String) {
    self.kind = "process_turn_failure"
    self.reason = reason
  }
}

public struct SignalboxProcessConservativeEvent: Codable, Equatable, Sendable {
  public let envelopeKind: String
  public let kind: String
  public let diagnostic: String

  public init(kind: String, diagnostic: String) {
    self.envelopeKind = "process_conservative"
    self.kind = kind
    self.diagnostic = diagnostic
  }

  private enum CodingKeys: String, CodingKey {
    case envelopeKind = "kind"
    case kind = "process_kind"
    case diagnostic
  }
}
