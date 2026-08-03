import Foundation

public enum SignalboxTimelineItem: Identifiable, Equatable, Sendable {
    case message(SignalboxTimelineMessage)
    case tool(SignalboxToolCard)
    case processEvidence(SignalboxProcessNoticeCard)
    case turnFailure(SignalboxTurnFailureCard)
    case unknown(SignalboxUnknownEventCard)

    public var id: String {
        switch self {
        case .message(let message):
            return "message-\(message.eventID.rawValue)"
        case .tool(let tool):
            return "tool-\(tool.invocationID.rawValue)"
        case .processEvidence(let notice):
            return "notice-\(notice.eventID.rawValue)"
        case .turnFailure(let failure):
            return "failure-\(failure.eventID.rawValue)"
        case .unknown(let unknown):
            return "unknown-\(unknown.eventID.rawValue)"
        }
    }
}

public struct SignalboxTimelineMessage: Equatable, Sendable {
    public let eventID: SignalboxEventID
    public let role: SignalboxMessageRole
    public let text: String
    public let unrecognizedKind: String?
    public let thinkingText: String?
    public let isStreaming: Bool
    public let createdAt: Date?
    public let label: String?
    public let systemImage: String?
}

public struct SignalboxProcessNoticeDetail: Equatable, Sendable {
    public let label: String
    public let value: String
}

public struct SignalboxProcessNoticeCard: Equatable, Sendable {
    public let eventID: SignalboxEventID
    public let title: String
    public let systemImage: String
    public let details: [SignalboxProcessNoticeDetail]
}

public struct SignalboxToolCard: Identifiable, Equatable, Sendable {
    public let eventID: SignalboxEventID
    public let invocationID: SignalboxToolInvocationID
    public let toolName: String
    public let status: SignalboxToolCardStatus
    public let arguments: String?
    public let output: String?
    public let statusUpdates: [String]
    public let decisionReason: String?
    public let childSessionID: SignalboxSessionID?
    public let decisionAvailable: Bool
    public let presentationName: String?
    public let presentationArgumentsLabel: String?
    public let presentationArguments: String?
    public let presentationOutputLabel: String?
    public let presentationOutput: String?

    public init(
        eventID: SignalboxEventID,
        invocationID: SignalboxToolInvocationID,
        toolName: String,
        status: SignalboxToolCardStatus,
        arguments: String?,
        output: String?,
        statusUpdates: [String],
        decisionReason: String?,
        childSessionID: SignalboxSessionID?,
        decisionAvailable: Bool = true,
        presentationName: String? = nil,
        presentationArgumentsLabel: String? = nil,
        presentationArguments: String? = nil,
        presentationOutputLabel: String? = nil,
        presentationOutput: String? = nil
    ) {
        self.eventID = eventID
        self.invocationID = invocationID
        self.toolName = toolName
        self.status = status
        self.arguments = arguments
        self.output = output
        self.statusUpdates = statusUpdates
        self.decisionReason = decisionReason
        self.childSessionID = childSessionID
        self.decisionAvailable = decisionAvailable
        self.presentationName = presentationName
        self.presentationArgumentsLabel = presentationArgumentsLabel
        self.presentationArguments = presentationArguments
        self.presentationOutputLabel = presentationOutputLabel
        self.presentationOutput = presentationOutput
    }

    public var id: SignalboxToolInvocationID { invocationID }

    public var displayName: String { presentationName ?? toolName }

    public var argumentsLabel: String { presentationArgumentsLabel ?? "Arguments" }

    public var outputLabel: String { presentationOutputLabel ?? "Output" }

    public var compactArgumentSummary: String {
        let trimmed = (presentationArguments ?? arguments ?? "")
            .trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty {
            return "No arguments"
        }
        if trimmed.count <= 180 {
            return trimmed
        }
        return String(trimmed.prefix(180)) + "..."
    }

    public var outputPreview: String {
        let trimmed = (presentationOutput ?? output ?? "")
            .trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty {
            return "No output yet"
        }
        if trimmed.count <= 480 {
            return trimmed
        }
        return String(trimmed.prefix(480)) + "..."
    }
}

public enum SignalboxToolCardStatus: Equatable, Sendable {
    case proposed
    case waitingForApproval
    case running
    case approved
    case denied
    case succeeded
    case failed
    case completed
    case closed

    public var label: String {
        switch self {
        case .proposed:
            return "Proposed"
        case .waitingForApproval:
            return "Needs Approval"
        case .running:
            return "Running"
        case .approved:
            return "Approved"
        case .denied:
            return "Denied"
        case .succeeded:
            return "Succeeded"
        case .failed:
            return "Failed"
        case .completed:
            return "Completed"
        case .closed:
            return "Closed"
        }
    }
}

public struct SignalboxTurnFailureCard: Equatable, Sendable {
    public let eventID: SignalboxEventID
    public let reason: String
    public let runnerID: SignalboxRunnerID?
    public let failedAt: Date?
}

public struct SignalboxUnknownEventCard: Equatable, Sendable {
    public let eventID: SignalboxEventID
    public let kind: String
    public let diagnostic: String
}

struct SignalboxEventNormalizationMetrics: Equatable, Sendable {
    fileprivate(set) var recordEvaluationCount = 0

    init() {}
}

fileprivate enum SignalboxTimelineLinkage {
    case linked
    case unlinked
}

public enum SignalboxEventNormalizer {
    public static func normalize(_ records: [SignalboxStoredEvent]) -> [SignalboxTimelineItem] {
        var metrics = SignalboxEventNormalizationMetrics()
        return normalize(records, recording: &metrics)
    }

    static func normalize(
        _ records: [SignalboxStoredEvent],
        recording metrics: inout SignalboxEventNormalizationMetrics
    ) -> [SignalboxTimelineItem] {
        let recordsByID = Dictionary(records.map { ($0.eventID, $0.event) }, uniquingKeysWith: { first, _ in first })
        let toolLinkEventIDs = Set(records.compactMap { record -> SignalboxEventID? in
            guard case .toolInvocation(let invocation) = record.event else {
                return nil
            }
            return invocation.functionCallEventID
        })
        let toolResponseEventIDs = Set(records.compactMap { record -> SignalboxEventID? in
            guard case .toolInvocation(let invocation) = record.event else {
                return nil
            }
            return invocation.functionResponseEventID
        })

        metrics.recordEvaluationCount += records.count
        return records.compactMap { record in
            normalize(
                record,
                recordsByID: recordsByID,
                functionCallLinkage: toolLinkEventIDs.contains(record.eventID) ? .linked : .unlinked,
                functionResponseLinkage: toolResponseEventIDs.contains(record.eventID) ? .linked : .unlinked
            )
        }
    }

