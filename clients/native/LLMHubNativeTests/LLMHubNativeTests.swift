import XCTest
@testable import LLMHubNative

@MainActor
final class LLMHubNativeTests: XCTestCase {
    func testMockServiceLoadsMainOperationsState() async throws {
        let service = MockHubService()
        let sessions = try await service.listSessions(archived: false)
        let runners = try await service.listRunners()
        let monitor = try await service.listMonitorSessions()

        XCTAssertEqual(sessions.count, 7)
        XCTAssertTrue(runners.contains { $0.status == .online })
        XCTAssertTrue(monitor.contains { $0.status.state == .waitingForConfirmation })
        XCTAssertTrue(monitor.contains { $0.status.state == .failed })
    }

    func testSettingsRejectsInvalidHubURL() {
        let settings = HubSettingsViewModel(
            keychain: KeychainSecretStore(),
            userDefaults: UserDefaults(suiteName: "LLMHubNativeTests")!
        )
        settings.hubURLText = "not a url"
        settings.apiKey = "key"

        guard case .failure = settings.configurationResult() else {
            return XCTFail("Expected invalid URL failure")
        }
    }
}
