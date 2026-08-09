#if os(iOS)
import SwiftUI
import XCTest

@testable import SignalboxNative

/// Goldens for the four kept screens `RootView` no longer reaches, and the
/// three screens reachable from them.
///
/// `SessionsScreen`, `MonitorScreen`, `RunnersScreen`, and `TemplatesScreen`
/// have no construction site in the application: `RootView` renders
/// `ProcessSessionsScreen` and the capability gates in their place. They are
/// kept anyway, and rendering them is the reason — a screen nothing builds is a
/// screen nothing can report a change to. So each is built here directly, which
/// needs no navigation because there is no navigation to it, and no visibility
/// change because all seven are already internal.
///
/// The seam is the one the reachable screens use, one layer lower.
/// `AppCoordinator(isMockMode:)` installs `MockSignalboxService` — the
/// fixture-backed conformer to the retained `SignalboxClientProtocol` that
/// these screens' view models call — and every screen here takes its data from
/// that one object, exactly as `ScreenshotScenario` hands the process screens
/// theirs. Nothing in this file states a fixture value; the two screens that
/// take a value rather than an environment read it back out of the same mock.
///
/// These are methods on `LiveScreenSnapshotTests` rather than a suite of their
/// own, and the reason is mechanical: `scripts/lib/snapshots.sh` spells one
/// suite identifier, `record-snapshots.sh` and `test-snapshots.sh` both select
/// it, and it names a class. A second class would be a second identifier those
/// two scripts have no way to carry, so a golden in it would be recorded by
/// nothing and verified by nothing. The file is still its own, because
/// `assertSnapshot` derives a reference's directory from the file its call site
/// is in: these land under `__Snapshots__/LiveScreenSnapshotTests+LegacyScreens`
/// and stay legible apart from the reachable screens' references.
extension LiveScreenSnapshotTests {
    // MARK: - The four kept screens

    func testLegacySessionList() async {
        await assertLiveScreenSnapshot(of: legacySessionsScreen(), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: legacySessionsScreen(), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: legacySessionsScreen(), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: legacySessionsScreen(), canvas: .iPadLandscape)
    }

    /// The unconfigured state is a different screen, not a dimmed one: with no
    /// service installed, `SessionsScreen` renders the transport gate in place
    /// of its whole body — no state picker, no list, no rows. The three screens
    /// below reach the same gate through an overlay instead, which leaves their
    /// navigation chrome and toolbar visible behind it, so each of the four is
    /// a different rendering of the same message and each is recorded.
    func testLegacySessionListWithoutAConfiguredServer() async {
        await assertLiveScreenSnapshot(of: unconfiguredSessionsScreen(), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: unconfiguredSessionsScreen(), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: unconfiguredSessionsScreen(), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: unconfiguredSessionsScreen(), canvas: .iPadLandscape)
    }

    func testLegacyMonitor() async {
        await assertLiveScreenSnapshot(of: legacyMonitorScreen(), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: legacyMonitorScreen(), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: legacyMonitorScreen(), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: legacyMonitorScreen(), canvas: .iPadLandscape)
    }

    func testLegacyMonitorWithoutAConfiguredServer() async {
        await assertLiveScreenSnapshot(of: unconfiguredMonitorScreen(), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: unconfiguredMonitorScreen(), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: unconfiguredMonitorScreen(), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: unconfiguredMonitorScreen(), canvas: .iPadLandscape)
    }

    func testLegacyRunners() async {
        await assertLiveScreenSnapshot(of: legacyRunnersScreen(), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: legacyRunnersScreen(), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: legacyRunnersScreen(), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: legacyRunnersScreen(), canvas: .iPadLandscape)
    }

    func testLegacyRunnersWithoutAConfiguredServer() async {
        await assertLiveScreenSnapshot(of: unconfiguredRunnersScreen(), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: unconfiguredRunnersScreen(), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: unconfiguredRunnersScreen(), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: unconfiguredRunnersScreen(), canvas: .iPadLandscape)
    }

    func testLegacyTemplates() async {
        await assertLiveScreenSnapshot(of: legacyTemplatesScreen(), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: legacyTemplatesScreen(), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: legacyTemplatesScreen(), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: legacyTemplatesScreen(), canvas: .iPadLandscape)
    }

    func testLegacyTemplatesWithoutAConfiguredServer() async {
        await assertLiveScreenSnapshot(of: unconfiguredTemplatesScreen(), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: unconfiguredTemplatesScreen(), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: unconfiguredTemplatesScreen(), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: unconfiguredTemplatesScreen(), canvas: .iPadLandscape)
    }

    // MARK: - The session detail screen, once per fixture session

