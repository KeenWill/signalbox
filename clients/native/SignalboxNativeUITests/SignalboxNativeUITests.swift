import XCTest

final class SignalboxNativeUITests: XCTestCase {
    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    @MainActor
    func testMainMockFlowSendsMessageAndApprovesTool() throws {
        let app = launchMockApp()

        let firstSession = app.buttons["session-row-11111111-1111-4111-8111-111111111111"]
        XCTAssertTrue(firstSession.waitForExistence(timeout: 20))
        firstSession.tap()

        let composer = app.descendants(matching: .any)["message-composer"]
        assertElementHittable(composer, named: "composer after session tap", in: app, timeout: 20)
        composer.tap()
        composer.typeText("Summarize the current runner state")
        app.buttons["send-message-button"].tap()
        XCTAssertTrue(app.staticTexts["I am checking the runner fleet and current task state. The local runner is online."].waitForExistence(timeout: 10))

        app.terminate()
        app.launchArguments = ["--mock-server", "--screenshot-state", "pending-approval"]
        app.launch()

        XCTAssertTrue(app.buttons["approve-tool-button"].waitForExistence(timeout: 10))
        app.buttons["approve-tool-button"].tap()
        XCTAssertTrue(app.staticTexts["Succeeded"].waitForExistence(timeout: 5))
    }

    @MainActor
    func testSettingsShowsInvalidConfigurationError() throws {
        let app = launchMockApp()

        tapTab(named: "Settings", in: app)
        let urlField = app.textFields["server-url-field"]
        XCTAssertTrue(urlField.waitForExistence(timeout: 5))
        urlField.tap()
        urlField.clearAndTypeText("bad-url")
        app.secureTextFields["api-key-field"].tap()
        app.secureTextFields["api-key-field"].clearAndTypeText("mock-key")
        app.buttons["test-connection-button"].tap()

        XCTAssertTrue(app.staticTexts["connection-error-text"].waitForExistence(timeout: 15))
    }

    @MainActor
    func testRealServerConnectionListsRunnerAndCreatesSessionWhenConfigured() throws {
        guard let configuration = realServerSmokeConfiguration() else {
            throw XCTSkip("Set SIGNALBOX_NATIVE_REAL_SERVER_URL and SIGNALBOX_NATIVE_REAL_SERVER_API_KEY, or create clients/native/.env, to run the real server UI smoke.")
        }

        let app = XCUIApplication()
        app.terminate()
        app.launchArguments = ["--reset-server-settings"]
        app.launch()

        tapTab(named: "Settings", in: app)
        let urlField = app.textFields["server-url-field"]
        XCTAssertTrue(urlField.waitForExistence(timeout: 10), "Missing server URL field")
        urlField.tap()
        urlField.clearAndTypeText(configuration.serverURL)

        let apiKeyField = app.secureTextFields["api-key-field"]
        XCTAssertTrue(apiKeyField.waitForExistence(timeout: 5), "Missing API key field")
        apiKeyField.tap()
        apiKeyField.clearAndTypeText(configuration.apiKey)
        dismissSavePasswordPromptIfPresent(in: app)

        app.buttons["test-connection-button"].tap()
        allowLocalNetworkPromptIfPresent(in: app)
        XCTAssertTrue(app.staticTexts["Connected"].waitForExistence(timeout: 30), "Real server connection did not reach Connected")
        dismissSavePasswordPromptIfPresent(in: app)

        app.buttons["save-settings-button"].tap()
        dismissSavePasswordPromptIfPresent(in: app)

        tapTab(named: "Runners", in: app)
        assertRealServerRunnerVisible(configuration, in: app)

        app.terminate()
        app.launchArguments = []
        app.launch()
        tapTab(named: "Sessions", in: app)
        XCTAssertTrue(app.buttons["create-session-button"].waitForExistence(timeout: 30), "Missing create session button after relaunch")
        app.buttons["create-session-button"].tap()
        dismissSavePasswordPromptIfPresent(in: app)

        let sessionTitle = "UI smoke \(Int(Date().timeIntervalSince1970))"
        let titleField = app.textFields["new-session-title"]
        assertElementHittable(titleField, named: "new session title field", in: app, timeout: 10)
        titleField.tap()
        titleField.clearAndTypeText(sessionTitle)
        app.buttons["confirm-create-session"].tap()

        XCTAssertTrue(app.staticTexts[sessionTitle].waitForExistence(timeout: 30), "Created session title did not appear")

        let realServerMessage = "Real server UI smoke message \(Int(Date().timeIntervalSince1970))"
        let composer = app.textFields["message-composer"]
        assertElementHittable(composer, named: "message composer", in: app, timeout: 30)
        composer.tap()
        composer.clearAndTypeText(realServerMessage)
        app.buttons["send-message-button"].tap()

        XCTAssertTrue(app.staticTexts[realServerMessage].waitForExistence(timeout: 30), "Sent real-server smoke message did not appear")
    }

