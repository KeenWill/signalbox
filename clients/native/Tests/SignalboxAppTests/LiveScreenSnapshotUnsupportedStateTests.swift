#if os(iOS)
import XCTest

@testable import SignalboxNative

/// Reduce Transparency is refused rather than pinned, so the refusal is held to
/// its own tests rather than trusted because every simulator measured so far
/// happened to have it off.
///
/// The decision is the whole guard: `assertLiveScreenSnapshot` reads
/// `UIAccessibility.isReduceTransparencyEnabled` and does what this function
/// says. A test cannot set that global — it is get-only, which is the same
/// reason the state cannot be pinned in the first place — so the boolean is
/// passed explicitly here and the branch that would otherwise only run on a
/// machine nobody has is the one these assertions reach.
final class LiveScreenSnapshotUnsupportedStateTests: XCTestCase {
    func testASimulatorWithoutReduceTransparencyIsSupported() {
        XCTAssertNil(liveScreenSnapshotUnsupportedState(reduceTransparency: false))
    }

    func testReduceTransparencyIsRefused() {
        XCTAssertNotNil(liveScreenSnapshotUnsupportedState(reduceTransparency: true))
    }

    /// The diagnostic has to name the setting and where to turn it off, because
    /// it is the whole remedy a reader gets: the run stops without rendering,
    /// so there is no exported difference image to look at.
    func testTheRefusalNamesTheSettingAndItsRemedy() {
        let diagnostic = liveScreenSnapshotUnsupportedState(reduceTransparency: true) ?? ""

        XCTAssertTrue(diagnostic.contains("Reduce Transparency"))
        XCTAssertTrue(diagnostic.contains("Accessibility"))
    }
}
#endif