    fileprivate static func normalize(
        _ record: SignalboxStoredEvent,
        recordsByID: [SignalboxEventID: SignalboxConversationEvent],
        functionCallLinkage: SignalboxTimelineLinkage,
        functionResponseLinkage: SignalboxTimelineLinkage
    ) -> SignalboxTimelineItem? {
        switch record.event {
        case .message(let event):
            return normalizedMessage(
                record: record,
                event: event,
                functionCallLinkage: functionCallLinkage,
                functionResponseLinkage: functionResponseLinkage
            )
        case .toolInvocation(let event):
            return .tool(toolCard(record: record, event: event, recordsByID: recordsByID))
        case .turnFailed(let event):
            return .turnFailure(
                SignalboxTurnFailureCard(
                    eventID: record.eventID,
                    reason: event.reason,
                    runnerID: event.runnerID,
                    failedAt: event.failedAt
                )
            )
        case .processMessage(let event):
            return .message(
                SignalboxTimelineMessage(
                    eventID: record.eventID,
                    role: event.role,
                    text: event.text,
                    unrecognizedKind: event.unrecognizedKind,
                    thinkingText: nil,
                    isStreaming: false,
                    createdAt: nil,
                    label: event.sourceAttribution?.presentationLabel,
                    systemImage: nil
                )
            )
        case .processContextSummary(let event):
            return .message(
                SignalboxTimelineMessage(
                    eventID: record.eventID,
                    role: .assistant,
                    text: event.text,
                    unrecognizedKind: nil,
                    thinkingText: nil,
                    isStreaming: false,
                    createdAt: nil,
                    label: "Context summary",
                    systemImage: "doc.text"
                )
            )
        case .processModelIdentity(let event):
            return .processEvidence(
                SignalboxProcessNoticeCard(
                    eventID: record.eventID,
                    title: "Model changed",
                    systemImage: "arrow.triangle.2.circlepath",
                    details: [
                        .init(label: "Turn", value: event.turnID.rawValue),
                        .init(label: "Selected model", value: event.selectedModelID.rawValue),
                        .init(label: "Defaults version", value: event.defaultsVersion.rawValue.description),
                    ]
                )
            )
        case .processModelCallUsage(let event):
            return .processEvidence(
                SignalboxProcessNoticeCard(
                    eventID: record.eventID,
                    title: "Model usage",
                    systemImage: "number.circle",
                    details: modelCallUsageDetails(event)
                )
            )
        case .processImportedContent(let event):
            return .processEvidence(importedContentCard(record: record, event: event))
        case .processTool(let event):
            let plan = planPresentation(
                toolName: event.toolName,
                arguments: event.arguments,
                output: event.output,
                status: event.status
            )
            return .tool(
                SignalboxToolCard(
                    eventID: record.eventID,
                    invocationID: event.toolRequestID,
                    toolName: event.toolName,
                    status: processToolStatus(event.status),
                    arguments: event.arguments,
                    output: event.output,
                    statusUpdates: [],
                    decisionReason: nil,
                    childSessionID: nil,
                    decisionAvailable: true,
                    presentationName: plan?.name,
                    presentationArgumentsLabel: plan?.argumentsLabel,
                    presentationArguments: plan?.arguments,
                    presentationOutputLabel: plan?.outputLabel,
                    presentationOutput: plan?.output
                )
            )
        case .processTurnFailure(let event):
            return .turnFailure(
                SignalboxTurnFailureCard(
                    eventID: record.eventID,
                    reason: event.reason,
                    runnerID: nil,
                    failedAt: nil
                )
            )
        case .processConservative(let event):
            return .unknown(
                SignalboxUnknownEventCard(
                    eventID: record.eventID,
                    kind: event.kind,
                    diagnostic: event.diagnostic
                )
            )
        case .unknown(let event):
            guard event.payload["visible_to_user"] != .bool(false) else {
                return nil
            }
            return .unknown(
                SignalboxUnknownEventCard(
                    eventID: record.eventID,
                    kind: event.kind,
                    diagnostic: event.decodingDiagnostic?.message
                        ?? event.payload.keys.sorted().joined(separator: ", ")
                )
            )
        }
    }

    private static func processToolStatus(
        _ status: SignalboxProcessToolStatus
    ) -> SignalboxToolCardStatus {
        switch status {
        case .proposed:
            return .proposed
        case .awaitingDecision:
            return .waitingForApproval
        case .completed:
            return .completed
        case .denied:
            return .denied
        case .closed:
            return .closed
        case .recoveryRequired:
            return .failed
        }
    }

    private struct PlanPresentation {
        let name: String
        let argumentsLabel: String
        let arguments: String?
        let outputLabel: String?
        let output: String?
    }

    private struct PlanReadArguments: Decodable {
        let afterEntryID: UInt64?
        let includeHistory: Bool?

        private enum CodingKeys: String, CodingKey {
            case afterEntryID = "after_entry_id"
            case includeHistory = "include_history"
        }

        init(from decoder: Decoder) throws {
            let payload = try SignalboxUntaggedPayload(from: decoder)
            try payload.rejectUnadmittedFields(
                ["after_entry_id", "include_history"],
                decoder: decoder
            )
            let container = try decoder.container(keyedBy: CodingKeys.self)
            afterEntryID = try container.decodeIfPresent(UInt64.self, forKey: .afterEntryID)
            if container.contains(.includeHistory) {
                includeHistory = try container.decode(Bool.self, forKey: .includeHistory)
            } else {
                includeHistory = nil
            }
            guard afterEntryID.map({ $0 > 0 }) ?? true else {
                throw DecodingError.dataCorruptedError(
                    forKey: .afterEntryID,
                    in: container,
                    debugDescription: "The plan-read cursor must be positive."
                )
            }
        }
    }

    private struct PlanReadOutput: Decodable {
        let entries: [PlanEntry]
        let nextAfterEntryID: UInt64?
        let planTruncated: Bool
        let history: [PlanEvent]?
        let historyTruncated: Bool

        private enum CodingKeys: String, CodingKey {
            case entries
            case nextAfterEntryID = "next_after_entry_id"
            case planTruncated = "plan_truncated"
            case history
            case historyTruncated = "history_truncated"
        }

        init(from decoder: Decoder) throws {
            let payload = try SignalboxUntaggedPayload(from: decoder)
            let fields: Set<String> = [
                "entries", "next_after_entry_id", "plan_truncated", "history",
                "history_truncated",
            ]
            try payload.rejectUnadmittedFields(fields, decoder: decoder)
            try payload.requireFields(fields, decoder: decoder)
            let container = try decoder.container(keyedBy: CodingKeys.self)
            entries = try container.decode([PlanEntry].self, forKey: .entries)
            nextAfterEntryID = try container.decodeIfPresent(UInt64.self, forKey: .nextAfterEntryID)
            planTruncated = try container.decode(Bool.self, forKey: .planTruncated)
            history = try container.decodeIfPresent([PlanEvent].self, forKey: .history)
            historyTruncated = try container.decode(Bool.self, forKey: .historyTruncated)
            let cursorMatchesTruncation = planTruncated
                ? entries.last.map { nextAfterEntryID == $0.entryID } ?? false
                : nextAfterEntryID == nil
            guard nextAfterEntryID.map({ $0 > 0 }) ?? true,
                cursorMatchesTruncation,
                SignalboxEventNormalizer.validPlanEntries(entries),
                SignalboxEventNormalizer.validPlanHistory(
                    history,
                    truncated: historyTruncated
                )
            else {
                throw DecodingError.dataCorruptedError(
                    forKey: .nextAfterEntryID,
                    in: container,
                    debugDescription:
                        "Plan read output contains contradictory pagination or relationships."
                )
            }
        }
    }

