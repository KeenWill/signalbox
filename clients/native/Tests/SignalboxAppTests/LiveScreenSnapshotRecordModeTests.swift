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

    /// Every mode the README and the recording script document, spelled out one
    /// per line so each is verifiable by reading it. If either document grows a
    /// fifth spelling, this test is where it fails to be accepted.
    func testEveryDocumentedModeIsAccepted() {
        XCTAssertEqual(liveScreenSnapshotRecordMode(requested: "all"), .all)
        XCTAssertEqual(liveScreenSnapshotRecordMode(requested: "missing"), .missing)
        XCTAssertEqual(liveScreenSnapshotRecordMode(requested: "failed"), .failed)
        XCTAssertEqual(liveScreenSnapshotRecordMode(requested: "never"), .never)
    }

    /// An unknown value is reported *and* records nothing, and the two are
    /// asserted separately on purpose.
    ///
    /// The matcher is what keeps them separate. An unfiltered `XCTExpectFailure`
    /// absorbs any failure in its scope, including the equality assertion's own
    /// — so a regression that stopped reporting the diagnostic and returned
    /// `.all` would satisfy the expectation with the assertion it broke, and
    /// this test would pass while permitting exactly the unsafe recording it
    /// exists to reject. Only the diagnostic is expected here; the returned mode
    /// is checked by an ordinary assertion outside the block.
    func testAnUnknownValueIsReportedAndRecordsNothing() {
        var mode: SnapshotTestingConfiguration.Record?

        XCTExpectFailure("an unknown record mode is reported to the runner") {
            mode = liveScreenSnapshotRecordMode(requested: "yes")
        } issueMatcher: { issue in
            issue.compactDescription.contains("it takes all, missing, failed, or never")
        }

        XCTAssertEqual(mode, .never)
    }
}
#endif
