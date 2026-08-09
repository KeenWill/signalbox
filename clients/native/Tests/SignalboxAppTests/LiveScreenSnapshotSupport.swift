#if os(iOS)
import Foundation
import SnapshotTesting
import SwiftUI
import UIKit
import XCTest

@testable import SignalboxNative

/// The canvas a live screen is rendered into.
///
/// Fixed sizes, not device configurations. Every canvas here is rendered on the
/// one simulator `scripts/lib/snapshots.sh` pins, so a canvas is a parameter of
/// the rendering rather than a second device to resolve: adding one costs a
/// golden per screen and no new destination, and the cross-device caveat
/// documented on `LiveScreenRenderer` is a property of changing the simulator,
/// not of changing the canvas.
///
/// The raw value is the suffix in a golden's file name, so it says the form
/// factor rather than the size class: a reader — or a model — browsing
/// `__Snapshots__` sees a name ending `.ipad-landscape.png` and needs nothing
/// else to know what it is looking at. The size class each canvas resolves to
/// is stated in the extension below, where it is a rendering input rather than
/// a label.
///
/// The geometry — this declaration and `size` — deliberately names no UIKit
/// type, and everything that does is in the extension below it. A macOS
/// destination can reuse this half and supply its own pinning in place of that
/// extension; that destination is not this suite's, and the note on
/// `LiveScreenRenderer` says what else it would need.
enum SnapshotCanvas: String, CaseIterable {
    case iPhonePortrait = "iphone-portrait"
    case iPhoneLandscape = "iphone-landscape"
    case iPadPortrait = "ipad-portrait"
    case iPadLandscape = "ipad-landscape"
    /// A sheet is not a screen: its content declares its own minimum size, and
    /// rendering it on a phone-width canvas would record it clipped to a width
    /// no presentation gives it.
    case sheet

    var size: CGSize {
        switch self {
        case .iPhonePortrait:
            return CGSize(width: 390, height: 844)
        case .iPhoneLandscape:
            return CGSize(width: 844, height: 390)
        case .iPadPortrait:
            return CGSize(width: 1024, height: 1366)
        case .iPadLandscape:
            return CGSize(width: 1366, height: 1024)
        case .sheet:
            return CGSize(width: 540, height: 620)
        }
    }

    /// Stated here rather than taken from the simulator the run resolved: the
    /// scale decides a golden's pixel dimensions, so an unpinned one re-records
    /// the suite whenever a 2x device replaces a 3x one. A scale of 2 shows the
    /// same layout and typography a 3x rendering would while keeping each
    /// golden a little over half the bytes, and it is the scale every shipping
    /// iPhone and iPad renders at, so no canvas here records hairlines or asset
    /// variants that no device produces.
    var displayScale: CGFloat { 2 }
}

/// Everything a canvas pins that only UIKit can express.
///
/// Split from the declaration above so the geometry stays portable; see the
/// note there. Nothing in this extension is reusable by an AppKit destination,
/// and that is the whole reason for the seam.
extension SnapshotCanvas {
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
        controller.traitOverrides.userInterfaceIdiom = userInterfaceIdiom
        controller.traitOverrides.horizontalSizeClass = horizontalSizeClass
        controller.traitOverrides.verticalSizeClass = verticalSizeClass
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

    /// The idiom each canvas renders as, pinned for the same reason the size
    /// classes are and against a stronger default.
    ///
    /// A trait override rather than a resolved value: this suite runs in one
    /// iPhone scene, so without this every canvas inherits `.phone` and the two
    /// named for an iPad record phone behaviour at iPad dimensions. No view in
    /// the application reads the idiom — it is the whole of
    /// `grep -rn userInterfaceIdiom clients/native/Sources` — but the framework
    /// containers those views are built from do: `RootView`'s
    /// `NavigationSplitView` and every `.sheet` presentation adapt by idiom as
    /// well as by size class, and a presented sheet is the case this suite
    /// actually records, in `testSessionListPresentingTheCreationSheet`.
    /// Without the override those goldens would accept a regression confined to
    /// the real pad presentation.
    ///
    /// What it does not buy is a real iPad. The trait is what the hosted
    /// content and its presentations resolve against, which is what decides
    /// these renderings; a device idiom read from `UIDevice` is not, and
    /// nothing here reads one. The remaining destination dependence is the
    /// window's corner mask and the glass materials, already stated on
    /// `LiveScreenRenderer`, and rendering on an iPad destination is what would
    /// close that rather than this.
    private var userInterfaceIdiom: UIUserInterfaceIdiom {
        switch self {
        case .iPhonePortrait, .iPhoneLandscape, .sheet:
            return .phone
        case .iPadPortrait, .iPadLandscape:
            return .pad
        }
    }

