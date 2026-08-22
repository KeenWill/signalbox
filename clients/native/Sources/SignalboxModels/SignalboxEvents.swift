import Foundation

public struct SignalboxStoredEvent: Codable, Identifiable, Equatable, Sendable {
    public let eventID: SignalboxEventID
    /// Optional display ordering independent of the stable event identity. It
    /// is projection-local and intentionally absent from the stored encoding.
    public var presentationOrder: SignalboxEventID? = nil
    public var event: SignalboxConversationEvent
    public var id: SignalboxEventID { eventID }

    public init(
        eventID: SignalboxEventID,
        presentationOrder: SignalboxEventID? = nil,
        event: SignalboxConversationEvent
    ) {
        self.eventID = eventID
        self.presentationOrder = presentationOrder
        self.event = event
    }

    private enum CodingKeys: String, CodingKey {
        case eventID = "event_id"
        case event
    }
}

/// Known event kinds fail soft into a payload-preserving unknown value when
/// their shape evolves. Timeline consumers can then retain ordering and a
/// decoding diagnostic without inventing semantics for the new shape.
public enum SignalboxConversationEvent: Codable, Equatable, Sendable {
    case message(SignalboxMessageEvent)
    case toolInvocation(SignalboxToolInvocationEvent)
    case turnFailed(SignalboxTurnFailedEvent)
    case processMessage(SignalboxProcessMessageEvent)
    case processContextSummary(SignalboxProcessContextSummaryEvent)
    case processModelIdentity(SignalboxProcessModelIdentityEvent)
    case processRunnerPlacement(SignalboxProcessRunnerPlacementEvent)
    case processModelCallUsage(SignalboxProcessModelCallUsageEvent)
    case processImportedContent(SignalboxProcessImportedContentEvent)
    case processTool(SignalboxProcessToolEvent)
    case processTurnFailure(SignalboxProcessTurnFailureEvent)
    case processConservative(SignalboxProcessConservativeEvent)
    case unknown(SignalboxUnknownEvent)

    public var kind: String {
        switch self {
        case .message:
            return "message"
        case .toolInvocation:
            return "tool_invocation"
        case .turnFailed:
            return "turn_failed"
        case .processMessage:
            return "process_message"
        case .processContextSummary:
            return "process_context_summary"
        case .processModelIdentity:
            return "process_model_identity"
        case .processRunnerPlacement:
            return "process_runner_placement"
        case .processModelCallUsage:
            return "process_model_call_usage"
        case .processImportedContent:
            return "process_imported_content"
        case .processTool:
            return "process_tool"
        case .processTurnFailure:
            return "process_turn_failure"
        case .processConservative:
            return "process_conservative"
        case .unknown(let event):
            return event.kind
        }
    }

