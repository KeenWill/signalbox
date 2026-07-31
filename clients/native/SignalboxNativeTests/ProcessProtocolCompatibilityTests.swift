import Foundation
import XCTest

@testable import SignalboxNative

final class ProcessProtocolCompatibilityTests: XCTestCase {
  func testSessionMetadataEncodesAbsentTitleAsExplicitNull() throws {
    let metadata = SignalboxProcessSessionMetadata(
      title: nil,
      tags: [],
      attributes: [:],
      archived: false
    )

    let encoded = try SignalboxJSONCoding.encoder().encode(metadata)

    XCTAssertEqual(
      String(decoding: encoded, as: UTF8.self),
      #"{"archived":false,"attributes":{},"tags":[],"title":null}"#
    )
  }

  func testSessionMetadataRequiresItsNullableTitleMember() throws {
    XCTAssertThrowsError(
      try SignalboxJSONCoding.decoder().decode(
        SignalboxProcessSessionMetadata.self,
        from: Data(#"{"archived":false,"attributes":{},"tags":[]}"#.utf8)
      )
    )
  }
}
