import Foundation

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
  public let kind: String
  public let role: SignalboxMessageRole
  public let text: String

  public init(role: SignalboxMessageRole, text: String) {
    self.kind = "process_message"
    self.role = role
    self.text = text
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