    /// The size class each canvas resolves to, stated rather than derived from
    /// its width. UIKit resolves a size class from the window a scene owns, and
    /// these windows are canvas-sized rather than device-sized, so a resolved
    /// one would follow the simulator this suite exists to be independent of.
    ///
    /// These are the classes the same geometry carries on a device: a phone is
    /// horizontally compact in both orientations and vertically compact only in
    /// landscape, and an iPad is regular in both directions either way. `RootView`
    /// is the only view in the application that reads a size class — it is
    /// the whole of `grep -rn horizontalSizeClass clients/native/Sources` — so
    /// the horizontal one is what separates its tab bar from its split view,
    /// and every other difference across these canvases is reflow at a
    /// different width and height.
    private var horizontalSizeClass: UIUserInterfaceSizeClass {
        switch self {
        case .iPhonePortrait, .iPhoneLandscape, .sheet:
            return .compact
        case .iPadPortrait, .iPadLandscape:
            return .regular
        }
    }

    /// The vertical class, and the reason a landscape canvas is a landscape
    /// reference without the scene being rotated.
    ///
    /// `LiveScreenRenderer` hosts every canvas in the one portrait scene the
    /// test bundle's host application owns, and nothing requests a geometry
    /// update, so `interfaceOrientation` stays portrait for all five. The fair
    /// worry is that the two landscape canvases then record portrait
    /// presentation behaviour at landscape dimensions, which would make their
    /// goldens accept a regression confined to real landscape.
    ///
    /// Measured rather than assumed, on the case that would show it first. A
    /// presented sheet is the most orientation-sensitive thing this suite
    /// renders, and `testSessionListPresentingTheCreationSheet` records it on
    /// `.iPhoneLandscape` as a full-screen sheet 844 points wide — which is
    /// what a vertically compact presentation does, and is not what the
    /// portrait scene would give it. Presentation geometry follows the canvas
    /// and these overrides; the scene's orientation does not reach it. The
    /// portrait canvas of that same test is the control: it records the form
    /// clipped at 390 points, so the two differ by the canvas rather than by
    /// the scene they share.
    ///
    /// What stays outside that measurement is anything reading the scene
    /// directly. Nothing in the application does — `interfaceOrientation` and
    /// `UIDevice` appear nowhere under `clients/native/Sources` — so the seam
    /// is between these traits and UIKit, which is where it was measured.
    private var verticalSizeClass: UIUserInterfaceSizeClass {
        switch self {
        case .iPhoneLandscape:
            return .compact
        case .iPhonePortrait, .iPadPortrait, .iPadLandscape, .sheet:
            return .regular
        }
    }
}

