#if os(iOS)
import SwiftUI
import XCTest

@testable import SignalboxNative

/// Goldens for the screens `RootView` reaches.
///
/// Every section it can select is here: Sessions, Monitor, Runners, Templates,
/// and Settings, along with the transport gate that stands in front of all of
/// them and the session transcript a selected session pushes. Each is
/// snapshotted on the canvas that reaches it, which is why Templates is the one
/// gate recorded on the regular canvas — the compact tab bar has no tab for it.
///
/// `ScreenshotScenario` is the determinism seam, unchanged: each case selects
/// a fixture set that the shipping mock coordinator installs behind the real
/// encoder, decoder, and framing, so this suite states which screen in which
/// state it renders and owns no fixtures of its own.
///
/// Every one of these is a change detector, which is what a golden is. They
/// catch an unintended visual change to a screen no assertion describes; they
/// state no law, and the view-model tests beside them keep doing that.
@MainActor
final class LiveScreenSnapshotTests: XCTestCase {
    func testTransportGateWithoutAConfiguredSocket() async {
        await assertLiveScreenSnapshot(of: rootView(for: .setup), canvas: .compact)
    }

    func testSessionListInACompactLayout() async {
        await assertLiveScreenSnapshot(of: rootView(for: .sessions), canvas: .compact)
    }

    func testSessionListInARegularLayout() async {
        await assertLiveScreenSnapshot(of: rootView(for: .sessions), canvas: .regular)
    }

    /// Named for the state it renders, not the fixture it uses. `.activeChat`
    /// names the screen, but the transcript it serves carries a turn whose
    /// `state.type` is `completed`, and the golden says "Completed" across the
    /// header. A turn still in flight renders a different header and a
    /// different usage card, and nothing here covers it — a test named for an
    /// active turn would have claimed otherwise while pinning this.
    func testSessionTranscriptForACompletedTurn() async {
        await assertLiveScreenSnapshot(of: rootView(for: .activeChat), canvas: .compact)
    }

    func testSessionTranscriptWithAToolRequestAwaitingApproval() async {
        await assertLiveScreenSnapshot(of: rootView(for: .pendingApproval), canvas: .compact)
    }

    func testSessionTranscriptWithACompletedTool() async {
        await assertLiveScreenSnapshot(of: rootView(for: .completedTool), canvas: .compact)
    }

    func testSessionTranscriptWithAFailedTool() async {
        await assertLiveScreenSnapshot(of: rootView(for: .failedTool), canvas: .compact)
    }

    func testSettingsScreen() async {
        await assertLiveScreenSnapshot(of: rootView(for: .settings), canvas: .compact)
    }

    /// The three capability gates are one view with three different messages,
    /// and each is snapshotted, because the message is the screen: a reader
    /// who reaches one of these sections is told what the process protocol does
    /// not expose, and nothing else on the screen distinguishes it.
    func testMonitorCapabilityGate() async {
        await assertLiveScreenSnapshot(of: rootView(for: .monitor), canvas: .compact)
    }

    func testRunnersCapabilityGate() async {
        await assertLiveScreenSnapshot(of: rootView(for: .runners), canvas: .compact)
    }

    /// Selected rather than named, and regular rather than compact, because
    /// that is the only way `RootView` reaches this screen: the compact tab bar
    /// has no Templates tab, and no `ScreenshotScenario` resolves to the
    /// section. Assigning it is what the sidebar row does, so this enters the
    /// screen the way the application does; the scenario still selects the
    /// fixtures, which keeps the determinism seam where the others have it.
    func testTemplatesCapabilityGate() async {
        let coordinator = AppCoordinator(
            isMockMode: ScreenshotScenario.sessions.requiresMockService,
            screenshotScenario: .sessions
        )
        coordinator.selectedSection = .templates
        await assertLiveScreenSnapshot(
            of: RootView().environmentObject(coordinator),
            canvas: .regular
        )
    }

    /// The sheet is snapshotted as its own screen. Compositing it onto the
    /// window that presents it is what the golden capture scripts do; nothing
    /// in process presents a sheet, so there is no parent here to composite
    /// onto and the content stands on its own canvas.
    func testSessionCreationSheetContent() async {
        let coordinator = AppCoordinator(
            isMockMode: ScreenshotScenario.newSession.requiresMockService,
            screenshotScenario: .newSession
        )
        await assertLiveScreenSnapshot(
            of: ProcessSessionCreationSheet {}.environmentObject(coordinator),
            canvas: .sheet
        )
    }

    private func rootView(for scenario: ScreenshotScenario) -> some View {
        RootView()
            .environmentObject(
                AppCoordinator(
                    isMockMode: scenario.requiresMockService,
                    screenshotScenario: scenario
                )
            )
    }
}
#endif