    /// One test per fixture session, because the screen is its transcript: the
    /// mock serves eight sessions and each carries a different set of events,
    /// so these eight goldens are eight renderings and not eight copies. They
    /// are the legacy counterpart of the process transcripts in
    /// `LiveScreenSnapshotTests.swift` — the same states through the other
    /// screen — which is what makes the pair worth having.
    func testLegacySessionDetailForACompletedTurn() async throws {
        let session = try await fixtureSession(MockSignalboxFixtures.activeSessionID)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPadLandscape)
    }

    func testLegacySessionDetailWithAToolRequestAwaitingApproval() async throws {
        let session = try await fixtureSession(MockSignalboxFixtures.approvalSessionID)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPadLandscape)
    }

    func testLegacySessionDetailWithAFailedTool() async throws {
        let session = try await fixtureSession(MockSignalboxFixtures.failedSessionID)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPadLandscape)
    }

    func testLegacySessionDetailRenderingMarkdownHeadingsAndLists() async throws {
        let session = try await fixtureSession(MockSignalboxFixtures.markdownBasicsSessionID)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPadLandscape)
    }

    func testLegacySessionDetailRenderingAMarkdownTable() async throws {
        let session = try await fixtureSession(MockSignalboxFixtures.markdownTableSessionID)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPadLandscape)
    }

    func testLegacySessionDetailRenderingMarkdownCodeAndQuotes() async throws {
        let session = try await fixtureSession(MockSignalboxFixtures.markdownCodeSessionID)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPadLandscape)
    }

    func testLegacySessionDetailRenderingAMarkdownIncidentReport() async throws {
        let session = try await fixtureSession(MockSignalboxFixtures.markdownSessionID)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPadLandscape)
    }

    /// The one fixture session the mock serves no events for, which is the only
    /// way to reach the screen's empty state: the timeline renders "No events
    /// yet" and the composer stays. It is also the only archived session, so
    /// this is the only golden with no runner named in its header.
    func testLegacySessionDetailForAnArchivedSessionWithNoEvents() async throws {
        let session = try await fixtureSession(
            MockSignalboxFixtures.archivedSessionID,
            in: .archived
        )
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: legacyDetailScreen(session), canvas: .iPadLandscape)
    }

    /// The artifact preview over the transcript that presents it, which is the
    /// state `.artifactPreview` names and the legacy screen honours:
    /// `SessionDetailScreen` presents the session's first artifact as a sheet
    /// once the scenario asks for it. `testLegacyArtifactPreviewContent` below
    /// records the same preview standing alone.
    func testLegacySessionDetailPresentingAnArtifactPreview() async throws {
        let session = try await fixtureSession(MockSignalboxFixtures.activeSessionID)
        await assertLiveScreenSnapshot(of: previewingDetailScreen(session), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: previewingDetailScreen(session), canvas: .iPhoneLandscape)
        await assertLiveScreenSnapshot(of: previewingDetailScreen(session), canvas: .iPadPortrait)
        await assertLiveScreenSnapshot(of: previewingDetailScreen(session), canvas: .iPadLandscape)
    }

    // MARK: - The two screens a sheet presents

    /// The one artifact the mock serves, which is the only one of the preview's
    /// three branches a fixture reaches: it carries `content_text`, so the
    /// golden is the text branch. The path-only branch and the no-preview
    /// branch are unrecorded, and deliberately — a golden for either would need
    /// an artifact this suite made up, and the fixtures belong to the mock.
    func testLegacyArtifactPreviewContent() async throws {
        let artifact = try await fixtureArtifact(MockSignalboxFixtures.activeSessionID)
        await assertLiveScreenSnapshot(of: ArtifactPreviewScreen(artifact: artifact), canvas: .sheet)
    }

    /// The legacy creation sheet's content, the counterpart of
    /// `testSessionCreationSheetContent`. This one takes its view model
    /// directly rather than an environment object, so the mock is wired to the
    /// model and refreshed before the render: the template and runner pickers
    /// are empty until it answers, and an unrefreshed model would record a
    /// form with two empty menus.
    func testLegacySessionCreationSheetContent() async {
        await assertLiveScreenSnapshot(of: await legacyCreationSheet(), canvas: .sheet)
    }

    // MARK: - Construction

    /// The screens, each with the coordinator its data comes from. A fresh
    /// coordinator per rendering rather than one shared across a test: these
    /// screens load in `.task`, and a second render against a coordinator whose
    /// mock had already answered would be recording a warm screen.
    private func legacySessionsScreen() -> some View {
        SessionsScreen().environmentObject(mockCoordinator())
    }

    private func unconfiguredSessionsScreen() -> some View {
        SessionsScreen().environmentObject(unconfiguredCoordinator())
    }

    private func legacyMonitorScreen() -> some View {
        MonitorScreen().environmentObject(mockCoordinator())
    }

    private func unconfiguredMonitorScreen() -> some View {
        MonitorScreen().environmentObject(unconfiguredCoordinator())
    }

    private func legacyRunnersScreen() -> some View {
        RunnersScreen().environmentObject(mockCoordinator())
    }

    private func unconfiguredRunnersScreen() -> some View {
        RunnersScreen().environmentObject(unconfiguredCoordinator())
    }

    private func legacyTemplatesScreen() -> some View {
        TemplatesScreen().environmentObject(mockCoordinator())
    }

    private func unconfiguredTemplatesScreen() -> some View {
        TemplatesScreen().environmentObject(unconfiguredCoordinator())
    }

    /// Wrapped in the navigation stack the application pushes it onto.
    /// `SessionDetailScreen` sets a navigation title and an inline display mode
    /// and hosts no stack of its own, so rendered bare it would record a screen
    /// with no title bar — a rendering the application never produces. It is
    /// the stack's root here rather than a pushed child, so the one thing the
    /// golden does not show is the back button.
    private func legacyDetailScreen(_ session: SignalboxSessionMetadata) -> some View {
        NavigationStack {
            SessionDetailScreen(session: session)
        }
        .environmentObject(mockCoordinator())
    }

    private func previewingDetailScreen(_ session: SignalboxSessionMetadata) -> some View {
        NavigationStack {
            SessionDetailScreen(session: session)
        }
        .environmentObject(
            AppCoordinator(isMockMode: true, screenshotScenario: .artifactPreview)
        )
    }

    private func legacyCreationSheet() async -> some View {
        let viewModel = SessionListViewModel { MockSignalboxService() }
        await viewModel.refresh()
        return CreateSessionSheet(viewModel: viewModel) { _ in }
    }

    /// A coordinator with the fixture-backed mock installed. `isMockMode`
    /// rather than a scenario, because these screens read no scenario: they
    /// take everything from `coordinator.service`, and that is what this
    /// switches on.
    private func mockCoordinator() -> AppCoordinator {
        AppCoordinator(isMockMode: true)
    }

    /// A coordinator with no service, reached the way `RootView`'s transport
    /// gate golden reaches it: `.setup` is the one scenario whose
    /// `requiresMockService` is false, and it also marks the settings not
    /// configured, so nothing here depends on what the host simulator had
    /// persisted.
    private func unconfiguredCoordinator() -> AppCoordinator {
        AppCoordinator(isMockMode: false, screenshotScenario: .setup)
    }

    /// Which collection the mock is asked for, as an axis rather than an answer.
    ///
    /// `docs/style.md` section 3: a boolean is an answer with its question
    /// erased, and `archived: true` at a call site is exactly that — the reader
    /// has to find the parameter to learn what was asked, and a future call
    /// picking the wrong collection still compiles. `in: .archived` carries the
    /// question.
    ///
    /// It converts to a `Bool` one line below because that is what
    /// `LegacySignalboxClientProtocol.listSessions(archived:)` takes. Changing
    /// that is a client API change and not this suite's to make; the label is
    /// put where the calls are.
    enum ArchiveScope {
        case active
        case archived

        var listsArchivedSessions: Bool {
            switch self {
            case .active:
                return false
            case .archived:
                return true
            }
        }

        /// Names the scope in the diagnostic, so a missing fixture says which
        /// collection was searched.
        var collectionName: String {
            switch self {
            case .active:
                return "active"
            case .archived:
                return "archived"
            }
        }
    }

    /// Reads a fixture session back out of the mock rather than building one.
    ///
    /// A separate `MockSignalboxService` instance from the coordinator's, and
    /// that is sound because the mock decodes the same immutable fixture every
    /// time: what this returns is the metadata the screen's own service will
    /// serve. Building a `SignalboxSessionMetadata` here instead would be this
    /// suite owning a fixture, which is the thing the seam exists to avoid.
    private func fixtureSession(
        _ sessionID: String,
        in scope: ArchiveScope = .active,
        file: StaticString = #filePath,
        line: UInt = #line
    ) async throws -> SignalboxSessionMetadata {
        let sessions = try await MockSignalboxService()
            .listSessions(archived: scope.listsArchivedSessions)
        return try XCTUnwrap(
            sessions.first { $0.id == SignalboxSessionID(rawValue: sessionID) },
            "the mock serves no \(scope.collectionName) session \(sessionID)",
            file: file,
            line: line
        )
    }

    private func fixtureArtifact(
        _ sessionID: String,
        file: StaticString = #filePath,
        line: UInt = #line
    ) async throws -> SignalboxArtifact {
        let artifacts = try await MockSignalboxService()
            .listArtifacts(sessionID: SignalboxSessionID(rawValue: sessionID))
        return try XCTUnwrap(
            artifacts.first,
            "the mock serves no artifact for session \(sessionID)",
            file: file,
            line: line
        )
    }
}
#endif
