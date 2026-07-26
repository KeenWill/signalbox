import Foundation

#if canImport(SignalboxModels)
  import SignalboxModels
#endif

public enum SignalboxProcessTranscriptProjectionError: LocalizedError, Equatable {
  case localIdentityExhausted
  case missingTriggerEvidence
  case missingTextContent
  case orphanedToolResult(String)

  public var errorDescription: String? {
    switch self {
    case .localIdentityExhausted:
      return "The native transcript presentation identity space was exhausted."
    case .missingTriggerEvidence:
      return "The side transcript snapshot omitted the durable evidence named by its trigger."
    case .missingTextContent:
      return "A text transcript entry ended without its required final content fragment."
    case .orphanedToolResult(let requestID):
      return "Tool result \(requestID) had no preceding tool-use projection."
    }
  }
}

public struct SignalboxProcessTranscriptProjection: Equatable, Sendable {
  public let records: [SignalboxStoredEvent]
  public let pendingInputs: [SignalboxProcessPendingInput]
  public let activity: SignalboxProcessActivity
  public let materializedAcceptedInputIDs: Set<SignalboxCanonicalUUID>

  public init(
    records: [SignalboxStoredEvent],
    pendingInputs: [SignalboxProcessPendingInput],
    activity: SignalboxProcessActivity,
    materializedAcceptedInputIDs: Set<SignalboxCanonicalUUID>
  ) {
    self.records = records
    self.pendingInputs = pendingInputs
    self.activity = activity
    self.materializedAcceptedInputIDs = materializedAcceptedInputIDs
  }
}

public struct SignalboxProcessTranscriptProjector: Sendable {
  private enum PresentationIdentity: Hashable, Sendable {
    case semantic(sourceSessionID: String, entryID: String)
    case toolRequest(String)
  }

  private struct ToolContext: Sendable {
    let turnID: SignalboxCanonicalUUID
    let modelCallID: SignalboxCanonicalUUID
  }

  private struct TextAssembly: Sendable {
    let message: SignalboxTranscriptTextEntryMessage
    var content = ""
  }

  private var presentationIDs: [PresentationIdentity: SignalboxEventID] = [:]
  private var toolsByRequestID: [String: SignalboxProcessToolEvent] = [:]
  private var toolContextsByRequestID: [String: ToolContext] = [:]

  public init() {}

  public mutating func projectAuthoritativeSnapshot(
    _ snapshot: SignalboxSynchronizationSnapshot
  ) throws -> SignalboxProcessTranscriptProjection {
    var candidate = self
    candidate.toolsByRequestID = [:]
    candidate.toolContextsByRequestID = [:]
    let projection = try candidate.project(snapshot, selection: .all)
    self = candidate
    return projection
  }

  public mutating func projectSideSnapshot(
    _ snapshot: SignalboxSynchronizationSnapshot,
    attributableTo trigger: SignalboxFollowedSessionEvent
  ) throws -> SignalboxProcessTranscriptProjection {
    var candidate = self
    guard candidate.containsRequiredEvidence(in: snapshot, for: trigger.event) else {
      throw SignalboxProcessTranscriptProjectionError.missingTriggerEvidence
    }
    let projection = try candidate.project(
      snapshot,
      selection: .trigger(trigger.event)
    )
    self = candidate
    return projection
  }

  private enum Selection {
    case all
    case trigger(SignalboxProcessSessionEvent)
  }

