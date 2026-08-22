import Foundation

#if canImport(SignalboxModels)
  import SignalboxModels
#endif

public enum SignalboxProcessTranscriptProjectionError: LocalizedError, Equatable {
  case localIdentityExhausted
  case missingTriggerEvidence
  case missingTextContent
  case mismatchedModelCallUsageTurn
  case orphanedToolResult(String)

  public var errorDescription: String? {
    switch self {
    case .localIdentityExhausted:
      return "The native transcript presentation identity space was exhausted."
    case .missingTriggerEvidence:
      return "The side transcript snapshot omitted the durable evidence named by its trigger."
    case .missingTextContent:
      return "A text transcript entry ended without its required final content fragment."
    case .mismatchedModelCallUsageTurn:
      return "Model-call usage was attributed to a different turn than its transcript evidence."
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
  public let toolApprovalDecisionsByRequestID: [String: SignalboxTranscriptToolApproval]

  public init(
    records: [SignalboxStoredEvent],
    pendingInputs: [SignalboxProcessPendingInput],
    activity: SignalboxProcessActivity,
    materializedAcceptedInputIDs: Set<SignalboxCanonicalUUID>,
    toolApprovalDecisionsByRequestID: [String: SignalboxTranscriptToolApproval]
  ) {
    self.records = records
    self.pendingInputs = pendingInputs
    self.activity = activity
    self.materializedAcceptedInputIDs = materializedAcceptedInputIDs
    self.toolApprovalDecisionsByRequestID = toolApprovalDecisionsByRequestID
  }
}

/// Presentation identities survive authoritative refreshes and side reads, but
/// only a wholly valid projection may advance that identity table. Candidate
/// state is committed after projection so malformed snapshots cannot consume
/// identities or discard retained tool context.
public struct SignalboxProcessTranscriptProjector: Sendable {
  private enum PresentationIdentity: Hashable, Sendable {
    case semantic(sourceSessionID: String, entryID: String)
    case modelCallUsage(String)
    case turnState(String)
  }

  private struct ToolCorrelation: Hashable, Sendable {
    let sourceSessionID: String
    let requestID: String
  }

  private struct ToolIdentity: Hashable, Sendable {
    let sourceSessionID: String
    let entryID: String
    let requestID: String

    var correlation: ToolCorrelation {
      ToolCorrelation(sourceSessionID: sourceSessionID, requestID: requestID)
    }

    var presentationIdentity: PresentationIdentity {
      .semantic(sourceSessionID: sourceSessionID, entryID: entryID)
    }
  }

  private struct ToolContext: Sendable {
    let turnID: SignalboxCanonicalUUID
    let modelCallID: SignalboxCanonicalUUID
    let presentationOrder: SignalboxEventID
  }

  private struct TextAssembly: Sendable {
    let message: SignalboxTranscriptTextEntryMessage
    var content = ""
  }

  private struct ModelCallAnchor: Sendable {
    let recordIndex: Int
    let entryIndex: SignalboxCanonicalUInt64
    let turnID: SignalboxCanonicalUUID?
  }

  private struct TerminalToolResultEvidence: Sendable {
    let entryID: SignalboxCanonicalUUID
    let requestID: String
    let attemptID: SignalboxCanonicalUUID?
    let closesAttemptWithoutID: Bool
  }

  private static let presentationLaneStride = 4
  private static let firstSemanticEventID = Int.min + 1
  private static let semanticEventIDLimit = Int.min / 2
  private static let firstTurnStateEventID = Int.max / 2 + 1
  private static let maximumAnchoredEntryIndex = UInt64(
    (firstTurnStateEventID - 3) / presentationLaneStride
  )
  private static let firstLeadingUsagePresentationOrder = Int.min / 2
  private static let firstTrailingUsagePresentationOrder = (Int.max / 4) * 3

  private var presentationIDs: [PresentationIdentity: SignalboxEventID] = [:]
  private var toolsByIdentity: [ToolIdentity: SignalboxProcessToolEvent] = [:]
  private var toolContextsByIdentity: [ToolIdentity: ToolContext] = [:]
  private var toolIdentitiesByCorrelation: [ToolCorrelation: ToolIdentity] = [:]
  private var nextSemanticEventID = Self.firstSemanticEventID
  private var nextSyntheticEventID = Self.firstTurnStateEventID
  private var nextModelCallUsageEventID = Int.min / 4

  public init() {}

  public mutating func projectAuthoritativeSnapshot(
    _ snapshot: SignalboxSynchronizationSnapshot
  ) throws -> SignalboxProcessTranscriptProjection {
    var candidate = self
    candidate.toolsByIdentity = [:]
    candidate.toolContextsByIdentity = [:]
    candidate.toolIdentitiesByCorrelation = [:]
    let projection = try candidate.project(snapshot, selection: .all)
    let retainedIdentities = candidate.retainedPresentationIdentities(in: snapshot)
    candidate.presentationIDs = candidate.presentationIDs.filter {
      retainedIdentities.contains($0.key)
    }
    self = candidate
    return projection
  }

  public mutating func projectSideSnapshot(
    _ snapshot: SignalboxSynchronizationSnapshot,
    attributableTo trigger: SignalboxFollowedSessionEvent
  ) throws -> SignalboxProcessTranscriptProjection {
    var candidate = self
    let terminalResultEntryIDs = candidate.terminalResultSuffixEntryIDs(
      in: snapshot,
      for: trigger.event
    )
    guard candidate.containsRequiredEvidence(
      in: snapshot,
      for: trigger.event,
      terminalResultEntryIDs: terminalResultEntryIDs
    ) else {
      throw SignalboxProcessTranscriptProjectionError.missingTriggerEvidence
    }
    let projection = try candidate.project(
      snapshot,
      selection: .trigger(
        trigger.event,
        nativeSourceSessionID: snapshot.sessionID,
        terminalResultEntryIDs: terminalResultEntryIDs
      )
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
    case .modelCallTransition(
      let turnID,
      let modelCallID,
      .terminal(.unknown(let disposition))
    ):
      content = (
        SignalboxProcessPresentation.retainedLabel(
          "model_call_transition.disposition.\(disposition)"
        ),
        "Turn \(turnID.rawValue), model call \(modelCallID.rawValue): "
          + "the daemon reported an unrecognized terminal disposition."
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
    case .sessionCreated, .sessionModelSettingsChanged, .turnModelSettingsResolved,
      .inputAccepted, .turnActivated, .modelCallTransition, .toolBatchTransition,
      .toolApprovalDecided, .contextCompacted, .turnCompleted, .turnFailed, .turnRefused,
      .turnCancelled, .turnReconciliationRequired, .turnToolReconciliationRequired,
      .runnerStateTransition:
      content = nil
    }
    guard let content else {
      return nil
    }
    return SignalboxProcessConservativeEvent(
      kind: content.kind,
      diagnostic: SignalboxProcessPresentation.retainedLabel(content.diagnostic)
    )
  }

  private enum Selection {
    case all
    case trigger(
      SignalboxProcessSessionEvent,
      nativeSourceSessionID: SignalboxCanonicalUUID,
      terminalResultEntryIDs: Set<SignalboxCanonicalUUID>
    )

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
    let modelCallAnchors = modelCallAnchors(
      in: snapshot.records,
      nativeSourceSessionID: snapshot.sessionID
    )
    let turnEntryAnchors = turnEntryAnchors(
      in: snapshot.records,
      nativeSourceSessionID: snapshot.sessionID
    )
    let sessionTurnAcceptancePositions = sessionTurnAcceptancePositions(in: snapshot.records)
    let sessionToolRequestPositions = sessionToolRequestPositions(
      in: snapshot.records,
      nativeSourceSessionID: snapshot.sessionID
    )
    let trailingModelCallUsageIDs = trailingModelCallUsageIDs(in: snapshot.records)
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
        } else if case .queuedDelegated = turn.state {
          // A queued successor does not prove that an unknown earlier turn is terminal.
        } else if case .queuedDelegationWake = turn.state {
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
          let unrecognized = try projectUnrecognizedTurnState(
            turn,
            anchorEntryIndex: turnEntryAnchors[turn.turnID.rawValue]
          )
        {
          store(unrecognized, in: &projectedByID, order: &projectedOrder)
        }
      case .modelCallUsage(let evidence):
        if usageIsSelected(evidence, selection: selection) {
          let anchor = modelCallAnchors[evidence.modelCallID.rawValue]
          guard anchor?.turnID == nil || anchor?.turnID == evidence.turnID else {
            throw SignalboxProcessTranscriptProjectionError.mismatchedModelCallUsageTurn
          }
          let trailsTranscript = trailingModelCallUsageIDs.contains(
            evidence.modelCallID.rawValue
          )
          let eventID = try claimModelCallUsagePresentationID(evidence)
          let usageRecord = SignalboxStoredEvent(
            eventID: eventID,
            presentationOrder: try modelCallUsagePresentationOrder(
              evidence,
              anchorEntryIndex: anchor?.entryIndex,
              trailingWhenUnanchored: trailsTranscript
            ),
            event: .processModelCallUsage(
              SignalboxProcessModelCallUsageEvent(evidence: evidence)
            )
          )
          if let anchor {
            anchoredUsageByRecordIndex[anchor.recordIndex, default: []].append(usageRecord)
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
          nativeSourceSessionID: snapshot.sessionID,
          awaitingToolDecisionRequestID: awaitingToolDecisionRequestID,
          sessionTurnAcceptancePositions: sessionTurnAcceptancePositions,
          sessionToolRequestPositions: sessionToolRequestPositions,
          selection: selection
        )
        if let projected {
          store(projected, in: &projectedByID, order: &projectedOrder)
          if case .delegationResult(
            let requestID,
            _,
            _,
            .foreground,
            _,
            let outcome,
            let content,
            _,
            _
          ) = message.entry {
            let toolResult = try updateTool(
              sourceSessionID: message.sourceSessionID.rawValue,
              requestID: requestID.rawValue,
              toolAttemptID: nil,
              output: content,
              status: outcome == .returned ? .completed : .closed
            )
            store(toolResult, in: &projectedByID, order: &projectedOrder)
          }
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
      materializedAcceptedInputIDs: materializedAcceptedInputIDs,
      toolApprovalDecisionsByRequestID: toolApprovalDecisions(in: snapshot)
    )
  }

  private func toolApprovalDecisions(
    in snapshot: SignalboxSynchronizationSnapshot
  ) -> [String: SignalboxTranscriptToolApproval] {
    Dictionary(
      snapshot.records.compactMap { record in
        guard case .entry(let message) = record,
          case .assistantToolUse(_, _, let requestID, _, _, let approval) = message.entry,
          let approval
        else {
          return nil
        }
        return (requestID.rawValue, approval)
      },
      uniquingKeysWith: { first, _ in first }
    )
  }

  private func modelCallAnchors(
    in records: [SignalboxSynchronizationSnapshot.Record],
    nativeSourceSessionID: SignalboxCanonicalUUID
  ) -> [String: ModelCallAnchor] {
    var anchors: [String: ModelCallAnchor] = [:]
    var modelCallsByToolCorrelation:
      [ToolCorrelation: (id: String, turnID: SignalboxCanonicalUUID)] = [:]
    var terminalModelCallIDsByTurnID: [String: String] = [:]
    var textModelCall: (
      id: String, entryIndex: SignalboxCanonicalUInt64, turnID: SignalboxCanonicalUUID?
    )?
    for case .turn(let turn) in records {
      if let modelCallID = transcriptMarkerTerminalModelCallID(for: turn.state) {
        terminalModelCallIDsByTurnID[turn.turnID.rawValue] = modelCallID.rawValue
      }
    }
    for (index, record) in records.enumerated() {
      switch record {
      case .textEntry(let message):
        guard message.sourceSessionID == nativeSourceSessionID else {
          textModelCall = nil
          continue
        }
        switch message.entry {
        case .assistant(let turnID, let modelCallID):
          textModelCall = (modelCallID.rawValue, message.entryIndex, turnID)
        case .contextSummary(let modelCallID, _, _, _, _):
          textModelCall = (modelCallID.rawValue, message.entryIndex, nil)
        case .user, .imported, .unknown:
          textModelCall = nil
        }
      case .content(let content):
        guard content.finalFragment else {
          continue
        }
        if let textModelCall {
          anchors[textModelCall.id] = ModelCallAnchor(
            recordIndex: index,
            entryIndex: textModelCall.entryIndex,
            turnID: textModelCall.turnID
          )
        }
        textModelCall = nil
      case .entry(let message):
        guard message.sourceSessionID == nativeSourceSessionID else {
          continue
        }
        switch message.entry {
        case .assistantToolUse(let turnID, let modelCallID, let requestID, _, _, _):
          let rawModelCallID = modelCallID.rawValue
          let correlation = ToolCorrelation(
            sourceSessionID: message.sourceSessionID.rawValue,
            requestID: requestID.rawValue
          )
          modelCallsByToolCorrelation[correlation] = (rawModelCallID, turnID)
          anchors[rawModelCallID] = ModelCallAnchor(
            recordIndex: index,
            entryIndex: message.entryIndex,
            turnID: turnID
          )
        case .toolExecutionResult(let requestID, _, _),
          .toolDenied(let requestID, _), .toolClosed(let requestID, _),
          .delegationResult(let requestID, _, _, .foreground, _, _, _, _, _):
          let correlation = ToolCorrelation(
            sourceSessionID: message.sourceSessionID.rawValue,
            requestID: requestID.rawValue
          )
          if let modelCall = modelCallsByToolCorrelation[correlation] {
            anchors[modelCall.id] = ModelCallAnchor(
              recordIndex: index,
              entryIndex: message.entryIndex,
              turnID: modelCall.turnID
            )
          }
        case .turnCompleted(let turnID), .turnFailed(let turnID),
          .turnCancelled(let turnID):
          if let modelCallID = terminalModelCallIDsByTurnID[turnID.rawValue],
            anchors[modelCallID] == nil
          {
            anchors[modelCallID] = ModelCallAnchor(
              recordIndex: index,
              entryIndex: message.entryIndex,
              turnID: turnID
            )
          }
        case .delegatedTask, .delegationMessage,
          .delegationResult(_, _, _, .background, _, _, _, _, _),
          .modelIdentityChanged, .runnerPlacementChanged, .imported, .unknown:
          break
        }
      case .turn, .modelCallUsage:
        break
      }
    }
    return anchors
  }

  private func turnEntryAnchors(
    in records: [SignalboxSynchronizationSnapshot.Record],
    nativeSourceSessionID: SignalboxCanonicalUUID
  ) -> [String: SignalboxCanonicalUInt64] {
    var anchors: [String: SignalboxCanonicalUInt64] = [:]
    for record in records {
      let anchor: (turnID: SignalboxCanonicalUUID, entryIndex: SignalboxCanonicalUInt64)?
      switch record {
      case .textEntry(let message):
        guard message.sourceSessionID == nativeSourceSessionID else {
          continue
        }
        switch message.entry {
        case .user(_, let turnID), .assistant(let turnID, _):
          anchor = (turnID, message.entryIndex)
        case .contextSummary, .imported, .unknown:
          anchor = nil
        }
      case .entry(let message):
        guard message.sourceSessionID == nativeSourceSessionID else {
          continue
        }
        switch message.entry {
        case .modelIdentityChanged(let turnID, _, _),
          .assistantToolUse(let turnID, _, _, _, _, _), .turnCompleted(let turnID),
          .turnFailed(let turnID), .turnCancelled(let turnID):
          anchor = (turnID, message.entryIndex)
        case .delegatedTask, .delegationMessage, .delegationResult,
          .runnerPlacementChanged, .toolExecutionResult, .toolDenied, .toolClosed,
          .imported, .unknown:
          anchor = nil
        }
      case .turn, .modelCallUsage, .content:
        anchor = nil
      }
      guard let anchor else {
        continue
      }
      if let existing = anchors[anchor.turnID.rawValue],
        existing.rawValue <= anchor.entryIndex.rawValue
      {
        continue
      }
      anchors[anchor.turnID.rawValue] = anchor.entryIndex
    }
    return anchors
  }

  private func sessionTurnAcceptancePositions(
    in records: [SignalboxSynchronizationSnapshot.Record]
  ) -> [SignalboxCanonicalUUID: SignalboxCanonicalUInt64] {
    records.reduce(into: [:]) { positions, record in
      if case .turn(let turn) = record {
        positions[turn.turnID] = turn.acceptancePosition
      }
    }
  }

  private func sessionToolRequestPositions(
    in records: [SignalboxSynchronizationSnapshot.Record],
    nativeSourceSessionID: SignalboxCanonicalUUID
  ) -> [SignalboxCanonicalUUID: SignalboxProcessToolRequestPosition] {
    var resultAttemptIDs: [SignalboxCanonicalUUID: SignalboxCanonicalUUID] = [:]
    var resultOutputs: [SignalboxCanonicalUUID: String] = [:]
    var ambiguousResultRequestIDs: Set<SignalboxCanonicalUUID> = []
    for case .entry(let message) in records {
      guard message.sourceSessionID == nativeSourceSessionID,
        case .toolExecutionResult(let requestID, let attemptID, let output) = message.entry
      else {
        continue
      }
      if let previousAttemptID = resultAttemptIDs[requestID],
        previousAttemptID != attemptID || resultOutputs[requestID] != output
      {
        ambiguousResultRequestIDs.insert(requestID)
      } else {
        resultAttemptIDs[requestID] = attemptID
        resultOutputs[requestID] = output
      }
    }
    return records.reduce(into: [:]) { positions, record in
      guard case .entry(let message) = record,
        message.sourceSessionID == nativeSourceSessionID,
        case .assistantToolUse(let turnID, _, let requestID, let toolName, _, _) = message.entry
      else {
        return
      }
      positions[requestID] = SignalboxProcessToolRequestPosition(
        turnID: turnID,
        entryIndex: message.entryIndex,
        toolName: toolName,
        toolAttemptID: ambiguousResultRequestIDs.contains(requestID)
          ? nil : resultAttemptIDs[requestID],
        toolOutput: !ambiguousResultRequestIDs.contains(requestID)
          ? resultOutputs[requestID] : nil
      )
    }
  }

  private func transcriptMarkerTerminalModelCallID(
    for state: SignalboxTranscriptTurnState
  ) -> SignalboxCanonicalUUID? {
    switch state {
    case .failed(_, _, let terminalModelCall):
      return terminalModelCall?.modelCallID
    case .completed(_, _, let terminalModelCallID):
      return terminalModelCallID
    case .cancelled(_, _, let terminalModelCallID):
      return terminalModelCallID
    case .queued, .queuedDelegated, .queuedDelegationWake, .delegationTerminated, .activeRunning,
      .activeAwaitingChild, .activeAwaitingModelCallRecovery,
      .activeAwaitingToolApproval, .activeAwaitingToolRecovery, .refused,
      .reconciliationRequired, .toolReconciliationRequired, .unknown:
      return nil
    }
  }

  private func trailingModelCallUsageIDs(
    in records: [SignalboxSynchronizationSnapshot.Record]
  ) -> Set<String> {
    var modelCallIDs: Set<String> = []
    for case .turn(let turn) in records {
      switch turn.state {
      case .activeAwaitingModelCallRecovery(_, let modelCallID),
        .refused(_, _, let modelCallID),
        .reconciliationRequired(_, _, let modelCallID):
        modelCallIDs.insert(modelCallID.rawValue)
      case .queued, .queuedDelegated, .queuedDelegationWake, .delegationTerminated, .activeRunning,
        .activeAwaitingChild, .activeAwaitingToolApproval,
        .activeAwaitingToolRecovery, .failed, .completed, .cancelled,
        .toolReconciliationRequired, .unknown:
        break
      }
    }
    return modelCallIDs
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
    let identity = PresentationIdentity.semantic(
      sourceSessionID: message.sourceSessionID.rawValue,
      entryID: message.entryID.rawValue
    )
    return SignalboxStoredEvent(
      eventID: try claimSemanticEventID(identity),
      presentationOrder: try semanticPresentationOrder(message.entryIndex),
      event: event
    )
  }

  private mutating func projectEntry(
    _ message: SignalboxTranscriptEntryMessage,
    nativeSourceSessionID: SignalboxCanonicalUUID,
    awaitingToolDecisionRequestID: String?,
    sessionTurnAcceptancePositions: [SignalboxCanonicalUUID: SignalboxCanonicalUInt64],
    sessionToolRequestPositions:
      [SignalboxCanonicalUUID: SignalboxProcessToolRequestPosition],
    selection: Selection
  ) throws -> SignalboxStoredEvent? {
    guard
      entryIsSelected(
        message,
        awaitingToolDecisionRequestID: awaitingToolDecisionRequestID,
        selection: selection
      )
    else {
      return nil
    }
    switch message.entry {
    case .delegatedTask(let spawningRequestID, let parentSessionID, let parentTurnID, let content):
      return try semanticRecord(
        message,
        event: .processConservative(
          SignalboxProcessConservativeEvent(
            kind: "delegated_task",
            diagnostic:
              "Parent session \(parentSessionID.rawValue), turn \(parentTurnID.rawValue), spawned request \(spawningRequestID.rawValue): \(content)"
          )
        )
      )
    case .delegationMessage(
      let spawningRequestID,
      let messageID,
      let senderSessionID,
      let recipientSessionID,
      _,
      _,
      let content
    ):
      return try semanticRecord(
        message,
        event: .processConservative(
          SignalboxProcessConservativeEvent(
            kind: "delegation_message",
            diagnostic:
              "Delegation \(spawningRequestID.rawValue) message \(messageID.rawValue) from \(senderSessionID.rawValue) to \(recipientSessionID.rawValue): \(content)"
          )
        )
      )
    case .delegationResult(
      let awaitRequestID,
      let spawningRequestID,
      let childSessionID,
      let mode,
      _,
      let outcome,
      let content,
      _,
      _
    ):
      let deliveredContent = content.map { ": \($0)" } ?? ""
      return try semanticRecord(
        message,
        event: .processConservative(
          SignalboxProcessConservativeEvent(
            kind: "delegation_result",
            diagnostic:
              "Delegation \(spawningRequestID.rawValue) child \(childSessionID.rawValue) delivered \(outcome.rawValue) to \(awaitRequestID.rawValue) in \(mode.rawValue) mode\(deliveredContent)"
          )
        )
      )
    case .modelIdentityChanged(let turnID, let defaultsVersion, let selectedModelID):
      return try semanticRecord(
        message,
        event: .processModelIdentity(
          try SignalboxProcessModelIdentityEvent(
            turnID: turnID,
            defaultsVersion: defaultsVersion,
            selectedModelID: selectedModelID
          )
        )
      )
    case .runnerPlacementChanged(
      let priorRunnerID,
      let newRunnerID,
      let placementRevision,
      let sandboxProfile
    ):
      return try semanticRecord(
        message,
        event: .processRunnerPlacement(
          try SignalboxProcessRunnerPlacementEvent(
            priorRunnerID: priorRunnerID,
            newRunnerID: newRunnerID,
            placementRevision: placementRevision,
            sandboxProfile: sandboxProfile
          )
        )
      )
    case .assistantToolUse(
      let turnID,
      let modelCallID,
      let requestID,
      let toolName,
      let arguments,
      _
    ):
      let request = requestID.rawValue
      let identity = ToolIdentity(
        sourceSessionID: message.sourceSessionID.rawValue,
        entryID: message.entryID.rawValue,
        requestID: request
      )
      let event = SignalboxProcessToolEvent(
        toolRequestID: SignalboxToolInvocationID(rawValue: request),
        turnID: turnID,
        sessionTurnAcceptancePositions: sessionTurnAcceptancePositions,
        sessionToolRequestPositions: sessionToolRequestPositions,
        toolName: toolName,
        arguments: arguments,
        output: nil,
        status: .proposed
      )
      toolsByIdentity[identity] = event
      let presentationOrder = try semanticPresentationOrder(message.entryIndex)
      toolContextsByIdentity[identity] = ToolContext(
        turnID: turnID,
        modelCallID: modelCallID,
        presentationOrder: presentationOrder
      )
      toolIdentitiesByCorrelation[identity.correlation] = identity
      let eventID = try claimSemanticEventID(identity.presentationIdentity)
      return toolRecord(
        event,
        awaitsDecision: message.sourceSessionID == nativeSourceSessionID
          && request == awaitingToolDecisionRequestID,
        eventID: eventID,
        presentationOrder: presentationOrder
      )
    case .toolExecutionResult(let requestID, let toolAttemptID, let content):
      return try updateTool(
        sourceSessionID: message.sourceSessionID.rawValue,
        requestID: requestID.rawValue,
        toolAttemptID: toolAttemptID,
        output: content,
        status: .completed
      )
    case .toolDenied(let requestID, let content):
      return try updateTool(
        sourceSessionID: message.sourceSessionID.rawValue,
        requestID: requestID.rawValue,
        toolAttemptID: nil,
        output: content,
        status: .denied
      )
    case .toolClosed(let requestID, let content):
      return try updateTool(
        sourceSessionID: message.sourceSessionID.rawValue,
        requestID: requestID.rawValue,
        toolAttemptID: nil,
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
    sourceSessionID: String,
    requestID: String,
    toolAttemptID: SignalboxCanonicalUUID?,
    output: String?,
    status: SignalboxProcessToolStatus
  ) throws -> SignalboxStoredEvent {
    let correlation = ToolCorrelation(
      sourceSessionID: sourceSessionID,
      requestID: requestID
    )
    guard let identity = toolIdentitiesByCorrelation[correlation],
      let prior = toolsByIdentity[identity]
    else {
      throw SignalboxProcessTranscriptProjectionError.orphanedToolResult(requestID)
    }
    let updated = SignalboxProcessToolEvent(
      toolRequestID: prior.toolRequestID,
      turnID: prior.turnID,
      sessionTurnAcceptancePositions: prior.sessionTurnAcceptancePositions,
      sessionToolRequestPositions: prior.sessionToolRequestPositions,
      toolAttemptID: toolAttemptID,
      toolName: prior.toolName,
      arguments: prior.arguments,
      output: output,
      status: status
    )
    toolsByIdentity[identity] = updated
    guard let eventID = presentationIDs[identity.presentationIdentity],
      let presentationOrder = toolContextsByIdentity[identity]?.presentationOrder
    else {
      throw SignalboxProcessTranscriptProjectionError.orphanedToolResult(requestID)
    }
    return toolRecord(
      updated,
      awaitsDecision: false,
      eventID: eventID,
      presentationOrder: presentationOrder
    )
  }

  private mutating func toolRecord(
    _ event: SignalboxProcessToolEvent,
    awaitsDecision: Bool,
    eventID: SignalboxEventID,
    presentationOrder: SignalboxEventID
  ) -> SignalboxStoredEvent {
    let status = awaitsDecision ? SignalboxProcessToolStatus.awaitingDecision : event.status
    let presented = SignalboxProcessToolEvent(
      toolRequestID: event.toolRequestID,
      turnID: event.turnID,
      sessionTurnAcceptancePositions: event.sessionTurnAcceptancePositions,
      sessionToolRequestPositions: event.sessionToolRequestPositions,
      toolAttemptID: event.toolAttemptID,
      toolName: event.toolName,
      arguments: event.arguments,
      output: event.output,
      status: status
    )
    return SignalboxStoredEvent(
      eventID: eventID,
      presentationOrder: presentationOrder,
      event: .processTool(presented)
    )
  }

  private mutating func semanticRecord(
    _ message: SignalboxTranscriptEntryMessage,
    event: SignalboxConversationEvent
  ) throws -> SignalboxStoredEvent {
    let identity = PresentationIdentity.semantic(
      sourceSessionID: message.sourceSessionID.rawValue,
      entryID: message.entryID.rawValue
    )
    return SignalboxStoredEvent(
      eventID: try claimSemanticEventID(identity),
      presentationOrder: try semanticPresentationOrder(message.entryIndex),
      event: event
    )
  }

  private mutating func claimSemanticEventID(
    _ identity: PresentationIdentity
  ) throws -> SignalboxEventID {
    if let existing = presentationIDs[identity] {
      return existing
    }
    guard nextSemanticEventID < Self.semanticEventIDLimit else {
      throw SignalboxProcessTranscriptProjectionError.localIdentityExhausted
    }
    let claimed = SignalboxEventID(rawValue: nextSemanticEventID)
    nextSemanticEventID += 1
    presentationIDs[identity] = claimed
    return claimed
  }

  private func semanticPresentationOrder(
    _ entryIndex: SignalboxCanonicalUInt64
  ) throws -> SignalboxEventID {
    guard entryIndex.rawValue <= Self.maximumAnchoredEntryIndex else {
      throw SignalboxProcessTranscriptProjectionError.localIdentityExhausted
    }
    return SignalboxEventID(
      rawValue: Int(entryIndex.rawValue) * Self.presentationLaneStride + 1
    )
  }

  private mutating func claimTrailingPresentationID(
    _ identity: PresentationIdentity
  ) throws -> SignalboxEventID {
    if let existing = presentationIDs[identity] {
      return existing
    }
    guard nextSyntheticEventID < Int.max else {
      throw SignalboxProcessTranscriptProjectionError.localIdentityExhausted
    }
    let claimed = SignalboxEventID(rawValue: nextSyntheticEventID)
    nextSyntheticEventID += 1
    presentationIDs[identity] = claimed
    return claimed
  }

  private mutating func claimTurnStatePresentationID(
    _ identity: PresentationIdentity
  ) throws -> SignalboxEventID {
    try claimTrailingPresentationID(identity)
  }

  private func turnStatePresentationOrder(
    eventID: SignalboxEventID,
    anchorEntryIndex: SignalboxCanonicalUInt64?
  ) throws -> SignalboxEventID {
    guard let anchorEntryIndex else {
      return eventID
    }
    guard anchorEntryIndex.rawValue <= Self.maximumAnchoredEntryIndex else {
      throw SignalboxProcessTranscriptProjectionError.localIdentityExhausted
    }
    return SignalboxEventID(
      rawValue: Int(anchorEntryIndex.rawValue) * Self.presentationLaneStride
    )
  }

  private mutating func claimModelCallUsagePresentationID(
    _ evidence: SignalboxTranscriptModelCallUsage
  ) throws -> SignalboxEventID {
    let identity = PresentationIdentity.modelCallUsage(evidence.modelCallID.rawValue)
    if let existing = presentationIDs[identity] {
      return existing
    }
    guard nextModelCallUsageEventID < 0 else {
      throw SignalboxProcessTranscriptProjectionError.localIdentityExhausted
    }
    let claimed = SignalboxEventID(rawValue: nextModelCallUsageEventID)
    nextModelCallUsageEventID += 1
    presentationIDs[identity] = claimed
    return claimed
  }

  private func modelCallUsagePresentationOrder(
    _ evidence: SignalboxTranscriptModelCallUsage,
    anchorEntryIndex: SignalboxCanonicalUInt64?,
    trailingWhenUnanchored: Bool
  ) throws -> SignalboxEventID {
    if let anchorEntryIndex {
      guard anchorEntryIndex.rawValue <= Self.maximumAnchoredEntryIndex else {
        throw SignalboxProcessTranscriptProjectionError.localIdentityExhausted
      }
      return SignalboxEventID(
        rawValue: Int(anchorEntryIndex.rawValue) * Self.presentationLaneStride + 2
      )
    }
    guard evidence.modelCallIndex.rawValue < UInt64(Int.max / 4) else {
      throw SignalboxProcessTranscriptProjectionError.localIdentityExhausted
    }
    let base =
      trailingWhenUnanchored
      ? Self.firstTrailingUsagePresentationOrder
      : Self.firstLeadingUsagePresentationOrder
    return SignalboxEventID(
      rawValue: base + Int(evidence.modelCallIndex.rawValue)
    )
  }

  private mutating func projectUnrecognizedTurnState(
    _ turn: SignalboxTranscriptTurn,
    anchorEntryIndex: SignalboxCanonicalUInt64?
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
    case .failed(_, _, let terminalModelCall):
      guard let terminalModelCall else {
        content = nil
        break
      }
      switch terminalModelCall.disposition {
      case .unknown(let disposition):
        content = (
          SignalboxProcessPresentation.retainedLabel(
            "model_call_transition.disposition.\(disposition)"
          ),
          "Turn \(turn.turnID.rawValue), model call \(terminalModelCall.modelCallID.rawValue): "
            + "the snapshot retained an unrecognized terminal disposition."
        )
      case .knownFailed:
        guard case .unknown(let cause)? = terminalModelCall.cause else {
          content = nil
          break
        }
        content = (
          SignalboxProcessPresentation.retainedLabel(
            "model_call_failure.cause.\(cause)"
          ),
          "Turn \(turn.turnID.rawValue), model call \(terminalModelCall.modelCallID.rawValue): "
            + "the snapshot retained an unrecognized provider-failure cause."
        )
      case .cancelled:
        content = nil
      }
    case .queued, .queuedDelegated, .queuedDelegationWake, .delegationTerminated,
      .activeAwaitingChild,
      .activeAwaitingModelCallRecovery, .activeAwaitingToolApproval,
      .activeAwaitingToolRecovery, .completed, .refused, .cancelled,
      .reconciliationRequired, .toolReconciliationRequired:
      content = nil
    }
    guard let content else {
      return nil
    }
    let eventID = try claimTurnStatePresentationID(.turnState(turn.turnID.rawValue))
    return SignalboxStoredEvent(
      eventID: eventID,
      presentationOrder: try turnStatePresentationOrder(
        eventID: eventID,
        anchorEntryIndex: anchorEntryIndex
      ),
      event: .processConservative(
        SignalboxProcessConservativeEvent(
          kind: content.kind,
          diagnostic: SignalboxProcessPresentation.retainedLabel(content.diagnostic)
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
    case .trigger(let trigger, _, _):
      switch trigger {
      case .modelCallTransition(let turnID, let modelCallID, .terminal),
        .turnCompleted(let turnID, let modelCallID, _, _):
        return turnID == evidence.turnID && modelCallID == evidence.modelCallID
      case .modelCallTransition(_, _, .prepared),
        .modelCallTransition(_, _, .inFlight),
        .modelCallTransition(_, _, .cancellationRequested),
        .modelCallTransition(_, _, .unknown):
        return false
      case .toolBatchTransition(_, _, let state):
        switch state {
        case .proposed, .resultsProjected, .recoveryRequired, .unknown:
          return false
        }
      case .contextCompacted(_, let modelCallID, _, _, _):
        return modelCallID == evidence.modelCallID
      case .sessionCreated, .sessionModelSettingsChanged, .turnModelSettingsResolved,
        .inputAccepted, .turnActivated, .turnFailed, .turnRefused, .turnCancelled,
        .toolApprovalDecided, .turnReconciliationRequired,
        .turnToolReconciliationRequired, .runnerStateTransition, .unknown:
        return false
      }
    }
  }

  private func textIsSelected(
    _ message: SignalboxTranscriptTextEntryMessage,
    selection: Selection
  ) -> Bool {
    switch selection {
    case .all:
      return true
    case .trigger(let trigger, let nativeSourceSessionID, _):
      guard message.sourceSessionID == nativeSourceSessionID else {
        return false
      }
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
    case .activeRunning, .activeAwaitingChild, .activeAwaitingToolApproval,
      .activeAwaitingModelCallRecovery, .activeAwaitingToolRecovery, .reconciliationRequired,
      .toolReconciliationRequired:
      return true
    case .queued, .queuedDelegated, .queuedDelegationWake, .delegationTerminated, .failed,
      .completed, .refused, .cancelled, .unknown:
      return false
    }
  }

  private func entryIsSelected(
    _ message: SignalboxTranscriptEntryMessage,
    awaitingToolDecisionRequestID: String?,
    selection: Selection
  ) -> Bool {
    switch selection {
    case .all:
      return true
    case .trigger(let trigger, let nativeSourceSessionID, let terminalResultEntryIDs):
      return entry(
        message,
        isAttributableTo: trigger,
        nativeSourceSessionID: nativeSourceSessionID,
        terminalResultEntryIDs: terminalResultEntryIDs,
        awaitingToolDecisionRequestID: awaitingToolDecisionRequestID
      )
    }
  }

  private func entry(
    _ message: SignalboxTranscriptEntryMessage,
    isAttributableTo trigger: SignalboxProcessSessionEvent,
    nativeSourceSessionID: SignalboxCanonicalUUID,
    terminalResultEntryIDs: Set<SignalboxCanonicalUUID>,
    awaitingToolDecisionRequestID: String?
  ) -> Bool {
    guard message.sourceSessionID == nativeSourceSessionID else {
      return false
    }
    if case .modelIdentityChanged = message.entry {
      return false
    }
    if case .runnerPlacementChanged = message.entry {
      return false
    }
    switch trigger {
    case .toolBatchTransition(let turnID, let modelCallID, let state):
      switch state {
      case .proposed:
        guard
          case .assistantToolUse(let entryTurnID, let entryModelCallID, _, _, _, _) =
            message.entry
        else {
          return false
        }
        return entryTurnID == turnID && entryModelCallID == modelCallID
      case .resultsProjected:
        return toolEntry(message, belongsTo: turnID, modelCallID: modelCallID)
      case .recoveryRequired, .unknown:
        return false
      }
    case .turnCompleted:
      return isExactTerminalMarker(
        message,
        for: trigger,
        nativeSourceSessionID: nativeSourceSessionID
      )
    case .turnFailed, .turnCancelled:
      return isExactTerminalMarker(
        message,
        for: trigger,
        nativeSourceSessionID: nativeSourceSessionID
      ) || (
        message.sourceSessionID == nativeSourceSessionID
          && terminalResultEntryIDs.contains(message.entryID)
      )
    case .turnToolReconciliationRequired:
      return terminalResultEntryIDs.contains(message.entryID)
    case .toolApprovalDecided(let turnID, _, _, _, _):
      guard let awaitingToolDecisionRequestID,
        case .assistantToolUse(let entryTurnID, _, let requestID, _, _, _) = message.entry
      else {
        return false
      }
      return entryTurnID == turnID && requestID.rawValue == awaitingToolDecisionRequestID
    case .sessionCreated, .sessionModelSettingsChanged, .turnModelSettingsResolved,
      .inputAccepted, .turnActivated, .modelCallTransition,
      .contextCompacted, .turnRefused, .turnReconciliationRequired,
      .runnerStateTransition, .unknown:
      return false
    }
  }

  private func toolEntry(
    _ message: SignalboxTranscriptEntryMessage,
    belongsTo turnID: SignalboxCanonicalUUID,
    modelCallID: SignalboxCanonicalUUID?
  ) -> Bool {
    let requestID: String
    switch message.entry {
    case .toolExecutionResult(let request, _, _),
      .toolDenied(let request, _),
      .toolClosed(let request, _):
      requestID = request.rawValue
    case .delegationResult(let request, _, _, .foreground, _, _, _, _, _):
      requestID = request.rawValue
    case .assistantToolUse:
      return false
    case .delegatedTask, .delegationMessage,
      .delegationResult(_, _, _, .background, _, _, _, _, _), .modelIdentityChanged,
      .runnerPlacementChanged, .turnCompleted, .turnFailed, .turnCancelled, .imported,
      .unknown:
      return false
    }
    let correlation = ToolCorrelation(
      sourceSessionID: message.sourceSessionID.rawValue,
      requestID: requestID
    )
    guard let identity = toolIdentitiesByCorrelation[correlation],
      let context = toolContextsByIdentity[identity],
      context.turnID == turnID
    else {
      return false
    }
    return modelCallID.map { context.modelCallID == $0 } ?? true
  }

  private func terminalResultSuffixEntryIDs(
    in snapshot: SignalboxSynchronizationSnapshot,
    for trigger: SignalboxProcessSessionEvent
  ) -> Set<SignalboxCanonicalUUID> {
    let turnID: SignalboxCanonicalUUID
    switch trigger {
    case .turnFailed(let triggerTurnID, _, _):
      turnID = triggerTurnID
    case .turnCancelled(let triggerTurnID, _, _):
      turnID = triggerTurnID
    case .turnToolReconciliationRequired(
      let triggerTurnID,
      let toolAttemptID,
      let terminalFrontierID
    ):
      return reconciliationResultSuffixEntryIDs(
        in: snapshot,
        turnID: triggerTurnID,
        requiredAttemptID: toolAttemptID,
        terminalFrontierID: terminalFrontierID
      )
    case .sessionCreated, .sessionModelSettingsChanged, .turnModelSettingsResolved,
      .inputAccepted, .turnActivated, .modelCallTransition, .toolBatchTransition,
      .toolApprovalDecided, .contextCompacted, .turnCompleted, .turnRefused,
      .turnReconciliationRequired, .runnerStateTransition, .unknown:
      return []
    }
    guard let markerIndex = snapshot.records.firstIndex(where: { record in
      guard case .entry(let message) = record else {
        return false
      }
      return isExactTerminalMarker(
        message,
        for: trigger,
        nativeSourceSessionID: snapshot.sessionID
      )
    }) else {
      return []
    }
    var entryIDs: Set<SignalboxCanonicalUUID> = []
    for record in snapshot.records[..<markerIndex].reversed() {
      guard case .entry(let message) = record,
        message.sourceSessionID == snapshot.sessionID,
        toolEntry(message, belongsTo: turnID, modelCallID: nil)
      else {
        break
      }
      entryIDs.insert(message.entryID)
    }
    return entryIDs
  }

  private func reconciliationResultSuffixEntryIDs(
    in snapshot: SignalboxSynchronizationSnapshot,
    turnID: SignalboxCanonicalUUID,
    requiredAttemptID: SignalboxCanonicalUUID,
    terminalFrontierID: SignalboxCanonicalUUID
  ) -> Set<SignalboxCanonicalUUID> {
    let frontierMatches = snapshot.records.contains { record in
      guard case .turn(let turn) = record,
        turn.turnID == turnID,
        case .toolReconciliationRequired(
          let snapshotFrontierID,
          _,
          let snapshotToolAttemptID
        ) = turn.state
      else {
        return false
      }
      return snapshotFrontierID == terminalFrontierID
        && snapshotToolAttemptID == requiredAttemptID
    }
    guard frontierMatches else {
      return []
    }
    guard let terminalRequestIDs = terminalToolRequestIDs(
      in: snapshot,
      turnID: turnID
    ) else {
      return []
    }
    var currentSuffix: [TerminalToolResultEvidence] = []
    var terminalSuffix: Set<SignalboxCanonicalUUID> = []
    for record in snapshot.records {
      if let evidence = terminalToolResultEvidence(
        for: record,
        in: snapshot,
        turnID: turnID
      ) {
        currentSuffix.append(evidence)
      } else if !currentSuffix.isEmpty {
        if terminalResultRun(
          currentSuffix,
          matches: terminalRequestIDs,
          requiredAttemptID: requiredAttemptID
        ) {
          terminalSuffix = Set(currentSuffix.map(\.entryID))
        }
        currentSuffix = []
      }
    }
    if terminalResultRun(
      currentSuffix,
      matches: terminalRequestIDs,
      requiredAttemptID: requiredAttemptID
    ) {
      terminalSuffix = Set(currentSuffix.map(\.entryID))
    }
    return terminalSuffix
  }

  private func terminalToolRequestIDs(
    in snapshot: SignalboxSynchronizationSnapshot,
    turnID: SignalboxCanonicalUUID
  ) -> Set<String>? {
    let terminalModelCallID = snapshot.records.reversed().compactMap {
      record -> SignalboxCanonicalUUID? in
      guard case .entry(let message) = record,
        message.sourceSessionID == snapshot.sessionID,
        case .assistantToolUse(let entryTurnID, let modelCallID, _, _, _, _) = message.entry,
        entryTurnID == turnID
      else {
        return nil
      }
      return modelCallID
    }.first
    guard let terminalModelCallID else {
      return nil
    }
    let requestIDs = Set(snapshot.records.compactMap { record -> String? in
      guard case .entry(let message) = record,
        message.sourceSessionID == snapshot.sessionID,
        case .assistantToolUse(
          let entryTurnID,
          let modelCallID,
          let requestID,
          _,
          _,
          _
        ) = message.entry,
        entryTurnID == turnID,
        modelCallID == terminalModelCallID
      else {
        return nil
      }
      return requestID.rawValue
    })
    return requestIDs.isEmpty ? nil : requestIDs
  }

  private func terminalToolResultEvidence(
    for record: SignalboxSynchronizationSnapshot.Record,
    in snapshot: SignalboxSynchronizationSnapshot,
    turnID: SignalboxCanonicalUUID
  ) -> TerminalToolResultEvidence? {
    guard case .entry(let message) = record,
      message.sourceSessionID == snapshot.sessionID,
      toolEntry(message, belongsTo: turnID, modelCallID: nil)
    else {
      return nil
    }
    switch message.entry {
    case .toolExecutionResult(let requestID, let attemptID, _):
      return TerminalToolResultEvidence(
        entryID: message.entryID,
        requestID: requestID.rawValue,
        attemptID: attemptID,
        closesAttemptWithoutID: false
      )
    case .toolDenied(let requestID, _):
      return TerminalToolResultEvidence(
        entryID: message.entryID,
        requestID: requestID.rawValue,
        attemptID: nil,
        closesAttemptWithoutID: false
      )
    case .toolClosed(let requestID, _):
      return TerminalToolResultEvidence(
        entryID: message.entryID,
        requestID: requestID.rawValue,
        attemptID: nil,
        closesAttemptWithoutID: true
      )
    case .delegationResult(let requestID, _, _, .foreground, _, _, _, _, _):
      return TerminalToolResultEvidence(
        entryID: message.entryID,
        requestID: requestID.rawValue,
        attemptID: nil,
        closesAttemptWithoutID: true
      )
    case .delegatedTask, .delegationMessage,
      .delegationResult(_, _, _, .background, _, _, _, _, _),
      .modelIdentityChanged, .runnerPlacementChanged, .assistantToolUse, .turnCompleted, .turnFailed,
      .turnCancelled, .imported, .unknown:
      return nil
    }
  }

  private func terminalResultRun(
    _ evidence: [TerminalToolResultEvidence],
    matches terminalRequestIDs: Set<String>,
    requiredAttemptID: SignalboxCanonicalUUID
  ) -> Bool {
    guard evidence.count == terminalRequestIDs.count,
      Set(evidence.map(\.requestID)) == terminalRequestIDs
    else {
      return false
    }
    let attemptIDs = evidence.compactMap(\.attemptID)
    if attemptIDs.contains(requiredAttemptID) {
      return true
    }
    let attemptLessEvidence = evidence.filter { $0.attemptID == nil }
    return attemptLessEvidence.count == 1
      && attemptLessEvidence[0].closesAttemptWithoutID
  }

  private func retainedPresentationIdentities(
    in snapshot: SignalboxSynchronizationSnapshot
  ) -> Set<PresentationIdentity> {
    Set(snapshot.records.compactMap { record in
      switch record {
      case .turn(let turn):
        return .turnState(turn.turnID.rawValue)
      case .modelCallUsage(let usage):
        return .modelCallUsage(usage.modelCallID.rawValue)
      case .entry(let message):
        return .semantic(
          sourceSessionID: message.sourceSessionID.rawValue,
          entryID: message.entryID.rawValue
        )
      case .textEntry(let message):
        return .semantic(
          sourceSessionID: message.sourceSessionID.rawValue,
          entryID: message.entryID.rawValue
        )
      case .content:
        return nil
      }
    })
  }

  private func isExactTerminalMarker(
    _ message: SignalboxTranscriptEntryMessage,
    for trigger: SignalboxProcessSessionEvent,
    nativeSourceSessionID: SignalboxCanonicalUUID
  ) -> Bool {
    guard message.sourceSessionID == nativeSourceSessionID else {
      return false
    }
    switch trigger {
    case .turnCompleted(let turnID, _, let completionEntryID, _):
      guard case .turnCompleted(let entryTurnID) = message.entry else {
        return false
      }
      return message.entryID == completionEntryID && entryTurnID == turnID
    case .turnFailed(let turnID, let failureEntryID, _):
      guard case .turnFailed(let entryTurnID) = message.entry else {
        return false
      }
      return message.entryID == failureEntryID && entryTurnID == turnID
    case .turnCancelled(let turnID, let cancellationEntryID, _):
      guard case .turnCancelled(let entryTurnID) = message.entry else {
        return false
      }
      return message.entryID == cancellationEntryID && entryTurnID == turnID
    case .sessionCreated, .sessionModelSettingsChanged, .turnModelSettingsResolved,
      .inputAccepted, .turnActivated, .modelCallTransition, .toolBatchTransition,
      .toolApprovalDecided, .contextCompacted, .turnRefused, .turnReconciliationRequired,
      .turnToolReconciliationRequired, .runnerStateTransition, .unknown:
      return false
    }
  }

  private func containsRequiredEvidence(
    in snapshot: SignalboxSynchronizationSnapshot,
    for trigger: SignalboxProcessSessionEvent,
    terminalResultEntryIDs: Set<SignalboxCanonicalUUID>
  ) -> Bool {
    switch trigger {
    case .toolBatchTransition(let turnID, let modelCallID, let state):
      switch state {
      case .proposed:
        return snapshot.records.contains { record in
          guard case .entry(let message) = record,
            message.sourceSessionID == snapshot.sessionID,
            case .assistantToolUse(let entryTurnID, let entryModelCallID, _, _, _, _) =
              message.entry
          else {
            return false
          }
          return entryTurnID == turnID && entryModelCallID == modelCallID
        }
      case .resultsProjected:
        let expectedCorrelations = Set(
          toolContextsByIdentity.compactMap {
            identity, context -> ToolCorrelation? in
            guard identity.sourceSessionID == snapshot.sessionID.rawValue,
              context.turnID == turnID,
              context.modelCallID == modelCallID
            else {
              return nil
            }
            return identity.correlation
          })
        guard !expectedCorrelations.isEmpty else {
          return false
        }
        let projectedCorrelations = Set(
          snapshot.records.compactMap {
            record -> ToolCorrelation? in
            guard case .entry(let message) = record,
              message.sourceSessionID == snapshot.sessionID
            else {
              return nil
            }
            switch message.entry {
            case .toolExecutionResult(let requestID, _, _), .toolDenied(let requestID, _),
              .toolClosed(let requestID, _),
              .delegationResult(let requestID, _, _, .foreground, _, _, _, _, _):
              return ToolCorrelation(
                sourceSessionID: message.sourceSessionID.rawValue,
                requestID: requestID.rawValue
              )
            case .delegatedTask, .delegationMessage,
              .delegationResult(_, _, _, .background, _, _, _, _, _),
              .modelIdentityChanged, .runnerPlacementChanged, .assistantToolUse, .turnCompleted, .turnFailed,
              .turnCancelled, .imported, .unknown:
              return nil
            }
          })
        return expectedCorrelations.isSubset(of: projectedCorrelations)
      case .recoveryRequired, .unknown:
        return true
      }
    case .contextCompacted(_, let modelCallID, _, let summaryEntryID, _):
      return snapshot.records.contains {
        guard case .textEntry(let message) = $0,
          message.sourceSessionID == snapshot.sessionID,
          message.entryID == summaryEntryID,
          case .contextSummary(let entryModelCallID, _, _, _, _) = message.entry
        else {
          return false
        }
        return entryModelCallID == modelCallID
      }
    case .turnCompleted(let turnID, let modelCallID, _, _):
      let hasAssistantText = snapshot.records.contains {
        guard case .textEntry(let message) = $0,
          message.sourceSessionID == snapshot.sessionID,
          case .assistant(let entryTurnID, let entryModelCallID) = message.entry
        else {
          return false
        }
        return entryTurnID == turnID && entryModelCallID == modelCallID
      }
      let hasCompletionMarker = snapshot.records.contains {
        guard case .entry(let message) = $0 else {
          return false
        }
        return isExactTerminalMarker(
          message,
          for: trigger,
          nativeSourceSessionID: snapshot.sessionID
        )
      }
      return hasCompletionMarker && hasAssistantText
    case .turnFailed:
      return snapshot.records.contains {
        guard case .entry(let message) = $0 else {
          return false
        }
        return isExactTerminalMarker(
          message,
          for: trigger,
          nativeSourceSessionID: snapshot.sessionID
        )
      }
    case .turnCancelled:
      return snapshot.records.contains {
        guard case .entry(let message) = $0 else {
          return false
        }
        return isExactTerminalMarker(
          message,
          for: trigger,
          nativeSourceSessionID: snapshot.sessionID
        )
      }
    case .turnRefused(let turnID, let modelCallID, let terminalFrontierID):
      return snapshot.records.contains { record in
        guard case .turn(let turn) = record,
          turn.turnID == turnID,
          case .refused(let frontierID, _, let terminalModelCallID) = turn.state
        else {
          return false
        }
        return frontierID == terminalFrontierID && terminalModelCallID == modelCallID
      }
    case .turnReconciliationRequired(let turnID, let modelCallID, let terminalFrontierID):
      return snapshot.records.contains { record in
        guard case .turn(let turn) = record,
          turn.turnID == turnID,
          case .reconciliationRequired(
            let frontierID,
            _,
            let terminalModelCallID
          ) = turn.state
        else {
          return false
        }
        return frontierID == terminalFrontierID && terminalModelCallID == modelCallID
      }
    case .turnToolReconciliationRequired:
      return !terminalResultEntryIDs.isEmpty
    case .sessionCreated, .sessionModelSettingsChanged, .turnModelSettingsResolved,
      .inputAccepted, .turnActivated, .modelCallTransition, .toolApprovalDecided,
      .runnerStateTransition, .unknown:
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
      return (
        .user,
        nil,
        SignalboxProcessMessageSourceAttribution.importedUserRole.presentationLabel,
        .importedUserRole
      )
    case .attested(.assistant):
      return (
        .assistant,
        nil,
        SignalboxProcessMessageSourceAttribution.importedAssistantRole.presentationLabel,
        .importedAssistantRole
      )
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
        .unknown,
        nil,
        SignalboxProcessMessageSourceAttribution.importedSpeakerNotAttested.presentationLabel,
        .importedSpeakerNotAttested
      )
    case .attestedAbsent:
      return (
        .unknown,
        nil,
        SignalboxProcessMessageSourceAttribution.importedSpeakerAbsent.presentationLabel,
        .importedSpeakerAbsent
      )
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
    case .queued, .queuedDelegated, .queuedDelegationWake:
      return .init(state: .queued, label: "Queued")
    case .delegationTerminated(_, let outcome, _, _):
      let label = outcome == .stopped ? "Stopped by parent" : "Cancelled by parent"
      return .init(state: .cancelled, label: label)
    case .activeRunning(_, let currentModelCall):
      if let currentModelCall, case .unknown = currentModelCall.state {
        return .init(state: .recoveryRequired, label: "Recovery required")
      }
      return .init(state: .running, label: "Running")
    case .activeAwaitingChild:
      return .init(state: .running, label: "Awaiting child")
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
