#if os(iOS)
import SwiftUI
import XCTest

@testable import SignalboxNative

/// Goldens for the screens `RootView` reaches, in every state
/// `ScreenshotScenario` selects, on every canvas.
///
/// Every section it can select is here: Sessions, Monitor, Runners, Templates,
/// and Settings, along with the transport gate that stands in front of all of
/// them and the session transcript a selected session pushes. Every
/// `ScreenshotScenario` case is either rendered by a test here or refused for a
/// reason written where its test would have been, and `.artifactPreview` is the
/// only refusal. Which of the two each case is, is stated by
/// `ScenarioDisposition.of(_:)` below — exhaustively, with no `default` — so a
/// case added later stops this suite compiling instead of quietly going
/// uncovered.
///
/// `ScreenshotScenario` is the determinism seam, unchanged: each case selects
/// a fixture set that the shipping mock coordinator installs behind the real
/// encoder, decoder, and framing, so this suite states which screen in which
/// state it renders and owns no fixtures of its own.
///
/// Each scenario is rendered on all four screen canvases rather than on the one
/// that distinguishes it. `RootView` branches on the horizontal size class
/// alone, so the phone canvases share its tab bar and the iPad canvases share
/// its split view — but the two phone canvases differ by 454 points of height
/// and the two iPad canvases by 342 points of width, and what a screen does
/// with that is the thing a golden is for. Near-duplicate references across
/// canvases are the accepted cost of a corpus that shows each screen at every
/// shape the application ships in.
///
/// Two tests skip the two phone canvases, each for a reason stated on it and
/// each a rendering that would be wrong rather than redundant: the Templates
/// gate has no compact destination to enter it through, and the presented
/// creation sheet records clipped there. Nothing else is pruned; a
/// near-duplicate of one screen across two canvases is a reference, not a
/// saving.
///
/// Every one of these is a change detector, which is what a golden is. They
/// catch an unintended visual change to a screen no assertion describes; they
/// state no law, and the view-model tests beside them keep doing that.
///
/// The goldens for the four kept screens `RootView` does not reach are in
/// `LiveScreenSnapshotTests+LegacyScreens.swift`, on this same class: the suite
/// identifier `scripts/lib/snapshots.sh` spells is a class, so every golden CI
/// must verify belongs to this one, while the file a test is written in is what
/// decides which `__Snapshots__` directory its references land in.
@MainActor
final class LiveScreenSnapshotTests: XCTestCase {
    /// What this suite does with one `ScreenshotScenario`.
    ///
    /// A disposition per case rather than a count of them. A count is satisfied
    /// by being counted: a new case failed `allCases.count == 15`, the
    /// diagnostic said to update the number, updating it was the whole edit,
    /// and the case stayed uncovered with the suite green. Neither answer below
    /// can be given by editing a number — one of them names a test that has to
    /// exist and the other carries the argument for why no golden is the right
    /// outcome.
    enum ScenarioDisposition {
        /// Rendered by a test in this file or its `+LegacyScreens` extension,
        /// on the canvases named there.
        case rendered
        /// Deliberately not rendered, for the reason given.
        case refused(reason: String)

        /// The disposition of every case, exhaustively.
        ///
        /// The missing `default` is the whole instrument. A case added to
        /// `ScreenshotScenario` makes this switch non-exhaustive, which is a
        /// compile error in this file rather than a red assertion somewhere in
        /// a run, so the next case cannot reach main uncovered and cannot be
        /// answered by a number. Listing the rendered cases one per line rather
        /// than in one joined pattern is deliberate for the same reason: the
        /// edit that adds a case should look like the edit that decides about
        /// it.
        static func of(_ scenario: ScreenshotScenario) -> ScenarioDisposition {
            switch scenario {
            case .setup:
                return .rendered
            case .sessions:
                return .rendered
            case .newSession:
                return .rendered
            case .activeChat:
                return .rendered
            case .markdownBasics:
                return .rendered
            case .markdownTable:
                return .rendered
            case .markdownCode:
                return .rendered
            case .markdownMessage:
                return .rendered
            case .pendingApproval:
                return .rendered
            case .completedTool:
                return .rendered
            case .failedTool:
                return .rendered
            case .runners:
                return .rendered
            case .monitor:
                return .rendered
            case .settings:
                return .rendered
            case .artifactPreview:
                return .refused(
                    reason: """
                        The alert that is the whole difference between this \
                        scenario and a completed turn cannot be rendered \
                        reproducibly here: its backdrop resamples a window \
                        this renderer is still compositing, so the screen \
                        never settles. The full argument is where its test \
                        would have been, below.
                        """
                )
            }
        }
    }