  private mutating func project(
    _ snapshot: SignalboxSynchronizationSnapshot,
    selection: Selection
  ) throws -> SignalboxProcessTranscriptProjection {
    var projectedByID: [SignalboxEventID: SignalboxStoredEvent] = [:]
    var projectedOrder: [SignalboxEventID] = []
    var pendingInputs: [SignalboxProcessPendingInput] = []
    var materializedAcceptedInputIDs: Set<SignalboxCanonicalUUID> = []
    var latestActivity = SignalboxProcessActivity.unavailable
    var activeActivity: SignalboxProcessActivity?
    var textAssembly: TextAssembly?
    var awaitingToolDecisionRequestID: String?

    for record in snapshot.records {
      switch record {
      case .turn(let turn):
        latestActivity = activity(for: turn.state)
        if turnStateIsActive(turn.state) {
          activeActivity = latestActivity
        }
        if case .queued(let acceptedInputID, let content) = turn.state {
          pendingInputs.append(
            SignalboxProcessPendingInput(
              id: acceptedInputID,
              turnID: turn.turnID,
              acceptancePosition: turn.acceptancePosition,
              content: content
            ))
        }
        if case .activeAwaitingToolApproval(let requestID) = turn.state {
          awaitingToolDecisionRequestID = requestID.rawValue
        }
      case .textEntry(let message):
        textAssembly = TextAssembly(message: message)
      case .content(let content):
        guard var assembly = textAssembly else {
          throw SignalboxProcessTranscriptProjectionError.missingTextContent
        }
        assembly.content += content.contentFragment
        if content.finalFragment {
          let projected = try projectText(
            assembly.message,
            content: assembly.content,
            selection: selection
          )
          if let projected {
            store(projected, in: &projectedByID, order: &projectedOrder)
            if case .user(let acceptedInputID, _) = assembly.message.entry {
              materializedAcceptedInputIDs.insert(acceptedInputID)
            }
          }
          textAssembly = nil
        } else {
          textAssembly = assembly
        }
      case .entry(let message):
        let projected = try projectEntry(
          message,
          awaitingToolDecisionRequestID: awaitingToolDecisionRequestID,
          selection: selection
        )
        if let projected {
          store(projected, in: &projectedByID, order: &projectedOrder)
        }
      }
    }
    guard textAssembly == nil else {
      throw SignalboxProcessTranscriptProjectionError.missingTextContent
    }
    pendingInputs.removeAll { materializedAcceptedInputIDs.contains($0.id) }
    return SignalboxProcessTranscriptProjection(
      records: projectedOrder.compactMap { projectedByID[$0] },
      pendingInputs: pendingInputs,
      activity: activeActivity ?? latestActivity,
      materializedAcceptedInputIDs: materializedAcceptedInputIDs
    )
  }

  private mutating func projectText(
    _ message: SignalboxTranscriptTextEntryMessage,
    content: String,
    selection: Selection
  ) throws -> SignalboxStoredEvent? {
    guard textIsSelected(message.entry, selection: selection) else {
      return nil
    }
    let role: SignalboxMessageRole
    switch message.entry {
    case .user:
      role = .user
    case .assistant:
      role = .assistant
    case .imported(_, _, let speaker):
      role = importedRole(speaker)
    case .unknown:
      role = .unknown
    }
    let eventID = try claimPresentationID(
      .semantic(
        sourceSessionID: message.sourceSessionID.rawValue,
        entryID: message.entryID.rawValue
      ),
      entryIndex: message.entryIndex
    )
    return SignalboxStoredEvent(
      eventID: eventID,
      event: .processMessage(
        SignalboxProcessMessageEvent(role: role, text: content)
      )
    )
  }

