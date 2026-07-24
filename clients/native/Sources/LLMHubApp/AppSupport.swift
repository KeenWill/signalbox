#if canImport(LLMHubClient)
import LLMHubClient
#endif
#if canImport(LLMHubModels)
import LLMHubModels
#endif
import Combine
import Foundation
import Security
import SwiftUI

enum NativeAppConstants {
    static let defaultHubURL = "http://127.0.0.1:8000"
    static let serviceName = "co.rdwd.LLMHubNative"
    static let apiKeyAccount = "hub-api-key"
    static let hubURLDefaultsKey = "hub-url"
}

extension Notification.Name {
    static let hubRefreshRequested = Notification.Name("hub-refresh-requested")
}

@MainActor
final class HubSettingsViewModel: ObservableObject {
    @Published var hubURLText: String
    @Published var apiKey: String
    @Published private(set) var connectionStatus: ConnectionStatus

    private let keychain: KeychainSecretStore
    private let userDefaults: UserDefaults

    convenience init(userDefaults: UserDefaults = .standard) {
        self.init(keychain: KeychainSecretStore(), userDefaults: userDefaults)
    }

    init(keychain: KeychainSecretStore, userDefaults: UserDefaults = .standard) {
        self.keychain = keychain
        self.userDefaults = userDefaults
        self.hubURLText = userDefaults.string(forKey: NativeAppConstants.hubURLDefaultsKey)
            ?? NativeAppConstants.defaultHubURL
        let storedAPIKey = keychain.readSecret(
            service: NativeAppConstants.serviceName,
            account: NativeAppConstants.apiKeyAccount
        ) ?? ""
        self.apiKey = storedAPIKey
        self.connectionStatus = storedAPIKey.isEmpty ? .notConfigured : .unknown
    }

    var canBuildClient: Bool {
        configurationResult().isSuccess
    }

    func configurationResult() -> Result<HubClientConfiguration, HubClientError> {
        guard let url = URL(string: hubURLText.trimmingCharacters(in: .whitespacesAndNewlines)) else {
            return .failure(.invalidConfiguration("Enter a valid hub URL."))
        }
        do {
            return .success(try HubClientConfiguration(baseURL: url, apiKey: apiKey))
        } catch let error as HubClientError {
            return .failure(error)
        } catch {
            return .failure(.invalidConfiguration(error.localizedDescription))
        }
    }

    func save() {
        do {
            try saveSettings()
            connectionStatus = apiKey.isEmpty ? .notConfigured : .unknown
        } catch {
            connectionStatus = .failed(error.localizedDescription)
        }
    }

    func buildClient() throws -> HubClient {
        switch configurationResult() {
        case .success(let configuration):
            return HubClient(configuration: configuration)
        case .failure(let error):
            throw error
        }
    }

    func testConnection(using client: HubClientProtocol? = nil) async {
        do {
            let resolvedClient: any HubClientProtocol
            if let client {
                resolvedClient = client
            } else {
                resolvedClient = try buildClient()
            }
            try await resolvedClient.testConnection()
            try saveSettings()
            connectionStatus = .connected
        } catch {
            connectionStatus = .failed(error.localizedDescription)
        }
    }

    private func saveSettings() throws {
        userDefaults.set(hubURLText.trimmingCharacters(in: .whitespacesAndNewlines), forKey: NativeAppConstants.hubURLDefaultsKey)
        try keychain.writeSecret(
            apiKey,
            service: NativeAppConstants.serviceName,
            account: NativeAppConstants.apiKeyAccount
        )
    }
}

enum ConnectionStatus: Equatable {
    case notConfigured
    case unknown
    case connected
    case failed(String)

    var label: String {
        switch self {
        case .notConfigured:
            return "Not configured"
        case .unknown:
            return "Not tested"
        case .connected:
            return "Connected"
        case .failed:
            return "Connection failed"
        }
    }
}

struct KeychainSecretStore: Sendable {
    func readSecret(service: String, account: String) -> String? {
        var query = baseQuery(service: service, account: account)
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne

        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        guard status == errSecSuccess, let data = result as? Data else {
            return nil
        }
        return String(data: data, encoding: .utf8)
    }

    func writeSecret(_ secret: String, service: String, account: String) throws {
        let data = Data(secret.utf8)
        let query = baseQuery(service: service, account: account)
        let attributes = [kSecValueData as String: data]
        let status = SecItemUpdate(query as CFDictionary, attributes as CFDictionary)
        switch status {
        case errSecSuccess:
            return
        case errSecItemNotFound:
            var addQuery = query
            addQuery[kSecValueData as String] = data
            let addStatus = SecItemAdd(addQuery as CFDictionary, nil)
            guard addStatus == errSecSuccess else {
                throw KeychainSecretStoreError.writeFailed(addStatus)
            }
        default:
            throw KeychainSecretStoreError.writeFailed(status)
        }
    }

    func deleteSecret(service: String, account: String) {
        SecItemDelete(baseQuery(service: service, account: account) as CFDictionary)
    }

    private func baseQuery(service: String, account: String) -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        ]
    }
}

enum KeychainSecretStoreError: LocalizedError, Equatable {
    case writeFailed(OSStatus)

    var errorDescription: String? {
        switch self {
        case .writeFailed(let status):
            return "Keychain save failed with OSStatus \(status)."
        }
    }
}

extension Result {
    var isSuccess: Bool {
        if case .success = self {
            return true
        }
        return false
    }
}
