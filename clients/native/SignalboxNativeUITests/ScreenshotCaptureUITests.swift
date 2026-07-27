import XCTest

final class ScreenshotCaptureUITests: XCTestCase {
    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    @MainActor
    func testCaptureScreenshotMatrix() throws {
        let outputDirectory = try screenshotOutputDirectory()
        try FileManager.default.createDirectory(at: outputDirectory, withIntermediateDirectories: true)
        try captureScreenshots(
            try ScreenshotCaptureSpecification.selectedByEnvironment(outputDirectory: outputDirectory),
            in: outputDirectory
        )
    }

    @MainActor
    private func captureScreenshots(
        _ specifications: [ScreenshotCaptureSpecification],
        in outputDirectory: URL
    ) throws {
        setLandscapeOrientationIfSupported()
        for specification in specifications {
            try captureScreenshot(specification, in: outputDirectory)
        }
    }

    @MainActor
    private func captureScreenshot(
        _ specification: ScreenshotCaptureSpecification,
        in outputDirectory: URL
    ) throws {
        let app = XCUIApplication()
        app.launchArguments = ["--screenshot-state", specification.state]
        app.launchArguments.append(contentsOf: specification.presentationArguments)
        app.launch()

        setLandscapeOrientationIfSupported()
        XCTAssertTrue(app.descendants(matching: .any)["signalbox-root"].waitForExistence(timeout: 20))
        if let expectedLandmark = specification.expectedLandmark {
            XCTAssertTrue(
                app.descendants(matching: .any)[expectedLandmark].waitForExistence(timeout: 20),
                "Screenshot scenario \(specification.name) did not present \(expectedLandmark)."
            )
        }
        Thread.sleep(forTimeInterval: specification.settleTime)

        let screenshotData = XCUIScreen.main.screenshot().pngRepresentation
        let outputURL = outputDirectory.appendingPathComponent("\(specification.name).png")
        try screenshotData.write(to: outputURL, options: .atomic)

        app.terminate()
    }

    private func screenshotOutputDirectory() throws -> URL {
        if let outputDirectoryPath = ProcessInfo.processInfo.environment["SIGNALBOX_NATIVE_SCREENSHOT_OUTPUT_DIR"], !outputDirectoryPath.isEmpty {
            return URL(fileURLWithPath: outputDirectoryPath, isDirectory: true)
        }

        let projectRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
        let captureOutputFile = projectRoot
            .appendingPathComponent("Screenshots", isDirectory: true)
            .appendingPathComponent(".capture-output-dir")
        guard FileManager.default.fileExists(atPath: captureOutputFile.path) else {
            throw XCTSkip("Screenshot capture output path is not configured.")
        }
        let configuredPath = try String(contentsOf: captureOutputFile, encoding: .utf8)
            .trimmingCharacters(in: .whitespacesAndNewlines)
        guard !configuredPath.isEmpty else {
            throw XCTSkip("Screenshot capture output path is empty.")
        }
        return URL(fileURLWithPath: configuredPath, isDirectory: true)
    }

    private func setLandscapeOrientationIfSupported() {
        #if os(iOS)
        XCUIDevice.shared.orientation = .landscapeLeft
        #endif
    }
}

private struct ScreenshotCaptureSpecification {
    let name: String
    let state: String
    let presentationArguments: [String]
    let settleTime: TimeInterval
    let expectedLandmark: String?

    init(
        name: String,
        state: String,
        presentationArguments: [String],
        settleTime: TimeInterval,
        expectedLandmark: String? = nil
    ) {
        self.name = name
        self.state = state
        self.presentationArguments = presentationArguments
        self.settleTime = settleTime
        self.expectedLandmark = expectedLandmark
    }

    static let all: [ScreenshotCaptureSpecification] = [
        .init(name: "setup", state: "setup", presentationArguments: [], settleTime: 2.0),
        .init(name: "sessions", state: "sessions", presentationArguments: [], settleTime: 2.0),
        .init(
            name: "new-session",
            state: "new-session",
            presentationArguments: [],
            settleTime: 2.5,
            expectedLandmark: "Creation unavailable"
        ),
        .init(name: "active-chat", state: "active-chat", presentationArguments: [], settleTime: 2.0),
        .init(
            name: "markdown-basics",
            state: "markdown-basics",
            presentationArguments: [],
            settleTime: 2.0,
            expectedLandmark: "Incident Handoff"
        ),
        .init(name: "markdown-table", state: "markdown-table", presentationArguments: [], settleTime: 2.0),
        .init(name: "markdown-code", state: "markdown-code", presentationArguments: [], settleTime: 2.0),
        .init(name: "markdown-message", state: "markdown-message", presentationArguments: [], settleTime: 2.0),
        .init(name: "pending-approval", state: "pending-approval", presentationArguments: [], settleTime: 2.0),
        .init(name: "completed-tool", state: "completed-tool", presentationArguments: [], settleTime: 2.0),
        .init(name: "failed-tool", state: "failed-tool", presentationArguments: [], settleTime: 2.0),
        .init(
            name: "artifact-preview",
            state: "artifact-preview",
            presentationArguments: [],
            settleTime: 2.5,
            expectedLandmark: "Artifacts unavailable"
        ),
        .init(name: "runners", state: "runners", presentationArguments: [], settleTime: 2.0),
        .init(name: "monitor", state: "monitor", presentationArguments: [], settleTime: 2.0),
        .init(name: "settings", state: "settings", presentationArguments: [], settleTime: 2.0),
        .init(name: "dark", state: "active-chat", presentationArguments: ["--screenshot-color-scheme", "dark"], settleTime: 2.0),
        .init(name: "large-type", state: "pending-approval", presentationArguments: ["--screenshot-dynamic-type", "accessibility3"], settleTime: 2.0),
    ]

    static func selectedByEnvironment(
        outputDirectory: URL
    ) throws -> [ScreenshotCaptureSpecification] {
        if ProcessInfo.processInfo.environment["SIGNALBOX_NATIVE_SCREENSHOT_ORIENTATION_ONLY"] == "1" {
            return []
        }
        if FileManager.default.fileExists(
            atPath: outputDirectory.appendingPathComponent(".capture-orientation-only").path
        ) {
            return []
        }
        let configuredNames = ProcessInfo.processInfo.environment["SIGNALBOX_NATIVE_SCREENSHOT_NAMES"]
            ?? (try? String(contentsOf: outputDirectory.appendingPathComponent(".capture-screenshot-names"), encoding: .utf8))
        guard let rawNames = configuredNames, !rawNames.isEmpty else {
            return all
        }

        let requestedNames = Set(
            rawNames
                .split(separator: ",")
                .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
                .filter { !$0.isEmpty }
        )
        guard !requestedNames.isEmpty else {
            return all
        }
        let knownNames = Set(all.map(\.name))
        let unknownNames = requestedNames.subtracting(knownNames).sorted()
        guard unknownNames.isEmpty else {
            throw ScreenshotCaptureConfigurationError.unknownNames(unknownNames)
        }
        return all.filter { requestedNames.contains($0.name) }
    }
}

private enum ScreenshotCaptureConfigurationError: LocalizedError {
    case unknownNames([String])

    var errorDescription: String? {
        switch self {
        case .unknownNames(let names):
            return "Unknown screenshot state names: \(names.joined(separator: ", "))"
        }
    }
}
