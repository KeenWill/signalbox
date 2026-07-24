import Foundation

public struct HubStoredEvent: Codable, Identifiable, Equatable, Sendable {
    public let eventID: HubEventID
    public var event: HubConversationEvent
    public var id: HubEventID { eventID }

    public init(eventID: HubEventID, event: HubConversationEvent) {
        self.eventID = eventID
        self.event = event
    }

    private enum CodingKeys: String, CodingKey {
        case eventID = "event_id"
        case event
    }
}

public enum HubConversationEvent: Codable, Equatable, Sendable {
    case message(HubMessageEvent)
    case toolInvocation(HubToolInvocationEvent)
    case turnFailed(HubTurnFailedEvent)
    case unknown(HubUnknownEvent)

    public var kind: String {
        switch self {
        case .message:
            return "message"
        case .toolInvocation:
            return "tool_invocation"
        case .turnFailed:
            return "turn_failed"
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
            self = .message(try HubMessageEvent(from: decoder))
        case "tool_invocation":
            self = .toolInvocation(try HubToolInvocationEvent(from: decoder))
        case "turn_failed":
            self = .turnFailed(try HubTurnFailedEvent(from: decoder))
        default:
            self = .unknown(try HubUnknownEvent(kind: kind, decoder: decoder))
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
        case .unknown(let event):
            try event.encode(to: encoder)
        }
    }
}

public struct HubUnknownEvent: Codable, Equatable, Sendable {
    public let kind: String
    public let payload: [String: HubJSONValue]

    public init(kind: String, payload: [String: HubJSONValue]) {
        self.kind = kind
        self.payload = payload
    }

    public init(kind: String, decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        self.kind = kind
        self.payload = try container.decode([String: HubJSONValue].self)
    }

    public func encode(to encoder: Encoder) throws {
        var payload = self.payload
        payload["kind"] = .string(kind)
        try payload.encode(to: encoder)
    }
}

public struct HubMessageEvent: Codable, Equatable, Sendable {
    public let kind: String
    public let message: HubMessage
    public let visibleToLLM: Bool
    public let visibleToUser: Bool
    public let isStreaming: Bool
    public let parentToolInvocation: HubToolInvocationID?
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

public struct HubMessage: Codable, Equatable, Sendable {
    public let role: HubMessageRole
    public let parts: [HubMessagePart]

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

public enum HubMessageRole: String, Codable, Equatable, Sendable {
    case system
    case user
    case assistant
    case tool
    case unknown

    public init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        self = HubMessageRole(rawValue: try container.decode(String.self)) ?? .unknown
    }
}

public enum HubMessagePart: Codable, Equatable, Sendable {
    case text(HubTextContent)
    case thinking(HubThinkingContent)
    case functionCall(HubFunctionCallContent)
    case functionResponse(HubFunctionResponseContent)
    case unknown(kind: String, payload: [String: HubJSONValue])

    private enum CodingKeys: String, CodingKey {
        case kind
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let kind = try container.decode(String.self, forKey: .kind)
        switch kind {
        case "text":
            self = .text(try HubTextContent(from: decoder))
        case "thinking":
            self = .thinking(try HubThinkingContent(from: decoder))
        case "function_call":
            self = .functionCall(try HubFunctionCallContent(from: decoder))
        case "function_response":
            self = .functionResponse(try HubFunctionResponseContent(from: decoder))
        default:
            let payload = try decoder.singleValueContainer().decode([String: HubJSONValue].self)
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

public struct HubTextContent: Codable, Equatable, Sendable {
    public let kind: String
    public let text: String
}

public struct HubThinkingContent: Codable, Equatable, Sendable {
    public let kind: String
    public let text: String
    public let signature: String?
}

public struct HubFunctionCallContent: Codable, Equatable, Sendable {
    public let kind: String
    public let name: String
    public let arguments: String
    public let callID: HubToolCallID

    private enum CodingKeys: String, CodingKey {
        case kind
        case name
        case arguments
        case callID = "call_id"
    }
}

public struct HubFunctionResponseContent: Codable, Equatable, Sendable {
    public let kind: String
    public let callID: HubToolCallID
    public let output: String

    private enum CodingKeys: String, CodingKey {
        case kind
        case callID = "call_id"
        case output
    }
}

public struct HubToolInvocationEvent: Codable, Equatable, Sendable {
    public let kind: String
    public let invocationID: HubToolInvocationID
    public let toolName: String
    public let toolCallID: HubToolCallID?
    public let functionCallEventID: HubEventID
    public let functionResponseEventID: HubEventID?
    public let result: HubToolResult?
    public let statusUpdates: [String]
    public let pendingConfirmation: Bool
    public let decision: HubToolDecision?
    public let decisionAt: Date?
    public let decisionReason: String?
    public let isCollapsedByOwner: Bool
    public let childSessionID: HubSessionID?
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

public enum HubToolResult: Codable, Equatable, Sendable {
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

public enum HubToolDecision: String, Codable, Equatable, Sendable {
    case approved
    case denied
}

public struct HubTurnFailedEvent: Codable, Equatable, Sendable {
    public let kind: String
    public let turnID: String
    public let reason: String
    public let failedAt: Date
    public let runnerID: HubRunnerID?
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