    /// Every refusal states a reason.
    ///
    /// The exhaustive switch above is what makes a new case a decision; this is
    /// what keeps `.refused` from being the cheap way to discharge it. It can
    /// only catch an empty string — no assertion can judge whether an argument
    /// is a good one — so it is a floor and the review of the reason is the
    /// rest.
    func testEveryRefusedScenarioStatesAReason() {
        for scenario in ScreenshotScenario.allCases {
            guard case .refused(let reason) = ScenarioDisposition.of(scenario) else { continue }
            XCTAssertFalse(
                reason.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
                """
                \(scenario) is refused without a reason. A scenario this suite \
                does not render carries the argument for why no golden is the \
                right outcome; write it, or render the scenario.
                """
            )
        }
    }

    func testTransportGateWithoutAConfiguredSocket() async {
        await assertLiveScreenSnapshot(of: rootView(for: .setup), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: rootView(for: .setup), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: rootView(for: .setup), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: rootView(for: .setup), canvas: .iPadLandscape)
    }

    func testSessionList() async {
        await assertLiveScreenSnapshot(of: rootView(for: .sessions), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: rootView(for: .sessions), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: rootView(for: .sessions), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: rootView(for: .sessions), canvas: .iPadLandscape)
    }

    /// The list with the creation sheet over it, which is what the scenario
    /// puts on screen: `ProcessSessionsScreen` presents the sheet itself once
    /// the scenario names it, and the presenting controller is inside the
    /// canvas window, so this renderer does capture it.
    ///
    /// The phone canvases are skipped because their rendering is broken rather
    /// than merely redundant. A presented sheet lays out against the
    /// presentation's own metrics, and this sheet's content declares a 420-point
    /// minimum width — wider than the 390-point phone canvas — so what those two
    /// record is the form centred and clipped on both edges, reading "cel" and
    /// "Cr" where its buttons are. That is the canvas cutting into a
    /// presentation, not the application, and it is the same fact
    /// `SnapshotCanvas.sheet` exists for. `testSessionCreationSheetContent`
    /// records the content at a width that fits it.
    func testSessionListPresentingTheCreationSheet() async {
        await assertLiveScreenSnapshot(of: rootView(for: .newSession), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: rootView(for: .newSession), canvas: .iPadLandscape)
    }

    /// Named for the state it renders, not the fixture it uses. `.activeChat`
    /// names the screen, but the transcript it serves carries a turn whose
    /// `state.type` is `completed`, and the golden says "Completed" across the
    /// header. A turn still in flight renders a different header and a
    /// different usage card, and nothing here covers it — a test named for an
    /// active turn would have claimed otherwise while pinning this.
    func testSessionTranscriptForACompletedTurn() async {
        await assertLiveScreenSnapshot(of: rootView(for: .activeChat), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: rootView(for: .activeChat), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: rootView(for: .activeChat), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: rootView(for: .activeChat), canvas: .iPadLandscape)
    }

    /// The four markdown scenarios are four different transcripts, not four
    /// renderings of one: each names its own fixture session, and between them
    /// they cover the constructs the transcript renderer handles separately —
    /// headings and lists, tables and links, fenced code and block quotes, and
    /// a long mixed document. A change to any one renderer shows up in one of
    /// these and not the others, which is why they are four tests.
    func testSessionTranscriptRenderingMarkdownHeadingsAndLists() async {
        await assertLiveScreenSnapshot(of: rootView(for: .markdownBasics), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: rootView(for: .markdownBasics), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: rootView(for: .markdownBasics), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: rootView(for: .markdownBasics), canvas: .iPadLandscape)
    }

    func testSessionTranscriptRenderingAMarkdownTable() async {
        await assertLiveScreenSnapshot(of: rootView(for: .markdownTable), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: rootView(for: .markdownTable), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: rootView(for: .markdownTable), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: rootView(for: .markdownTable), canvas: .iPadLandscape)
    }

    func testSessionTranscriptRenderingMarkdownCodeAndQuotes() async {
        await assertLiveScreenSnapshot(of: rootView(for: .markdownCode), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: rootView(for: .markdownCode), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: rootView(for: .markdownCode), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: rootView(for: .markdownCode), canvas: .iPadLandscape)
    }

    func testSessionTranscriptRenderingAMarkdownIncidentReport() async {
        await assertLiveScreenSnapshot(of: rootView(for: .markdownMessage), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: rootView(for: .markdownMessage), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: rootView(for: .markdownMessage), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: rootView(for: .markdownMessage), canvas: .iPadLandscape)
    }

    func testSessionTranscriptWithAToolRequestAwaitingApproval() async {
        await assertLiveScreenSnapshot(of: rootView(for: .pendingApproval), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: rootView(for: .pendingApproval), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: rootView(for: .pendingApproval), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: rootView(for: .pendingApproval), canvas: .iPadLandscape)
    }

