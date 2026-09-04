#if os(iOS)
import SwiftUI
import XCTest

@testable import SignalboxNative

/// The settle gate is what makes an in-process golden reproducible, so it is
/// held to its own tests rather than trusted because the goldens happen to
/// pass.
@MainActor
final class LiveScreenRendererTests: XCTestCase {
    /// The screen is unchanged for the first 400ms and then loads, so the two
    /// renderings a first match would accept exist long before the content
    /// does. Only the floor outlasts them, which is what this pins: a renderer
    /// that returned on the first pair of identical renderings would return the
    /// pre-load frame.
    func testRenderingWaitsPastAFirstStableMatchForContentThatIsStillLoading() async {
        let loadsLate = await LiveScreenRenderer.render(
            DelayedContentView(before: .red, after: .blue, delay: .milliseconds(400)),
            canvas: .iPhonePortrait
        )
        let loadedThroughout = await LiveScreenRenderer.render(
            DelayedContentView(before: .blue, after: .blue, delay: .milliseconds(400)),
            canvas: .iPhonePortrait
        )

        XCTAssertEqual(
            loadsLate.pngData(),
            loadedThroughout.pngData(),
            "the renderer returned a frame from before the screen finished loading"
        )
    }

    /// A screen still moving when total elapsed time passes the floor. Gating on
    /// elapsed time plus adjacent-frame equality accepts it: two renderings one
    /// sampling interval apart match anywhere inside a hold, and mid-motion is
    /// not settled. Measuring the floor from the last frame that differed
    /// rejects every one of those, so what this pins is that the frame returned
    /// is the one the screen came to rest on, not a particular timing.
    func testRenderingRejectsAFrameTakenWhileTheScreenIsStillMoving() async {
        let stillMoving = await LiveScreenRenderer.render(
            SteppingThenRestingView(),
            canvas: .iPhonePortrait,
            timeout: .seconds(20)
        )
        let atRest = await LiveScreenRenderer.render(
            RestingView(),
            canvas: .iPhonePortrait,
            timeout: .seconds(20)
        )

        XCTAssertEqual(
            stillMoving.pngData(),
            atRest.pngData(),
            "the renderer returned a frame from before the screen stopped moving"
        )
    }

    /// The counterpart: the floor is a floor and not the whole gate. The
    /// timeout has to exceed `minimumSettle`, or no rendering is ever eligible
    /// and the failure this expects would be reported for a static screen too,
    /// leaving a renderer that stopped comparing pixels indistinguishable from
    /// one that still does.
    func testRenderingFailsRatherThanReturnAFrameOfAScreenThatNeverSettles() async {
        XCTExpectFailure("a screen that never stops changing has no frame a golden can name")

        _ = await LiveScreenRenderer.render(
            ContinuouslyChangingView(),
            canvas: .iPhonePortrait,
            timeout: LiveScreenRenderer.minimumSettle + .milliseconds(500)
        )
    }
}

/// Fills the canvas with `before`, then with `after` once `delay` has passed.
private struct DelayedContentView: View {
    let before: Color
    let after: Color
    let delay: Duration
    @State private var loaded = false

    var body: some View {
        (loaded ? after : before)
            .task {
                try? await Task.sleep(for: delay)
                loaded = true
            }
    }
}

/// Moves a bar down the canvas, one point per tick, so no two renderings the
/// settle gate compares can match by coincidence.
///
/// Monotone rather than cyclic, and that is the point: a shade that wrapped
/// every twentieth tick could return to an earlier value between two samples
/// taken `settleInterval` apart, the gate would accept it, and the expected
/// failure would go unrecorded on whichever run the scheduler happened to
/// delay. A position that only ever increases cannot come back within the
/// canvas, which is far longer than any timeout this suite passes.
private struct ContinuouslyChangingView: View {
    @State private var tick = 0

    var body: some View {
        Color.black
            .frame(height: 8)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
            .offset(y: Double(tick))
            .task {
                while !Task.isCancelled {
                    try? await Task.sleep(for: .milliseconds(20))
                    tick += 1
                }
            }
    }
}

/// Steps a bar down the canvas in holds far shorter than the settle floor, for
/// long enough that the total elapsed time passes the floor mid-motion, then
/// stops for good at `restingOffset`.
///
/// Every hold is deliberately below the floor. A hold that outlasted the floor
/// would be a settled state, and a correct renderer returning it would fail
/// this test for being right. Each hold here is long enough to span consecutive
/// samples, which is all the behaviour under test needs: a gate reading total
/// elapsed time finds two matching renderings mid-motion and returns one, and a
/// gate measuring from the last change cannot, because the bar keeps moving.
private struct SteppingThenRestingView: View {
    static let restingOffset = 15
    @State private var tick = 0

    var body: some View {
        Color.black
            .frame(height: 8)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
            .offset(y: Double(tick))
            .task {
                while tick < Self.restingOffset {
                    try? await Task.sleep(for: .milliseconds(200))
                    tick += 1
                }
            }
    }
}

/// The same bar, already at rest where `SteppingThenRestingView` ends up.
private struct RestingView: View {
    var body: some View {
        Color.black
            .frame(height: 8)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
            .offset(y: Double(SteppingThenRestingView.restingOffset))
    }
}
#endif
