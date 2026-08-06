#if os(iOS)
import Foundation
import SnapshotTesting
import SwiftUI
import UIKit
import XCTest

@testable import SignalboxNative

/// The canvas a live screen is rendered into.
///
/// Fixed sizes, not device configurations: the layouts branch on the
/// horizontal size class alone, so a device matrix would re-record every
/// golden whenever the resolved simulator changes while testing nothing the
/// size class does not already separate.
enum SnapshotCanvas: String {
    case compact
    case regular
    /// A sheet is not a screen: its content declares its own minimum size, and
    /// rendering it on a phone-width canvas would record it clipped to a width
    /// no presentation gives it.
    case sheet

    var size: CGSize {
        switch self {
        case .compact:
            return CGSize(width: 390, height: 844)
        case .regular:
            return CGSize(width: 1024, height: 768)
        case .sheet:
            return CGSize(width: 540, height: 620)
        }
    }

    /// Stated here rather than taken from the simulator the run resolved: the
    /// scale decides a golden's pixel dimensions, so an unpinned one re-records
    /// the suite whenever a 2x device replaces a 3x one. A scale of 2 shows the
    /// same layout and typography a 3x rendering would while keeping each
    /// golden a little over half the bytes.
    var displayScale: CGFloat { 2 }

    /// Overrides every remaining trait a golden's pixels depend on. Interface
    /// style decides its colors and the content-size category its text
    /// metrics; both otherwise follow whatever the host application inherited.
    func overrideTraits(on controller: UIViewController) {
        controller.traitOverrides.horizontalSizeClass = horizontalSizeClass
        controller.traitOverrides.verticalSizeClass = .regular
        controller.traitOverrides.userInterfaceStyle = .light
        controller.traitOverrides.displayScale = displayScale
        controller.traitOverrides.layoutDirection = .leftToRight
        controller.traitOverrides.preferredContentSizeCategory = .large
    }

    private var horizontalSizeClass: UIUserInterfaceSizeClass {
        switch self {
        case .compact, .sheet:
            return .compact
        case .regular:
            return .regular
        }
    }
}

/// Renders a live screen in process, without running the application.
///
/// The accepted cost is fidelity. This hosts one screen in one window; a
/// running application is what owns scene lifecycle, window chrome, and sheet
/// presentation, so none of those reach a golden here. Sheet content is
/// snapshotted as its own standalone screen rather than composited onto a
/// parent, which is why no presentation seam appears in this file.
@MainActor
enum LiveScreenRenderer {
    /// The renderer re-renders on this interval while waiting for the screen
    /// to stop changing.
    nonisolated static let settleInterval = Duration.milliseconds(50)

    /// A screen must be unchanged across two renderings taken at least this
    /// far apart before it is accepted as settled. Screens load through the
    /// in-memory harness after they appear, and two renderings of the same
    /// not-yet-populated screen are identical, so an unqualified first match
    /// would accept the frame before the first response arrives.
    ///
    /// A quarter second satisfied that and was still too short. The glass bar
    /// over a scrolling list has two stable renderings — one before the list
    /// behind it finishes laying out and one after — and at 250ms a run
    /// reached each about half the time, differing in 0.014% of pixels: enough
    /// to fail, far too little to loosen the tolerance for. A second is past
    /// that transition on every run measured. A screen that is genuinely still
    /// changing is caught by the timeout below, not by this floor.
    nonisolated static let minimumSettle = Duration.milliseconds(1000)

    /// A screen still changing after this long is reported as a failure rather
    /// than snapshotted. Anything rendering a continuous animation reaches this
    /// bound, and a golden of one arbitrary frame of it would fail on its own
    /// next run.
    nonisolated static let settleTimeout = Duration.seconds(5)

    /// Renders `view` once its rendering has stopped changing.
    static func render(
        _ view: some View,
        canvas: SnapshotCanvas,
        timeout: Duration = settleTimeout,
        file: StaticString = #filePath,
        line: UInt = #line
    ) async -> UIImage {
        // A scenario that selects a session pushes a navigation destination,
        // which animates through intermediate frames unless this is off. Core
        // Animation's clock is stopped below as well, so a layer animation that
        // survives this holds one frame instead of never settling.
        UIView.setAnimationsEnabled(false)

        let content = UIHostingController(rootView: view)
        let host = UIViewController()
        let scene = hostWindowScene()
        // The glass materials the navigation chrome uses sample the backdrop
        // behind the window they are in, and behind a window in a test process
        // is the host application's own. This opaque window sits between the
        // two, above the host application and below the canvas, so what a
        // material samples is a flat color instead of a blurred ghost of
        // whatever the host happened to be showing. It extends past the canvas
        // by more than a blur radius on every side.
        let backdrop = UIWindow(windowScene: scene)
        backdrop.frame = CGRect(origin: .zero, size: canvas.size).insetBy(dx: -200, dy: -200)
        backdrop.backgroundColor = .systemBackground
        backdrop.isOpaque = true
        backdrop.windowLevel = .normal + 1
        backdrop.isHidden = false
        let window = UIWindow(windowScene: scene)
        window.windowLevel = .normal + 2
        canvas.overrideTraits(on: content)
        host.view.backgroundColor = .systemBackground
        host.addChild(content)
        host.view.addSubview(content.view)
        content.didMove(toParent: host)
        window.rootViewController = host
        window.makeKeyAndVisible()
        // After the window is attached and keyed: a scene sizes a new window to
        // its own bounds, and the canvas is the size a golden is recorded at.
        window.frame = CGRect(origin: .zero, size: canvas.size)
        host.view.frame = window.bounds
        content.view.frame = window.bounds
        content.view.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        window.layer.speed = 0
        window.layer.timeOffset = 0
        defer {
            // Detached from the scene, not merely hidden: a window left in the
            // scene stays in the next render's backdrop, which would make a
            // golden depend on the order its suite ran in.
            window.isHidden = true
            window.rootViewController = nil
            window.windowScene = nil
            backdrop.isHidden = true
            backdrop.windowScene = nil
        }

        var previous = rendering(of: window, canvas: canvas)
        var elapsed = Duration.zero
        while elapsed < timeout {
            try? await Task.sleep(for: settleInterval)
            elapsed += settleInterval
            let current = rendering(of: window, canvas: canvas)
            if elapsed >= minimumSettle, pixels(of: current) == pixels(of: previous) {
                return current
            }
            previous = current
        }

        XCTFail(
            "The screen was still changing after \(timeout); no frame of it is a golden.",
            file: file,
            line: line
        )
        return previous
    }