    func testSessionTranscriptWithACompletedTool() async {
        await assertLiveScreenSnapshot(of: rootView(for: .completedTool), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: rootView(for: .completedTool), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: rootView(for: .completedTool), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: rootView(for: .completedTool), canvas: .iPadLandscape)
    }

    func testSessionTranscriptWithAFailedTool() async {
        await assertLiveScreenSnapshot(of: rootView(for: .failedTool), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: rootView(for: .failedTool), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: rootView(for: .failedTool), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: rootView(for: .failedTool), canvas: .iPadLandscape)
    }

    // `.artifactPreview` has no test here, and the absence is the finding
    // rather than an omission.
    //
    // `ProcessConversationDetailScreen` answers that scenario with an alert
    // saying the protocol exposes no artifact operation, and the alert is the
    // whole difference between it and `testSessionTranscriptForACompletedTurn`
    // — both serve the same session — so a rendering without the alert would be
    // a duplicate reference under a name claiming otherwise.
    //
    // The alert cannot be rendered reproducibly here. Its backdrop is a blur
    // material that resamples what is behind it, and behind it in this renderer
    // is a window being composited by `drawHierarchy`, so the two never come to
    // rest: `LiveScreenRenderer` reported "the screen was still changing after
    // 5.0 seconds" on the iPad canvases of a verifying run, and the phone
    // canvases had already recorded the transcript's own text mirrored and
    // upside down behind the alert. That is the settle gate doing its job — a
    // golden of one arbitrary frame of it fails on its own next run — and the
    // honest outcome is no golden rather than a flaky one.
    //
    // The state the scenario names is still recorded, one screen over:
    // `testLegacySessionDetailPresentingAnArtifactPreview` renders the legacy
    // transcript answering the same scenario by presenting the preview, and it
    // settles on every canvas. What is uncovered is the process transcript's
    // refusal alert, and nothing short of a renderer that can host a stable
    // presentation would cover it.

    /// The three capability gates are one view with three different messages,
    /// and each is snapshotted, because the message is the screen: a reader
    /// who reaches one of these sections is told what the process protocol does
    /// not expose, and nothing else on the screen distinguishes it.
    func testMonitorCapabilityGate() async {
        await assertLiveScreenSnapshot(of: rootView(for: .monitor), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: rootView(for: .monitor), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: rootView(for: .monitor), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: rootView(for: .monitor), canvas: .iPadLandscape)
    }

    func testRunnersCapabilityGate() async {
        await assertLiveScreenSnapshot(of: rootView(for: .runners), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: rootView(for: .runners), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: rootView(for: .runners), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: rootView(for: .runners), canvas: .iPadLandscape)
    }

    func testSettingsScreen() async {
        await assertLiveScreenSnapshot(of: rootView(for: .settings), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: rootView(for: .settings), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: rootView(for: .settings), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: rootView(for: .settings), canvas: .iPadLandscape)
    }

    /// Selected rather than named, and the one test in the file that skips the
    /// phone canvases, because that is the only way `RootView` reaches this
    /// screen: the compact tab bar has no Templates tab, and no
    /// `ScreenshotScenario` resolves to the section. Assigning the section is
    /// what the sidebar row does, so this enters the screen the way the
    /// application does; the scenario still selects the fixtures, which keeps
    /// the determinism seam where the others have it.
    ///
    /// The skip is a real absence rather than a saving. Assigning
    /// `.templates` on a phone canvas selects a tag the compact `TabView` has
    /// no tab for, and what it renders then is the tab bar's fallback, not the
    /// Templates section — a golden of it would be a reference for a screen no
    /// reader can reach and would go on passing after the section gained a tab.
    func testTemplatesCapabilityGate() async {
        await assertLiveScreenSnapshot(of: templatesRootView(), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: templatesRootView(), canvas: .iPadLandscape)
    }

    /// The sheet's content as its own screen, on the canvas a sheet declares
    /// for itself. Compositing it onto the window that presents it is what
    /// `testSessionListPresentingTheCreationSheet` records; this is the same
    /// content at the size its own minimum width asks for, which is what a
    /// reader comparing the form's fields wants and what the presented golden
    /// crops.
    func testSessionCreationSheetContent() async {
        await assertLiveScreenSnapshot(of: processCreationSheet(), canvas: .sheet)
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

    private func templatesRootView() -> some View {
        let coordinator = AppCoordinator(
            isMockMode: ScreenshotScenario.sessions.requiresMockService,
            screenshotScenario: .sessions
        )
        coordinator.selectedSection = .templates
        return RootView().environmentObject(coordinator)
    }

    private func processCreationSheet() -> some View {
        let coordinator = AppCoordinator(
            isMockMode: ScreenshotScenario.newSession.requiresMockService,
            screenshotScenario: .newSession
        )
        return ProcessSessionCreationSheet {}.environmentObject(coordinator)
    }
}
#endif
