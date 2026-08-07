#if os(iOS)
import SnapshotTesting
import XCTest

@testable import SignalboxNative

/// The record mode decides whether a run asserts against the goldens or
/// rewrites them, so it is held to its own tests rather than trusted because
/// the recording script happens to pass the right string.
///
/// The safety-critical case is the one with no configuration at all: that is
/// every ordinary run, including CI's blocking step, and a regression turning
/// it into `.all` would rewrite all ten references and report a pass.
final class LiveScreenSnapshotRecordModeTests: XCTestCase {
    func testAnAbsentVariableRecordsNothing() {
        XCTAssertEqual(liveScreenSnapshotRecordMode(requested: nil), .never)
    }

    func testAnEmptyVariableRecordsNothing() {
        XCTAssertEqual(liveScreenSnapshotRecordMode(requested: ""), .never)
    }

    /// Each mode the README and the recording script document, so the two
    /// vocabularies cannot drift apart without a failure here.
    func testEveryDocumentedModeIsAccepted() {
        let documented: [(String, SnapshotTestingConfiguration.Record)] = [
            ("all", .all),
            ("missing", .missing),
            ("failed", .failed),
            ("never", .never),
        ]

        for (spelling, expected) in documented {
            XCTAssertEqual(
                liveScreenSnapshotRecordMode(requested: spelling),
                expected,
                "\(spelling) is documented but was not accepted"
            )
        }
    }

    /// An unknown value is reported rather than absorbed: a typo that silently
    /// fell back to `.never` would look exactly like a correct verification run,
    /// and a typo that silently fell back to recording would be worse.
    func testAnUnknownValueIsReportedAndRecordsNothing() {
        XCTExpectFailure("an unknown record mode is reported to the runner")

        XCTAssertEqual(liveScreenSnapshotRecordMode(requested: "yes"), .never)
    }
}
#endif
