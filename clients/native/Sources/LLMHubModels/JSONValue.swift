import Foundation

public enum HubJSONValue: Codable, Equatable, Sendable {
    case object([String: HubJSONValue])
    case array([HubJSONValue])
    case string(String)
    case number(Double)
    case bool(Bool)
    case null

    public init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        if container.decodeNil() {
            self = .null
        } else if let value = try? container.decode(Bool.self) {
            self = .bool(value)
        } else if let value = try? container.decode(Double.self) {
            self = .number(value)
        } else if let value = try? container.decode(String.self) {
            self = .string(value)
        } else if let value = try? container.decode([HubJSONValue].self) {
            self = .array(value)
        } else {
            self = .object(try container.decode([String: HubJSONValue].self))
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

public enum HubJSONCoding {
    public static func decoder() -> JSONDecoder {
        let decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .custom { decoder in
            let container = try decoder.singleValueContainer()
            let rawValue = try container.decode(String.self)
            return try HubDateParser.parse(rawValue)
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

public enum HubDateParser {
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
        if let date = fractionalFormatter().date(from: rawValue) {
            return date
        }
        if let date = wholeSecondFormatter().date(from: rawValue) {
            return date
        }
        throw DecodingError.dataCorrupted(
            .init(
                codingPath: [],
                debugDescription: "Invalid hub timestamp: \(rawValue)"
            )
        )
    }
}