    private struct PlanEntry: Decodable, Equatable {
        let entryID: UInt64
        let text: String
        let status: String
        let dependencies: [UInt64]
        let readiness: String

        private enum CodingKeys: String, CodingKey {
            case entryID = "entry_id"
            case text
            case status
            case dependencies
            case readiness
        }

        init(
            entryID: UInt64,
            text: String,
            status: String,
            dependencies: [UInt64],
            readiness: String
        ) {
            self.entryID = entryID
            self.text = text
            self.status = status
            self.dependencies = dependencies
            self.readiness = readiness
        }

        init(from decoder: Decoder) throws {
            let payload = try SignalboxUntaggedPayload(from: decoder)
            let fields: Set<String> = [
                "entry_id", "text", "status", "dependencies", "readiness",
            ]
            try payload.rejectUnadmittedFields(fields, decoder: decoder)
            try payload.requireFields(fields, decoder: decoder)
            let container = try decoder.container(keyedBy: CodingKeys.self)
            entryID = try container.decode(UInt64.self, forKey: .entryID)
            text = try container.decode(String.self, forKey: .text)
            status = try container.decode(String.self, forKey: .status)
            dependencies = try container.decode([UInt64].self, forKey: .dependencies)
            readiness = try container.decode(String.self, forKey: .readiness)
            guard entryID > 0,
                SignalboxEventNormalizer.validPlanText(text),
                SignalboxEventNormalizer.validPlanStatus(status),
                dependencies.allSatisfy({ $0 > 0 }),
                Set(dependencies).count == dependencies.count,
                dependencies.count
                    <= SignalboxProcessProtocol.maximumPlanDependenciesPerEntry,
                SignalboxEventNormalizer.validPlanReadiness(readiness)
            else {
                throw DecodingError.dataCorrupted(
                    .init(
                        codingPath: decoder.codingPath,
                        debugDescription: "Plan entry contains invalid current values."
                    )
                )
            }
        }
    }

    private struct PlanEvent: Decodable {
        let ordinal: UInt64
        let kind: String
        let entryID: UInt64
        let text: String?
        let status: String?
        let dependencyID: UInt64?
        let provenance: PlanEventProvenance

        private enum CodingKeys: String, CodingKey {
            case ordinal
            case kind
            case entryID = "entry_id"
            case text
            case status
            case dependencyID = "dependency_id"
            case provenance
        }

        init(from decoder: Decoder) throws {
            let payload = try SignalboxUntaggedPayload(from: decoder)
            let container = try decoder.container(keyedBy: CodingKeys.self)
            ordinal = try container.decode(UInt64.self, forKey: .ordinal)
            kind = try container.decode(String.self, forKey: .kind)
            entryID = try container.decode(UInt64.self, forKey: .entryID)
            provenance = try container.decode(PlanEventProvenance.self, forKey: .provenance)
            guard ordinal > 0, entryID > 0 else {
                throw DecodingError.dataCorruptedError(
                    forKey: .ordinal,
                    in: container,
                    debugDescription: "Plan event identities must be positive."
                )
            }
            switch kind {
            case "created":
                try payload.rejectUnadmittedFields(
                    ["ordinal", "kind", "entry_id", "text", "provenance"],
                    decoder: decoder
                )
                text = try container.decode(String.self, forKey: .text)
                status = nil
                dependencyID = nil
                guard entryID == ordinal, text.map(SignalboxEventNormalizer.validPlanText) == true
                else {
                    throw DecodingError.dataCorruptedError(
                        forKey: .text,
                        in: container,
                        debugDescription: "Created plan event is invalid."
                    )
                }
            case "text_revised":
                try payload.rejectUnadmittedFields(
                    ["ordinal", "kind", "entry_id", "text", "provenance"],
                    decoder: decoder
                )
                text = try container.decode(String.self, forKey: .text)
                status = nil
                dependencyID = nil
                guard entryID < ordinal,
                    text.map(SignalboxEventNormalizer.validPlanText) == true
                else {
                    throw DecodingError.dataCorruptedError(
                        forKey: .text,
                        in: container,
                        debugDescription: "Revised plan text is invalid."
                    )
                }
            case "status_changed":
                try payload.rejectUnadmittedFields(
                    ["ordinal", "kind", "entry_id", "status", "provenance"],
                    decoder: decoder
                )
                text = nil
                status = try container.decode(String.self, forKey: .status)
                dependencyID = nil
                guard entryID < ordinal,
                    status.map(SignalboxEventNormalizer.validPlanStatus) == true
                else {
                    throw DecodingError.dataCorruptedError(
                        forKey: .status,
                        in: container,
                        debugDescription: "Plan event status is invalid."
                    )
                }
            case "depends_on":
                try payload.rejectUnadmittedFields(
                    ["ordinal", "kind", "entry_id", "dependency_id", "provenance"],
                    decoder: decoder
                )
                text = nil
                status = nil
                dependencyID = try container.decode(UInt64.self, forKey: .dependencyID)
                guard entryID < ordinal,
                    dependencyID.map({ $0 > 0 && $0 < ordinal && $0 != entryID }) == true
                else {
                    throw DecodingError.dataCorruptedError(
                        forKey: .dependencyID,
                        in: container,
                        debugDescription: "Plan dependency identity must be positive."
                    )
                }
            default:
                throw DecodingError.dataCorruptedError(
                    forKey: .kind,
                    in: container,
                    debugDescription: "Plan event kind is unrecognized."
                )
            }
        }
    }

    private struct PlanEventProvenance: Decodable {
        let turnID: SignalboxCanonicalUUID
        let issuingAttemptID: SignalboxCanonicalUUID
        let requestID: SignalboxCanonicalUUID
        let attemptID: SignalboxCanonicalUUID
        let generation: UInt64

        private enum CodingKeys: String, CodingKey {
            case turnID = "turn_id"
            case issuingAttemptID = "issuing_attempt_id"
            case requestID = "request_id"
            case attemptID = "attempt_id"
            case generation
        }

        init(from decoder: Decoder) throws {
            let payload = try SignalboxUntaggedPayload(from: decoder)
            try payload.rejectUnadmittedFields(
                ["turn_id", "issuing_attempt_id", "request_id", "attempt_id", "generation"],
                decoder: decoder
            )
            let container = try decoder.container(keyedBy: CodingKeys.self)
            turnID = try container.decode(SignalboxCanonicalUUID.self, forKey: .turnID)
            issuingAttemptID = try container.decode(
                SignalboxCanonicalUUID.self,
                forKey: .issuingAttemptID
            )
            requestID = try container.decode(SignalboxCanonicalUUID.self, forKey: .requestID)
            attemptID = try container.decode(SignalboxCanonicalUUID.self, forKey: .attemptID)
            generation = try container.decode(UInt64.self, forKey: .generation)
            guard generation > 0 else {
                throw DecodingError.dataCorruptedError(
                    forKey: .generation,
                    in: container,
                    debugDescription: "Plan event generation must be positive."
                )
            }
        }
    }

    private struct PlanWriteArguments: Decodable {
        let kind: String
        let entryID: UInt64?
        let text: String?
        let status: String?
        let dependencyID: UInt64?

        private enum CodingKeys: String, CodingKey {
            case kind
            case entryID = "entry_id"
            case text
            case status
            case dependencyID = "dependency_id"
        }

