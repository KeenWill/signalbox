import Foundation

public struct SignalboxSessionID: RawRepresentable, Codable, Hashable, Identifiable, Sendable {
    public let rawValue: String
    public var id: String { rawValue }

    public init(rawValue: String) {
        self.rawValue = rawValue
    }
}

public struct SignalboxTemplateID: RawRepresentable, Codable, Hashable, Identifiable, Sendable {
    public let rawValue: String
    public var id: String { rawValue }

    public init(rawValue: String) {
        self.rawValue = rawValue
    }
}

public struct SignalboxRunnerID: RawRepresentable, Codable, Hashable, Identifiable, Sendable {
    public let rawValue: String
    public var id: String { rawValue }

    public init(rawValue: String) {
        self.rawValue = rawValue
    }
}

public struct SignalboxToolInvocationID: RawRepresentable, Codable, Hashable, Identifiable, Sendable {
    public let rawValue: String
    public var id: String { rawValue }

    public init(rawValue: String) {
        self.rawValue = rawValue
    }
}

public struct SignalboxArtifactID: RawRepresentable, Codable, Hashable, Identifiable, Sendable {
    public let rawValue: String
    public var id: String { rawValue }

    public init(rawValue: String) {
        self.rawValue = rawValue
    }
}

public struct SignalboxEventID: RawRepresentable, Codable, Hashable, Comparable, Identifiable, Sendable {
    public let rawValue: Int
    public var id: Int { rawValue }

    public init(rawValue: Int) {
        self.rawValue = rawValue
    }

    public static func < (lhs: SignalboxEventID, rhs: SignalboxEventID) -> Bool {
        lhs.rawValue < rhs.rawValue
    }
}

public struct SignalboxToolCallID: RawRepresentable, Codable, Hashable, Identifiable, Sendable {
    public let rawValue: String
    public var id: String { rawValue }

    public init(rawValue: String) {
        self.rawValue = rawValue
    }
}
