import Foundation

public struct HubSessionID: RawRepresentable, Codable, Hashable, Identifiable, Sendable {
    public let rawValue: String
    public var id: String { rawValue }

    public init(rawValue: String) {
        self.rawValue = rawValue
    }
}

public struct HubTemplateID: RawRepresentable, Codable, Hashable, Identifiable, Sendable {
    public let rawValue: String
    public var id: String { rawValue }

    public init(rawValue: String) {
        self.rawValue = rawValue
    }
}

public struct HubRunnerID: RawRepresentable, Codable, Hashable, Identifiable, Sendable {
    public let rawValue: String
    public var id: String { rawValue }

    public init(rawValue: String) {
        self.rawValue = rawValue
    }
}

public struct HubToolInvocationID: RawRepresentable, Codable, Hashable, Identifiable, Sendable {
    public let rawValue: String
    public var id: String { rawValue }

    public init(rawValue: String) {
        self.rawValue = rawValue
    }
}

public struct HubArtifactID: RawRepresentable, Codable, Hashable, Identifiable, Sendable {
    public let rawValue: String
    public var id: String { rawValue }

    public init(rawValue: String) {
        self.rawValue = rawValue
    }
}

public struct HubEventID: RawRepresentable, Codable, Hashable, Comparable, Identifiable, Sendable {
    public let rawValue: Int
    public var id: Int { rawValue }

    public init(rawValue: Int) {
        self.rawValue = rawValue
    }

    public static func < (lhs: HubEventID, rhs: HubEventID) -> Bool {
        lhs.rawValue < rhs.rawValue
    }
}

public struct HubToolCallID: RawRepresentable, Codable, Hashable, Identifiable, Sendable {
    public let rawValue: String
    public var id: String { rawValue }

    public init(rawValue: String) {
        self.rawValue = rawValue
    }
}