        init(from decoder: Decoder) throws {
            let payload = try SignalboxUntaggedPayload(from: decoder)
            let container = try decoder.container(keyedBy: CodingKeys.self)
            kind = try container.decode(String.self, forKey: .kind)
            let admittedFields: Set<String>
            switch kind {
            case "create":
                admittedFields = ["kind", "text"]
            case "revise":
                admittedFields = ["kind", "entry_id", "text"]
            case "set_status":
                admittedFields = ["kind", "entry_id", "status"]
            case "depends_on":
                admittedFields = ["kind", "entry_id", "dependency_id"]
            default:
                throw DecodingError.dataCorruptedError(
                    forKey: .kind,
                    in: container,
                    debugDescription: "The plan operation is unrecognized."
                )
            }
            try payload.rejectUnadmittedFields(admittedFields, decoder: decoder)
            try payload.requireFields(admittedFields, decoder: decoder)
            entryID = try container.decodeIfPresent(UInt64.self, forKey: .entryID)
            text = try container.decodeIfPresent(String.self, forKey: .text)
            status = try container.decodeIfPresent(String.self, forKey: .status)
            dependencyID = try container.decodeIfPresent(UInt64.self, forKey: .dependencyID)
            guard SignalboxEventNormalizer.validPlanWriteArguments(self) else {
                throw DecodingError.dataCorrupted(
                    .init(
                        codingPath: decoder.codingPath,
                        debugDescription: "The plan operation contains invalid values."
                    )
                )
            }
        }
    }

    private struct PlanWriteOutput: Decodable {
        let event: PlanEvent

        private enum CodingKeys: String, CodingKey {
            case event
        }

        init(from decoder: Decoder) throws {
            let payload = try SignalboxUntaggedPayload(from: decoder)
            try payload.rejectUnadmittedFields(["event"], decoder: decoder)
            try payload.requireFields(["event"], decoder: decoder)
            event = try decoder.container(keyedBy: CodingKeys.self).decode(
                PlanEvent.self,
                forKey: .event
            )
        }
    }

    private static func planPresentation(
        toolName: String,
        arguments: String?,
        output: String?,
        status: SignalboxProcessToolStatus
    ) -> PlanPresentation? {
        switch toolName {
        case "plan_read":
            let decodedArguments = arguments.flatMap(decodePlanReadArguments)
            let decodedOutput = status == .completed
                ? output.flatMap { decode(PlanReadOutput.self, from: $0) }
                : nil
            let presentationOutput = decodedArguments.flatMap { arguments in
                decodedOutput.flatMap { output in
                    validPlanReadResponse(arguments: arguments, output: output)
                        ? formattedPlanReadOutput(output)
                        : nil
                }
            }
            return PlanPresentation(
                name: "Plan read",
                argumentsLabel: "Read request",
                arguments: decodedArguments.map(formattedPlanReadArguments),
                outputLabel: presentationOutput == nil ? nil : "Current plan",
                output: presentationOutput
            )
        case "plan_write":
            let decodedArguments = arguments.flatMap(decodePlanWriteArguments)
            let decodedOutput = status == .completed
                ? output.flatMap { decode(PlanWriteOutput.self, from: $0) }
                : nil
            let presentationOutput = decodedArguments.flatMap { arguments in
                decodedOutput.flatMap { output in
                    planEvent(output.event, matches: arguments)
                        ? formattedPlanEvent(output.event)
                        : nil
                }
            }
            return PlanPresentation(
                name: "Plan update",
                argumentsLabel: "Plan operation",
                arguments: decodedArguments.map(formattedPlanWriteArguments),
                outputLabel: presentationOutput == nil ? nil : "Appended event",
                output: presentationOutput
            )
        default:
            return nil
        }
    }

    private static func formattedPlanReadArguments(_ value: PlanReadArguments) -> String {
        return [
            "After entry: \(value.afterEntryID?.description ?? "Beginning")",
            "Include history: \(yesNo(value.includeHistory ?? false))",
        ].joined(separator: "\n")
    }

    private static func formattedPlanReadOutput(_ value: PlanReadOutput) -> String {
        let entries = value.entries.map { entry in
            let dependencies = entry.dependencies.isEmpty
                ? "None"
                : entry.dependencies.map { "#\($0)" }.joined(separator: ", ")
            return "#\(entry.entryID) [\(planStatusLabel(entry.status)), "
                + "\(planReadinessLabel(entry.readiness))] \(escapedPlanText(entry.text))\n"
                + "Dependencies: \(dependencies)"
        }
        let history: [String]
        switch value.history {
        case nil:
            history = ["Not included"]
        case .some(let events) where events.isEmpty:
            history = ["None"]
        case .some(let events):
            history = events.map(formattedPlanEvent)
        }
        return (["Entries"] + (entries.isEmpty ? ["None"] : entries) + [
            "Next entry: \(value.nextAfterEntryID?.description ?? "None")",
            "Plan truncated: \(yesNo(value.planTruncated))",
            "History",
        ] + history + [
            "History truncated: \(yesNo(value.historyTruncated))"
        ]).joined(separator: "\n")
    }

    private static func decodePlanReadArguments(_ json: String) -> PlanReadArguments? {
        decode(PlanReadArguments.self, from: json)
    }

    private static func formattedPlanWriteArguments(_ value: PlanWriteArguments) -> String {
        return formattedPlanOperation(
            kind: value.kind,
            entryID: value.entryID,
            text: value.text,
            status: value.status,
            dependencyID: value.dependencyID
        )
    }

    private static func decodePlanWriteArguments(_ json: String) -> PlanWriteArguments? {
        decode(PlanWriteArguments.self, from: json)
    }

    private static func validPlanWriteArguments(_ value: PlanWriteArguments) -> Bool {
        switch value.kind {
        case "create":
            return value.text.map(validPlanText) == true
        case "revise":
            return value.entryID.map({ $0 > 0 }) == true
                && value.text.map(validPlanText) == true
        case "set_status":
            return value.entryID.map({ $0 > 0 }) == true
                && value.status.map(validPlanStatus) == true
        case "depends_on":
            return value.entryID.map({ $0 > 0 }) == true
                && value.dependencyID.map({ $0 > 0 }) == true
        default:
            return false
        }
    }

    private static func planEvent(
        _ event: PlanEvent,
        matches arguments: PlanWriteArguments
    ) -> Bool {
        switch arguments.kind {
        case "create":
            return event.kind == "created" && event.text == arguments.text
        case "revise":
            return event.kind == "text_revised"
                && event.entryID == arguments.entryID
                && event.text == arguments.text
        case "set_status":
            return event.kind == "status_changed"
                && event.entryID == arguments.entryID
                && event.status == arguments.status
        case "depends_on":
            return event.kind == "depends_on"
                && event.entryID == arguments.entryID
                && event.dependencyID == arguments.dependencyID
        default:
            return false
        }
    }

