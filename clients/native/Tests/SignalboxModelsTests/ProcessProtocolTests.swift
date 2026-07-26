import Foundation
import XCTest

@testable import SignalboxNative

final class ProcessProtocolTests: XCTestCase {
  private let sessionID = "11111111-1111-4111-8111-111111111111"
  private let turnID = "22222222-2222-4222-8222-222222222222"

  func testClientFrameUsesVersionFiveAndCanonicalStringScalars() throws {
    let frame = SignalboxProcessClientFrame(
      requestID: try SignalboxRequestID(validating: 7),
      request: .readTranscript(
        sessionID: try SignalboxCanonicalUUID(validating: sessionID)
      )
    )

    let encoded = try SignalboxJSONCoding.encoder().encode(frame)

    XCTAssertEqual(
      String(decoding: encoded, as: UTF8.self),
      #"{"request":{"session_id":"11111111-1111-4111-8111-111111111111","type":"read_transcript"},"request_id":"7","version":5}"#
    )
  }

  func testCanonicalDecimalRejectsLeadingZeroes() {
    let encoded = Data(#""01""#.utf8)

    XCTAssertThrowsError(
      try SignalboxJSONCoding.decoder().decode(
        SignalboxCanonicalUInt64.self,
        from: encoded
      )
    )
  }

  func testClientRequestIdentityRejectsReservedZero() {
    let encoded = Data(#""0""#.utf8)

    XCTAssertThrowsError(
      try SignalboxJSONCoding.decoder().decode(
        SignalboxRequestID.self,
        from: encoded
      )
    )
  }

  func testKnownSessionEventDecodesItsTypedPayload() throws {
    let encoded = Data(
      """
      {
        "version":5,
        "request_id":"9",
        "message":{
          "type":"session_event",
          "cursor":"12",
          "session_id":"\(sessionID)",
          "event":{
            "type":"turn_activated",
            "turn_id":"\(turnID)",
            "current_attempt_id":"33333333-3333-4333-8333-333333333333"
          }
        }
      }
      """.utf8
    )

    let frame = try SignalboxJSONCoding.decoder().decode(
      SignalboxProcessServerFrame.self,
      from: encoded
    )

    XCTAssertEqual(
      frame.message,
      .sessionEvent(
        SignalboxFollowedSessionEvent(
          cursor: SignalboxCanonicalUInt64(rawValue: 12),
          sessionID: try SignalboxCanonicalUUID(validating: sessionID),
          event: .turnActivated(
            turnID: try SignalboxCanonicalUUID(validating: turnID),
            currentAttemptID: try SignalboxCanonicalUUID(
              validating: "33333333-3333-4333-8333-333333333333"
            )
          )
        )
      )
    )
  }

  func testUnknownSessionEventDoesNotDiscardItsFrame() throws {
    let encoded = Data(
      """
      {
        "version":5,
        "request_id":"9",
        "message":{
          "type":"session_event",
          "cursor":"12",
          "session_id":"\(sessionID)",
          "event":{"type":"future_transition","field":"retained"}
        }
      }
      """.utf8
    )

    let frame = try SignalboxJSONCoding.decoder().decode(
      SignalboxProcessServerFrame.self,
      from: encoded
    )

    XCTAssertEqual(
      frame.message,
      .sessionEvent(
        SignalboxFollowedSessionEvent(
          cursor: SignalboxCanonicalUInt64(rawValue: 12),
          sessionID: try SignalboxCanonicalUUID(validating: sessionID),
          event: .unknown(
            kind: "future_transition",
            payload: [
              "type": .string("future_transition"),
              "field": .string("retained"),
            ],
            decodingDiagnostic: nil
          )
        )
      )
    )
  }

  func testMalformedKnownMessageDegradesWithDiagnostic() throws {
    let encoded = Data(
      """
      {
        "version":5,
        "request_id":"9",
        "message":{"type":"session_created","session_id":17}
      }
      """.utf8
    )

    let frame = try SignalboxJSONCoding.decoder().decode(
      SignalboxProcessServerFrame.self,
      from: encoded
    )

    XCTAssertEqual(
      frame.message,
      .unknown(
        kind: "session_created",
        payload: [
          "type": .string("session_created"),
          "session_id": .number(17),
        ],
        decodingDiagnostic: SignalboxDecodingDiagnostic(
          message: "Unexpected field type at message.session_id."
        )
      )
    )
  }
}
