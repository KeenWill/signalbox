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
            canvas: .compact
        )
        let loadedThroughout = await LiveScreenRenderer.render(
            DelayedContentView(before: .blue, after: .blue, delay: .milliseconds(400)),
            canvas: .compact
        )

        XCTAssertEqual(
            loadsLate.pngData(),
            loadedThroughout.pngData(),
            "the renderer returned a frame from before the screen finished loading"
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
            canvas: .compact,
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

/// Fills the canvas with a shade that never repeats, so no two renderings the
/// settle gate compares can match by coincidence.
private struct ContinuouslyChangingView: View {
    @State private var shade = 0.0

    var body: some View {
        Color(white: shade)
            .task {
                while !Task.isCancelled {
                    try? await Task.sleep(for: .milliseconds(20))
                    shade = (shade + 0.05).truncatingRemainder(dividingBy: 1)
                }
            }
    }
}
#endif