    private static func validPlanReadResponse(
        arguments: PlanReadArguments,
        output: PlanReadOutput
    ) -> Bool {
        guard (arguments.includeHistory ?? false) == (output.history != nil),
            output.entries.allSatisfy({ entry in
                arguments.afterEntryID.map { entry.entryID > $0 } ?? true
            })
        else {
            return false
        }
        let completeUncursoredPlan = arguments.afterEntryID == nil && !output.planTruncated
        if completeUncursoredPlan {
            let visibleEntryIDs = Set(output.entries.map(\.entryID))
            guard output.entries.allSatisfy({ entry in
                entry.dependencies.allSatisfy(visibleEntryIDs.contains)
            }) else {
                return false
            }
        }
        guard let history = output.history else {
            return true
        }
        guard let folded = foldedPlanHistory(history) else {
            return false
        }
        guard !output.historyTruncated else {
            return true
        }
        var expected = folded
        if let afterEntryID = arguments.afterEntryID {
            expected.removeAll { $0.entryID <= afterEntryID }
        }
        let expectedTruncated =
            expected.count > SignalboxProcessProtocol.maximumPlanReadEntries
        expected = Array(expected.prefix(SignalboxProcessProtocol.maximumPlanReadEntries))
        return output.entries == expected && output.planTruncated == expectedTruncated
    }

    private static func validPlanText(_ text: String) -> Bool {
        !text.isEmpty
            && text.unicodeScalars.count
                <= SignalboxProcessProtocol.maximumPlanTextUnicodeScalars
            && text.unicodeScalars.allSatisfy { $0.value != 0 }
    }

    private static func validPlanStatus(_ status: String) -> Bool {
        switch status {
        case "pending", "in_progress", "completed", "abandoned": return true
        default: return false
        }
    }

    private static func validPlanReadiness(_ readiness: String) -> Bool {
        switch readiness {
        case "ready", "waiting": return true
        default: return false
        }
    }

    private static func validPlanEntries(_ entries: [PlanEntry]) -> Bool {
        guard entries.count <= SignalboxProcessProtocol.maximumPlanReadEntries else {
            return false
        }
        var statuses: [UInt64: String] = [:]
        var priorEntryID: UInt64?
        for entry in entries {
            guard priorEntryID.map({ entry.entryID > $0 }) ?? true,
                statuses.updateValue(entry.status, forKey: entry.entryID) == nil
            else {
                return false
            }
            priorEntryID = entry.entryID
        }
        for entry in entries {
            let visibleIncomplete = entry.dependencies.contains { dependencyID in
                statuses[dependencyID].map { $0 != "completed" } ?? false
            }
            let allVisibleCompleted = entry.dependencies.allSatisfy { dependencyID in
                statuses[dependencyID] == "completed"
            }
            guard !((entry.readiness == "ready" && visibleIncomplete)
                || (entry.readiness == "waiting" && allVisibleCompleted))
            else {
                return false
            }
        }
        let dependencies = Dictionary(uniqueKeysWithValues: entries.map { entry in
            (entry.entryID, entry.dependencies.filter { statuses[$0] != nil })
        })
        return !planDependenciesContainCycle(dependencies)
    }

    private enum PlanDependencyVisit {
        case visiting
        case visited
    }

    private static func planDependenciesContainCycle(
        _ dependencies: [UInt64: [UInt64]]
    ) -> Bool {
        var visits: [UInt64: PlanDependencyVisit] = [:]
        for entryID in dependencies.keys {
            if planDependencyVisit(entryID, dependencies: dependencies, visits: &visits) {
                return true
            }
        }
        return false
    }

    private static func planDependencyVisit(
        _ entryID: UInt64,
        dependencies: [UInt64: [UInt64]],
        visits: inout [UInt64: PlanDependencyVisit]
    ) -> Bool {
        switch visits[entryID] {
        case .visiting:
            return true
        case .visited:
            return false
        case nil:
            visits[entryID] = .visiting
        }
        for dependencyID in dependencies[entryID, default: []] {
            if planDependencyVisit(dependencyID, dependencies: dependencies, visits: &visits) {
                return true
            }
        }
        visits[entryID] = .visited
        return false
    }

    private static func validPlanHistory(
        _ history: [PlanEvent]?,
        truncated: Bool
    ) -> Bool {
        guard let history else {
            return !truncated
        }
        guard history.count <= SignalboxProcessProtocol.maximumPlanHistoryEvents,
            !truncated || !history.isEmpty
        else {
            return false
        }
        var attemptIDs: Set<SignalboxCanonicalUUID> = []
        for (index, event) in history.enumerated() {
            guard event.ordinal == UInt64(index) + 1,
                attemptIDs.insert(event.provenance.attemptID).inserted
            else {
                return false
            }
        }
        return foldedPlanHistory(history) != nil
    }

    private static func foldedPlanHistory(_ history: [PlanEvent]) -> [PlanEntry]? {
        var entries: [UInt64: PlanEntry] = [:]
        var creationOrder: [UInt64] = []
        for event in history {
            switch event.kind {
            case "created":
                guard let text = event.text, entries[event.entryID] == nil else {
                    return nil
                }
                entries[event.entryID] = PlanEntry(
                    entryID: event.entryID,
                    text: text,
                    status: "pending",
                    dependencies: [],
                    readiness: "ready"
                )
                creationOrder.append(event.entryID)
            case "text_revised":
                guard let text = event.text, let entry = entries[event.entryID] else {
                    return nil
                }
                entries[event.entryID] = PlanEntry(
                    entryID: entry.entryID,
                    text: text,
                    status: entry.status,
                    dependencies: entry.dependencies,
                    readiness: entry.readiness
                )
            case "status_changed":
                guard let status = event.status, let entry = entries[event.entryID] else {
                    return nil
                }
                entries[event.entryID] = PlanEntry(
                    entryID: entry.entryID,
                    text: entry.text,
                    status: status,
                    dependencies: entry.dependencies,
                    readiness: entry.readiness
                )
            case "depends_on":
                guard let dependencyID = event.dependencyID,
                    var entry = entries[event.entryID],
                    entries[dependencyID] != nil
                else {
                    return nil
                }
                if !entry.dependencies.contains(dependencyID) {
                    guard entry.dependencies.count
                        < SignalboxProcessProtocol.maximumPlanDependenciesPerEntry
                    else {
                        return nil
                    }
                    let dependencies = Dictionary(uniqueKeysWithValues: entries.values.map {
                        ($0.entryID, $0.dependencies)
                    })
                    var candidate = dependencies
                    candidate[event.entryID, default: []].append(dependencyID)
                    guard !planDependenciesContainCycle(candidate) else {
                        return nil
                    }
                    entry = PlanEntry(
                        entryID: entry.entryID,
                        text: entry.text,
                        status: entry.status,
                        dependencies: entry.dependencies + [dependencyID],
                        readiness: entry.readiness
                    )
                    entries[event.entryID] = entry
                }
            default:
                return nil
            }
        }
        let completed = Set(entries.values.lazy.filter { $0.status == "completed" }.map(\.entryID))
        return creationOrder.compactMap { entryID in
            guard let entry = entries[entryID] else {
                return nil
            }
            let readiness = entry.dependencies.allSatisfy(completed.contains) ? "ready" : "waiting"
            return PlanEntry(
                entryID: entry.entryID,
                text: entry.text,
                status: entry.status,
                dependencies: entry.dependencies,
                readiness: readiness
            )
        }
    }