    private enum CodingKeys: String, CodingKey {
        case kind
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let kind = try container.decode(String.self, forKey: .kind)
        switch kind {
        case "message":
            do {
                self = .message(try SignalboxMessageEvent(from: decoder))
            } catch {
                self = .unknown(
                    try SignalboxUnknownEvent(
                        kind: kind,
                        decoder: decoder,
                        decodingDiagnostic: SignalboxDecodingDiagnostic(error: error)
                    )
                )
            }
        case "tool_invocation":
            do {
                self = .toolInvocation(try SignalboxToolInvocationEvent(from: decoder))
            } catch {
                self = .unknown(
                    try SignalboxUnknownEvent(
                        kind: kind,
                        decoder: decoder,
                        decodingDiagnostic: SignalboxDecodingDiagnostic(error: error)
                    )
                )
            }
        case "turn_failed":
            do {
                self = .turnFailed(try SignalboxTurnFailedEvent(from: decoder))
            } catch {
                self = .unknown(
                    try SignalboxUnknownEvent(
                        kind: kind,
                        decoder: decoder,
                        decodingDiagnostic: SignalboxDecodingDiagnostic(error: error)
                    )
                )
            }
        case "process_message":
            do {
                self = .processMessage(try SignalboxProcessMessageEvent(from: decoder))
            } catch {
                self = .unknown(
                    try SignalboxUnknownEvent(
                        kind: kind,
                        decoder: decoder,
                        decodingDiagnostic: SignalboxDecodingDiagnostic(error: error)
                    )
                )
            }
        case "process_context_summary":
            do {
                self = .processContextSummary(try SignalboxProcessContextSummaryEvent(closedFrom: decoder))
            } catch {
                self = .unknown(
                    try SignalboxUnknownEvent(
                        kind: kind,
                        decoder: decoder,
                        decodingDiagnostic: SignalboxDecodingDiagnostic(error: error)
                    )
                )
            }
        case "process_model_identity":
            do {
                self = .processModelIdentity(try SignalboxProcessModelIdentityEvent(closedFrom: decoder))
            } catch {
                self = .unknown(
                    try SignalboxUnknownEvent(
                        kind: kind,
                        decoder: decoder,
                        decodingDiagnostic: SignalboxDecodingDiagnostic(error: error)
                    )
                )
            }
        case "process_runner_placement":
            do {
                self = .processRunnerPlacement(
                    try SignalboxProcessRunnerPlacementEvent(closedFrom: decoder)
                )
            } catch {
                self = .unknown(
                    try SignalboxUnknownEvent(
                        kind: kind,
                        decoder: decoder,
                        decodingDiagnostic: SignalboxDecodingDiagnostic(error: error)
                    )
                )
            }
        case "process_model_call_usage":
            do {
                self = .processModelCallUsage(try SignalboxProcessModelCallUsageEvent(closedFrom: decoder))
            } catch {
                self = .unknown(
                    try SignalboxUnknownEvent(
                        kind: kind,
                        decoder: decoder,
                        decodingDiagnostic: SignalboxDecodingDiagnostic(error: error)
                    )
                )
            }
        case "process_imported_content":
            do {
                self = .processImportedContent(
                    try SignalboxProcessImportedContentEvent(closedFrom: decoder)
                )
            } catch {
                self = .unknown(
                    try SignalboxUnknownEvent(
                        kind: kind,
                        decoder: decoder,
                        decodingDiagnostic: SignalboxDecodingDiagnostic(error: error)
                    )
                )
            }
        case "process_tool":
            do {
                self = .processTool(try SignalboxProcessToolEvent(closedFrom: decoder))
            } catch {
                self = .unknown(
                    try SignalboxUnknownEvent(
                        kind: kind,
                        decoder: decoder,
                        decodingDiagnostic: SignalboxDecodingDiagnostic(error: error)
                    )
                )
            }
        case "process_turn_failure":
            do {
                self = .processTurnFailure(try SignalboxProcessTurnFailureEvent(from: decoder))
            } catch {
                self = .unknown(
                    try SignalboxUnknownEvent(
                        kind: kind,
                        decoder: decoder,
                        decodingDiagnostic: SignalboxDecodingDiagnostic(error: error)
                    )
                )
            }
        case "process_conservative":
            do {
                self = .processConservative(try SignalboxProcessConservativeEvent(from: decoder))
            } catch {
                self = .unknown(
                    try SignalboxUnknownEvent(
                        kind: kind,
                        decoder: decoder,
                        decodingDiagnostic: SignalboxDecodingDiagnostic(error: error)
                    )
                )
            }
        default:
            self = .unknown(try SignalboxUnknownEvent(kind: kind, decoder: decoder))
        }
    }

    public func encode(to encoder: Encoder) throws {
        switch self {
        case .message(let event):
            try event.encode(to: encoder)
        case .toolInvocation(let event):
            try event.encode(to: encoder)
        case .turnFailed(let event):
            try event.encode(to: encoder)
        case .processMessage(let event):
            try event.encode(to: encoder)
        case .processContextSummary(let event):
            try event.encode(to: encoder)
        case .processModelIdentity(let event):
            try event.encode(to: encoder)
        case .processRunnerPlacement(let event):
            try event.encode(to: encoder)
        case .processModelCallUsage(let event):
            try event.encode(to: encoder)
        case .processImportedContent(let event):
            try event.encode(to: encoder)
        case .processTool(let event):
            try event.encode(to: encoder)
        case .processTurnFailure(let event):
            try event.encode(to: encoder)
        case .processConservative(let event):
            try event.encode(to: encoder)
        case .unknown(let event):
            try event.encode(to: encoder)
        }
    }
}