  private mutating func projectEntry(
    _ message: SignalboxTranscriptEntryMessage,
    awaitingToolDecisionRequestID: String?,
    selection: Selection
  ) throws -> SignalboxStoredEvent? {
    guard entryIsSelected(message, selection: selection) else {
      return nil
    }
    switch message.entry {
    case .assistantToolUse(
      let turnID,
      let modelCallID,
      let requestID,
      let toolName,
      let arguments
    ):
      let request = requestID.rawValue
      let event = SignalboxProcessToolEvent(
        toolRequestID: SignalboxToolInvocationID(rawValue: request),
        toolName: toolName,
        arguments: arguments,
        output: nil,
        status: .proposed
      )
      toolsByRequestID[request] = event
      toolContextsByRequestID[request] = ToolContext(
        turnID: turnID,
        modelCallID: modelCallID
      )
      let eventID = try claimPresentationID(
        .toolRequest(request),
        entryIndex: message.entryIndex
      )
      return toolRecord(
        event,
        awaitsDecision: request == awaitingToolDecisionRequestID,
        eventID: eventID
      )
    case .toolExecutionResult(let requestID, _, let content):
      return try updateTool(
        requestID: requestID.rawValue,
        output: content,
        status: .completed
      )
    case .toolDenied(let requestID, let content):
      return try updateTool(
        requestID: requestID.rawValue,
        output: content,
        status: .denied
      )
    case .toolClosed(let requestID, let content):
      return try updateTool(
        requestID: requestID.rawValue,
        output: content,
        status: .closed
      )
    case .turnFailed:
      return try semanticRecord(
        message,
        event: .processTurnFailure(
          SignalboxProcessTurnFailureEvent(reason: "Turn failed.")
        )
      )
    case .imported(_, _, _, let contentKind):
      return try semanticRecord(
        message,
        event: .processConservative(
          SignalboxProcessConservativeEvent(
            kind: "imported_\(contentKind.rawValue)",
            diagnostic: "The process snapshot preserves this imported content conservatively."
          )
        )
      )
    case .turnCancelled:
      return try semanticRecord(
        message,
        event: .processConservative(
          SignalboxProcessConservativeEvent(
            kind: "turn_cancelled",
            diagnostic: "The turn was cancelled."
          )
        )
      )
    case .turnCompleted:
      return nil
    case .unknown(let kind, _, let diagnostic):
      return try semanticRecord(
        message,
        event: .processConservative(
          SignalboxProcessConservativeEvent(
            kind: kind,
            diagnostic: diagnostic?.message ?? "The entry kind is not rendered by this client."
          )
        )
      )
    }
  }

  private mutating func updateTool(
    requestID: String,
    output: String,
    status: SignalboxProcessToolStatus
  ) throws -> SignalboxStoredEvent {
    guard let prior = toolsByRequestID[requestID] else {
      throw SignalboxProcessTranscriptProjectionError.orphanedToolResult(requestID)
    }
    let updated = SignalboxProcessToolEvent(
      toolRequestID: prior.toolRequestID,
      toolName: prior.toolName,
      arguments: prior.arguments,
      output: output,
      status: status
    )
    toolsByRequestID[requestID] = updated
    guard let eventID = presentationIDs[.toolRequest(requestID)] else {
      throw SignalboxProcessTranscriptProjectionError.orphanedToolResult(requestID)
    }
    return toolRecord(updated, awaitsDecision: false, eventID: eventID)
  }

  private mutating func toolRecord(
    _ event: SignalboxProcessToolEvent,
    awaitsDecision: Bool,
    eventID: SignalboxEventID
  ) -> SignalboxStoredEvent {
    let status = awaitsDecision ? SignalboxProcessToolStatus.awaitingDecision : event.status
    let presented = SignalboxProcessToolEvent(
      toolRequestID: event.toolRequestID,
      toolName: event.toolName,
      arguments: event.arguments,
      output: event.output,
      status: status
    )
    return SignalboxStoredEvent(
      eventID: eventID,
      event: .processTool(presented)
    )
  }

  private mutating func semanticRecord(
    _ message: SignalboxTranscriptEntryMessage,
    event: SignalboxConversationEvent
  ) throws -> SignalboxStoredEvent {
    SignalboxStoredEvent(
      eventID: try claimPresentationID(
        .semantic(
          sourceSessionID: message.sourceSessionID.rawValue,
          entryID: message.entryID.rawValue
        ),
        entryIndex: message.entryIndex
      ),
      event: event
    )
  }

  private mutating func claimPresentationID(
    _ identity: PresentationIdentity,
    entryIndex: SignalboxCanonicalUInt64
  ) throws -> SignalboxEventID {
    if let existing = presentationIDs[identity] {
      return existing
    }
    guard entryIndex.rawValue < UInt64(Int.max) else {
      throw SignalboxProcessTranscriptProjectionError.localIdentityExhausted
    }
    let claimed = SignalboxEventID(rawValue: Int(entryIndex.rawValue) + 1)
    presentationIDs[identity] = claimed
    return claimed
  }