    private static func formattedPlanEvent(_ event: PlanEvent) -> String {
        let operation = "Event #\(event.ordinal): " + formattedPlanOperation(
            kind: event.kind,
            entryID: event.entryID,
            text: event.text,
            status: event.status,
            dependencyID: event.dependencyID
        )
        return [
            operation,
            "Turn: \(event.provenance.turnID.rawValue)",
            "Issuing attempt: \(event.provenance.issuingAttemptID.rawValue)",
            "Request: \(event.provenance.requestID.rawValue)",
            "Attempt: \(event.provenance.attemptID.rawValue)",
            "Generation: \(event.provenance.generation)",
        ].joined(separator: "\n")
    }

    private static func formattedPlanOperation(
        kind: String,
        entryID: UInt64?,
        text: String?,
        status: String?,
        dependencyID: UInt64?
    ) -> String {
        switch kind {
        case "create", "created":
            return "Create entry\(entryID.map { " #\($0)" } ?? ""): "
                + escapedPlanText(text ?? "Text not reported")
        case "revise", "text_revised":
            return "Revise entry #\(entryID?.description ?? "not reported"): "
                + escapedPlanText(text ?? "Text not reported")
        case "set_status", "status_changed":
            let statusLabel = status.map(planStatusLabel) ?? "Status not reported"
            return "Set entry #\(entryID?.description ?? "not reported") to \(statusLabel)"
        case "depends_on":
            return "Make entry #\(entryID?.description ?? "not reported") depend on entry "
                + "#\(dependencyID?.description ?? "not reported")"
        default:
            return "Unrecognized plan operation (\(kind))"
        }
    }

    private static func escapedPlanText(_ text: String) -> String {
        var escaped = ""
        escaped.reserveCapacity(text.utf8.count)
        for scalar in text.unicodeScalars {
            switch scalar {
            case "\\":
                escaped += "\\\\"
            case "\r":
                escaped += "\\r"
            case "\n":
                escaped += "\\n"
            case _ where CharacterSet.newlines.contains(scalar):
                let value = String(scalar.value, radix: 16, uppercase: true)
                escaped += "\\u{\(value)}"
            default:
                escaped.unicodeScalars.append(scalar)
            }
        }
        return escaped
    }

    private static func planStatusLabel(_ status: String) -> String {
        switch status {
        case "pending": return "Pending"
        case "in_progress": return "In progress"
        case "completed": return "Completed"
        case "abandoned": return "Abandoned"
        default: return "Unrecognized status (\(status))"
        }
    }

    private static func planReadinessLabel(_ readiness: String) -> String {
        switch readiness {
        case "ready": return "Ready"
        case "waiting": return "Waiting"
        default: return "Unrecognized readiness (\(readiness))"
        }
    }

    private static func decode<Value: Decodable>(
        _ type: Value.Type,
        from json: String
    ) -> Value? {
        try? SignalboxJSONCoding.decoder().decode(type, from: Data(json.utf8))
    }

    private static func yesNo(_ value: Bool) -> String {
        value ? "Yes" : "No"
    }

    private static func importedContentCard(
        record: SignalboxStoredEvent,
        event: SignalboxProcessImportedContentEvent
    ) -> SignalboxProcessNoticeCard {
        let presentation: (title: String, systemImage: String)
        switch event.contentKind {
        case .sourceEvent:
            presentation = ("Imported source event", "doc.badge.gearshape")
        case .sourceMessageBlock:
            presentation = ("Imported message block", "text.bubble")
        case .text:
            presentation = ("Imported text unavailable", "text.quote")
        case .toolCall:
            presentation = ("Imported tool call", "wrench.and.screwdriver")
        case .toolResult:
            presentation = ("Imported tool result", "checkmark.rectangle")
        case .thinking:
            presentation = ("Imported thinking", "brain")
        case .redactedThinking:
            presentation = ("Imported redacted thinking", "eye.slash")
        case .document:
            presentation = ("Imported document", "doc")
        case .messageContentAbsent:
            presentation = ("Imported message content absent", "rectangle.slash")
        }
        return SignalboxProcessNoticeCard(
            eventID: record.eventID,
            title: presentation.title,
            systemImage: presentation.systemImage,
            details: [
                .init(label: "Source speaker", value: event.sourceSpeaker)
            ]
        )
    }

    private static func normalizedMessage(
        record: SignalboxStoredEvent,
        event: SignalboxMessageEvent,
        functionCallLinkage: SignalboxTimelineLinkage,
        functionResponseLinkage: SignalboxTimelineLinkage
    ) -> SignalboxTimelineItem? {
        guard event.visibleToUser else {
            return nil
        }
        if functionResponseLinkage == .linked {
            return nil
        }
        let textParts = event.message.parts.compactMap { part -> String? in
            if case .text(let content) = part {
                return content.text
            }
            return nil
        }
        let thinkingText = event.message.parts.compactMap { part -> String? in
            if case .thinking(let content) = part {
                return content.text
            }
            return nil
        }
        .joined(separator: "\n")
        let text = textParts.joined(separator: "\n")
        if text.isEmpty && thinkingText.isEmpty && functionCallLinkage == .linked {
            return nil
        }
        if event.message.role == .tool {
            return nil
        }
        return .message(
            SignalboxTimelineMessage(
                eventID: record.eventID,
                role: event.message.role,
                text: text,
                unrecognizedKind: nil,
                thinkingText: thinkingText.isEmpty ? nil : thinkingText,
                isStreaming: event.isStreaming,
                createdAt: event.createdAt,
                label: nil,
                systemImage: nil
            )
        )
    }

    private static func tokenLabel(_ value: SignalboxCanonicalUInt64?) -> String {
        value?.rawValue.description ?? "Not reported"
    }

    private static func modelCallUsageDetails(
        _ event: SignalboxProcessModelCallUsageEvent
    ) -> [SignalboxProcessNoticeDetail] {
        var details = [
            SignalboxProcessNoticeDetail(label: "Turn", value: event.turnID.rawValue),
            .init(label: "Model call", value: event.modelCallID.rawValue),
            .init(label: "Usage provenance", value: event.usageProvenance),
            .init(label: "Input tokens", value: tokenLabel(event.inputTokens)),
            .init(label: "Output tokens", value: tokenLabel(event.outputTokens)),
            .init(
                label: "Cache creation input tokens",
                value: tokenLabel(event.cacheCreationInputTokens)
            ),
            .init(
                label: "Cache read input tokens",
                value: tokenLabel(event.cacheReadInputTokens)
            ),
        ]
        if let amount = event.costAmountUSD,
           let rateVersion = event.costRateVersion,
           let label = event.costLabel {
            details.append(.init(label: "Cost (USD)", value: amount))
            details.append(.init(label: "Cost rate version", value: rateVersion))
            details.append(.init(label: "Cost label", value: label))
        }
        return details
    }

