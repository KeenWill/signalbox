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
        return "Imported \(conversationID.rawValue.prefix(8))"
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

public struct SignalboxProcessMessageEvent: Codable, Equatable, Sendable {
  private enum CodingKeys: String, CodingKey {
    case kind
    case role
    case text
    case unrecognizedKind
  }

  public let kind: String
  public let role: SignalboxMessageRole
  public let text: String
  public let unrecognizedKind: String?

  public init(
    role: SignalboxMessageRole,
    text: String,
    unrecognizedKind: String? = nil
  ) {
    self.kind = "process_message"
    self.role = role
    self.text = text
    self.unrecognizedKind = unrecognizedKind.map {
      SignalboxProcessPresentation.retainedLabel($0)
    }
  }

  public init(from decoder: Decoder) throws {
    let container = try decoder.container(keyedBy: CodingKeys.self)
    self.kind = try container.decode(String.self, forKey: .kind)
    self.role = try container.decode(SignalboxMessageRole.self, forKey: .role)
    self.text = try container.decode(String.self, forKey: .text)
    self.unrecognizedKind = try container.decodeIfPresent(
      String.self,
      forKey: .unrecognizedKind
    ).map {
      SignalboxProcessPresentation.retainedLabel($0)
    }
  }

  public func encode(to encoder: Encoder) throws {
    var container = encoder.container(keyedBy: CodingKeys.self)
    try container.encode(kind, forKey: .kind)
    try container.encode(role, forKey: .role)
    try container.encode(text, forKey: .text)
    try container.encodeIfPresent(unrecognizedKind, forKey: .unrecognizedKind)
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

public struct SignalboxProcessToolEvent: Codable, Equatable, Sendable {
  public let kind: String
  public let toolRequestID: SignalboxToolInvocationID
  public let toolName: String
  public let arguments: String?
  public let output: String?
  public let status: SignalboxProcessToolStatus

  public init(
    toolRequestID: SignalboxToolInvocationID,
    toolName: String,
    arguments: String?,
    output: String?,
    status: SignalboxProcessToolStatus
  ) {
    self.kind = "process_tool"
    self.toolRequestID = toolRequestID
    self.toolName = toolName
    self.arguments = arguments
    self.output = output
    self.status = status
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
