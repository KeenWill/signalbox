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

/// Presentation identities survive authoritative refreshes and side reads, but
/// only a wholly valid projection may advance that identity table. Candidate
/// state is committed after projection so malformed snapshots cannot consume
/// identities or discard retained tool context.
public struct SignalboxProcessTranscriptProjector: Sendable {
  private enum PresentationIdentity: Hashable, Sendable {
    case semantic(sourceSessionID: String, entryID: String)
    case toolRequest(String)
    case modelCallUsage(String)
    case turnState(String)
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
  private var nextSyntheticEventID = Int.min / 2
  private var nextModelCallUsageEventID = Int.max / 4

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

  public func projectUnrecognizedFollowedEvent(
    _ followed: SignalboxFollowedSessionEvent
  ) -> SignalboxProcessConservativeEvent? {
    let content: (kind: String, diagnostic: String)?
    switch followed.event {
    case .modelCallTransition(let turnID, let modelCallID, .unknown(let kind, _)):
      content = (
        SignalboxProcessPresentation.retainedLabel(
          "model_call_transition.state.\(kind)"
        ),
        "Turn \(turnID.rawValue), model call \(modelCallID.rawValue): "
          + "the daemon reported an unrecognized model-call state."
      )
    case .toolBatchTransition(let turnID, let modelCallID, .unknown(let kind, _)):
      content = (
        SignalboxProcessPresentation.retainedLabel(
          "tool_batch_transition.state.\(kind)"
        ),
        "Turn \(turnID.rawValue), model call \(modelCallID.rawValue): "
          + "the daemon reported an unrecognized tool-batch state."
      )
    case .unknown(let kind, _, let diagnostic):
      content = (
        SignalboxProcessPresentation.retainedLabel(kind),
        diagnostic?.message ?? "The daemon reported an unrecognized session event."
      )
    case .sessionCreated, .inputAccepted, .turnActivated, .modelCallTransition,
      .toolBatchTransition, .contextCompacted, .turnCompleted, .turnFailed,
      .turnRefused, .turnCancelled, .turnReconciliationRequired,
      .turnToolReconciliationRequired:
      content = nil
    }
    guard let content else {
      return nil
    }
    return SignalboxProcessConservativeEvent(
      kind: content.kind,
      diagnostic: content.diagnostic
    )
  }

  private enum Selection {
    case all
    case trigger(SignalboxProcessSessionEvent)

    var includesConservativeSnapshotEvidence: Bool {
      switch self {
      case .all: return true
      case .trigger: return false
      }
    }
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
    var unknownTurnActivity: SignalboxProcessActivity?
    var textAssembly: TextAssembly?
    var awaitingToolDecisionRequestID: String?
    let modelCallAnchorIndices = modelCallAnchorRecordIndices(in: snapshot.records)
    var anchoredUsageByRecordIndex: [Int: [SignalboxStoredEvent]] = [:]
    var unanchoredUsage: [SignalboxStoredEvent] = []

