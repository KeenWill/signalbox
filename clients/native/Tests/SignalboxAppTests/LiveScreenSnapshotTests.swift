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
/// One test prunes a canvas, for a reason stated on it and because the
/// rendering would be wrong rather than redundant: the Templates gate skips
/// both phone canvases, having no compact destination to enter it through.
/// Nothing else is pruned. The presented creation sheet was the other, until
/// the arithmetic on its note showed it was recording the application's own
/// clipping rather than the canvas's; a near-duplicate of one screen across two
/// canvases is a reference, not a saving, and so is an unflattering one.
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
        /// Rendered by the named test, on the canvases named there.
        ///
        /// The name is checked rather than decorative:
        /// `testEveryRenderedScenarioNamesASnapshotProducingTest` fails when no test on
        /// this class defines it, and
        /// `testNoTwoRenderedScenariosClaimTheSameTest` fails when a second
        /// scenario points at a test that already answers for another. Together
        /// they are what a bare `.rendered` was missing — that case could be
        /// returned for a new scenario to clear the compile error without a
        /// snapshot existing anywhere, and the documentation above claimed
        /// otherwise. Discharging the decision now means writing a test.
        ///
        /// What it still does not prove is that the named test renders *this*
        /// scenario; nothing here reads the argument the test passes to
        /// `rootView(for:)`. It proves the test exists and answers for nothing
        /// else, which is what makes the next case's cheapest path an actual
        /// rendering.
        case rendered(by: String)
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
                return .rendered(by: "testTransportGateWithoutAConfiguredSocket")
            case .sessions:
                return .rendered(by: "testSessionList")
            case .newSession:
                return .rendered(by: "testSessionListPresentingTheCreationSheet")
            case .activeChat:
                return .rendered(by: "testSessionTranscriptForACompletedTurn")
            case .markdownBasics:
                return .rendered(by: "testSessionTranscriptRenderingMarkdownHeadingsAndLists")
            case .markdownTable:
                return .rendered(by: "testSessionTranscriptRenderingAMarkdownTable")
            case .markdownCode:
                return .rendered(by: "testSessionTranscriptRenderingMarkdownCodeAndQuotes")
            case .markdownMessage:
                return .rendered(by: "testSessionTranscriptRenderingAMarkdownIncidentReport")
            case .pendingApproval:
                return .rendered(by: "testSessionTranscriptWithAToolRequestAwaitingApproval")
            case .completedTool:
                return .rendered(by: "testSessionTranscriptWithACompletedTool")
            case .failedTool:
                return .rendered(by: "testSessionTranscriptWithAFailedTool")
            case .runners:
                return .rendered(by: "testRunnersCapabilityGate")
            case .monitor:
                return .rendered(by: "testMonitorCapabilityGate")
            case .settings:
                return .rendered(by: "testSettingsScreen")
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

        /// Whether this disposition carries a written reason.
        ///
        /// A rendered case does not and needs none; a refusal that does not is
        /// the gap `testTheArtifactPreviewRefusalStatesAReason` reports. The
        /// `switch` is here rather than in that test body because
        /// `docs/agents/testing-style.md` rule 2 governs test bodies, and a
        /// straight-line assertion still needs the branch to happen somewhere.
        var statesARefusalReason: Bool {
            switch self {
            case .rendered:
                return false
            case .refused(let reason):
                return !reason.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            }
        }

        /// The test this disposition names, or `nil` for a refusal.
        var renderingTestName: String? {
            switch self {
            case .rendered(let name):
                return name
            case .refused:
                return nil
            }
        }
    }

    /// The names the rendered dispositions carry, refusals dropped.
    ///
    /// Parameterized for the same reason the two detectors below are, and it
    /// was the gap they left: they are handed the lists this produces, so an
    /// extraction that dropped every rendered disposition — returning `[]` —
    /// gave both of them nothing to find and both went green. The known-answer
    /// tests for the detectors could not see that, because their inputs never
    /// came through here.
    static func renderingTestNames(of dispositions: [ScenarioDisposition]) -> [String] {
        dispositions.compactMap(\.renderingTestName)
    }

    /// The test every rendered scenario names, in `allCases` order.
    ///
    /// These are computed properties rather than test bodies because they
    /// iterate, and `docs/agents/testing-style.md` rule 2 governs test bodies.
    /// The tests below are the straight-line assertions over what they return.
    private static var claimedRenderingTestNames: [String] {
        renderingTestNames(of: ScreenshotScenario.allCases.map(ScenarioDisposition.of))
    }

    /// The names in `claimed` that `defined` does not contain.
    ///
    /// Takes both sides rather than reading them, so that
    /// `testTheCoverageDetectorsAnswerTheirOwnQuestion` can hand it an input
    /// with a known answer. Called only on the suite's real lists, its passing
    /// would say nothing: those lists are consistent today, so an
    /// implementation that returned `[]` unconditionally would be green and the
    /// enforcement it exists for would be silently off.
    static func names(in claimed: [String], missingFrom defined: Set<String>) -> [String] {
        claimed.filter { !defined.contains($0) }.sorted()
    }

    /// The names `claimed` lists more than once. Parameterized for the reason
    /// above, and it is the more important of the two: a duplicate is the
    /// mistake this guard exists to catch and the one its own input never has.
    static func namesUsedMoreThanOnce(in claimed: [String]) -> [String] {
        var counts: [String: Int] = [:]
        for name in claimed {
            counts[name, default: 0] += 1
        }
        return counts.filter { $0.value > 1 }.map(\.key).sorted()
    }

    /// Every test this class defines, by method name.
    ///
    /// `defaultTestSuite` is the enumeration XCTest itself runs, so it sees an
    /// `async` test the Objective-C selector for one does not: a method written
    /// `func testFoo() async` is bridged under a different selector, and asking
    /// the runtime whether the class responds to `testFoo` would report every
    /// test in this file missing. A name here reads `-[Class testFoo]`, so the
    /// last space-separated component with its bracket removed is the method.
    private static var definedTestNames: Set<String> {
        Set(
            defaultTestSuite.tests.map { test in
                String(test.name.split(separator: " ").last ?? "")
                    .trimmingCharacters(in: CharacterSet(charactersIn: "[]"))
            }
        )
    }

    /// The names no test on this class defines.
    /// The test a golden's file name belongs to, or `nil` if it names none.
    ///
    /// A golden is written as `<test>.<canvas>.png`, so the first
    /// dot-separated component is the method that recorded it. The `test`
    /// prefix is required because a directory holds other files —
    /// `MANIFEST.sha256` next door is one — and a name that cannot be a test
    /// method should not become an entry in an inventory of them.
    static func testName(fromGoldenNamed fileName: String) -> String? {
        guard fileName.hasSuffix(".png") else { return nil }
        guard let first = fileName.split(separator: ".").first else { return nil }
        let name = String(first)
        return name.hasPrefix("test") ? name : nil
    }

    /// The tests that have at least one committed golden.
    ///
    /// Read off `__Snapshots__` rather than listed, because a list would be the
    /// hand-maintained inventory this file has twice replaced with something
    /// derived. The directory is the same one the assertions read their
    /// references from — `#filePath` locates it exactly as
    /// `assertSnapshot` does — so a name is in here when, and only when, a
    /// rendering for it is committed.
    private static var snapshotProducingTestNames: Set<String> {
        let root = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .appendingPathComponent("__Snapshots__")
        let manager = FileManager.default
        let suiteDirectories =
            (try? manager.contentsOfDirectory(at: root, includingPropertiesForKeys: nil)) ?? []
        return Set(
            suiteDirectories
                .flatMap { (try? manager.contentsOfDirectory(atPath: $0.path)) ?? [] }
                .compactMap(testName(fromGoldenNamed:))
        )
    }

    /// The claimed names that no committed rendering answers for.
    ///
    /// The defined set is intersected with the snapshot-producing one, and that
    /// intersection is the whole point. `defaultTestSuite` admits every method
    /// on this class, which by now includes the detector tests, the refusal
    /// test and the legacy screens' — so a scenario added later could have
    /// named `testTheDuplicateDetectorReportsADuplicate`, satisfied both guards
    /// (it exists, and no other scenario had claimed it) and still rendered
    /// nothing. Requiring a golden is what makes the claim mean what it says.
    private static var claimedNamesMissingFromTheSuite: [String] {
        names(
            in: claimedRenderingTestNames,
            missingFrom: definedTestNames.intersection(snapshotProducingTestNames)
        )
    }

    /// The names more than one scenario claims.
    private static var claimedNamesUsedMoreThanOnce: [String] {
        namesUsedMoreThanOnce(in: claimedRenderingTestNames)
    }

    /// Inputs for the helper tests below, whose spellings are arbitrary.
    ///
    /// `docs/style.md` section 1: a literal at a use site claims that exact
    /// value matters, and none of these do — the detectors read repetition and
    /// membership, never the characters. Spelled `"a"` and `"b"` inline they
    /// were indistinguishable from the load-bearing names elsewhere in this
    /// file, where `"testSessionList"` has to be exactly that. Named here, the
    /// reader can tell the two apart without checking either.
    ///
    /// What is deliberately *not* named is the empty and whitespace-only
    /// reasons in `testTheRefusalReasonCheckRejectsAnEmptyReason`: those are
    /// load-bearing, and the same section says to spell a load-bearing literal
    /// at the assertion.
    private enum ArbitraryClaim {
        /// Arbitrary; only that it is distinct from `second` matters, and that
        /// the duplicate cases can repeat it.
        static let first = "first-claim"
        /// Arbitrary; only that it is distinct from `first` matters. Sorts
        /// after it, which is the order the detectors return.
        static let second = "second-claim"
        /// Arbitrary; only that no defined set in these tests contains it.
        /// Sorts before both of the others.
        static let absent = "absent-claim"
        /// Arbitrary; only that it is non-empty matters.
        static let statedReason = "a stated reason"
    }

    /// The refusal-reason check rejects an empty reason.
    ///
    /// Found by mutation rather than by review: neutering
    /// `statesARefusalReason` to `return true` left
    /// `testTheArtifactPreviewRefusalStatesAReason` green, because that test
    /// asserts the property is true of the one refusal the suite has, and a
    /// property hardcoded to true is true of it. The assertion was reading the
    /// answer it wanted from an implementation that could no longer be wrong.
    ///
    /// This is the known-answer half, and the empty and whitespace cases are
    /// the whole point of it — they are the inputs a working implementation
    /// answers `false` for and a broken one cannot.
    func testTheRefusalReasonCheckRejectsAnEmptyReason() {
        XCTAssertTrue(
            ScenarioDisposition.refused(reason: ArbitraryClaim.statedReason).statesARefusalReason
        )
        XCTAssertFalse(ScenarioDisposition.refused(reason: "").statesARefusalReason)
        XCTAssertFalse(ScenarioDisposition.refused(reason: "   \n\t ").statesARefusalReason)
        XCTAssertFalse(
            ScenarioDisposition.rendered(by: ArbitraryClaim.first).statesARefusalReason
        )
    }

    /// The extractor keeps rendered names and drops refusals.
    ///
    /// The step between the dispositions and the two detectors, and the one
    /// place a regression disabled every check downstream without failing any
    /// of them: both detectors answer `[]` for `[]`, which is the answer they
    /// want, so an extraction returning nothing left the enforcement off and
    /// the suite green. A mixture with a non-empty expected result is what
    /// distinguishes it from that.
    func testTheNameExtractorKeepsRenderedNamesAndDropsRefusals() {
        XCTAssertEqual(
            Self.renderingTestNames(of: [
                .rendered(by: ArbitraryClaim.first),
                .refused(reason: ArbitraryClaim.statedReason),
                .rendered(by: ArbitraryClaim.second),
            ]),
            [ArbitraryClaim.first, ArbitraryClaim.second]
        )
        XCTAssertEqual(
            Self.renderingTestNames(of: [.rendered(by: ArbitraryClaim.first)]),
            [ArbitraryClaim.first]
        )
        XCTAssertEqual(
            Self.renderingTestNames(of: [.refused(reason: ArbitraryClaim.statedReason)]),
            []
        )
        XCTAssertEqual(Self.renderingTestNames(of: []), [])
    }

    /// The suite's own claimed names are not empty.
    ///
    /// The extractor's test covers the function; this covers the wiring into
    /// it. `claimedRenderingTestNames` maps `allCases` through
    /// `ScenarioDisposition.of` before the extraction, and a regression there
    /// is the same silent failure by a different route. Naming one test the
    /// mapping has to produce is enough to tell an empty list from a real one,
    /// and `testSessionList` has to exist regardless — the coverage test above
    /// requires it.
    func testTheClaimedNamesAreDrawnFromTheScenarios() {
        XCTAssertTrue(Self.claimedRenderingTestNames.contains("testSessionList"))
        XCTAssertFalse(Self.claimedRenderingTestNames.isEmpty)
    }

    /// The duplicate detector reports a duplicate.
    ///
    /// The coverage tests below run these detectors over the suite's real
    /// lists, which are consistent — no name missing, no name claimed twice.
    /// That is the answer those tests want and the reason neither of them can
    /// vouch for the detector that produced it: an implementation returning
    /// `[]` for every input passes both, and the enforcement is off with
    /// nothing red. So each detector is also handed input whose answer is known
    /// and is not empty.
    ///
    /// One detector per test, per `docs/agents/testing-style.md` rule 7: these
    /// two helpers are independent, and a single plural test would report a
    /// regression in either under one name. Straight-line cases inside each,
    /// per rule 2, so a failure names the case. The inputs are letters because
    /// the names carry no meaning here — what is under test is counting, not
    /// the suite's mapping, which is what the coverage tests check.
    func testTheDuplicateDetectorReportsADuplicate() {
        XCTAssertEqual(
            Self.namesUsedMoreThanOnce(
                in: [ArbitraryClaim.first, ArbitraryClaim.second, ArbitraryClaim.first]
            ),
            [ArbitraryClaim.first]
        )
        XCTAssertEqual(
            Self.namesUsedMoreThanOnce(
                in: [
                    ArbitraryClaim.first, ArbitraryClaim.first,
                    ArbitraryClaim.second, ArbitraryClaim.second,
                ]
            ),
            [ArbitraryClaim.first, ArbitraryClaim.second]
        )
        XCTAssertEqual(
            Self.namesUsedMoreThanOnce(in: [ArbitraryClaim.first, ArbitraryClaim.second]),
            []
        )
        XCTAssertEqual(Self.namesUsedMoreThanOnce(in: []), [])
    }

    /// The membership detector reports a missing name.
    ///
    /// Split from the duplicate detector's test for the reason given there:
    /// they are separate helpers answering separate questions, and a failure
    /// should name which enforcement stopped working.
    func testTheMembershipDetectorReportsAMissingName() {
        XCTAssertEqual(
            Self.names(
                in: [ArbitraryClaim.first, ArbitraryClaim.absent],
                missingFrom: [ArbitraryClaim.first]
            ),
            [ArbitraryClaim.absent]
        )
        XCTAssertEqual(
            Self.names(in: [ArbitraryClaim.second, ArbitraryClaim.first], missingFrom: []),
            [ArbitraryClaim.first, ArbitraryClaim.second]
        )
        XCTAssertEqual(
            Self.names(
                in: [ArbitraryClaim.first],
                missingFrom: [ArbitraryClaim.first, ArbitraryClaim.second]
            ),
            []
        )
        XCTAssertEqual(Self.names(in: [], missingFrom: [ArbitraryClaim.first]), [])
    }

    /// The golden-name parser reads the test out of a file name.
    ///
    /// Known-answer half of the inventory: the directory scan below is what
    /// makes the claim structural, and this is the part of it that can be
    /// wrong quietly.
    func testTheGoldenNameParserReadsTheTestName() {
        XCTAssertEqual(
            Self.testName(fromGoldenNamed: "testSessionList.iphone-portrait.png"),
            "testSessionList"
        )
        XCTAssertEqual(
            Self.testName(fromGoldenNamed: "testSessionCreationSheetContent.sheet.png"),
            "testSessionCreationSheetContent"
        )
        XCTAssertNil(Self.testName(fromGoldenNamed: "MANIFEST.sha256"))
        XCTAssertNil(Self.testName(fromGoldenNamed: "notATestMethod.iphone-portrait.png"))
    }

    /// The inventory holds the tests that record goldens, and only those.
    ///
    /// The second assertion is the finding itself, written down: a detector
    /// test is a method on this class and would satisfy `definedTestNames`, so
    /// if it were also in here the restriction would buy nothing and a future
    /// scenario could claim it without rendering anything.
    func testTheSnapshotInventoryHoldsOnlyRenderingTests() {
        XCTAssertTrue(Self.snapshotProducingTestNames.contains("testSessionList"))
        XCTAssertFalse(
            Self.snapshotProducingTestNames.contains("testTheDuplicateDetectorReportsADuplicate")
        )
    }

    /// Every rendered scenario names a test that records a golden.
    ///
    /// This is what makes `.rendered` cost something. Without it the case was a
    /// bare tag: a scenario added later cleared the compile error by returning
    /// it, no snapshot had to exist, and the suite went on reporting exhaustive
    /// coverage — the same failure the count had, one level along. The cheapest
    /// way to discharge the decision is now writing the test.
    func testEveryRenderedScenarioNamesASnapshotProducingTest() {
        XCTAssertEqual(
            Self.claimedNamesMissingFromTheSuite,
            [],
            """
            A scenario says it is rendered by a test this class does not \
            define. Write the test, or refuse the scenario with a reason.
            """
        )
    }

    /// No two rendered scenarios name the same test.
    ///
    /// The existence check alone would accept a new scenario pointing at a test
    /// that already answers for a different one, which is the same uncovered
    /// case reached by a shorter edit. Together the two make a new `.rendered`
    /// require a test that exists and answers for nothing else.
    func testNoTwoRenderedScenariosClaimTheSameTest() {
        XCTAssertEqual(
            Self.claimedNamesUsedMoreThanOnce,
            [],
            """
            One test is claimed by more than one scenario, so at least one of \
            them has no rendering of its own. Give it a test, or refuse it.
            """
        )
    }

    /// The one refused scenario states a reason.
    ///
    /// The exhaustive switch above is what makes a new case a decision; this is
    /// what keeps `.refused` from being the cheap way to discharge it. It can
    /// only catch an empty string — no assertion can judge whether an argument
    /// is a good one — so it is a floor and the review of the reason is the
    /// rest.
    ///
    /// One named test rather than a scan over `allCases`:
    /// `docs/agents/testing-style.md` rule 2 unrolls a loop over same-behaviour
    /// cases into straight-line calls, and a failure here names the scenario
    /// that caused it instead of reporting from one anonymous site inside a
    /// `for` body. `.artifactPreview` is the only case `ScenarioDisposition`
    /// refuses, so it is the only test; a second refusal gets a second test,
    /// and the switch is what puts that decision in front of whoever adds one.
    func testTheArtifactPreviewRefusalStatesAReason() {
        XCTAssertTrue(
            ScenarioDisposition.of(.artifactPreview).statesARefusalReason,
            """
            .artifactPreview is refused without a reason. A scenario this suite \
            does not render carries the argument for why no golden is the right \
            outcome; write it, or render the scenario.
            """
        )
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
    /// Every canvas, including the portrait phone one that records the form
    /// clipped — because that clipping is the application's and not the
    /// canvas's. `ProcessSessionCreationSheet` declares `minWidth: 520`, and a
    /// sheet presented on a horizontally compact phone gets that phone's width:
    /// 390 points here, 402 on the `iPhone 17 Pro` this suite pins, and no
    /// shipping iPhone reaches 520 in portrait. So the golden reading "cel" and
    /// "Cr" where its buttons are, and "w Session" where its title is, is what
    /// a portrait phone shows, and a reference for it is worth having twice
    /// over: it is the only record of that presentation, and it is what will
    /// change the day the declared minimum is reconciled with the devices the
    /// sheet is presented on.
    ///
    /// It was skipped until the review wave on debaa425 argued the opposite —
    /// that the canvas was cutting into a presentation a device would not cut.
    /// The arithmetic is what settles it: 390 and 402 are both far below 520,
    /// so the canvas is reproducing the clip rather than causing it. What
    /// `SnapshotCanvas.sheet` gives, at 540 points, is the content at the width
    /// it asks for, which no phone canvas can be; that is a second question and
    /// `testSessionCreationSheetContent` is where it is asked.
    ///
    /// The landscape phone canvas is not skipped, and the reason it once was
    /// did not survive being looked at. It is 844 points wide, so nothing
    /// clips; it is vertically compact, so the sheet presents full-screen the
    /// way an iPhone in landscape presents one, and the golden shows the whole
    /// form legible with the list fully covered. That is a presentation neither
    /// iPad canvas records — those get the centred form sheet over a dimmed
    /// list — so skipping it was dropping the only reference for a shape the
    /// application ships.
    ///
    /// It also answers a question about this renderer worth writing down. The
    /// suite runs in one portrait scene and never rotates it, so a fair worry
    /// is that a landscape canvas records portrait presentation behaviour at
    /// landscape dimensions. This golden is the measurement: an 844-point-wide
    /// full-screen sheet on a portrait scene is presentation geometry following
    /// the canvas and the overridden traits, not the scene. See the note on
    /// `SnapshotCanvas.verticalSizeClass`.
    ///
    /// The number is this screen's and not the legacy one's. `CreateSessionSheet`
    /// declares `minWidth: 420`, and it is what
    /// `testLegacySessionCreationSheetContent` renders; reading that value here
    /// would put the clipping threshold 100 points low and make a canvas that
    /// still clips look like one that fits.
    func testSessionListPresentingTheCreationSheet() async {
        await assertLiveScreenSnapshot(of: rootView(for: .newSession), canvas: .iPhonePortrait)
        await assertLiveScreenSnapshot(of: rootView(for: .newSession), canvas: .iPhoneLandscape)
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
        await assertLiveScreenSnapshot(
            of: rootView(for: .sessions, selecting: .templates),
            canvas: .iPadPortrait
        )
        await assertLiveScreenSnapshot(
            of: rootView(for: .sessions, selecting: .templates),
            canvas: .iPadLandscape
        )
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

    /// `RootView` on the section the scenario itself selects.
    ///
    /// The common case, and the one where naming the section at the call site
    /// would restate what the scenario already decides: `AppCoordinator` sets
    /// `selectedSection` from `screenshotScenario.selectedSection`, so a
    /// section named here would agree with it or silently disagree.
    private func rootView(for scenario: ScreenshotScenario) -> some View {
        rootView(for: scenario, selecting: scenario.selectedSection)
    }

    /// `RootView` on a section the scenario does not select.
    ///
    /// Both values stay at the call site because both decide what is
    /// snapshotted: the scenario picks the fixtures, and the section picks the
    /// screen they are shown on. This helper wires the coordinator and nothing
    /// else — `docs/agents/testing-style.md` rule 16 — and it has no branch, so
    /// the overload above is what supplies the ordinary section rather than a
    /// default argument deciding it here.
    private func rootView(
        for scenario: ScreenshotScenario,
        selecting section: AppSection
    ) -> some View {
        let coordinator = AppCoordinator(
            isMockMode: scenario.requiresMockService,
            screenshotScenario: scenario
        )
        coordinator.selectedSection = section
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
