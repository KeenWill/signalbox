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

    /// Stated here for the same reason the scale is: a dynamic color resolves
    /// against the trait collection of whatever it is set on, so an inherited
    /// interface style would record a different golden on a dark-mode host.
    static let userInterfaceStyle = UIUserInterfaceStyle.light

    /// The canvas is the whole safe area.
    ///
    /// A window takes its insets from the scene's device, and navigation and
    /// tab chrome lay out against them, so pinning the size without pinning
    /// these pinned half the geometry: every golden here failed on an iPhone
    /// 17e after being recorded on an iPhone 17 Pro. Zero rather than some
    /// device's numbers, because a fixed canvas that is not a device has no
    /// cutout and no home indicator to reserve room for — the same reason
    /// there is no scene lifecycle or window chrome in these renderings.
    static let safeAreaInsets = UIEdgeInsets.zero

    /// Increased Contrast is a simulator setting, not a device model, so it
    /// survives choosing the right simulator. It rewrites the system colors
    /// and the materials, and it is pinned on the windows as well as on the
    /// content because the backdrop resolves a system color too.
    static let accessibilityContrast = UIAccessibilityContrast.normal

    /// Overrides every remaining trait a golden's pixels depend on. Interface
    /// style decides its colors and the content-size category its text
    /// metrics; both otherwise follow whatever the host application inherited.
    /// Bold Text and Increased Contrast are pinned for the same reason and are
    /// worse, being device state rather than device geometry: a run on the
    /// pinned simulator model would still record a different golden with either
    /// switched on.
    func overrideTraits(on controller: UIViewController) {
        controller.traitOverrides.horizontalSizeClass = horizontalSizeClass
        controller.traitOverrides.verticalSizeClass = .regular
        controller.traitOverrides.userInterfaceStyle = Self.userInterfaceStyle
        controller.traitOverrides.displayScale = displayScale
        controller.traitOverrides.layoutDirection = .leftToRight
        controller.traitOverrides.preferredContentSizeCategory = .large
        controller.traitOverrides.legibilityWeight = .regular
        controller.traitOverrides.accessibilityContrast = Self.accessibilityContrast
    }

    /// Pins the appearance of a window whose pixels reach a golden.
    ///
    /// The traits above reach the hosted content and nothing above it, so the
    /// windows are pinned separately: both fill themselves with the dynamic
    /// `.systemBackground`, and the backdrop's resolution is what the
    /// navigation chrome's glass materials sample. Only the interface style is
    /// pinned here, along with the contrast that decides which system color it
    /// resolves to.
    func overrideTraits(on window: UIWindow) {
        window.overrideUserInterfaceStyle = Self.userInterfaceStyle
        window.traitOverrides.accessibilityContrast = Self.accessibilityContrast
    }

    /// Pins the safe area the hosted content lays out against.
    ///
    /// A safe area is geometry rather than a trait, so no override reaches it.
    /// `additionalSafeAreaInsets` is added to what a controller inherits, and
    /// the window's own insets are what it inherits, so the difference between
    /// the two is what leaves the content seeing exactly the canvas's. It is
    /// set on the host rather than on the hosted controller because a child's
    /// safe area derives from its parent's.
    func pinSafeArea(of controller: UIViewController, in window: UIWindow) {
        // Read after a layout pass: a window resolves its insets from the
        // scene, and an unlaid window reports zero for all of them, which would
        // silently make this a no-op on exactly the devices it exists for.
        window.layoutIfNeeded()
        let inherited = window.safeAreaInsets
        controller.additionalSafeAreaInsets = UIEdgeInsets(
            top: Self.safeAreaInsets.top - inherited.top,
            left: Self.safeAreaInsets.left - inherited.left,
            bottom: Self.safeAreaInsets.bottom - inherited.bottom,
            right: Self.safeAreaInsets.right - inherited.right
        )
        window.layoutIfNeeded()
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
///
/// The second cost is the destination, and it is bounded rather than absent.
/// Everything a layout resolves against is pinned below — size, scale, size
/// class, interface style, content-size category, layout direction, and safe
/// area — and with those pinned, nine of the ten goldens were verified
/// byte-identical across three different iPhone simulators. The tenth is the
/// regular canvas, which is wider than a phone screen: the window's corner mask
/// and the glass materials composite against the device there, so that one
/// golden still resolves differently on a different phone. CI pins its
/// simulator, and `scripts/test-xcode.sh` resolves the newest one locally, so
/// the suite is reproducible where it runs; a destination that is not CI's can
/// legitimately fail that golden alone.
@MainActor
enum LiveScreenRenderer {
    /// The renderer re-renders on this interval while waiting for the screen
    /// to stop changing.
    nonisolated static let settleInterval = Duration.milliseconds(50)

    /// A screen must have rendered identically for at least this long, measured
    /// from the last frame that differed, before it is accepted as settled.
    /// Screens load through the in-memory harness after they appear, and two
    /// renderings of the same not-yet-populated screen are identical, so an
    /// unqualified first match would accept the frame before the first response
    /// arrives.
    ///
    /// A quarter second satisfied that and was still too short. The glass bar
    /// over a scrolling list has two stable renderings — one before the list
    /// behind it finishes laying out and one after — and at 250ms a run
    /// reached each about half the time, differing in 0.014% of pixels: enough
    /// to fail, far too little to loosen the tolerance for. A second is past
    /// that transition on every run measured. A screen that is genuinely still
    /// changing is caught by the timeout below, not by this floor.
    ///
    /// This floor is also the gate's horizon, and stating it is the honest
    /// cost: content that first arrives *after* it is indistinguishable from
    /// content that never arrives, because two renderings of a screen that has
    /// not started loading match exactly as well as two renderings of a
    /// finished one. No purely temporal gate can separate them; only a screen
    /// that reported its own readiness could, and that would be a readiness
    /// protocol through every screen's view model rather than a property of
    /// this renderer. Every scenario here is served by the in-memory harness,
    /// which answers without a scheduled delay, so each one settles well inside
    /// the floor; a scenario that did not would need that protocol first.
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
        //
        // Restored rather than left set: this is process-wide, the test bundle
        // runs inside the host application, and only the snapshot suite is
        // excluded from the blocking workflow. A renderer that left animations
        // off would make every later test in the process depend on whether it
        // had run first.
        let animationsWereEnabled = UIView.areAnimationsEnabled
        UIView.setAnimationsEnabled(false)
        defer { UIView.setAnimationsEnabled(animationsWereEnabled) }

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
        canvas.overrideTraits(on: backdrop)
        backdrop.frame = CGRect(origin: .zero, size: canvas.size).insetBy(dx: -200, dy: -200)
        backdrop.backgroundColor = .systemBackground
        backdrop.isOpaque = true
        backdrop.windowLevel = .normal + 1
        backdrop.isHidden = false
        let window = UIWindow(windowScene: scene)
        canvas.overrideTraits(on: window)
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
        // After the frame, because a window's insets are resolved against the
        // frame it actually occupies.
        canvas.pinSafeArea(of: host, in: window)
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
        // How long the rendering has been identical, reset by any change, not
        // how long the screen has been on the air. Gating on total elapsed time
        // and adjacent-frame equality would accept a screen that changed just
        // past the floor and then paused for a single interval: two matching
        // frames 50ms apart, which is a transient state, not a settled one.
        // Measured from the last change, a frame is returned only after the
        // screen has held it for the whole floor. A screen that never changes
        // is unchanged from the first sample, so the floor still bounds the
        // earliest possible return and the horizon documented on it holds.
        var unchanged = Duration.zero
        while elapsed < timeout {
            try? await Task.sleep(for: settleInterval)
            elapsed += settleInterval
            let current = rendering(of: window, canvas: canvas)
            unchanged = pixels(of: current) == pixels(of: previous)
                ? unchanged + settleInterval
                : .zero
            if unchanged >= minimumSettle {
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
    liveScreenSnapshotRecordMode(
        requested: ProcessInfo.processInfo.environment["SIGNALBOX_NATIVE_SNAPSHOT_RECORD"]
    )
}

/// The same decision over an explicit value, which is what makes it testable.
///
/// The environment read is the only part left in the caller above, because the
/// branch that matters is not "was the variable set" but "what is returned when
/// it was not": every ordinary test run takes the `.never` path, and a
/// regression that turned it into `.all` would rewrite every golden and report
/// a pass. That default and the rejection of an unknown value are both pinned
/// by `LiveScreenSnapshotRecordModeTests`, which a shell `case` cannot reach.
func liveScreenSnapshotRecordMode(
    requested: String?
) -> SnapshotTestingConfiguration.Record {
    guard let requested, !requested.isEmpty else {
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