    private func assertElementExists(
        _ element: XCUIElement,
        named elementName: String,
        in app: XCUIApplication,
        timeout: TimeInterval,
        file: StaticString = #filePath,
        line: UInt = #line
    ) {
        guard !element.waitForExistence(timeout: timeout) else {
            return
        }
        let attachment = XCTAttachment(screenshot: app.screenshot())
        attachment.name = "Missing \(elementName)"
        attachment.lifetime = .keepAlways
        add(attachment)
        XCTFail("Missing \(elementName)", file: file, line: line)
    }

    private func assertElementHittable(
        _ element: XCUIElement,
        named elementName: String,
        in app: XCUIApplication,
        timeout: TimeInterval,
        file: StaticString = #filePath,
        line: UInt = #line
    ) {
        let predicate = NSPredicate(format: "isHittable == true")
        let expectation = XCTNSPredicateExpectation(predicate: predicate, object: element)
        let result = XCTWaiter.wait(for: [expectation], timeout: timeout)
        guard result == .completed else {
            let attachment = XCTAttachment(screenshot: app.screenshot())
            attachment.name = "Missing hittable \(elementName)"
            attachment.lifetime = .keepAlways
            add(attachment)
            XCTFail("Missing hittable \(elementName)", file: file, line: line)
            return
        }
    }

    private func dismissSavePasswordPromptIfPresent(in app: XCUIApplication) {
        let appNotNowButton = app.buttons["Not Now"]
        if appNotNowButton.waitForExistence(timeout: 1) {
            appNotNowButton.tap()
            return
        }

        let springboard = XCUIApplication(bundleIdentifier: "com.apple.springboard")
        let springboardNotNowButton = springboard.buttons["Not Now"]
        if springboardNotNowButton.waitForExistence(timeout: 1) {
            springboardNotNowButton.tap()
        }
    }

    private func allowLocalNetworkPromptIfPresent(in app: XCUIApplication) {
        let appAllowButton = app.buttons["Allow"]
        if appAllowButton.waitForExistence(timeout: 2) {
            appAllowButton.tap()
            return
        }

        let springboard = XCUIApplication(bundleIdentifier: "com.apple.springboard")
        let springboardAllowButton = springboard.buttons["Allow"]
        if springboardAllowButton.waitForExistence(timeout: 2) {
            springboardAllowButton.tap()
            return
        }

        let appOKButton = app.buttons["OK"]
        if appOKButton.waitForExistence(timeout: 1) {
            appOKButton.tap()
            return
        }

        let springboardOKButton = springboard.buttons["OK"]
        if springboardOKButton.waitForExistence(timeout: 1) {
            springboardOKButton.tap()
        }
    }

    private func tapTab(
        named tabName: String,
        in app: XCUIApplication,
        file: StaticString = #filePath,
        line: UInt = #line
    ) {
        let tabBarButton = app.tabBars.buttons[tabName]
        if tabBarButton.waitForExistence(timeout: 5) {
            tabBarButton.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5)).tap()
            return
        }

        let sidebarButton = app.buttons[tabName]
        if sidebarButton.waitForExistence(timeout: 5) {
            assertElementHittable(sidebarButton, named: "\(tabName) navigation", in: app, timeout: 5, file: file, line: line)
            sidebarButton.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5)).tap()
            return
        }

        let attachment = XCTAttachment(screenshot: app.screenshot())
        attachment.name = "Missing \(tabName) navigation"
        attachment.lifetime = .keepAlways
        add(attachment)
        XCTFail("Missing \(tabName) navigation", file: file, line: line)
    }
}

private struct RealServerSmokeConfiguration {
    let serverURL: String
    let apiKey: String
    let expectedRunnerID: String?
}