  private func textIsSelected(
    _ entry: SignalboxTranscriptTextEntry,
    selection: Selection
  ) -> Bool {
    switch selection {
    case .all:
      return true
    case .trigger(let trigger):
      switch entry {
      case .user:
        return false
      case .assistant(let turnID, let modelCallID):
        let producingCall: (turnID: SignalboxCanonicalUUID, modelCallID: SignalboxCanonicalUUID)?
        switch trigger {
        case .toolBatchTransition(let triggerTurnID, let triggerModelCallID, .proposed),
          .turnCompleted(let triggerTurnID, let triggerModelCallID, _, _):
          producingCall = (triggerTurnID, triggerModelCallID)
        default:
          producingCall = nil
        }
        guard let producingCall else {
          return false
        }
        return turnID == producingCall.turnID && modelCallID == producingCall.modelCallID
      case .imported, .unknown:
        return false
      }
    }
  }

  private func turnStateIsActive(_ state: SignalboxTranscriptTurnState) -> Bool {
    switch state {
    case .activeRunning, .activeAwaitingToolApproval, .activeAwaitingModelCallRecovery,
      .activeAwaitingToolRecovery, .reconciliationRequired, .toolReconciliationRequired:
      return true
    case .queued, .failed, .completed, .refused, .cancelled, .unknown:
      return false
    }
  }

  private func turnID(
    for trigger: SignalboxProcessSessionEvent
  ) -> SignalboxCanonicalUUID? {
    switch trigger {
    case .inputAccepted(_, let turnID, _, _),
      .turnActivated(let turnID, _),
      .modelCallTransition(let turnID, _, _),
      .toolBatchTransition(let turnID, _, _),
      .turnCompleted(let turnID, _, _, _),
      .turnFailed(let turnID, _, _),
      .turnRefused(let turnID, _, _),
      .turnCancelled(let turnID, _, _),
      .turnReconciliationRequired(let turnID, _, _),
      .turnToolReconciliationRequired(let turnID, _, _):
      return turnID
    case .sessionCreated, .unknown:
      return nil
    }
  }

  private func entryIsSelected(
    _ message: SignalboxTranscriptEntryMessage,
    selection: Selection
  ) -> Bool {
    switch selection {
    case .all:
      return true
    case .trigger(let trigger):
      return entry(message, isAttributableTo: trigger)
    }
  }

  private func entry(
    _ message: SignalboxTranscriptEntryMessage,
    isAttributableTo trigger: SignalboxProcessSessionEvent
  ) -> Bool {
    switch trigger {
    case .toolBatchTransition(let turnID, let modelCallID, let state):
      switch state {
      case .proposed:
        guard
          case .assistantToolUse(let entryTurnID, let entryModelCallID, _, _, _) =
            message.entry
        else {
          return false
        }
        return entryTurnID == turnID && entryModelCallID == modelCallID
      case .resultsProjected:
        return toolEntry(message.entry, belongsTo: turnID, modelCallID: modelCallID)
      case .recoveryRequired, .unknown:
        return false
      }
    case .turnCompleted(let turnID, _, let completionEntryID, _):
      return message.entryID == completionEntryID
        || toolEntry(message.entry, belongsTo: turnID, modelCallID: nil)
    case .turnFailed(let turnID, let failureEntryID, _):
      return message.entryID == failureEntryID
        || toolEntry(message.entry, belongsTo: turnID, modelCallID: nil)
    case .turnCancelled(let turnID, let cancellationEntryID, _):
      return message.entryID == cancellationEntryID
        || toolEntry(message.entry, belongsTo: turnID, modelCallID: nil)
    case .turnToolReconciliationRequired(_, let toolAttemptID, _):
      guard case .toolExecutionResult(_, let entryAttemptID, _) = message.entry else {
        return false
      }
      return entryAttemptID == toolAttemptID
    case .sessionCreated, .inputAccepted, .turnActivated, .modelCallTransition, .turnRefused,
      .turnReconciliationRequired, .unknown:
      return false
    }
  }

  private func toolEntry(
    _ entry: SignalboxTranscriptEntry,
    belongsTo turnID: SignalboxCanonicalUUID,
    modelCallID: SignalboxCanonicalUUID?
  ) -> Bool {
    let requestID: String
    switch entry {
    case .toolExecutionResult(let request, _, _),
      .toolDenied(let request, _),
      .toolClosed(let request, _):
      requestID = request.rawValue
    case .assistantToolUse:
      return false
    case .turnCompleted, .turnFailed, .turnCancelled, .imported, .unknown:
      return false
    }
    guard let context = toolContextsByRequestID[requestID],
      context.turnID == turnID
    else {
      return false
    }
    return modelCallID.map { context.modelCallID == $0 } ?? true
  }

