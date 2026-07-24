import Foundation

public enum HubToolPermission: Equatable, Sendable, Codable {
    case auto
    case confirm
    case unknown(String)

    public init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        switch try container.decode(String.self) {
        case "auto":
            self = .auto
        case "confirm":
            self = .confirm
        case let value:
            self = .unknown(value)
        }
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        switch self {
        case .auto:
            try container.encode("auto")
        case .confirm:
            try container.encode("confirm")
        case .unknown(let value):
            try container.encode(value)
        }
    }

    public var label: String {
        switch self {
        case .auto:
            return "Auto"
        case .confirm:
            return "Confirm"
        case .unknown(let value):
            return value
        }
    }
}

public enum HubSessionState: String, Codable, Equatable, Sendable {
    case idle
    case prompting
    case waitingForConfirmation = "waiting_for_confirmation"
    case compacting
    case stopping
    case failed
    case unknown
}

public struct HubSessionStatus: Codable, Equatable, Sendable {
    public let state: HubSessionState
    public let rawState: String
    public let currentToolCalls: [String]
    public let statusUpdates: [String]
    public let pendingUserMessages: Int
    public let reason: String?
    public let failedAt: Date?

    public init(
        state: HubSessionState,
        rawState: String? = nil,
        currentToolCalls: [String] = [],
        statusUpdates: [String] = [],
        pendingUserMessages: Int = 0,
        reason: String? = nil,
        failedAt: Date? = nil
    ) {
        self.state = state
        self.rawState = rawState ?? state.rawValue
        self.currentToolCalls = currentToolCalls
        self.statusUpdates = statusUpdates
        self.pendingUserMessages = pendingUserMessages
        self.reason = reason
        self.failedAt = failedAt
    }

    private enum CodingKeys: String, CodingKey {
        case state
        case currentToolCalls = "current_tool_calls"
        case statusUpdates = "status_updates"
        case pendingUserMessages = "pending_user_messages"
        case reason
        case failedAt = "failed_at"
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let rawState = try container.decode(String.self, forKey: .state)
        self.state = HubSessionState(rawValue: rawState) ?? .unknown
        self.rawState = rawState
        self.currentToolCalls = try container.decodeIfPresent([String].self, forKey: .currentToolCalls) ?? []
        self.statusUpdates = try container.decodeIfPresent([String].self, forKey: .statusUpdates) ?? []
        self.pendingUserMessages = try container.decodeIfPresent(Int.self, forKey: .pendingUserMessages) ?? 0
        self.reason = try container.decodeIfPresent(String.self, forKey: .reason)
        self.failedAt = try container.decodeIfPresent(Date.self, forKey: .failedAt)
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(rawState, forKey: .state)
        if !currentToolCalls.isEmpty {
            try container.encode(currentToolCalls, forKey: .currentToolCalls)
        }
        if !statusUpdates.isEmpty {
            try container.encode(statusUpdates, forKey: .statusUpdates)
        }
        if pendingUserMessages > 0 {
            try container.encode(pendingUserMessages, forKey: .pendingUserMessages)
        }
        try container.encodeIfPresent(reason, forKey: .reason)
        try container.encodeIfPresent(failedAt, forKey: .failedAt)
    }

    public var label: String {
        switch state {
        case .idle:
            return "Idle"
        case .prompting:
            return "Running"
        case .waitingForConfirmation:
            return "Needs Approval"
        case .compacting:
            return "Compacting"
        case .stopping:
            return "Stopping"
        case .failed:
            return "Failed"
        case .unknown:
            return rawState
        }
    }
}

public enum HubTemplateDerivativeStatus: Equatable, Sendable, Codable {
    case notDerived
    case current
    case stale
    case sourceMissing
    case sourceArchived
    case unknown(String)

    public init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        switch try container.decode(String.self) {
        case "not_derived":
            self = .notDerived
        case "current":
            self = .current
        case "stale":
            self = .stale
        case "source_missing":
            self = .sourceMissing
        case "source_archived":
            self = .sourceArchived
        case let value:
            self = .unknown(value)
        }
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        switch self {
        case .notDerived:
            try container.encode("not_derived")
        case .current:
            try container.encode("current")
        case .stale:
            try container.encode("stale")
        case .sourceMissing:
            try container.encode("source_missing")
        case .sourceArchived:
            try container.encode("source_archived")
        case .unknown(let value):
            try container.encode(value)
        }
    }

    public var label: String {
        switch self {
        case .notDerived:
            return "Not Derived"
        case .current:
            return "Current"
        case .stale:
            return "Stale"
        case .sourceMissing:
            return "Source Missing"
        case .sourceArchived:
            return "Source Archived"
        case .unknown(let value):
            return value
        }
    }
}