private func realServerSmokeConfiguration() -> RealServerSmokeConfiguration? {
    let environment = ProcessInfo.processInfo.environment
    if let serverURL = environment["SIGNALBOX_NATIVE_REAL_SERVER_URL"], !serverURL.isEmpty,
       let apiKey = environment["SIGNALBOX_NATIVE_REAL_SERVER_API_KEY"], !apiKey.isEmpty {
        let expectedRunnerID = environment["SIGNALBOX_NATIVE_REAL_SERVER_RUNNER_ID"].flatMap { $0.isEmpty ? nil : $0 }
        return RealServerSmokeConfiguration(serverURL: serverURL, apiKey: apiKey, expectedRunnerID: expectedRunnerID)
    }

    let sourceFile = URL(fileURLWithPath: #filePath)
    let nativeProjectDirectory = sourceFile
        .deletingLastPathComponent()
        .deletingLastPathComponent()
    let envFile = nativeProjectDirectory.appendingPathComponent(".env")
    return realServerSmokeConfiguration(from: envFile, defaultServerURL: "http://127.0.0.1:8000")
}

private func realServerSmokeConfiguration(from envFile: URL, defaultServerURL: String?) -> RealServerSmokeConfiguration? {
    guard let contents = try? String(contentsOf: envFile, encoding: .utf8) else {
        return nil
    }
    let values = parseDotEnv(contents)
    let apiKey = values["SIGNALBOX_NATIVE_REAL_SERVER_API_KEY"] ?? values["SIGNALBOX_API_KEY"]
    guard let apiKey, !apiKey.isEmpty else {
        return nil
    }
    let expectedRunnerID = values["SIGNALBOX_NATIVE_REAL_SERVER_RUNNER_ID"].flatMap { $0.isEmpty ? nil : $0 }
    let serverURL = values["SIGNALBOX_NATIVE_REAL_SERVER_URL"]
        ?? values["SIGNALBOX_BASE_URL"].flatMap { $0.isEmpty ? nil : $0 }
        ?? defaultServerURL
    guard let serverURL, !serverURL.isEmpty else {
        return nil
    }
    return RealServerSmokeConfiguration(serverURL: serverURL, apiKey: apiKey, expectedRunnerID: expectedRunnerID)
}

@MainActor
private func assertRealServerRunnerVisible(
    _ configuration: RealServerSmokeConfiguration,
    in app: XCUIApplication,
    file: StaticString = #filePath,
    line: UInt = #line
) {
    if let expectedRunnerID = configuration.expectedRunnerID {
        let runnerCard = app.descendants(matching: .any)["runner-\(expectedRunnerID)"]
        XCTAssertTrue(
            runnerCard.waitForExistence(timeout: 30),
            "Missing expected runner \(expectedRunnerID)",
            file: file,
            line: line
        )
        XCTAssertEqual(
            runnerCard.value as? String,
            "online",
            "Expected runner \(expectedRunnerID) to be online",
            file: file,
            line: line
        )
    } else {
        let onlineRunnerCard = app.descendants(matching: .any)
            .matching(NSPredicate(format: "identifier BEGINSWITH %@ AND value == %@", "runner-", "online"))
            .firstMatch
        XCTAssertTrue(
            onlineRunnerCard.waitForExistence(timeout: 30),
            "Expected at least one online runner",
            file: file,
            line: line
        )
    }
}

private func parseDotEnv(_ contents: String) -> [String: String] {
    Dictionary(
        contents
            .split(whereSeparator: \.isNewline)
            .compactMap { line -> (String, String)? in
                let trimmed = line.trimmingCharacters(in: .whitespaces)
                guard !trimmed.isEmpty, !trimmed.hasPrefix("#"),
                      let separator = trimmed.firstIndex(of: "=") else {
                    return nil
                }
                let key = String(trimmed[..<separator])
                var rawValue = String(trimmed[trimmed.index(after: separator)...]
                    .trimmingCharacters(in: .whitespaces)
                )
                if rawValue.count >= 2,
                   let firstCharacter = rawValue.first,
                   let lastCharacter = rawValue.last,
                   (firstCharacter == "\"" && lastCharacter == "\"") || (firstCharacter == "'" && lastCharacter == "'") {
                    rawValue.removeFirst()
                    rawValue.removeLast()
                }
                return (key, rawValue)
            },
        uniquingKeysWith: { first, _ in first }
    )
}

@MainActor
private func launchMockApp() -> XCUIApplication {
    let app = XCUIApplication()
    app.terminate()
    app.launchArguments = ["--mock-server"]
    app.launch()
    return app
}

private extension XCUIElement {
    func clearAndTypeText(_ text: String) {
        tap()
        if let currentValue = value as? String, !currentValue.isEmpty {
            let deleteString = String(repeating: XCUIKeyboardKey.delete.rawValue, count: currentValue.count + 8)
            typeText(deleteString)
        }
        typeText(text)
    }
}