    for (recordIndex, record) in snapshot.records.enumerated() {
      switch record {
      case .turn(let turn):
        latestActivity = activity(for: turn.state)
        if case .unknown = turn.state {
          unknownTurnActivity = latestActivity
        } else if case .queued = turn.state {
          // A queued successor does not prove that an unknown earlier turn is terminal.
        } else {
          unknownTurnActivity = nil
        }
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
        if selection.includesConservativeSnapshotEvidence,
          let unrecognized = try projectUnrecognizedTurnState(turn)
        {
          store(unrecognized, in: &projectedByID, order: &projectedOrder)
        }
      case .modelCallUsage(let evidence):
        if usageIsSelected(evidence, selection: selection) {
          let usageRecord = SignalboxStoredEvent(
            eventID: try claimModelCallUsagePresentationID(evidence),
            event: .processModelCallUsage(
              SignalboxProcessModelCallUsageEvent(evidence: evidence)
            )
          )
          if let anchorIndex = modelCallAnchorIndices[evidence.modelCallID.rawValue] {
            anchoredUsageByRecordIndex[anchorIndex, default: []].append(usageRecord)
          } else {
            unanchoredUsage.append(usageRecord)
          }
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
      for usageRecord in anchoredUsageByRecordIndex.removeValue(forKey: recordIndex) ?? [] {
        store(usageRecord, in: &projectedByID, order: &projectedOrder)
      }
    }
    guard textAssembly == nil else {
      throw SignalboxProcessTranscriptProjectionError.missingTextContent
    }
    pendingInputs.removeAll { materializedAcceptedInputIDs.contains($0.id) }
    for anchorIndex in anchoredUsageByRecordIndex.keys.sorted() {
      for usageRecord in anchoredUsageByRecordIndex[anchorIndex] ?? [] {
        store(usageRecord, in: &projectedByID, order: &projectedOrder)
      }
    }
    for usageRecord in unanchoredUsage {
      store(usageRecord, in: &projectedByID, order: &projectedOrder)
    }
    return SignalboxProcessTranscriptProjection(
      records: projectedOrder.compactMap { projectedByID[$0] },
      pendingInputs: pendingInputs,
      activity: unknownTurnActivity ?? activeActivity ?? latestActivity,
      materializedAcceptedInputIDs: materializedAcceptedInputIDs
    )
  }

  private func modelCallAnchorRecordIndices(
    in records: [SignalboxSynchronizationSnapshot.Record]
  ) -> [String: Int] {
    var anchors: [String: Int] = [:]
    var modelCallIDsByToolRequestID: [String: String] = [:]
    var textModelCallID: String?
    for (index, record) in records.enumerated() {
      switch record {
      case .textEntry(let message):
        switch message.entry {
        case .assistant(_, let modelCallID),
          .contextSummary(let modelCallID, _, _, _, _):
          textModelCallID = modelCallID.rawValue
        case .user, .imported, .unknown:
          textModelCallID = nil
        }
      case .content(let content):
        guard content.finalFragment else {
          continue
        }
        if let textModelCallID {
          anchors[textModelCallID] = index
        }
        textModelCallID = nil
      case .entry(let message):
        switch message.entry {
        case .assistantToolUse(_, let modelCallID, let requestID, _, _):
          let rawModelCallID = modelCallID.rawValue
          modelCallIDsByToolRequestID[requestID.rawValue] = rawModelCallID
          anchors[rawModelCallID] = index
        case .toolExecutionResult(let requestID, _, _),
          .toolDenied(let requestID, _), .toolClosed(let requestID, _):
          if let modelCallID = modelCallIDsByToolRequestID[requestID.rawValue] {
            anchors[modelCallID] = index
          }
        case .modelIdentityChanged, .turnCompleted, .turnFailed, .turnCancelled,
          .imported, .unknown:
          break
        }
      case .turn, .modelCallUsage:
        break
      }
    }
    return anchors
  }

  private mutating func projectText(
    _ message: SignalboxTranscriptTextEntryMessage,
    content: String,
    selection: Selection
  ) throws -> SignalboxStoredEvent? {
    guard textIsSelected(message, selection: selection) else {
      return nil
    }
    let event: SignalboxConversationEvent
    switch message.entry {
    case .user:
      event = .processMessage(SignalboxProcessMessageEvent(role: .user, text: content))
    case .assistant:
      event = .processMessage(SignalboxProcessMessageEvent(role: .assistant, text: content))
    case .contextSummary:
      event = .processContextSummary(SignalboxProcessContextSummaryEvent(text: content))
    case .imported(_, _, let speaker):
      let presentation = importedPresentation(speaker)
      event = .processMessage(
        SignalboxProcessMessageEvent(
          role: presentation.role,
          text: content,
          unrecognizedKind: presentation.unrecognizedKind,
          sourceAttribution: presentation.sourceAttribution
        )
      )
    case .unknown(let kind, _, _):
      event = .processMessage(
        SignalboxProcessMessageEvent(
          role: .unknown,
          text: content,
          unrecognizedKind: SignalboxProcessPresentation.retainedLabel(kind)
        )
      )
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
      event: event
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
    case .modelIdentityChanged(let turnID, let defaultsVersion, let selectedModelID):
      return try semanticRecord(
        message,
        event: .processModelIdentity(
          SignalboxProcessModelIdentityEvent(
            turnID: turnID,
            defaultsVersion: defaultsVersion,
            selectedModelID: selectedModelID
          )
        )
      )
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
    case .imported(_, _, let sourceSpeaker, let contentKind):
      guard let presentationKind = importedContentKind(contentKind) else {
        return try semanticRecord(
          message,
          event: .processConservative(
            SignalboxProcessConservativeEvent(
              kind: SignalboxProcessPresentation.retainedLabel(
                "imported_\(contentKind.rawValue)"
              ),
              diagnostic: "The transcript contains an unrecognized imported content kind."
            )
          )
        )
      }
      return try semanticRecord(
        message,
        event: .processImportedContent(
          SignalboxProcessImportedContentEvent(
            contentKind: presentationKind,
            sourceSpeaker: importedSpeakerLabel(sourceSpeaker)
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
            kind: SignalboxProcessPresentation.retainedLabel(kind),
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

  private mutating func claimSyntheticPresentationID(
    _ identity: PresentationIdentity
  ) throws -> SignalboxEventID {
    if let existing = presentationIDs[identity] {
      return existing
    }
    guard nextSyntheticEventID < 0 else {
      throw SignalboxProcessTranscriptProjectionError.localIdentityExhausted
    }
    let claimed = SignalboxEventID(rawValue: nextSyntheticEventID)
    nextSyntheticEventID += 1
    presentationIDs[identity] = claimed
    return claimed
  }

  private mutating func claimModelCallUsagePresentationID(
    _ evidence: SignalboxTranscriptModelCallUsage
  ) throws -> SignalboxEventID {
    let identity = PresentationIdentity.modelCallUsage(evidence.modelCallID.rawValue)
    if let existing = presentationIDs[identity] {
      return existing
    }
    guard nextModelCallUsageEventID < Int.max / 2 else {
      throw SignalboxProcessTranscriptProjectionError.localIdentityExhausted
    }
    let claimed = SignalboxEventID(rawValue: nextModelCallUsageEventID)
    nextModelCallUsageEventID += 1
    presentationIDs[identity] = claimed
    return claimed
  }

  private mutating func projectUnrecognizedTurnState(
    _ turn: SignalboxTranscriptTurn
  ) throws -> SignalboxStoredEvent? {
    let content: (kind: String, diagnostic: String)?
    switch turn.state {
    case .unknown(let kind, _, let diagnostic):
      content = (
        SignalboxProcessPresentation.retainedLabel("turn.state.\(kind)"),
        "Turn \(turn.turnID.rawValue): "
          + (diagnostic?.message ?? "the snapshot retained an unrecognized turn state.")
      )
    case .activeRunning(_, let currentModelCall):
      guard let currentModelCall else {
        content = nil
        break
      }
      switch currentModelCall.state {
      case .unknown(let kind, _):
        content = (
          SignalboxProcessPresentation.retainedLabel(
            "current_model_call.state.\(kind)"
          ),
          "Turn \(turn.turnID.rawValue), model call \(currentModelCall.modelCallID.rawValue): "
            + "the snapshot retained an unrecognized current model-call state."
        )
      case .prepared, .inFlight, .cancellationRequested:
        content = nil
      }
    case .queued, .activeAwaitingModelCallRecovery, .activeAwaitingToolApproval,
      .activeAwaitingToolRecovery, .failed, .completed, .refused, .cancelled,
      .reconciliationRequired, .toolReconciliationRequired:
      content = nil
    }
    guard let content else {
      return nil
    }
    return SignalboxStoredEvent(
      eventID: try claimSyntheticPresentationID(.turnState(turn.turnID.rawValue)),
      event: .processConservative(
        SignalboxProcessConservativeEvent(
          kind: content.kind,
          diagnostic: content.diagnostic
        )
      )
    )
  }

  private func usageIsSelected(
    _ evidence: SignalboxTranscriptModelCallUsage,
    selection: Selection
  ) -> Bool {
    switch selection {
    case .all:
      return true
    case .trigger(let trigger):
      return turnID(for: trigger) == evidence.turnID
    }
  }

  private func textIsSelected(
    _ message: SignalboxTranscriptTextEntryMessage,
    selection: Selection
  ) -> Bool {
    switch selection {
    case .all:
      return true
    case .trigger(let trigger):
      switch message.entry {
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
      case .contextSummary(let modelCallID, _, _, _, _):
        guard
          case .contextCompacted(_, let triggerModelCallID, _, let summaryEntryID, _) = trigger
        else {
          return false
        }
        return message.entryID == summaryEntryID && modelCallID == triggerModelCallID
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
    case .sessionCreated, .contextCompacted, .unknown:
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
    case .turnToolReconciliationRequired(let turnID, let toolAttemptID, _):
      guard
        case .toolExecutionResult(let requestID, let entryAttemptID, _) = message.entry,
        let context = toolContextsByRequestID[requestID.rawValue]
      else {
        return false
      }
      return entryAttemptID == toolAttemptID && context.turnID == turnID
    case .sessionCreated, .inputAccepted, .turnActivated, .modelCallTransition,
      .contextCompacted, .turnRefused, .turnReconciliationRequired, .unknown:
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
    case .modelIdentityChanged, .turnCompleted, .turnFailed, .turnCancelled, .imported, .unknown:
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
        let expectedRequestIDs = Set(
          toolContextsByRequestID.compactMap {
            requestID, context -> String? in
            guard context.turnID == turnID, context.modelCallID == modelCallID else {
              return nil
            }
            return requestID
          })
        guard !expectedRequestIDs.isEmpty else {
          return false
        }
        let projectedRequestIDs = Set(
          snapshot.records.compactMap {
            record -> String? in
            guard case .entry(let message) = record else {
              return nil
            }
            switch message.entry {
            case .toolExecutionResult(let requestID, _, _), .toolDenied(let requestID, _),
              .toolClosed(let requestID, _):
              return requestID.rawValue
            case .modelIdentityChanged, .assistantToolUse, .turnCompleted, .turnFailed,
              .turnCancelled, .imported,
              .unknown:
              return nil
            }
          })
        return expectedRequestIDs.isSubset(of: projectedRequestIDs)
      case .recoveryRequired, .unknown:
        return true
      }
    case .contextCompacted(_, let modelCallID, _, let summaryEntryID, _):
      return snapshot.records.contains {
        guard case .textEntry(let message) = $0,
          message.entryID == summaryEntryID,
          case .contextSummary(let entryModelCallID, _, _, _, _) = message.entry
        else {
          return false
        }
        return entryModelCallID == modelCallID
      }
    case .turnCompleted(let turnID, let modelCallID, let completionEntryID, _):
      let hasAssistantText = snapshot.records.contains {
        guard case .textEntry(let message) = $0,
          case .assistant(let entryTurnID, let entryModelCallID) = message.entry
        else {
          return false
        }
        return entryTurnID == turnID && entryModelCallID == modelCallID
      }
      let hasCompletionMarker = snapshot.records.contains {
        guard case .entry(let message) = $0,
          message.entryID == completionEntryID,
          case .turnCompleted(let entryTurnID) = message.entry
        else {
          return false
        }
        return entryTurnID == turnID
      }
      return hasAssistantText && hasCompletionMarker
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
    case .turnToolReconciliationRequired(let turnID, let toolAttemptID, _):
      return snapshot.records.contains {
        guard case .entry(let message) = $0,
          case .toolExecutionResult(let requestID, let entryAttemptID, _) = message.entry,
          let context = toolContextsByRequestID[requestID.rawValue]
        else {
          return false
        }
        return entryAttemptID == toolAttemptID && context.turnID == turnID
      }
    case .sessionCreated, .inputAccepted, .turnActivated, .modelCallTransition, .turnRefused,
      .turnReconciliationRequired, .unknown:
      return true
    }
  }

  private func importedPresentation(
    _ speaker: SignalboxImportedSourceSpeaker
  ) -> (
    role: SignalboxMessageRole,
    unrecognizedKind: String?,
    sourceLabel: String,
    sourceAttribution: SignalboxProcessMessageSourceAttribution?
  ) {
    switch speaker {
    case .attested(.user):
      return (.user, nil, "User role", .importedUserRole)
    case .attested(.assistant):
      return (.assistant, nil, "Assistant role", .importedAssistantRole)
    case .attested(.unknown(let value)):
      let label = SignalboxProcessPresentation.retainedLabel(
        "Unrecognized speaker (\(value))"
      )
      return (
        .unknown,
        label,
        label,
        nil
      )
    case .unknown(let kind, _):
      let label = SignalboxProcessPresentation.retainedLabel("Unknown speaker (\(kind))")
      return (
        .unknown,
        label,
        label,
        nil
      )
    case .notAttested:
      return (
        .unknown, nil, "Speaker not attested", .importedSpeakerNotAttested
      )
    case .attestedAbsent:
      return (.unknown, nil, "Speaker absent", .importedSpeakerAbsent)
    }
  }

  private func importedContentKind(
    _ kind: SignalboxImportedContentKind
  ) -> SignalboxProcessImportedContentKind? {
    switch kind {
    case .sourceEvent:
      return .sourceEvent
    case .sourceMessageBlock:
      return .sourceMessageBlock
    case .text:
      return .text
    case .toolCall:
      return .toolCall
    case .toolResult:
      return .toolResult
    case .thinking:
      return .thinking
    case .redactedThinking:
      return .redactedThinking
    case .document:
      return .document
    case .messageContentAbsent:
      return .messageContentAbsent
    case .unknown:
      return nil
    }
  }

  private func importedSpeakerLabel(
    _ speaker: SignalboxImportedSourceSpeaker
  ) -> String {
    importedPresentation(speaker).sourceLabel
  }

  private func activity(
    for state: SignalboxTranscriptTurnState
  ) -> SignalboxProcessActivity {
    switch state {
    case .queued:
      return .init(state: .queued, label: "Queued")
    case .activeRunning(_, let currentModelCall):
      if let currentModelCall, case .unknown = currentModelCall.state {
        return .init(state: .recoveryRequired, label: "Recovery required")
      }
      return .init(state: .running, label: "Running")
    case .activeAwaitingToolApproval:
      return .init(state: .waitingForToolDecision, label: "Tool decision unavailable")
    case .activeAwaitingModelCallRecovery, .activeAwaitingToolRecovery,
      .reconciliationRequired, .toolReconciliationRequired:
      return .init(state: .recoveryRequired, label: "Recovery required")
    case .failed(_, _, let terminalModelCall):
      if let terminalModelCall, case .unknown(let value) = terminalModelCall.disposition {
        let label = SignalboxProcessPresentation.retainedLabel(
          "Failed: unrecognized disposition (\(value))"
        )
        return .init(state: .failed, label: label)
      }
      guard let cause = terminalModelCall?.cause else {
        return .init(state: .failed, label: "Failed")
      }
      let label = SignalboxProcessPresentation.retainedLabel(
        "Failed: \(providerFailureLabel(cause))"
      )
      return .init(state: .failed, label: label)
    case .completed:
      return .init(state: .completed, label: "Completed")
    case .refused:
      return .init(state: .refused, label: "Refused")
    case .cancelled:
      return .init(state: .cancelled, label: "Cancelled")
    case .unknown:
      return .init(state: .recoveryRequired, label: "Recovery required")
    }
  }

  private func providerFailureLabel(_ cause: SignalboxFailedModelCallCause) -> String {
    switch cause {
    case .credentialRejected:
      return "provider rejected credential"
    case .permissionDenied:
      return "credential lacks permission"
    case .invalidRequest:
      return "invalid provider request"
    case .targetNotFound:
      return "model or resource not found"
    case .requestTooLarge:
      return "provider request too large"
    case .rateLimited:
      return "provider rate limited; retry later"
    case .quotaExhausted:
      return "provider quota exhausted"
    case .overloaded:
      return "provider overloaded; retry later"
    case .providerInternal:
      return "provider internal error"
    case .unrecognized:
      return "unrecognized provider error"
    case .unknown(let value):
      return "unrecognized provider error (\(value))"
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