public struct SignalboxUnknownEvent: Codable, Equatable, Sendable {
    public let kind: String
    public let payload: [String: SignalboxJSONValue]
    public let decodingDiagnostic: SignalboxDecodingDiagnostic?

    public init(
        kind: String,
        payload: [String: SignalboxJSONValue],
        decodingDiagnostic: SignalboxDecodingDiagnostic? = nil
    ) {
        self.kind = kind
        self.payload = payload
        self.decodingDiagnostic = decodingDiagnostic
    }

    public init(
        kind: String,
        decoder: Decoder,
        decodingDiagnostic: SignalboxDecodingDiagnostic? = nil
    ) throws {
        let container = try decoder.singleValueContainer()
        self.kind = kind
        self.payload = try container.decode([String: SignalboxJSONValue].self)
        self.decodingDiagnostic = decodingDiagnostic
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        let payload = try container.decode([String: SignalboxJSONValue].self)
        guard case .string(let kind) = payload["kind"] else {
            throw DecodingError.keyNotFound(
                DynamicEventCodingKey("kind"),
                .init(codingPath: decoder.codingPath, debugDescription: "Unknown event is missing its kind.")
            )
        }
        self.kind = kind
        self.payload = payload
        self.decodingDiagnostic = nil
    }

    public func encode(to encoder: Encoder) throws {
        var payload = self.payload
        payload["kind"] = .string(kind)
        try payload.encode(to: encoder)
    }
}

private struct DynamicEventCodingKey: CodingKey {
    let stringValue: String
    let intValue: Int? = nil

    init(_ stringValue: String) {
        self.stringValue = stringValue
    }

    init?(stringValue: String) {
        self.stringValue = stringValue
    }

    init?(intValue: Int) {
        return nil
    }
}

public struct SignalboxMessageEvent: Codable, Equatable, Sendable {
    public let kind: String
    public let message: SignalboxMessage
    public let visibleToLLM: Bool
    public let visibleToUser: Bool
    public let isStreaming: Bool
    public let parentToolInvocation: SignalboxToolInvocationID?
    public let createdAt: Date
    public let lastModifiedAt: Date
    public let createdFrom: String

    private enum CodingKeys: String, CodingKey {
        case kind
        case message
        case visibleToLLM = "visible_to_llm"
        case visibleToUser = "visible_to_user"
        case isStreaming = "is_streaming"
        case parentToolInvocation = "parent_tool_invocation"
        case createdAt = "created_at"
        case lastModifiedAt = "last_modified_at"
        case createdFrom = "created_from"
    }
}

public struct SignalboxMessage: Codable, Equatable, Sendable {
    public let role: SignalboxMessageRole
    public let parts: [SignalboxMessagePart]

    public var visibleText: String {
        parts.compactMap { part in
            switch part {
            case .text(let content):
                return content.text
            case .thinking(let content):
                return content.text
            case .functionCall, .functionResponse, .unknown:
                return nil
            }
        }
        .joined(separator: "\n")
    }
}

public struct SignalboxFunctionCallContent: Codable, Equatable, Sendable {
    public let kind: String
    public let name: String
    public let arguments: String
    public let callID: SignalboxToolCallID

    private enum CodingKeys: String, CodingKey {
        case kind
        case name
        case arguments
        case callID = "call_id"
    }
}

public struct SignalboxFunctionResponseContent: Codable, Equatable, Sendable {
    public let kind: String
    public let callID: SignalboxToolCallID
    public let output: String

    private enum CodingKeys: String, CodingKey {
        case kind
        case callID = "call_id"
        case output
    }
}

/// Correlated tool invocation state projected from conversation events.
public struct SignalboxToolInvocationEvent: Codable, Equatable, Sendable {
    public let kind: String
    public let invocationID: SignalboxToolInvocationID
    public let toolName: String
    public let toolCallID: SignalboxToolCallID?
    public let functionCallEventID: SignalboxEventID
    public let functionResponseEventID: SignalboxEventID?
    public let result: SignalboxToolResult?
    public let statusUpdates: [String]
    public let pendingConfirmation: Bool
    public let decision: SignalboxToolDecision?
    public let decisionAt: Date?
    public let decisionReason: String?
    public let isCollapsedByOwner: Bool
    public let childSessionID: SignalboxSessionID?
    public let lastModifiedAt: Date

