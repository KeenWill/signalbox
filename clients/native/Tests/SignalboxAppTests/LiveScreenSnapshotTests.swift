#if os(iOS)
import SwiftUI
import XCTest

@testable import SignalboxNative

/// Goldens for the screens `RootView` reaches.
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

    func testSessionTranscriptForAnActiveTurn() async {
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

    func testMonitorCapabilityGate() async {
        await assertLiveScreenSnapshot(of: rootView(for: .monitor), canvas: .compact)
    }

    /// The sheet is snapshotted as its own screen. Compositing it onto the
    /// window that presents it is what the golden capture scripts do; nothing
    /// in process presents a sheet, and porting that seam is deferred.
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
