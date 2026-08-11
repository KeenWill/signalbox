#if os(iOS)
import SwiftUI
import XCTest

@testable import SignalboxNative

/// The four canvases every scenario is promised on.
///
/// Named once because all fourteen dispositions declare it and a repeated
/// literal set would be fourteen places to disagree. A scenario that ever needs
/// a different set spells that set instead, and the difference is then visible
/// against this name.
extension Set where Element == SnapshotCanvas {
    static let everyScreenCanvas: Set<SnapshotCanvas> = [
        .iPhonePortrait, .iPhoneLandscape, .iPadPortrait, .iPadLandscape,
    ]
}

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
    /// What this test rendered, recorded by `assertScenarioSnapshot` as it went.
    ///
    /// XCTest builds a fresh instance per test method, so this is per-test
    /// without any resetting: `tearDown` reads exactly what the method that
    /// just ran produced. It is the evidence the two structural checks are
    /// made of — a claim can no longer be satisfied by a method that exists and
    /// happens to have PNGs, because the rendering itself is what reports.
    private var scenarioRenderings: [(scenario: ScreenshotScenario, canvas: SnapshotCanvas)] = []

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
        /// The association is now checked at the point of rendering, which is
        /// what the name alone could not do. `assertScenarioSnapshot` compares
        /// the running test's `#function` against the name here before it
        /// renders anything, and `tearDown` requires a test named here to have
        /// rendered this scenario on exactly `canvases`. A claim pointing at a
        /// test that renders something else — a legacy screen, a section gate —
        /// fails when that test runs, rather than passing because the method
        /// exists and has PNGs.
        case rendered(by: String, on: Set<SnapshotCanvas>)
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
                return .rendered(by: "testTransportGateWithoutAConfiguredSocket", on: .everyScreenCanvas)
            case .sessions:
                return .rendered(by: "testSessionList", on: .everyScreenCanvas)
            case .newSession:
                return .rendered(by: "testSessionListPresentingTheCreationSheet", on: .everyScreenCanvas)
            case .activeChat:
                return .rendered(by: "testSessionTranscriptForACompletedTurn", on: .everyScreenCanvas)
            case .markdownBasics:
                return .rendered(by: "testSessionTranscriptRenderingMarkdownHeadingsAndLists", on: .everyScreenCanvas)
            case .markdownTable:
                return .rendered(by: "testSessionTranscriptRenderingAMarkdownTable", on: .everyScreenCanvas)
            case .markdownCode:
                return .rendered(by: "testSessionTranscriptRenderingMarkdownCodeAndQuotes", on: .everyScreenCanvas)
            case .markdownMessage:
                return .rendered(by: "testSessionTranscriptRenderingAMarkdownIncidentReport", on: .everyScreenCanvas)
            case .pendingApproval:
                return .rendered(by: "testSessionTranscriptWithAToolRequestAwaitingApproval", on: .everyScreenCanvas)
            case .completedTool:
                return .rendered(by: "testSessionTranscriptWithACompletedTool", on: .everyScreenCanvas)
            case .failedTool:
                return .rendered(by: "testSessionTranscriptWithAFailedTool", on: .everyScreenCanvas)
            case .runners:
                return .rendered(by: "testRunnersCapabilityGate", on: .everyScreenCanvas)
            case .monitor:
                return .rendered(by: "testMonitorCapabilityGate", on: .everyScreenCanvas)
            case .settings:
                return .rendered(by: "testSettingsScreen", on: .everyScreenCanvas)
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

        /// The test this disposition names, or `nil` for a refusal.
        var renderingTestName: String? {
            switch self {
            case .rendered(let name, _):
                return name
            case .refused:
                return nil
            }
        }

        /// The canvases this disposition promises, or `nil` for a refusal.
        var declaredCanvases: Set<SnapshotCanvas>? {
            switch self {
            case .rendered(_, let canvases):
                return canvases
            case .refused:
                return nil
            }
        }

        /// The reason a refusal states, or `nil` for a rendering.
        ///
        /// Optional rather than a `Bool`, so "renders" and "refuses blankly"
        /// stay distinguishable: the predecessor collapsed both into `false`
        /// and so could not be used to find the blank refusals among every
        /// disposition.
        var refusalReason: String? {
            switch self {
            case .rendered:
                return nil
            case .refused(let reason):
                return reason
            }
        }
    }

    /// Every scenario beside its disposition.
    static var everyDisposition: [(scenario: ScreenshotScenario, disposition: ScenarioDisposition)] {
        ScreenshotScenario.allCases.map { ($0, ScenarioDisposition.of($0)) }
    }

    /// The scenarios whose refusal states no reason.
    ///
    /// Takes its input so the known-answer test can hand it a blank refusal:
    /// run only over the suite's own dispositions it would answer `[]`, which
    /// is the answer the coverage test wants, and an implementation that always
    /// answered `[]` would be indistinguishable from a working one.
    static func scenariosRefusedWithoutAReason(
        in dispositions: [(scenario: ScreenshotScenario, disposition: ScenarioDisposition)]
    ) -> [String] {
        dispositions
            .compactMap { pair -> String? in
                guard let reason = pair.disposition.refusalReason else { return nil }
                let stated = reason.trimmingCharacters(in: .whitespacesAndNewlines)
                return stated.isEmpty ? String(describing: pair.scenario) : nil
            }
            .sorted()
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
    /// `testTheMembershipDetectorReportsAMissingName` can hand it an input
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
        Set(defaultTestSuite.tests.map { methodName(ofTestCaseName: $0.name) })
    }

    /// The method out of an `XCTest.name`, which reads `-[Class testFoo]`.
    ///
    /// Shared by `definedTestNames` and `tearDown` so the two cannot disagree
    /// about what a test is called, and parameterized so
    /// `testTheTestCaseNameParserReadsTheMethod` can hand it a known input.
    static func methodName(ofTestCaseName name: String) -> String {
        String(name.split(separator: " ").last ?? "")
            .trimmingCharacters(in: CharacterSet(charactersIn: "[]"))
    }

    /// The names no test on this class defines.
    /// The test a golden's file name belongs to, or `nil` if it names none.
    ///
    /// A golden is written as `<test>.<canvas>.png`, so the first
    /// dot-separated component is the method that recorded it. The `test`
    /// prefix is required because a directory holds other files —
    /// `MANIFEST.sha256` next door is one — and a name that cannot be a test
    /// method should not become an entry in an inventory of them.
    static func goldenIdentity(ofFileNamed fileName: String) -> (test: String, canvas: String)? {
        guard fileName.hasSuffix(".png") else { return nil }
        let components = fileName.split(separator: ".")
        guard components.count >= 3 else { return nil }
        let name = String(components[0])
        guard name.hasPrefix("test") else { return nil }
        return (name, String(components[1]))
    }

    /// The test a golden belongs to, discarding its canvas.
    static func testName(fromGoldenNamed fileName: String) -> String? {
        goldenIdentity(ofFileNamed: fileName)?.test
    }

    /// The tests that have at least one committed golden.
    ///
    /// Read off `__Snapshots__` rather than listed, because a list would be the
    /// hand-maintained inventory this file has twice replaced with something
    /// derived. The directory is the same one the assertions read their
    /// references from — `#filePath` locates it exactly as
    /// `assertSnapshot` does — so a name is in here when, and only when, a
    /// rendering for it is committed.
    private static var committedGoldenNames: [String] {
        let root = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .appendingPathComponent("__Snapshots__")
        let manager = FileManager.default
        let suiteDirectories =
            (try? manager.contentsOfDirectory(at: root, includingPropertiesForKeys: nil)) ?? []
        return suiteDirectories
            .flatMap { (try? manager.contentsOfDirectory(atPath: $0.path)) ?? [] }
    }

    /// The canvases each test has a committed golden for.
    ///
    /// The canvas is kept rather than discarded, which the predecessor did not
    /// do: it reduced every file to a test name, so a test that stopped
    /// rendering one canvas still appeared in the inventory — its stale PNG was
    /// on disk and the name had not changed. Keeping the suffix is what lets
    /// `testEveryRenderedScenarioHasAGoldenForEveryDeclaredCanvas` compare what
    /// is committed against what the disposition promises.
    static func canvasesByTest(inGoldensNamed fileNames: [String]) -> [String: Set<String>] {
        var canvases: [String: Set<String>] = [:]
        for fileName in fileNames {
            guard let identity = goldenIdentity(ofFileNamed: fileName) else { continue }
            canvases[identity.test, default: []].insert(identity.canvas)
        }
        return canvases
    }

    static var committedCanvasesByTest: [String: Set<String>] {
        canvasesByTest(inGoldensNamed: committedGoldenNames)
    }

    /// The canvases each claimed test promises, by test name.
    static var declaredCanvasesByTest: [String: Set<String>] {
        var declared: [String: Set<String>] = [:]
        for pair in everyDisposition {
            guard let test = pair.disposition.renderingTestName,
                let canvases = pair.disposition.declaredCanvases
            else { continue }
            declared[test] = Set(canvases.map(\.rawValue))
        }
        return declared
    }

    /// The claimed tests whose committed goldens differ from what they promise.
    ///
    /// Both directions matter. Fewer goldens than declared is a canvas that was
    /// never recorded; more is a golden left behind by a canvas the body
    /// stopped rendering, which is the case the review raised — the file stays
    /// on disk and nothing else notices.
    static func testsWhoseGoldensDoNotMatchTheirDeclaration(
        declared: [String: Set<String>],
        committed: [String: Set<String>]
    ) -> [String] {
        declared
            .filter { test, canvases in (committed[test] ?? []) != canvases }
            .map(\.key)
            .sorted()
    }

    private static var snapshotProducingTestNames: Set<String> {
        Set(committedCanvasesByTest.keys)
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
    /// reasons in `testTheBlankRefusalDetectorReportsABlankReason`: those are
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

    /// The blank-refusal detector reports a refusal with no reason.
    ///
    /// Known-answer half. The predecessor asserted a property of the one
    /// refusal the suite happens to have, which a version hardcoded to `true`
    /// satisfied — mutation found that, and the same shape would return here if
    /// this only ever saw dispositions that are already fine. The empty and
    /// whitespace reasons are the whole point: they are what a working
    /// implementation reports and a broken one cannot.
    func testTheBlankRefusalDetectorReportsABlankReason() {
        XCTAssertEqual(
            Self.scenariosRefusedWithoutAReason(in: [(.setup, .refused(reason: ""))]),
            ["setup"]
        )
        XCTAssertEqual(
            Self.scenariosRefusedWithoutAReason(in: [(.setup, .refused(reason: "   \n\t "))]),
            ["setup"]
        )
        XCTAssertEqual(
            Self.scenariosRefusedWithoutAReason(
                in: [(.setup, .refused(reason: ArbitraryClaim.statedReason))]
            ),
            []
        )
        XCTAssertEqual(
            Self.scenariosRefusedWithoutAReason(
                in: [(.setup, .rendered(by: ArbitraryClaim.first, on: .everyScreenCanvas))]
            ),
            []
        )
        XCTAssertEqual(Self.scenariosRefusedWithoutAReason(in: []), [])
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
                .rendered(by: ArbitraryClaim.first, on: .everyScreenCanvas),
                .refused(reason: ArbitraryClaim.statedReason),
                .rendered(by: ArbitraryClaim.second, on: .everyScreenCanvas),
            ]),
            [ArbitraryClaim.first, ArbitraryClaim.second]
        )
        XCTAssertEqual(
            Self.renderingTestNames(of: [.rendered(by: ArbitraryClaim.first, on: .everyScreenCanvas)]),
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
    func testTheGoldenNameParserReadsTheTestAndCanvas() {
        XCTAssertEqual(
            Self.goldenIdentity(ofFileNamed: "testSessionList.iphone-portrait.png")?.test,
            "testSessionList"
        )
        XCTAssertEqual(
            Self.goldenIdentity(ofFileNamed: "testSessionList.iphone-portrait.png")?.canvas,
            "iphone-portrait"
        )
        XCTAssertEqual(
            Self.goldenIdentity(ofFileNamed: "testSessionCreationSheetContent.sheet.png")?.canvas,
            "sheet"
        )
        XCTAssertNil(Self.goldenIdentity(ofFileNamed: "MANIFEST.sha256"))
        XCTAssertNil(Self.goldenIdentity(ofFileNamed: "notATestMethod.iphone-portrait.png"))
    }

    /// The canvas index groups a test's goldens together.
    func testTheCanvasIndexGroupsGoldensByTest() {
        XCTAssertEqual(
            Self.canvasesByTest(inGoldensNamed: [
                "testOne.iphone-portrait.png", "testOne.ipad-portrait.png",
                "testTwo.sheet.png", "MANIFEST.sha256",
            ]),
            [
                "testOne": ["iphone-portrait", "ipad-portrait"],
                "testTwo": ["sheet"],
            ]
        )
        XCTAssertEqual(Self.canvasesByTest(inGoldensNamed: []), [:])
    }

    /// The declaration comparison reports a canvas that is missing or left over.
    func testTheCanvasComparisonReportsADifference() {
        XCTAssertEqual(
            Self.testsWhoseGoldensDoNotMatchTheirDeclaration(
                declared: ["testOne": ["iphone-portrait", "ipad-portrait"]],
                committed: ["testOne": ["iphone-portrait"]]
            ),
            ["testOne"]
        )
        XCTAssertEqual(
            Self.testsWhoseGoldensDoNotMatchTheirDeclaration(
                declared: ["testOne": ["iphone-portrait"]],
                committed: ["testOne": ["iphone-portrait", "ipad-portrait"]]
            ),
            ["testOne"]
        )
        XCTAssertEqual(
            Self.testsWhoseGoldensDoNotMatchTheirDeclaration(
                declared: ["testOne": ["iphone-portrait"]],
                committed: ["testOne": ["iphone-portrait"]]
            ),
            []
        )
        XCTAssertEqual(
            Self.testsWhoseGoldensDoNotMatchTheirDeclaration(declared: [:], committed: [:]),
            []
        )
    }

    /// The test-case name parser reads the method out of `-[Class testFoo]`.
    func testTheTestCaseNameParserReadsTheMethod() {
        XCTAssertEqual(
            Self.methodName(ofTestCaseName: "-[LiveScreenSnapshotTests testSessionList]"),
            "testSessionList"
        )
        XCTAssertEqual(Self.methodName(ofTestCaseName: "testSessionList"), "testSessionList")
    }

    /// The function-name parser drops the parentheses `#function` carries.
    func testTheFunctionNameParserDropsTheParentheses() {
        XCTAssertEqual(Self.methodName(ofFunction: "testSessionList()"), "testSessionList")
        XCTAssertEqual(Self.methodName(ofFunction: "testSessionList"), "testSessionList")
    }

    /// The renderer check accepts the claimed test and rejects any other.
    func testTheRendererCheckAcceptsOnlyTheClaimedTest() {
        XCTAssertTrue(Self.test("testSessionList", isTheClaimedRendererOf: .sessions))
        XCTAssertFalse(Self.test("testLegacySessionList", isTheClaimedRendererOf: .sessions))
        XCTAssertFalse(Self.test("testSettingsScreen", isTheClaimedRendererOf: .sessions))
        XCTAssertFalse(Self.test("testSessionList", isTheClaimedRendererOf: .artifactPreview))
    }

    /// The claim lookup finds the scenario that names a test, and only then.
    ///
    /// `tearDown` returns early when this answers `nil`, so a version that
    /// always did would disable the whole converse check silently — which is
    /// what the mutation sweep found before this test existed.
    func testTheClaimLookupFindsTheScenarioThatNamesATest() {
        XCTAssertEqual(Self.scenarioClaimedBy("testSessionList"), .sessions)
        XCTAssertEqual(Self.scenarioClaimedBy("testSettingsScreen"), .settings)
        XCTAssertNil(Self.scenarioClaimedBy("testTheDuplicateDetectorReportsADuplicate"))
        XCTAssertNil(Self.scenarioClaimedBy("testLegacySessionList"))
    }

    /// The declaration index covers the claimed tests and nothing else.
    ///
    /// An empty index would make the canvas comparison compare nothing and
    /// report nothing, which is the answer that check wants.
    func testTheDeclarationIndexCoversEveryClaimedTest() {
        XCTAssertEqual(
            Self.declaredCanvasesByTest["testSessionList"],
            ["iphone-portrait", "iphone-landscape", "ipad-portrait", "ipad-landscape"]
        )
        XCTAssertNil(Self.declaredCanvasesByTest["testTheDuplicateDetectorReportsADuplicate"])
        XCTAssertNil(Self.declaredCanvasesByTest["testTemplatesCapabilityGate"])
    }

    /// Every claimed test has a golden for every canvas it promises.
    ///
    /// The inventory check says a claimed test records something; this says it
    /// records exactly what its disposition promises. Without it a canvas
    /// dropped from a body leaves its golden on disk and every other guard
    /// stays green.
    func testEveryRenderedScenarioHasAGoldenForEveryDeclaredCanvas() {
        XCTAssertEqual(
            Self.testsWhoseGoldensDoNotMatchTheirDeclaration(
                declared: Self.declaredCanvasesByTest,
                committed: Self.committedCanvasesByTest
            ),
            [],
            """
            A claimed test's committed goldens do not match the canvases its \
            disposition promises. A canvas removed from the body leaves its \
            golden behind; a canvas added needs one recorded.
            """
        )
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
    /// No scenario is refused without a reason — every one of them, not one.
    ///
    /// The predecessor inspected `.artifactPreview` alone, so a scenario added
    /// later could return `.refused(reason: "")`, compile, be ignored by the
    /// rendered-name checks, and leave this green. The refusal is derived from
    /// every disposition now, so the one-line way out is closed.
    func testNoScenarioIsRefusedWithoutAReason() {
        XCTAssertEqual(
            Self.scenariosRefusedWithoutAReason(in: Self.everyDisposition),
            [],
            """
            A scenario is refused without a reason. A scenario this suite does \
            not render carries the argument for why no golden is the right \
            outcome; write it, or render the scenario.
            """
        )
    }

    func testTransportGateWithoutAConfiguredSocket() async {
        await assertScenarioSnapshot(.setup, canvas: .iPhonePortrait)
        await assertScenarioSnapshot(.setup, canvas: .iPhoneLandscape)
        await assertScenarioSnapshot(.setup, canvas: .iPadPortrait)
        await assertScenarioSnapshot(.setup, canvas: .iPadLandscape)
    }

    func testSessionList() async {
        await assertScenarioSnapshot(.sessions, canvas: .iPhonePortrait)
        await assertScenarioSnapshot(.sessions, canvas: .iPhoneLandscape)
        await assertScenarioSnapshot(.sessions, canvas: .iPadPortrait)
        await assertScenarioSnapshot(.sessions, canvas: .iPadLandscape)
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
        await assertScenarioSnapshot(.newSession, canvas: .iPhonePortrait)
        await assertScenarioSnapshot(.newSession, canvas: .iPhoneLandscape)
        await assertScenarioSnapshot(.newSession, canvas: .iPadPortrait)
        await assertScenarioSnapshot(.newSession, canvas: .iPadLandscape)
    }

    /// Named for the state it renders, not the fixture it uses. `.activeChat`
    /// names the screen, but the transcript it serves carries a turn whose
    /// `state.type` is `completed`, and the golden says "Completed" across the
    /// header. A turn still in flight renders a different header and a
    /// different usage card, and nothing here covers it — a test named for an
    /// active turn would have claimed otherwise while pinning this.
    func testSessionTranscriptForACompletedTurn() async {
        await assertScenarioSnapshot(.activeChat, canvas: .iPhonePortrait)
        await assertScenarioSnapshot(.activeChat, canvas: .iPhoneLandscape)
        await assertScenarioSnapshot(.activeChat, canvas: .iPadPortrait)
        await assertScenarioSnapshot(.activeChat, canvas: .iPadLandscape)
    }

    /// The four markdown scenarios are four different transcripts, not four
    /// renderings of one: each names its own fixture session, and between them
    /// they cover the constructs the transcript renderer handles separately —
    /// headings and lists, tables and links, fenced code and block quotes, and
    /// a long mixed document. A change to any one renderer shows up in one of
    /// these and not the others, which is why they are four tests.
    func testSessionTranscriptRenderingMarkdownHeadingsAndLists() async {
        await assertScenarioSnapshot(.markdownBasics, canvas: .iPhonePortrait)
        await assertScenarioSnapshot(.markdownBasics, canvas: .iPhoneLandscape)
        await assertScenarioSnapshot(.markdownBasics, canvas: .iPadPortrait)
        await assertScenarioSnapshot(.markdownBasics, canvas: .iPadLandscape)
    }

    func testSessionTranscriptRenderingAMarkdownTable() async {
        await assertScenarioSnapshot(.markdownTable, canvas: .iPhonePortrait)
        await assertScenarioSnapshot(.markdownTable, canvas: .iPhoneLandscape)
        await assertScenarioSnapshot(.markdownTable, canvas: .iPadPortrait)
        await assertScenarioSnapshot(.markdownTable, canvas: .iPadLandscape)
    }

    func testSessionTranscriptRenderingMarkdownCodeAndQuotes() async {
        await assertScenarioSnapshot(.markdownCode, canvas: .iPhonePortrait)
        await assertScenarioSnapshot(.markdownCode, canvas: .iPhoneLandscape)
        await assertScenarioSnapshot(.markdownCode, canvas: .iPadPortrait)
        await assertScenarioSnapshot(.markdownCode, canvas: .iPadLandscape)
    }

    func testSessionTranscriptRenderingAMarkdownIncidentReport() async {
        await assertScenarioSnapshot(.markdownMessage, canvas: .iPhonePortrait)
        await assertScenarioSnapshot(.markdownMessage, canvas: .iPhoneLandscape)
        await assertScenarioSnapshot(.markdownMessage, canvas: .iPadPortrait)
        await assertScenarioSnapshot(.markdownMessage, canvas: .iPadLandscape)
    }

    func testSessionTranscriptWithAToolRequestAwaitingApproval() async {
        await assertScenarioSnapshot(.pendingApproval, canvas: .iPhonePortrait)
        await assertScenarioSnapshot(.pendingApproval, canvas: .iPhoneLandscape)
        await assertScenarioSnapshot(.pendingApproval, canvas: .iPadPortrait)
        await assertScenarioSnapshot(.pendingApproval, canvas: .iPadLandscape)
    }

    func testSessionTranscriptWithACompletedTool() async {
        await assertScenarioSnapshot(.completedTool, canvas: .iPhonePortrait)
        await assertScenarioSnapshot(.completedTool, canvas: .iPhoneLandscape)
        await assertScenarioSnapshot(.completedTool, canvas: .iPadPortrait)
        await assertScenarioSnapshot(.completedTool, canvas: .iPadLandscape)
    }

    func testSessionTranscriptWithAFailedTool() async {
        await assertScenarioSnapshot(.failedTool, canvas: .iPhonePortrait)
        await assertScenarioSnapshot(.failedTool, canvas: .iPhoneLandscape)
        await assertScenarioSnapshot(.failedTool, canvas: .iPadPortrait)
        await assertScenarioSnapshot(.failedTool, canvas: .iPadLandscape)
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
        await assertScenarioSnapshot(.monitor, canvas: .iPhonePortrait)
        await assertScenarioSnapshot(.monitor, canvas: .iPhoneLandscape)
        await assertScenarioSnapshot(.monitor, canvas: .iPadPortrait)
        await assertScenarioSnapshot(.monitor, canvas: .iPadLandscape)
    }

    func testRunnersCapabilityGate() async {
        await assertScenarioSnapshot(.runners, canvas: .iPhonePortrait)
        await assertScenarioSnapshot(.runners, canvas: .iPhoneLandscape)
        await assertScenarioSnapshot(.runners, canvas: .iPadPortrait)
        await assertScenarioSnapshot(.runners, canvas: .iPadLandscape)
    }

    func testSettingsScreen() async {
        await assertScenarioSnapshot(.settings, canvas: .iPhonePortrait)
        await assertScenarioSnapshot(.settings, canvas: .iPhoneLandscape)
        await assertScenarioSnapshot(.settings, canvas: .iPadPortrait)
        await assertScenarioSnapshot(.settings, canvas: .iPadLandscape)
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

    /// Renders a scenario and records that this test is what rendered it.
    ///
    /// The binding the disposition's name could not provide. `#function` is the
    /// method actually running, so comparing it against the claimed name here
    /// fails a scenario pointed at a test that renders something else — the
    /// case the review raised, where a claim on `testLegacySessionList` passed
    /// every guard because that method exists and has committed PNGs.
    ///
    /// Only scenario renderings go through here. `testTemplatesCapabilityGate`
    /// and `testSessionCreationSheetContent` call `assertLiveScreenSnapshot`
    /// directly and deliberately: neither is a scenario's own rendering — one
    /// shows a section the scenario does not select, the other a sheet's
    /// content standing alone — and routing them through this would claim they
    /// were.
    private func assertScenarioSnapshot(
        _ scenario: ScreenshotScenario,
        canvas: SnapshotCanvas,
        testName: String = #function,
        file: StaticString = #filePath,
        line: UInt = #line
    ) async {
        let runningTest = Self.methodName(ofFunction: testName)
        XCTAssertTrue(
            Self.test(runningTest, isTheClaimedRendererOf: scenario),
            """
            \(scenario) is rendered by \(runningTest), but its disposition \
            names a different test. A scenario and the test that renders it \
            have to agree; fix whichever is wrong.
            """,
            file: file,
            line: line
        )
        scenarioRenderings.append((scenario, canvas))
        // Forwarded rather than defaulted: `assertLiveScreenSnapshot` derives a
        // golden's name from `#function`, so leaving it to default here would
        // name every reference after this wrapper instead of after the test.
        await assertLiveScreenSnapshot(
            of: rootView(for: scenario),
            canvas: canvas,
            file: file,
            testName: testName,
            line: line
        )
    }

    /// `#function` without its parentheses.
    ///
    /// `#function` in a method reads `testSessionList()`; the disposition names
    /// the method. Parameterized so
    /// `testTheFunctionNameParserDropsTheParentheses` can hand it a known input,
    /// because a version returning the empty string would make every comparison
    /// above fail loudly rather than silently — but one returning its argument
    /// unchanged would make them all fail too, and neither is what this should
    /// do.
    static func methodName(ofFunction function: String) -> String {
        guard let parenthesis = function.firstIndex(of: "(") else { return function }
        return String(function[function.startIndex..<parenthesis])
    }

    /// Requires the test that just ran to have rendered what it claims.
    ///
    /// The other half of the binding, and the half that catches a claim never
    /// exercised at all. `assertScenarioSnapshot` establishes that a rendered
    /// scenario is claimed by the test rendering it; this establishes the
    /// converse — a test named by a disposition rendered that scenario, on
    /// exactly the canvases the disposition promises. A scenario pointed at a
    /// legacy test or a section gate fails here when that test runs, because it
    /// rendered no scenario at all.
    ///
    /// Per test rather than per suite, which is what makes it safe: it needs no
    /// ordering between tests and no assumption that every test ran, so a
    /// `-only-testing` run checks exactly the tests it selected.
    override func tearDown() {
        verifyThisTestRenderedWhatItClaims()
        super.tearDown()
    }

    private func verifyThisTestRenderedWhatItClaims() {
        let runningTest = Self.methodName(ofTestCaseName: name)
        guard let claimed = Self.scenarioClaimedBy(runningTest) else { return }
        let disposition = ScenarioDisposition.of(claimed)
        XCTAssertEqual(
            Set(scenarioRenderings.filter { $0.scenario == claimed }.map(\.canvas)),
            disposition.declaredCanvases ?? [],
            """
            \(runningTest) is named by \(claimed) but did not render it on the \
            canvases that scenario promises. A canvas dropped from the body \
            leaves its committed golden on disk, so nothing else notices.
            """
        )
    }

    /// Whether this test is the one the scenario's disposition names.
    ///
    /// Extracted from `assertScenarioSnapshot` so it has a known-answer test:
    /// inside the assertion it was only ever evaluated on agreeing pairs, so a
    /// version returning `true` was indistinguishable from a working one — the
    /// vacuity this file has now hit three times.
    static func test(_ testName: String, isTheClaimedRendererOf scenario: ScreenshotScenario)
        -> Bool
    {
        ScenarioDisposition.of(scenario).renderingTestName == testName
    }

    /// The scenario whose disposition names this test, if any.
    static func scenarioClaimedBy(_ testName: String) -> ScreenshotScenario? {
        ScreenshotScenario.allCases.first { scenario in
            ScenarioDisposition.of(scenario).renderingTestName == testName
        }
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