public struct HubTemplate: Codable, Identifiable, Equatable, Sendable {
    public let id: HubTemplateID
    public let title: String
    public let description: String?
    public let templateVersion: String?
    public let promptVersion: String?
    public let derivedFromTemplateID: HubTemplateID?
    public let derivedFromTemplateVersion: String?
    public let derivedFromTemplatePromptVersion: String?
    public let derivativeStatus: HubTemplateDerivativeStatus?
    public let currentSourceTemplateVersion: String?
    public let currentSourceTemplatePromptVersion: String?
    public let systemPrompt: String
    public let defaultTools: [String]?
    public let defaultModelAlias: String?
    public let defaultToolPermissionOverrides: [String: HubToolPermission]
    public let defaultDangerousAutoApproveAllTools: Bool
    public let isBuiltin: Bool
    public let isArchived: Bool
    public let createdAt: Date
    public let lastModifiedAt: Date

    private enum CodingKeys: String, CodingKey {
        case id
        case title
        case description
        case templateVersion = "template_version"
        case promptVersion = "prompt_version"
        case derivedFromTemplateID = "derived_from_template_id"
        case derivedFromTemplateVersion = "derived_from_template_version"
        case derivedFromTemplatePromptVersion = "derived_from_template_prompt_version"
        case derivativeStatus = "derivative_status"
        case currentSourceTemplateVersion = "current_source_template_version"
        case currentSourceTemplatePromptVersion = "current_source_template_prompt_version"
        case systemPrompt = "system_prompt"
        case defaultTools = "default_tools"
        case defaultModelAlias = "default_model_alias"
        case defaultToolPermissionOverrides = "default_tool_permission_overrides"
        case defaultDangerousAutoApproveAllTools = "default_dangerous_auto_approve_all_tools"
        case isBuiltin = "is_builtin"
        case isArchived = "is_archived"
        case createdAt = "created_at"
        case lastModifiedAt = "last_modified_at"
    }
}

public struct HubSessionMetadata: Codable, Identifiable, Equatable, Sendable {
    public let id: HubSessionID
    public let createdFromTemplateID: HubTemplateID?
    public var title: String?
    public var description: String?
    public let systemPrompt: String
    public let modelAlias: String
    public let enabledTools: [String]?
    public let toolPermissionOverrides: [String: HubToolPermission]
    public let dangerousAutoApproveAllTools: Bool
    public let runnerID: HubRunnerID?
    public let tags: [String]
    public var isArchived: Bool
    public let parentSessionID: HubSessionID?
    public let linkedWorkspacePath: String?
    public let guidanceTargetPath: String?
    public let createdAt: Date
    public let lastModifiedAt: Date
    public let lastTurnEndedAt: Date?
    public let createdFrom: String
    public let lastPromptedFrom: String?
    public let sourceApp: String?

    private enum CodingKeys: String, CodingKey {
        case id
        case createdFromTemplateID = "created_from_template_id"
        case title
        case description
        case systemPrompt = "system_prompt"
        case modelAlias = "model_alias"
        case enabledTools = "enabled_tools"
        case toolPermissionOverrides = "tool_permission_overrides"
        case dangerousAutoApproveAllTools = "dangerous_auto_approve_all_tools"
        case runnerID = "runner_id"
        case tags
        case isArchived = "is_archived"
        case parentSessionID = "parent_session_id"
        case linkedWorkspacePath = "linked_workspace_path"
        case guidanceTargetPath = "guidance_target_path"
        case createdAt = "created_at"
        case lastModifiedAt = "last_modified_at"
        case lastTurnEndedAt = "last_turn_ended_at"
        case createdFrom = "created_from"
        case lastPromptedFrom = "last_prompted_from"
        case sourceApp = "source_app"
    }

    public var displayTitle: String {
        if let title, !title.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            return title
        }
        return "Session \(id.rawValue.prefix(8))"
    }
}

public struct HubSessionView: Codable, Equatable, Sendable {
    public let metadata: HubSessionMetadata
    public let status: HubSessionStatus
}

public struct HubRunnerTool: Codable, Identifiable, Equatable, Sendable {
    public var id: String { name }
    public let name: String
    public let description: String
    public let jsonSchema: [String: HubJSONValue]
    public let defaultPermission: HubToolPermission

    private enum CodingKeys: String, CodingKey {
        case name
        case description
        case jsonSchema = "json_schema"
        case defaultPermission = "default_permission"
    }
}

public struct HubRunner: Codable, Identifiable, Equatable, Sendable {
    public let id: HubRunnerID
    public let environmentTag: String
    public let status: HubRunnerStatus
    public let registeredAt: Date
    public let lastSeenAt: Date?
    public let toolCatalog: [HubRunnerTool]

