import Foundation

public struct SignalboxTemplateListResponse: Codable, Equatable, Sendable {
    public let templates: [SignalboxTemplate]
}

public struct SignalboxRunnerListResponse: Codable, Equatable, Sendable {
    public let runners: [SignalboxRunner]
}

public struct SignalboxSessionListResponse: Codable, Equatable, Sendable {
    public let sessions: [SignalboxSessionMetadata]
    public let limit: Int
    public let offset: Int
    public let total: Int?
}

public struct SignalboxEventPage: Codable, Equatable, Sendable {
    public let events: [SignalboxStoredEvent]
    public let limit: Int
    public let nextAfter: SignalboxEventID?

    private enum CodingKeys: String, CodingKey {
        case events
        case limit
        case nextAfter = "next_after"
    }
}

public struct SignalboxAppendUserMessageResponse: Codable, Equatable, Sendable {
    public let eventID: SignalboxEventID
    public let event: SignalboxConversationEvent
    public let sessionStatus: SignalboxSessionStatus

    private enum CodingKeys: String, CodingKey {
        case eventID = "event_id"
        case event
        case sessionStatus = "session_status"
    }
}

public struct SignalboxArtifactListResponse: Codable, Equatable, Sendable {
    public let artifacts: [SignalboxArtifact]
    public let limit: Int
    public let offset: Int
    public let total: Int?
}

public struct SignalboxMonitorSessionListResponse: Codable, Equatable, Sendable {
    public let sessions: [SignalboxMonitorSessionSummary]
    public let limit: Int
    public let offset: Int
    public let total: Int?
}

public struct SignalboxMonitorSessionDetail: Codable, Equatable, Sendable {
    public let summary: SignalboxMonitorSessionSummary
    public let recentEvents: [SignalboxStoredEvent]

    private enum CodingKeys: String, CodingKey {
        case summary
        case recentEvents = "recent_events"
    }
}

public enum SignalboxServerMessage: Codable, Equatable, Sendable {
    case streamHello(SignalboxStreamHello)
    case eventAppended(SignalboxStreamEventMutation)
    case eventUpdated(SignalboxStreamEventMutation)
    case eventDeleted(SignalboxEventID)
    case statusChanged(SignalboxSessionStatus)
    case metadataChanged(SignalboxSessionMetadata)
    case artifactCreated(SignalboxArtifact)
    case heartbeat(Date)
    case diagnostic(SignalboxDecodingDiagnostic)
    case unknown(
        kind: String,
        payload: [String: SignalboxJSONValue],
        decodingDiagnostic: SignalboxDecodingDiagnostic?
    )

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
        case .diagnostic:
            return "client_diagnostic"
        case .unknown(let kind, _, _):
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
        do {
            switch kind {
            case "stream_hello":
                self = .streamHello(try SignalboxStreamHello(from: decoder))
            case "event_appended":
                self = .eventAppended(try SignalboxStreamEventMutation(from: decoder))
            case "event_updated":
                self = .eventUpdated(try SignalboxStreamEventMutation(from: decoder))
            case "event_deleted":
                self = .eventDeleted(try container.decode(SignalboxEventID.self, forKey: .eventID))
            case "status_changed":
                self = .statusChanged(try container.decode(SignalboxSessionStatus.self, forKey: .status))
            case "metadata_changed":
                self = .metadataChanged(try container.decode(SignalboxSessionMetadata.self, forKey: .metadata))
            case "artifact_created":
                self = .artifactCreated(try container.decode(SignalboxArtifact.self, forKey: .artifact))
            case "heartbeat":
                self = .heartbeat(try container.decode(Date.self, forKey: .sentAt))
            default:
                let payload = try decoder.singleValueContainer().decode([String: SignalboxJSONValue].self)
                self = .unknown(kind: kind, payload: payload, decodingDiagnostic: nil)
            }
        } catch {
            let payload = try decoder.singleValueContainer().decode([String: SignalboxJSONValue].self)
            self = .unknown(
                kind: kind,
                payload: payload,
                decodingDiagnostic: SignalboxDecodingDiagnostic(error: error)
            )
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
        case .diagnostic(let diagnostic):
            throw EncodingError.invalidValue(
                diagnostic,
                .init(codingPath: encoder.codingPath, debugDescription: "Client diagnostics are not wire messages.")
            )
        case .unknown(let kind, let payload, _):
            var payload = payload
            payload["kind"] = .string(kind)
            try payload.encode(to: encoder)
        }
    }
}

public struct SignalboxStreamHello: Codable, Equatable, Sendable {
    public let kind: String
    public let session: SignalboxSessionMetadata
    public let status: SignalboxSessionStatus
    public let events: [SignalboxStoredEvent]
}

public struct SignalboxStreamEventMutation: Codable, Equatable, Sendable {
    public let eventID: SignalboxEventID
    public let event: SignalboxConversationEvent

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