    private static func rendering(of window: UIWindow, canvas: SnapshotCanvas) -> UIImage {
        // The pixel format is stated rather than derived from the trait
        // collection: an extended color range would make a golden depend on the
        // host's display capabilities.
        let format = UIGraphicsImageRendererFormat()
        format.scale = canvas.displayScale
        format.opaque = true
        format.preferredRange = .standard
        let renderer = UIGraphicsImageRenderer(bounds: window.bounds, format: format)
        // Draws the composited hierarchy rather than rendering the layer tree.
        // Rendering the layer tree is the isolated option and was tried first,
        // but a list's content draws through a compositing path it does not
        // reproduce, leaving the whole list black. Drawing the composite needs
        // the backdrop window below to be deterministic; see `render`.
        return renderer.image { _ in
            window.drawHierarchy(in: window.bounds, afterScreenUpdates: true)
        }
    }

    private static func pixels(of image: UIImage) -> Data? {
        image.cgImage?.dataProvider?.data as Data?
    }

    /// The test bundle runs inside the host application, so the screen being
    /// rendered belongs to that application's scene; there is no case in which
    /// it is absent.
    private static func hostWindowScene() -> UIWindowScene {
        guard
            let scene = UIApplication.shared.connectedScenes
                .compactMap({ $0 as? UIWindowScene })
                .first
        else {
            preconditionFailure("The snapshot host application has no window scene.")
        }
        return scene
    }
}

/// Asserts a live screen against its committed golden.
///
/// `precision: 1` holds every pixel to the comparison; `perceptualPrecision:
/// 0.98` is the tolerance each pixel is held to, which is roughly the
/// precision of the human eye. Together they absorb the sub-pixel
/// anti-aliasing drift a text rasterizer produces between runs while still
/// failing on a color, metric, or layout change a reader would see. A single
/// tolerance at 1.0 fails on drift no reader can see; a pixel precision below
/// 1.0 would instead let a small region change entirely.
@MainActor
func assertLiveScreenSnapshot(
    of view: some View,
    canvas: SnapshotCanvas,
    fileID: StaticString = #fileID,
    file: StaticString = #filePath,
    testName: String = #function,
    line: UInt = #line
) async {
    let rendering = await LiveScreenRenderer.render(view, canvas: canvas, file: file, line: line)
    // The record mode is passed per assertion rather than installed around the
    // suite: `withSnapshotTesting` carries it in a task local, and XCTest runs
    // an async test body in a task that does not inherit one.
    assertSnapshot(
        of: rendering,
        as: .image(
            precision: 1,
            perceptualPrecision: 0.98,
            scale: canvas.displayScale
        ),
        named: canvas.rawValue,
        record: liveScreenSnapshotRecordMode(),
        fileID: fileID,
        file: file,
        testName: testName,
        line: line
    )
}

/// The record mode the suite runs under.
///
/// `never` by default so a missing golden fails instead of being written and
/// silently passing on a retry. `SIGNALBOX_NATIVE_SNAPSHOT_RECORD` takes the
/// library's own vocabulary — `all`, `missing`, `failed`, `never` — rather
/// than a second one that would have to be kept in step with it.
func liveScreenSnapshotRecordMode() -> SnapshotTestingConfiguration.Record {
    guard
        let requested = ProcessInfo.processInfo.environment["SIGNALBOX_NATIVE_SNAPSHOT_RECORD"],
        !requested.isEmpty
    else {
        return .never
    }
    guard let mode = SnapshotTestingConfiguration.Record(rawValue: requested) else {
        XCTFail(
            """
            SIGNALBOX_NATIVE_SNAPSHOT_RECORD was "\(requested)"; \
            it takes all, missing, failed, or never.
            """
        )
        return .never
    }
    return mode
}
#endif