    private enum CodingKeys: String, CodingKey {
        case id
        case environmentTag = "environment_tag"
        case status
        case registeredAt = "registered_at"
        case lastSeenAt = "last_seen_at"
        case toolCatalog = "tool_catalog"
    }
}

public enum HubRunnerStatus: String, Codable, Equatable, Sendable {
    case online
    case offline
    case unknown

    public init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        let rawValue = try container.decode(String.self)
        self = HubRunnerStatus(rawValue: rawValue) ?? .unknown
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(rawValue)
    }
}

public struct HubArtifact: Codable, Identifiable, Equatable, Sendable {
    public let id: HubArtifactID
    public let sessionID: HubSessionID
    public let eventID: HubEventID?
    public let kind: String
    public let title: String
    public let mimeType: String?
    public let path: String?
    public let contentText: String?
    public let metadata: [String: HubJSONValue]
    public let createdAt: Date

    private enum CodingKeys: String, CodingKey {
        case id
        case sessionID = "session_id"
        case eventID = "event_id"
        case kind
        case title
        case mimeType = "mime_type"
        case path
        case contentText = "content_text"
        case metadata
        case createdAt = "created_at"
    }
}

public struct HubTaskSummary: Codable, Equatable, Sendable {
    public let total: Int
    public let todo: Int
    public let inProgress: Int
    public let blocked: Int
    public let done: Int
    public let cancelled: Int

    private enum CodingKeys: String, CodingKey {
        case total
        case todo
        case inProgress = "in_progress"
        case blocked
        case done
        case cancelled
    }

    public init(
        total: Int = 0,
        todo: Int = 0,
        inProgress: Int = 0,
        blocked: Int = 0,
        done: Int = 0,
        cancelled: Int = 0
    ) {
        self.total = total
        self.todo = todo
        self.inProgress = inProgress
        self.blocked = blocked
        self.done = done
        self.cancelled = cancelled
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        self.total = try container.decodeIfPresent(Int.self, forKey: .total) ?? 0
        self.todo = try container.decodeIfPresent(Int.self, forKey: .todo) ?? 0
        self.inProgress = try container.decodeIfPresent(Int.self, forKey: .inProgress) ?? 0
        self.blocked = try container.decodeIfPresent(Int.self, forKey: .blocked) ?? 0
        self.done = try container.decodeIfPresent(Int.self, forKey: .done) ?? 0
        self.cancelled = try container.decodeIfPresent(Int.self, forKey: .cancelled) ?? 0
    }
}

public struct HubMonitorSessionSummary: Codable, Identifiable, Equatable, Sendable {
    public var id: HubSessionID { metadata.id }
    public let metadata: HubSessionMetadata
    public let status: HubSessionStatus
    public let runnerStatus: HubRunnerStatus
    public let eventCount: Int
    public let messageCount: Int
    public let toolInvocationCount: Int
    public let artifactCount: Int
    public let taskSummary: HubTaskSummary
    public let lastEventID: HubEventID?
    public let lastEventAt: Date?
    public let lastActivityAt: Date
    public let lastEventKind: String?

    private enum CodingKeys: String, CodingKey {
        case metadata
        case status
        case runnerStatus = "runner_status"
        case eventCount = "event_count"
        case messageCount = "message_count"
        case toolInvocationCount = "tool_invocation_count"
        case artifactCount = "artifact_count"
        case taskSummary = "task_summary"
        case lastEventID = "last_event_id"
        case lastEventAt = "last_event_at"
        case lastActivityAt = "last_activity_at"
        case lastEventKind = "last_event_kind"
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        self.metadata = try container.decode(HubSessionMetadata.self, forKey: .metadata)
        self.status = try container.decode(HubSessionStatus.self, forKey: .status)
        self.runnerStatus = try container.decode(HubRunnerStatus.self, forKey: .runnerStatus)
        self.eventCount = try container.decode(Int.self, forKey: .eventCount)
        self.messageCount = try container.decode(Int.self, forKey: .messageCount)
        self.toolInvocationCount = try container.decode(Int.self, forKey: .toolInvocationCount)
        self.artifactCount = try container.decode(Int.self, forKey: .artifactCount)
        self.taskSummary = try container.decodeIfPresent(HubTaskSummary.self, forKey: .taskSummary) ?? HubTaskSummary()
        self.lastEventID = try container.decodeIfPresent(HubEventID.self, forKey: .lastEventID)
        self.lastEventAt = try container.decodeIfPresent(Date.self, forKey: .lastEventAt)
        self.lastActivityAt = try container.decode(Date.self, forKey: .lastActivityAt)
        self.lastEventKind = try container.decodeIfPresent(String.self, forKey: .lastEventKind)
    }
}
