#if canImport(SignalboxClient)
import SignalboxClient
#endif
#if canImport(SignalboxModels)
import SignalboxModels
#endif
import Combine
import Foundation
import Security
import SwiftUI

enum NativeAppConstants {
    static let defaultServerURL = "http://127.0.0.1:8000"
    static let serviceName = "co.rdwd.SignalboxNative"
    static let apiKeyAccount = "signalboxd-api-key"
    static let serverURLDefaultsKey = "signalboxd-url"
}

extension Notification.Name {
    static let refreshRequested = Notification.Name("signalbox-refresh-requested")
}

@MainActor
final class SignalboxSettingsViewModel: ObservableObject {
    @Published var serverURLText: String
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
        self.serverURLText = userDefaults.string(forKey: NativeAppConstants.serverURLDefaultsKey)
            ?? NativeAppConstants.defaultServerURL
        let storedAPIKey = keychain.readSecret() ?? ""
        self.apiKey = storedAPIKey
        self.connectionStatus = storedAPIKey.isEmpty ? .notConfigured : .unknown
    }

    var canBuildClient: Bool {
        configurationResult().isSuccess
    }

    func configurationResult() -> Result<SignalboxClientConfiguration, SignalboxClientError> {
        guard let url = URL(string: serverURLText.trimmingCharacters(in: .whitespacesAndNewlines)) else {
            return .failure(.invalidConfiguration("Enter a valid server URL."))
        }
        do {
            return .success(try SignalboxClientConfiguration(baseURL: url, apiKey: apiKey))
        } catch let error as SignalboxClientError {
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

    func buildClient() throws -> SignalboxAPIClient {
        switch configurationResult() {
        case .success(let configuration):
            return SignalboxAPIClient(configuration: configuration)
        case .failure(let error):
            throw error
        }
    }

    func testConnection(using client: SignalboxClientProtocol? = nil) async {
        do {
            let resolvedClient: any SignalboxClientProtocol
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
        userDefaults.set(serverURLText.trimmingCharacters(in: .whitespacesAndNewlines), forKey: NativeAppConstants.serverURLDefaultsKey)
        try keychain.writeSecret(apiKey)
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
    private let service: String
    private let account: String

    init(
        service: String = NativeAppConstants.serviceName,
        account: String = NativeAppConstants.apiKeyAccount
    ) {
        self.service = service
        self.account = account
    }

    func readSecret() -> String? {
        var query = baseQuery()
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne

        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        guard status == errSecSuccess, let data = result as? Data else {
            return nil
        }
        return String(data: data, encoding: .utf8)
    }

    func writeSecret(_ secret: String) throws {
        let data = Data(secret.utf8)
        let query = baseQuery()
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

    func deleteSecret() {
        SecItemDelete(baseQuery() as CFDictionary)
    }

    private func baseQuery() -> [String: Any] {
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