    private enum CodingKeys: String, CodingKey {
        case kind
        case invocationID = "invocation_id"
        case toolName = "tool_name"
        case toolCallID = "tool_call_id"
        case functionCallEventID = "function_call_event_id"
        case functionResponseEventID = "function_response_event_id"
        case result
        case statusUpdates = "status_updates"
        case pendingConfirmation = "pending_confirmation"
        case decision
        case decisionAt = "decision_at"
        case decisionReason = "decision_reason"
        case isCollapsedByOwner = "is_collapsed_by_owner"
        case childSessionID = "child_session_id"
        case lastModifiedAt = "last_modified_at"
    }
}

public enum SignalboxMessageRole: String, Codable, Equatable, Sendable {
    case system
    case user
    case assistant
    case tool
    case unknown

    public init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        self = SignalboxMessageRole(rawValue: try container.decode(String.self)) ?? .unknown
    }
}

public enum SignalboxMessagePart: Codable, Equatable, Sendable {
    case text(SignalboxTextContent)
    case thinking(SignalboxThinkingContent)
    case functionCall(SignalboxFunctionCallContent)
    case functionResponse(SignalboxFunctionResponseContent)
    case unknown(kind: String, payload: [String: SignalboxJSONValue])

    private enum CodingKeys: String, CodingKey {
        case kind
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let kind = try container.decode(String.self, forKey: .kind)
        switch kind {
        case "text":
            self = .text(try SignalboxTextContent(from: decoder))
        case "thinking":
            self = .thinking(try SignalboxThinkingContent(from: decoder))
        case "function_call":
            self = .functionCall(try SignalboxFunctionCallContent(from: decoder))
        case "function_response":
            self = .functionResponse(try SignalboxFunctionResponseContent(from: decoder))
        default:
            let payload = try decoder.singleValueContainer().decode([String: SignalboxJSONValue].self)
            self = .unknown(kind: kind, payload: payload)
        }
    }

    public func encode(to encoder: Encoder) throws {
        switch self {
        case .text(let content):
            try content.encode(to: encoder)
        case .thinking(let content):
            try content.encode(to: encoder)
        case .functionCall(let content):
            try content.encode(to: encoder)
        case .functionResponse(let content):
            try content.encode(to: encoder)
        case .unknown(let kind, let payload):
            var payload = payload
            payload["kind"] = .string(kind)
            try payload.encode(to: encoder)
        }
    }
}

public struct SignalboxTextContent: Codable, Equatable, Sendable {
    public let kind: String
    public let text: String
}

public struct SignalboxThinkingContent: Codable, Equatable, Sendable {
    public let kind: String
    public let text: String
    public let signature: String?
}

public enum SignalboxToolResult: Codable, Equatable, Sendable {
    case succeeded
    case failed
    case unknown(String)

    public init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        let rawValue = try container.decode(String.self)
        switch rawValue {
        case "succeeded":
            self = .succeeded
        case "failed":
            self = .failed
        default:
            self = .unknown(rawValue)
        }
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        switch self {
        case .succeeded:
            try container.encode("succeeded")
        case .failed:
            try container.encode("failed")
        case .unknown(let rawValue):
            try container.encode(rawValue)
        }
    }
}

public enum SignalboxToolDecision: String, Codable, Equatable, Sendable {
    case approved
    case denied
}

public struct SignalboxTurnFailedEvent: Codable, Equatable, Sendable {
    public let kind: String
    public let turnID: String
    public let reason: String
    public let failedAt: Date
    public let runnerID: SignalboxRunnerID?
    public let visibleToLLM: Bool
    public let visibleToUser: Bool
    public let createdAt: Date
    public let lastModifiedAt: Date
    public let createdFrom: String

    private enum CodingKeys: String, CodingKey {
        case kind
        case turnID = "turn_id"
        case reason
        case failedAt = "failed_at"
        case runnerID = "runner_id"
        case visibleToLLM = "visible_to_llm"
        case visibleToUser = "visible_to_user"
        case createdAt = "created_at"
        case lastModifiedAt = "last_modified_at"
        case createdFrom = "created_from"
    }
}