  private func containsRequiredEvidence(
    in snapshot: SignalboxSynchronizationSnapshot,
    for trigger: SignalboxProcessSessionEvent
  ) -> Bool {
    switch trigger {
    case .toolBatchTransition(let turnID, let modelCallID, let state):
      switch state {
      case .proposed:
        return snapshot.records.contains { record in
          guard case .entry(let message) = record,
            case .assistantToolUse(let entryTurnID, let entryModelCallID, _, _, _) =
              message.entry
          else {
            return false
          }
          return entryTurnID == turnID && entryModelCallID == modelCallID
        }
      case .resultsProjected:
        return snapshot.records.contains { record in
          guard case .entry(let message) = record else {
            return false
          }
          return toolEntry(message.entry, belongsTo: turnID, modelCallID: modelCallID)
        }
      case .recoveryRequired, .unknown:
        return true
      }
    case .turnCompleted(let turnID, _, let completionEntryID, _):
      return snapshot.records.contains {
        guard case .entry(let message) = $0,
          message.entryID == completionEntryID,
          case .turnCompleted(let entryTurnID) = message.entry
        else {
          return false
        }
        return entryTurnID == turnID
      }
    case .turnFailed(let turnID, let failureEntryID, _):
      return snapshot.records.contains {
        guard case .entry(let message) = $0,
          message.entryID == failureEntryID,
          case .turnFailed(let entryTurnID) = message.entry
        else {
          return false
        }
        return entryTurnID == turnID
      }
    case .turnCancelled(let turnID, let cancellationEntryID, _):
      return snapshot.records.contains {
        guard case .entry(let message) = $0,
          message.entryID == cancellationEntryID,
          case .turnCancelled(let entryTurnID) = message.entry
        else {
          return false
        }
        return entryTurnID == turnID
      }
    case .turnToolReconciliationRequired(_, let toolAttemptID, _):
      return snapshot.records.contains {
        guard case .entry(let message) = $0,
          case .toolExecutionResult(_, let entryAttemptID, _) = message.entry
        else {
          return false
        }
        return entryAttemptID == toolAttemptID
      }
    case .sessionCreated, .inputAccepted, .turnActivated, .modelCallTransition, .turnRefused,
      .turnReconciliationRequired, .unknown:
      return true
    }
  }

  private func importedRole(
    _ speaker: SignalboxImportedSourceSpeaker
  ) -> SignalboxMessageRole {
    guard case .attested(let importedSpeaker) = speaker else {
      return .unknown
    }
    switch importedSpeaker {
    case .user:
      return .user
    case .assistant:
      return .assistant
    }
  }

  private func activity(
    for state: SignalboxTranscriptTurnState
  ) -> SignalboxProcessActivity {
    switch state {
    case .queued:
      return .init(state: .queued, label: "Queued")
    case .activeRunning:
      return .init(state: .running, label: "Running")
    case .activeAwaitingToolApproval:
      return .init(state: .waitingForToolDecision, label: "Tool decision unavailable")
    case .activeAwaitingModelCallRecovery, .activeAwaitingToolRecovery,
      .reconciliationRequired, .toolReconciliationRequired:
      return .init(state: .recoveryRequired, label: "Recovery required")
    case .failed:
      return .init(state: .failed, label: "Failed")
    case .completed:
      return .init(state: .completed, label: "Completed")
    case .refused:
      return .init(state: .refused, label: "Refused")
    case .cancelled:
      return .init(state: .cancelled, label: "Cancelled")
    case .unknown:
      return .unavailable
    }
  }

  private func store(
    _ record: SignalboxStoredEvent,
    in projectedByID: inout [SignalboxEventID: SignalboxStoredEvent],
    order: inout [SignalboxEventID]
  ) {
    if projectedByID.updateValue(record, forKey: record.eventID) == nil {
      order.append(record.eventID)
    }
  }
}