/// Renders a live screen in process, without running the application.
///
/// The accepted cost is fidelity. This hosts one screen in one window, and a
/// running application is what owns scene lifecycle and window chrome, so
/// neither of those reaches a golden here.
///
/// Sheet presentation is the exception, and it is supported rather than
/// absent. A screen that presents its own sheet does so into the canvas
/// window, and the presented controller is inside the hierarchy
/// `drawHierarchy` captures, so the sheet and the screen behind it both reach
/// the golden — `testSessionListPresentingTheCreationSheet` is that case, and
/// its references show the form over the dimmed list. What does not reach a
/// golden is a presentation the application never makes: nothing here drives a
/// sheet onto a screen that did not present one.
///
/// Snapshotting sheet content standalone on `SnapshotCanvas.sheet` is the
/// second way to record a sheet and not a correction of the first. It exists
/// for content whose declared minimum width the presenting canvas cannot give
/// it, where the presented rendering would record the form clipped; the two
/// tests for the creation sheet are the pair, and the note on each says which
/// question it answers.
///
/// The second cost is the destination, and it is bounded rather than absent.
/// Everything a layout resolves against is pinned below — size, scale, size
/// class, interface style, content-size category, layout direction, and safe
/// area — and with those pinned, a golden on the phone-sized canvases was
/// verified byte-identical across different iPhone simulators. A canvas wider
/// than the host phone's screen is the exception, and for one reason: the
/// window's corner mask and the glass materials composite against the device,
/// so every golden recorded on one still resolves differently on a different
/// phone. That is the two iPad canvases and the 540-point `sheet` canvas —
/// a width rule rather than an iPad rule, and stating it as the latter is what
/// left `*.sheet.png` unclassified in the fallback warning
/// `scripts/lib/snapshots.sh` prints. That is a property of changing the simulator and not of adding a
/// canvas — every canvas in one run renders on the one destination
/// `scripts/lib/snapshots.sh` pins — so the cost of the matrix is paid once, in
/// goldens that only CI's destination reproduces. CI pins that destination and
/// `scripts/test-snapshots.sh` resolves the same one locally, so the suite is
/// reproducible where it runs; a destination that is not CI's can legitimately
/// fail the iPad-canvas goldens alone.
///
/// The third cost is the platform, and it is refused rather than bounded. This
/// renderer is UIKit: it hosts in a `UIWindow`, pins `UITraitCollection`
/// overrides, and draws through `UIGraphicsImageRenderer`, none of which AppKit
/// has. `SnapshotCanvas`'s geometry is written to be reusable by a macOS
/// destination — the cases and their sizes name no UIKit type — but the pinning
/// and the drawing are not, and a macOS golden needs its own renderer and its
/// own destination. Nothing here renders `RootView`'s `macDesktopLayout`.
///
/// One appearance input is refused rather than pinned, because it is not a
/// trait and nothing can override it; see `liveScreenSnapshotUnsupportedState`.
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
    // Before the render rather than after it: a refused state has no golden to
    // compare against, so there is nothing to spend a settle on.
    if let unsupported = liveScreenSnapshotUnsupportedState(
        reduceTransparency: UIAccessibility.isReduceTransparencyEnabled
    ) {
        XCTFail(unsupported, file: file, line: line)
        return
    }
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

/// The one appearance input these goldens depend on that no canvas can pin.
///
/// Everything else the rendering resolves against is a trait, and
/// `overrideTraits` pins each of them: interface style, content-size category,
/// Bold Text, Increased Contrast. Reduce Transparency is not a trait.
/// `UITraitCollection` has no property for it, so there is nothing for an
/// override to set, and SwiftUI derives
/// `EnvironmentValues.accessibilityReduceTransparency` from
/// `UIAccessibility.isReduceTransparencyEnabled` as a get-only value, so
/// `.environment(_:_:)` does not accept its key path — pinning it on the hosted
/// view does not compile, let alone work.
///
/// It is worth refusing rather than ignoring because it changes these
/// particular goldens: the screens here render `.thinMaterial` and `.bar`
/// backgrounds, which resolve to opaque fills when it is on. Left unchecked, a
/// verifying run on such a machine fails every golden for a reason none of them
/// names, and a recording run silently rewrites them all to match one
/// simulator's accessibility state. The refusal is what a pin would have been:
/// a run under an appearance these references do not describe stops, and says
/// which one.
///
/// Returns the diagnostic, or `nil` when the state is one a golden can be
/// recorded and compared under. Split from the `UIAccessibility` read so the
/// decision is reachable by a test, for the same reason the record mode is.
func liveScreenSnapshotUnsupportedState(reduceTransparency: Bool) -> String? {
    guard reduceTransparency else {
        return nil
    }
    return """
        This simulator has Reduce Transparency on, and it repaints the material \
        backgrounds these goldens record. It is not a trait, so the canvas \
        cannot pin it the way it pins Bold Text and Increased Contrast: turn it \
        off under Settings > Accessibility > Display & Text Size, or run against \
        a simulator that has it off.
        """
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
