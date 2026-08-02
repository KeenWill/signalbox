import Foundation

public enum SignalboxJSONValue: Codable, Equatable, Sendable {
    case object([String: SignalboxJSONValue])
    case array([SignalboxJSONValue])
    case string(String)
    case number(Double)
    case bool(Bool)
    case null

    public init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        // Decode Bool before Double: Foundation-backed JSON decoders may bridge
        // both through NSNumber, but wire booleans must not become 0 or 1.
        if container.decodeNil() {
            self = .null
        } else if let value = try? container.decode(Bool.self) {
            self = .bool(value)
        } else if let value = try? container.decode(Double.self) {
            self = .number(value)
        } else if let value = try? container.decode(String.self) {
            self = .string(value)
        } else if let value = try? container.decode([SignalboxJSONValue].self) {
            self = .array(value)
        } else {
            self = .object(try container.decode([String: SignalboxJSONValue].self))
        }
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        switch self {
        case .object(let value):
            try container.encode(value)
        case .array(let value):
            try container.encode(value)
        case .string(let value):
            try container.encode(value)
        case .number(let value):
            try container.encode(value)
        case .bool(let value):
            try container.encode(value)
        case .null:
            try container.encodeNil()
        }
    }

    public var displayString: String {
        switch self {
        case .object(let object):
            let fields = object.keys.sorted().prefix(4).joined(separator: ", ")
            return object.count > 4 ? "{\(fields), ...}" : "{\(fields)}"
        case .array(let array):
            return "[\(array.count) items]"
        case .string(let string):
            return string
        case .number(let number):
            return String(number)
        case .bool(let bool):
            return bool ? "true" : "false"
        case .null:
            return "null"
        }
    }
}

public enum SignalboxJSONCoding {
    public static func decoder() -> JSONDecoder {
        let decoder = SignalboxDuplicateAwareJSONDecoder()
        decoder.dateDecodingStrategy = .custom { decoder in
            let container = try decoder.singleValueContainer()
            let rawValue = try container.decode(String.self)
            return try SignalboxDateParser.parse(
                rawValue,
                codingPath: decoder.codingPath
            )
        }
        return decoder
    }

    public static func encoder() -> JSONEncoder {
        let encoder = JSONEncoder()
        encoder.dateEncodingStrategy = .iso8601
        encoder.outputFormatting = [.sortedKeys]
        return encoder
    }
}

public struct SignalboxDecodingDiagnostic: Equatable, Sendable {
    public let message: String

    public init(message: String) {
        self.message = message
    }

    public init(error: Error) {
        switch error {
        case DecodingError.keyNotFound(let key, let context):
            self.message = "Missing required field at \(Self.path(context.codingPath + [key]))."
        case DecodingError.typeMismatch(_, let context):
            self.message = "Unexpected field type at \(Self.path(context.codingPath))."
        case DecodingError.valueNotFound(_, let context):
            self.message = "Missing required value at \(Self.path(context.codingPath))."
        case DecodingError.dataCorrupted(let context):
            self.message = "Invalid field value at \(Self.path(context.codingPath))."
        default:
            self.message = "Could not decode the server payload."
        }
    }

    private static func path(_ codingPath: [any CodingKey]) -> String {
        let components = codingPath.map { key in
            if let index = key.intValue {
                return "[\(index)]"
            }
            return key.stringValue
        }
        guard !components.isEmpty else {
            return "the payload"
        }
        return components.reduce(into: "") { path, component in
            if component.hasPrefix("[") {
                path += component
            } else {
                path += path.isEmpty ? component : ".\(component)"
            }
        }
    }
}

public enum SignalboxDateParser {
    // The wire admits both fractional and whole-second RFC 3339 timestamps;
    // ISO8601DateFormatter requires separate option sets for those shapes.
    private static func fractionalFormatter() -> ISO8601DateFormatter {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return formatter
    }

    private static func wholeSecondFormatter() -> ISO8601DateFormatter {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime]
        return formatter
    }

    public static func parse(_ rawValue: String) throws -> Date {
        try parse(rawValue, codingPath: [])
    }

    static func parse(
        _ rawValue: String,
        codingPath: [any CodingKey]
    ) throws -> Date {
        if let date = fractionalFormatter().date(from: rawValue) {
            return date
        }
        if let date = wholeSecondFormatter().date(from: rawValue) {
            return date
        }
        throw DecodingError.dataCorrupted(
            .init(
                codingPath: codingPath,
                debugDescription: "Invalid server timestamp: \(rawValue)"
            )
        )
    }
}
