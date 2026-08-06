#if os(iOS)
import SwiftUI
import XCTest

@testable import SignalboxNative

/// The settle gate is what makes an in-process golden reproducible, so it is
/// held to its own tests rather than trusted because the goldens happen to
/// pass.
@MainActor
final class LiveScreenRendererTests: XCTestCase {
    /// The load lands after `minimumSettle`, so only the unchanged-rendering
    /// gate can catch it; a renderer that merely waited a fixed minimum would
    /// still return the pre-load frame.
    func testRenderingWaitsForContentThatLoadsAfterTheMinimumSettle() async {
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

    func testRenderingFailsRatherThanReturnAFrameOfAScreenThatNeverSettles() async {
        XCTExpectFailure("a screen that never stops changing has no frame a golden can name")

        _ = await LiveScreenRenderer.render(
            ContinuouslyChangingView(),
            canvas: .compact,
            timeout: .milliseconds(600)
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
