import Foundation

#if canImport(SignalboxModels)
  import SignalboxModels
#endif

/// A validated immutable inventory whose entry objects are decoded on demand.
public final class SignalboxImportedConversationInventory: Sendable {
  public let importedConversationID: SignalboxCanonicalUUID
  private let data: Data
  private let ranges: [Range<Int>]

  init(importedConversationID: SignalboxCanonicalUUID, data: Data, ranges: [Range<Int>]) {
    self.importedConversationID = importedConversationID
    self.data = data
    self.ranges = ranges
  }

  public var entryCount: Int { ranges.count }

  public func entries(in range: Range<Int>) throws -> [SignalboxImportedConversationEntry] {
    guard range.lowerBound >= 0, range.upperBound <= entryCount else {
      throw SignalboxProcessServiceError.invalidPage("The imported entry page is outside the inventory.")
    }
    return try range.map {
      try SignalboxJSONCoding.decoder().decode(
        SignalboxImportedConversationEntry.self, from: data.subdata(in: ranges[$0]))
    }
  }
}

final class SignalboxImportedEntrySpool {
  private let url: URL
  private let file: FileHandle
  private var ranges: [Range<Int>] = []
  private var offset = 0

  init() throws {
    url = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    guard FileManager.default.createFile(
      atPath: url.path, contents: nil, attributes: [.posixPermissions: 0o600])
    else { throw CocoaError(.fileWriteUnknown) }
    do {
      file = try FileHandle(forUpdating: url)
    } catch {
      try? FileManager.default.removeItem(at: url)
      throw error
    }
  }

  deinit {
    try? file.close()
    try? FileManager.default.removeItem(at: url)
  }

  var count: Int { ranges.count }

  func append(_ entry: SignalboxImportedConversationEntry) throws {
    let preview: SignalboxJSONValue = entry.textPreview.map {
      .object(["preview": .string($0.preview), "truncated": .bool($0.truncated)])
    } ?? .null
    let encoded = try SignalboxJSONCoding.encoder().encode(SignalboxJSONValue.object([
      "type": .string("imported_conversation_entry"),
      "position": .string(String(entry.position.rawValue)),
      "imported_entry_id": .string(entry.importedEntryID.rawValue),
      "source_speaker": speaker(entry.sourceSpeaker),
      "content_kind": .string(entry.contentKind.rawValue),
      "text_preview": preview,
    ]))
    try file.write(contentsOf: encoded)
    ranges.append(offset..<(offset + encoded.count))
    offset += encoded.count
  }

  func finish(importedConversationID: SignalboxCanonicalUUID) throws
    -> SignalboxImportedConversationInventory
  {
    try file.close()
    let data = try Data(contentsOf: url, options: .mappedIfSafe)
    return SignalboxImportedConversationInventory(
      importedConversationID: importedConversationID, data: data, ranges: ranges)
  }

  private func speaker(_ value: SignalboxImportedSourceSpeaker) -> SignalboxJSONValue {
    switch value {
    case .notAttested: return .object(["type": .string("not_attested")])
    case .attestedAbsent: return .object(["type": .string("attested_absent")])
    case .attested(let speaker):
      let label: String
      switch speaker {
      case .user: label = "user"
      case .assistant: label = "assistant"
      case .unknown(let value): label = value
      }
      return .object(["type": .string("attested"), "speaker": .string(label)])
    case .unknown(let kind, var payload):
      payload["type"] = .string(kind)
      return .object(payload)
    }
  }
}