    private static func toolCard(
        record: SignalboxStoredEvent,
        event: SignalboxToolInvocationEvent,
        recordsByID: [SignalboxEventID: SignalboxConversationEvent]
    ) -> SignalboxToolCard {
        let functionCall = messageEvent(recordsByID[event.functionCallEventID])
        let functionResponse = event.functionResponseEventID.flatMap { messageEvent(recordsByID[$0]) }
        let functionCalls = functionCall?.message.parts.compactMap { part -> SignalboxFunctionCallContent? in
            if case .functionCall(let content) = part {
                return content
            }
            return nil
        } ?? []
        let functionResponses = functionResponse?.message.parts.compactMap { part -> SignalboxFunctionResponseContent? in
            if case .functionResponse(let content) = part {
                return content
            }
            return nil
        } ?? []
        let arguments = matchingContent(functionCalls, toolCallID: event.toolCallID)?.arguments
        let output = matchingContent(functionResponses, toolCallID: event.toolCallID)?.output

        return SignalboxToolCard(
            eventID: record.eventID,
            invocationID: event.invocationID,
            toolName: event.toolName,
            status: toolStatus(event: event, output: output),
            arguments: arguments,
            output: output,
            statusUpdates: event.statusUpdates,
            decisionReason: event.decisionReason,
            childSessionID: event.childSessionID
        )
    }

    private static func matchingContent<Content>(
        _ content: [Content],
        toolCallID: SignalboxToolCallID?,
        callID: (Content) -> SignalboxToolCallID
    ) -> Content? {
        if let toolCallID {
            return content.first { callID($0) == toolCallID }
        }
        guard content.count == 1 else {
            return nil
        }
        return content[0]
    }

    private static func matchingContent(
        _ content: [SignalboxFunctionCallContent],
        toolCallID: SignalboxToolCallID?
    ) -> SignalboxFunctionCallContent? {
        matchingContent(content, toolCallID: toolCallID, callID: \.callID)
    }

    private static func matchingContent(
        _ content: [SignalboxFunctionResponseContent],
        toolCallID: SignalboxToolCallID?
    ) -> SignalboxFunctionResponseContent? {
        matchingContent(content, toolCallID: toolCallID, callID: \.callID)
    }

    private static func messageEvent(_ event: SignalboxConversationEvent?) -> SignalboxMessageEvent? {
        guard case .message(let messageEvent) = event else {
            return nil
        }
        return messageEvent
    }

    private static func toolStatus(event: SignalboxToolInvocationEvent, output: String?) -> SignalboxToolCardStatus {
        if event.pendingConfirmation {
            return .waitingForApproval
        }
        if event.decision == .denied {
            return .denied
        }
        if event.result == .failed {
            return .failed
        }
        if event.result == .succeeded {
            return .succeeded
        }
        if output != nil {
            return .completed
        }
        if event.decision == .approved {
            return .approved
        }
        return .running
    }
}

/// Stable reference-backed timeline storage for SwiftUI collection consumers.
///
/// The collection keeps the normalized array single-owned while a view retains
/// the collection across renders, so appending does not trigger an array
/// copy-on-write clone of the preceding timeline.
public final class SignalboxTimelineCollection: RandomAccessCollection {
    public typealias Index = Int
    public typealias Element = SignalboxTimelineItem

    fileprivate var items: [SignalboxTimelineItem] = []

    public var startIndex: Int {
        items.startIndex
    }

    public var endIndex: Int {
        items.endIndex
    }

    public subscript(position: Int) -> SignalboxTimelineItem {
        items[position]
    }
}

public enum SignalboxEventNormalizerError: Error, Equatable, Sendable, LocalizedError {
    /// A whole-history snapshot named the same event more than once.
    ///
    /// Event IDs identify a record, so a single snapshot cannot legitimately
    /// carry one twice; the client refuses such a snapshot rather than pick a
    /// winner it cannot justify.
    case duplicateEventIDs([SignalboxEventID])

    public var errorDescription: String? {
        switch self {
        case .duplicateEventIDs(let eventIDs):
            let list = eventIDs.map { "\($0.rawValue)" }.joined(separator: ", ")
            return "The session history repeated event \(list) and could not be loaded."
        }
    }
}

/// Maintains a normalized timeline across incremental event mutations.
///
/// The stored records, the event-ID index, and the timeline are three views of
/// one history, and every mutation keeps them in agreement:
///
/// - A mutation for an event ID the normalizer already holds is an *update*:
///   the later record replaces the stored one and its timeline item is
///   renormalized in place. Stream replay depends on this — a frame buffered
///   behind a history resynchronization may restate an event the authoritative
///   snapshot already delivered, and the later frame is the correction.
/// - A whole-history snapshot that names the same event ID twice is corrupt
///   input, not a correction: `replaceAll(with:)` rejects it and leaves the
///   previously loaded history untouched, so the caller can fail the refresh
///   and recover instead of rendering a history no structure agrees on.
public final class SignalboxIncrementalEventNormalizer {
    public private(set) var records: [SignalboxStoredEvent] = []
    public let timeline = SignalboxTimelineCollection()
    private(set) var metrics = SignalboxEventNormalizationMetrics()

    private var recordsByID: [SignalboxEventID: SignalboxConversationEvent] = [:]
    private var invocationEventIDsByFunctionCallEventID: [SignalboxEventID: Set<SignalboxEventID>] = [:]
    private var invocationEventIDsByFunctionResponseEventID: [SignalboxEventID: Set<SignalboxEventID>] = [:]

    public var timelineItems: [SignalboxTimelineItem] {
        Array(timeline)
    }

    public init() {}

    public init(records: [SignalboxStoredEvent]) throws {
        try replaceAll(with: records)
    }

    /// Replaces the whole history with an authoritative snapshot.
    ///
    /// - Throws: ``SignalboxEventNormalizerError/duplicateEventIDs(_:)`` when
    ///   the snapshot names an event ID more than once. Nothing is mutated in
    ///   that case, so the previously loaded history stays renderable while the
    ///   caller fails the refresh.
    public func replaceAll(with records: [SignalboxStoredEvent]) throws {
        let sortedRecords = records.sorted { $0.eventID < $1.eventID }
        // Index before storing anything: a snapshot cannot be applied halfway,
        // and only the index can tell a duplicate ID from a fresh one.
        let recordsByID = try Self.eventsByID(in: sortedRecords)

        self.records = sortedRecords
        self.recordsByID = recordsByID
        timeline.items = []
        metrics = SignalboxEventNormalizationMetrics()
        invocationEventIDsByFunctionCallEventID = [:]
        invocationEventIDsByFunctionResponseEventID = [:]

        for record in self.records {
            addInvocationLinks(for: record.event, invocationEventID: record.eventID)
        }
        for record in self.records {
            reevaluate(record.eventID)
        }
    }

    /// Indexes a snapshot sorted by event ID, rejecting any repeated ID.
    private static func eventsByID(
        in sortedRecords: [SignalboxStoredEvent]
    ) throws -> [SignalboxEventID: SignalboxConversationEvent] {
        var eventsByID: [SignalboxEventID: SignalboxConversationEvent] = [:]
        eventsByID.reserveCapacity(sortedRecords.count)
        var duplicateEventIDs: [SignalboxEventID] = []
        for record in sortedRecords {
            guard eventsByID.updateValue(record.event, forKey: record.eventID) != nil else {
                continue
            }
            // The sort groups repeats, so reporting each ID once needs no set.
            if duplicateEventIDs.last != record.eventID {
                duplicateEventIDs.append(record.eventID)
            }
        }
        guard duplicateEventIDs.isEmpty else {
            throw SignalboxEventNormalizerError.duplicateEventIDs(duplicateEventIDs)
        }
        return eventsByID
    }

