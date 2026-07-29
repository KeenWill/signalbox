import XCTest

final class SignalboxNativeUITests: XCTestCase {
    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    @MainActor
    func testMainMockFlowSubmitsInput() throws {
        let app = launchMockApp()
        let submittedContent = "Summarize the current runner state"

        let firstSession = app.buttons["session-row-11111111-1111-4111-8111-111111111111"]
        XCTAssertTrue(firstSession.waitForExistence(timeout: 20))
        firstSession.tap()

        let composer = app.descendants(matching: .any)["message-composer"]
        assertElementHittable(composer, named: "composer after session tap", in: app, timeout: 20)
        composer.tap()
        composer.typeText(submittedContent)
        app.buttons["send-message-button"].tap()
        XCTAssertTrue(app.staticTexts[submittedContent].waitForExistence(timeout: 10))
    }

    @MainActor
    func testProcessToolDecisionOffersApproveAndDeny() throws {
        let app = XCUIApplication()
        app.launchArguments = ["--mock-server", "--screenshot-state", "pending-approval"]
        app.launch()

        XCTAssertTrue(app.buttons["approve-tool-button"].waitForExistence(timeout: 10))
        XCTAssertTrue(app.buttons["deny-tool-button"].exists)
    }

    @MainActor
    func testSettingsDescribesSingleVersionTransportGate() throws {
        let app = launchMockApp()

        tapTab(named: "Settings", in: app)
        XCTAssertTrue(app.descendants(matching: .any)["wire-diagnostic"].waitForExistence(timeout: 5))
        XCTAssertTrue(app.descendants(matching: .any)["remote-transport-diagnostic"].exists)
        XCTAssertFalse(app.secureTextFields["api-key-field"].exists)
    }

    @MainActor
    func testRealServerConnectionListsRunnerAndCreatesSessionWhenConfigured() throws {
        throw XCTSkip("Remote/mobile transport is an owner design gate; signalboxd currently exposes only a local Unix socket.")
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

@MainActor
private func launchMockApp() -> XCUIApplication {
    let app = XCUIApplication()
    app.terminate()
    app.launchArguments = ["--mock-server"]
    app.launch()
    return app
}
