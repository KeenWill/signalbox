import XCTest
@testable import SignalboxNative

@MainActor
final class SignalboxNativeTests: XCTestCase {
    func testMockServiceLoadsMainOperationsState() async throws {
        let service = MockSignalboxService()
        let sessions = try await service.listSessions(archived: false)
        let runners = try await service.listRunners()
        let monitor = try await service.listMonitorSessions()

        XCTAssertEqual(sessions.count, 7)
        XCTAssertTrue(runners.contains { $0.status == .online })
        XCTAssertTrue(monitor.contains { $0.status.state == .waitingForConfirmation })
        XCTAssertTrue(monitor.contains { $0.status.state == .failed })
    }

    func testSettingsRejectsInvalidServerURL() {
        let settings = SignalboxSettingsViewModel(
            keychain: KeychainSecretStore(),
            userDefaults: UserDefaults(suiteName: "SignalboxNativeTests")!
        )
        settings.serverURLText = "not a url"
        settings.apiKey = "key"

        guard case .failure = settings.configurationResult() else {
            return XCTFail("Expected invalid URL failure")
        }
    }
}