    /// Stores `record`, replacing any record already held under its event ID.
    ///
    /// A repeated event ID is a correction, not a second event: the stored
    /// record, the event-ID index, and the timeline item are all updated in
    /// place, so no structure can retain a stale copy.
    public func upsert(_ record: SignalboxStoredEvent) {
        let eventID = record.eventID
        let oldEvent = recordsByID[eventID]
        var affectedEventIDs: Set<SignalboxEventID> = [eventID]

        addLinkedEventIDs(from: oldEvent, to: &affectedEventIDs)
        addLinkedInvocationEventIDs(for: eventID, to: &affectedEventIDs)
        removeInvocationLinks(for: oldEvent, invocationEventID: eventID)

        let index = recordInsertionIndex(for: eventID)
        if index < records.count, records[index].eventID == eventID {
            records[index] = record
        } else {
            records.insert(record, at: index)
        }
        recordsByID[eventID] = record.event
        addInvocationLinks(for: record.event, invocationEventID: eventID)
        addLinkedEventIDs(from: record.event, to: &affectedEventIDs)
        addLinkedInvocationEventIDs(for: eventID, to: &affectedEventIDs)

        for affectedEventID in affectedEventIDs.sorted() {
            reevaluate(affectedEventID)
        }
    }

    public func upsert(contentsOf records: [SignalboxStoredEvent]) {
        for record in records {
            upsert(record)
        }
    }

    public func remove(eventID: SignalboxEventID) {
        guard let removedEvent = recordsByID.removeValue(forKey: eventID) else {
            return
        }
        var affectedEventIDs: Set<SignalboxEventID> = []
        addLinkedEventIDs(from: removedEvent, to: &affectedEventIDs)
        addLinkedInvocationEventIDs(for: eventID, to: &affectedEventIDs)
        removeInvocationLinks(for: removedEvent, invocationEventID: eventID)

        let index = recordInsertionIndex(for: eventID)
        if index < records.count, records[index].eventID == eventID {
            records.remove(at: index)
        }
        setTimelineItem(nil, for: eventID)
        for affectedEventID in affectedEventIDs.sorted() {
            reevaluate(affectedEventID)
        }
    }

    private func reevaluate(_ eventID: SignalboxEventID) {
        guard let event = recordsByID[eventID] else {
            setTimelineItem(nil, for: eventID)
            return
        }
        metrics.recordEvaluationCount += 1
        let record = SignalboxStoredEvent(eventID: eventID, event: event)
        let item = SignalboxEventNormalizer.normalize(
            record,
            recordsByID: recordsByID,
            functionCallLinkage: invocationEventIDsByFunctionCallEventID[eventID]?.isEmpty == false
                ? .linked
                : .unlinked,
            functionResponseLinkage: invocationEventIDsByFunctionResponseEventID[eventID]?.isEmpty == false
                ? .linked
                : .unlinked
        )
        setTimelineItem(item, for: eventID)
    }

    private func addInvocationLinks(
        for event: SignalboxConversationEvent,
        invocationEventID: SignalboxEventID
    ) {
        guard case .toolInvocation(let invocation) = event else {
            return
        }
        invocationEventIDsByFunctionCallEventID[invocation.functionCallEventID, default: []]
            .insert(invocationEventID)
        if let functionResponseEventID = invocation.functionResponseEventID {
            invocationEventIDsByFunctionResponseEventID[functionResponseEventID, default: []]
                .insert(invocationEventID)
        }
    }

    private func removeInvocationLinks(
        for event: SignalboxConversationEvent?,
        invocationEventID: SignalboxEventID
    ) {
        guard case .toolInvocation(let invocation)? = event else {
            return
        }
        Self.remove(
            invocationEventID,
            from: &invocationEventIDsByFunctionCallEventID,
            linkedEventID: invocation.functionCallEventID
        )
        if let functionResponseEventID = invocation.functionResponseEventID {
            Self.remove(
                invocationEventID,
                from: &invocationEventIDsByFunctionResponseEventID,
                linkedEventID: functionResponseEventID
            )
        }
    }

    private func addLinkedEventIDs(
        from event: SignalboxConversationEvent?,
        to affectedEventIDs: inout Set<SignalboxEventID>
    ) {
        guard case .toolInvocation(let invocation)? = event else {
            return
        }
        affectedEventIDs.insert(invocation.functionCallEventID)
        if let functionResponseEventID = invocation.functionResponseEventID {
            affectedEventIDs.insert(functionResponseEventID)
        }
    }

    private func addLinkedInvocationEventIDs(
        for eventID: SignalboxEventID,
        to affectedEventIDs: inout Set<SignalboxEventID>
    ) {
        affectedEventIDs.formUnion(invocationEventIDsByFunctionCallEventID[eventID] ?? [])
        affectedEventIDs.formUnion(invocationEventIDsByFunctionResponseEventID[eventID] ?? [])
    }

    private static func remove(
        _ invocationEventID: SignalboxEventID,
        from links: inout [SignalboxEventID: Set<SignalboxEventID>],
        linkedEventID: SignalboxEventID
    ) {
        links[linkedEventID]?.remove(invocationEventID)
        if links[linkedEventID]?.isEmpty == true {
            links.removeValue(forKey: linkedEventID)
        }
    }

    private func recordInsertionIndex(for eventID: SignalboxEventID) -> Int {
        if let lastRecord = records.last, lastRecord.eventID < eventID {
            return records.count
        }
        return insertionIndex(count: records.count) { index in
            records[index].eventID < eventID
        }
    }

    private func timelineInsertionIndex(for eventID: SignalboxEventID) -> Int {
        if let lastItem = timeline.last, timelineEventID(lastItem) < eventID {
            return timeline.count
        }
        return insertionIndex(count: timeline.count) { index in
            timelineEventID(timeline[index]) < eventID
        }
    }

    private func insertionIndex(
        count: Int,
        isOrderedBeforeTarget: (Int) -> Bool
    ) -> Int {
        var lowerBound = 0
        var upperBound = count
        while lowerBound < upperBound {
            let middle = lowerBound + (upperBound - lowerBound) / 2
            if isOrderedBeforeTarget(middle) {
                lowerBound = middle + 1
            } else {
                upperBound = middle
            }
        }
        return lowerBound
    }

    private func setTimelineItem(
        _ item: SignalboxTimelineItem?,
        for eventID: SignalboxEventID
    ) {
        let index = timelineInsertionIndex(for: eventID)
        let itemExists = index < timeline.count && timelineEventID(timeline[index]) == eventID
        if let item {
            if itemExists {
                timeline.items[index] = item
            } else {
                timeline.items.insert(item, at: index)
            }
        } else if itemExists {
            timeline.items.remove(at: index)
        }
    }

    private func timelineEventID(_ item: SignalboxTimelineItem) -> SignalboxEventID {
        switch item {
        case .message(let message):
            return message.eventID
        case .tool(let tool):
            return tool.eventID
        case .processEvidence(let notice):
            return notice.eventID
        case .turnFailure(let failure):
            return failure.eventID
        case .unknown(let unknown):
            return unknown.eventID
        }
    }
}
