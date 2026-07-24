import Foundation

public struct HubTemplateListResponse: Codable, Equatable, Sendable {
    public let templates: [HubTemplate]
}

public struct HubRunnerListResponse: Codable, Equatable, Sendable {
    public let runners: [HubRunner]
}

public struct HubSessionListResponse: Codable, Equatable, Sendable {
    public let sessions: [HubSessionMetadata]
    public let limit: Int
    public let offset: Int
    public let total: Int?
}

public struct HubEventPage: Codable, Equatable, Sendable {
    public let events: [HubStoredEvent]
    public let limit: Int
    public let nextAfter: HubEventID?

    private enum CodingKeys: String, CodingKey {
        case events
        case limit
        case nextAfter = "next_after"
    }
}

public struct HubAppendUserMessageResponse: Codable, Equatable, Sendable {
    public let eventID: HubEventID
    public let event: HubConversationEvent
    public let sessionStatus: HubSessionStatus

    private enum CodingKeys: String, CodingKey {
        case eventID = "event_id"
        case event
        case sessionStatus = "session_status"
    }
}

public struct HubArtifactListResponse: Codable, Equatable, Sendable {
    public let artifacts: [HubArtifact]
    public let limit: Int
    public let offset: Int
    public let total: Int?
}

public struct HubMonitorSessionListResponse: Codable, Equatable, Sendable {
    public let sessions: [HubMonitorSessionSummary]
    public let limit: Int
    public let offset: Int
    public let total: Int?
}

public struct HubMonitorSessionDetail: Codable, Equatable, Sendable {
    public let summary: HubMonitorSessionSummary
    public let recentEvents: [HubStoredEvent]

    private enum CodingKeys: String, CodingKey {
        case summary
        case recentEvents = "recent_events"
    }
}

public enum HubServerMessage: Codable, Equatable, Sendable {
    case streamHello(HubStreamHello)
    case eventAppended(HubStreamEventMutation)
    case eventUpdated(HubStreamEventMutation)
    case eventDeleted(HubEventID)
    case statusChanged(HubSessionStatus)
    case metadataChanged(HubSessionMetadata)
    case artifactCreated(HubArtifact)
    case heartbeat(Date)
    case unknown(kind: String, payload: [String: HubJSONValue])

    public var kind: String {
        switch self {
        case .streamHello:
            return "stream_hello"
        case .eventAppended:
            return "event_appended"
        case .eventUpdated:
            return "event_updated"
        case .eventDeleted:
            return "event_deleted"
        case .statusChanged:
            return "status_changed"
        case .metadataChanged:
            return "metadata_changed"
        case .artifactCreated:
            return "artifact_created"
        case .heartbeat:
            return "heartbeat"
        case .unknown(let kind, _):
            return kind
        }
    }

    private enum CodingKeys: String, CodingKey {
        case kind
        case eventID = "event_id"
        case status
        case metadata
        case artifact
        case sentAt = "sent_at"
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let kind = try container.decode(String.self, forKey: .kind)
        switch kind {
        case "stream_hello":
            self = .streamHello(try HubStreamHello(from: decoder))
        case "event_appended":
            self = .eventAppended(try HubStreamEventMutation(from: decoder))
        case "event_updated":
            self = .eventUpdated(try HubStreamEventMutation(from: decoder))
        case "event_deleted":
            self = .eventDeleted(try container.decode(HubEventID.self, forKey: .eventID))
        case "status_changed":
            self = .statusChanged(try container.decode(HubSessionStatus.self, forKey: .status))
        case "metadata_changed":
            self = .metadataChanged(try container.decode(HubSessionMetadata.self, forKey: .metadata))
        case "artifact_created":
            self = .artifactCreated(try container.decode(HubArtifact.self, forKey: .artifact))
        case "heartbeat":
            self = .heartbeat(try container.decode(Date.self, forKey: .sentAt))
        default:
            let payload = try decoder.singleValueContainer().decode([String: HubJSONValue].self)
            self = .unknown(kind: kind, payload: payload)
        }
    }

    public func encode(to encoder: Encoder) throws {
        switch self {
        case .streamHello(let message):
            try message.encode(to: encoder)
        case .eventAppended(let mutation):
            try mutation.encode(kind: "event_appended", to: encoder)
        case .eventUpdated(let mutation):
            try mutation.encode(kind: "event_updated", to: encoder)
        case .eventDeleted(let eventID):
            var container = encoder.container(keyedBy: CodingKeys.self)
            try container.encode("event_deleted", forKey: .kind)
            try container.encode(eventID, forKey: .eventID)
        case .statusChanged(let status):
            var container = encoder.container(keyedBy: CodingKeys.self)
            try container.encode("status_changed", forKey: .kind)
            try container.encode(status, forKey: .status)
        case .metadataChanged(let metadata):
            var container = encoder.container(keyedBy: CodingKeys.self)
            try container.encode("metadata_changed", forKey: .kind)
            try container.encode(metadata, forKey: .metadata)
        case .artifactCreated(let artifact):
            var container = encoder.container(keyedBy: CodingKeys.self)
            try container.encode("artifact_created", forKey: .kind)
            try container.encode(artifact, forKey: .artifact)
        case .heartbeat(let date):
            var container = encoder.container(keyedBy: CodingKeys.self)
            try container.encode("heartbeat", forKey: .kind)
            try container.encode(date, forKey: .sentAt)
        case .unknown(let kind, let payload):
            var payload = payload
            payload["kind"] = .string(kind)
            try payload.encode(to: encoder)
        }
    }
}

public struct HubStreamHello: Codable, Equatable, Sendable {
    public let kind: String
    public let session: HubSessionMetadata
    public let status: HubSessionStatus
    public let events: [HubStoredEvent]
}

public struct HubStreamEventMutation: Codable, Equatable, Sendable {
    public let eventID: HubEventID
    public let event: HubConversationEvent

    private enum CodingKeys: String, CodingKey {
        case eventID = "event_id"
        case event
    }

    fileprivate func encode(kind: String, to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: DynamicCodingKeys.self)
        try container.encode(kind, forKey: DynamicCodingKeys("kind"))
        try container.encode(eventID, forKey: DynamicCodingKeys("event_id"))
        try container.encode(event, forKey: DynamicCodingKeys("event"))
    }
}

private struct DynamicCodingKeys: CodingKey {
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
