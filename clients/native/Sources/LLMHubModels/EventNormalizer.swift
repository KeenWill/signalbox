import Foundation

public enum HubTimelineItem: Identifiable, Equatable, Sendable {
    case message(HubTimelineMessage)
    case tool(HubToolCard)
    case turnFailure(HubTurnFailureCard)
    case unknown(HubUnknownEventCard)

    public var id: String {
        switch self {
        case .message(let message):
            return "message-\(message.eventID.rawValue)"
        case .tool(let tool):
            return "tool-\(tool.invocationID.rawValue)"
        case .turnFailure(let failure):
            return "failure-\(failure.eventID.rawValue)"
        case .unknown(let unknown):
            return "unknown-\(unknown.eventID.rawValue)"
        }
    }
}

public struct HubTimelineMessage: Equatable, Sendable {
    public let eventID: HubEventID
    public let role: HubMessageRole
    public let text: String
    public let thinkingText: String?
    public let isStreaming: Bool
    public let createdAt: Date
}

public struct HubToolCard: Identifiable, Equatable, Sendable {
    public let eventID: HubEventID
    public let invocationID: HubToolInvocationID
    public let toolName: String
    public let status: HubToolCardStatus
    public let arguments: String?
    public let output: String?
    public let statusUpdates: [String]
    public let decisionReason: String?
    public let childSessionID: HubSessionID?

    public var id: HubToolInvocationID { invocationID }

    public var compactArgumentSummary: String {
        let trimmed = (arguments ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty {
            return "No arguments"
        }
        if trimmed.count <= 180 {
            return trimmed
        }
        return String(trimmed.prefix(180)) + "..."
    }

    public var outputPreview: String {
        let trimmed = (output ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty {
            return "No output yet"
        }
        if trimmed.count <= 480 {
            return trimmed
        }
        return String(trimmed.prefix(480)) + "..."
    }
}

public enum HubToolCardStatus: Equatable, Sendable {
    case waitingForApproval
    case running
    case approved
    case denied
    case succeeded
    case failed
    case completed

    public var label: String {
        switch self {
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
        }
    }
}

public struct HubTurnFailureCard: Equatable, Sendable {
    public let eventID: HubEventID
    public let reason: String
    public let runnerID: HubRunnerID?
    public let failedAt: Date
}

public struct HubUnknownEventCard: Equatable, Sendable {
    public let eventID: HubEventID
    public let kind: String
    public let diagnostic: String
}

public enum HubEventNormalizer {
    public static func normalize(_ records: [HubStoredEvent]) -> [HubTimelineItem] {
        let recordsByID = Dictionary(records.map { ($0.eventID, $0.event) }, uniquingKeysWith: { first, _ in first })
        let toolLinkEventIDs = Set(records.compactMap { record -> HubEventID? in
            guard case .toolInvocation(let invocation) = record.event else {
                return nil
            }
            return invocation.functionCallEventID
        })
        let toolResponseEventIDs = Set(records.compactMap { record -> HubEventID? in
            guard case .toolInvocation(let invocation) = record.event else {
                return nil
            }
            return invocation.functionResponseEventID
        })

        return records.compactMap { record in
            switch record.event {
            case .message(let event):
                return normalizedMessage(
                    record: record,
                    event: event,
                    linkedFunctionCallEventIDs: toolLinkEventIDs,
                    linkedFunctionResponseEventIDs: toolResponseEventIDs
                )
            case .toolInvocation(let event):
                return .tool(toolCard(record: record, event: event, recordsByID: recordsByID))
            case .turnFailed(let event):
                return .turnFailure(
                    HubTurnFailureCard(
                        eventID: record.eventID,
                        reason: event.reason,
                        runnerID: event.runnerID,
                        failedAt: event.failedAt
                    )
                )
            case .unknown(let event):
                return .unknown(
                    HubUnknownEventCard(
                        eventID: record.eventID,
                        kind: event.kind,
                        diagnostic: event.payload.keys.sorted().joined(separator: ", ")
                    )
                )
            }
        }
    }

    private static func normalizedMessage(
        record: HubStoredEvent,
        event: HubMessageEvent,
        linkedFunctionCallEventIDs: Set<HubEventID>,
        linkedFunctionResponseEventIDs: Set<HubEventID>
    ) -> HubTimelineItem? {
        guard event.visibleToUser else {
            return nil
        }
        if linkedFunctionResponseEventIDs.contains(record.eventID) {
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
        if text.isEmpty && thinkingText.isEmpty && linkedFunctionCallEventIDs.contains(record.eventID) {
            return nil
        }
        if event.message.role == .tool {
            return nil
        }
        return .message(
            HubTimelineMessage(
                eventID: record.eventID,
                role: event.message.role,
                text: text,
                thinkingText: thinkingText.isEmpty ? nil : thinkingText,
                isStreaming: event.isStreaming,
                createdAt: event.createdAt
            )
        )
    }

    private static func toolCard(
        record: HubStoredEvent,
        event: HubToolInvocationEvent,
        recordsByID: [HubEventID: HubConversationEvent]
    ) -> HubToolCard {
        let functionCall = messageEvent(recordsByID[event.functionCallEventID])
        let functionResponse = event.functionResponseEventID.flatMap { messageEvent(recordsByID[$0]) }
        let arguments = functionCall?.message.parts.compactMap { part -> String? in
            if case .functionCall(let content) = part {
                return content.arguments
            }
            return nil
        }
        .first
        let output = functionResponse?.message.parts.compactMap { part -> String? in
            if case .functionResponse(let content) = part {
                return content.output
            }
            return nil
        }
        .first

        return HubToolCard(
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

    private static func messageEvent(_ event: HubConversationEvent?) -> HubMessageEvent? {
        guard case .message(let messageEvent) = event else {
            return nil
        }
        return messageEvent
    }

    private static func toolStatus(event: HubToolInvocationEvent, output: String?) -> HubToolCardStatus {
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
