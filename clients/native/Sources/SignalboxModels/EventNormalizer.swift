import Foundation

public enum SignalboxTimelineItem: Identifiable, Equatable, Sendable {
    case message(SignalboxTimelineMessage)
    case tool(SignalboxToolCard)
    case turnFailure(SignalboxTurnFailureCard)
    case unknown(SignalboxUnknownEventCard)

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

public struct SignalboxTimelineMessage: Equatable, Sendable {
    public let eventID: SignalboxEventID
    public let role: SignalboxMessageRole
    public let text: String
    public let thinkingText: String?
    public let isStreaming: Bool
    public let createdAt: Date
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

    public var id: SignalboxToolInvocationID { invocationID }

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

public enum SignalboxToolCardStatus: Equatable, Sendable {
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

public struct SignalboxTurnFailureCard: Equatable, Sendable {
    public let eventID: SignalboxEventID
    public let reason: String
    public let runnerID: SignalboxRunnerID?
    public let failedAt: Date
}

public struct SignalboxUnknownEventCard: Equatable, Sendable {
    public let eventID: SignalboxEventID
    public let kind: String
    public let diagnostic: String
}

public enum SignalboxEventNormalizer {
    public static func normalize(_ records: [SignalboxStoredEvent]) -> [SignalboxTimelineItem] {
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
                    SignalboxTurnFailureCard(
                        eventID: record.eventID,
                        reason: event.reason,
                        runnerID: event.runnerID,
                        failedAt: event.failedAt
                    )
                )
            case .unknown(let event):
                return .unknown(
                    SignalboxUnknownEventCard(
                        eventID: record.eventID,
                        kind: event.kind,
                        diagnostic: event.payload.keys.sorted().joined(separator: ", ")
                    )
                )
            }
        }
    }

    private static func normalizedMessage(
        record: SignalboxStoredEvent,
        event: SignalboxMessageEvent,
        linkedFunctionCallEventIDs: Set<SignalboxEventID>,
        linkedFunctionResponseEventIDs: Set<SignalboxEventID>
    ) -> SignalboxTimelineItem? {
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
            SignalboxTimelineMessage(
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
