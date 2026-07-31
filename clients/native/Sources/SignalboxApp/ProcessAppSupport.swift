import Combine
import Foundation
import Security

#if canImport(SignalboxClient)
  import SignalboxClient
#endif

enum NativeProcessConstants {
  static let socketEnvironmentKey = "SIGNALBOX_SOCKET_PATH"
  static let runtimeEnvironmentKey = "DEVENV_RUNTIME"
  static let socketDefaultsKey = "signalbox-process-socket-path"

  static func defaultSocketPath(
    environment: [String: String]
  ) -> String? {
    // A relative runtime directory would make daemon identity depend on the
    // app's launch directory, so it is not an admissible implicit endpoint.
    guard
      let runtimePath = environment[runtimeEnvironmentKey],
      runtimePath.hasPrefix("/")
    else {
      return nil
    }
    let runtimeDirectory = URL(fileURLWithPath: runtimePath)
    return runtimeDirectory
      .appendingPathComponent("signalbox", isDirectory: true)
      .appendingPathComponent("signalboxd.sock", isDirectory: false)
      .path
  }
}

enum LegacyCredentialCleanupOutcome {
  case complete
  case retryLater
}

enum LegacyRemoteSettingsCleanup {
  static let serverURLDefaultsKey = "hub-url"
  static let completionDefaultsKey = "signalbox-retired-remote-settings-cleaned"
  static let keychainService = "co.rdwd.SignalboxNative"
  static let keychainAccount = "hub-api-key"

  static func perform(userDefaults: UserDefaults = .standard) {
    perform(userDefaults: userDefaults, deleteCredential: deleteCredential)
  }

  static func perform(
    userDefaults: UserDefaults,
    deleteCredential: () -> LegacyCredentialCleanupOutcome
  ) {
    let legacyStateReappeared = userDefaults.object(forKey: serverURLDefaultsKey) != nil
    guard legacyStateReappeared || !userDefaults.bool(forKey: completionDefaultsKey) else {
      return
    }
    if legacyStateReappeared {
      // Legacy builds write the URL before the keychain item. Its reappearance
      // therefore invalidates a prior completion marker after a rollback.
      userDefaults.removeObject(forKey: completionDefaultsKey)
    }
    userDefaults.removeObject(forKey: serverURLDefaultsKey)
    // Mark completion only after the credential is absent. Transient keychain
    // failures must retry on a later launch, but successful migration must not
    // leave permanent keychain access in the process-only product.
    guard deleteCredential() == .complete else {
      return
    }
    userDefaults.set(true, forKey: completionDefaultsKey)
  }

  private static func deleteCredential() -> LegacyCredentialCleanupOutcome {
    let query: [String: Any] = [
      kSecClass as String: kSecClassGenericPassword,
      kSecAttrService as String: keychainService,
      kSecAttrAccount as String: keychainAccount,
    ]
    switch SecItemDelete(query as CFDictionary) {
    case errSecSuccess, errSecItemNotFound:
      return .complete
    default:
      return .retryLater
    }
  }
}

extension Notification.Name {
  static let refreshRequested = Notification.Name("signalbox-refresh-requested")
  static let processServiceChanged = Notification.Name("signalbox-process-service-changed")
}

@MainActor
final class SignalboxProcessSettingsViewModel: ObservableObject {
  @Published var socketPath: String
  @Published private(set) var connectionStatus: ConnectionStatus = .unknown

  private let userDefaults: UserDefaults

  init(
    userDefaults: UserDefaults = .standard,
    environment: [String: String] = ProcessInfo.processInfo.environment
  ) {
    self.userDefaults = userDefaults
    // An explicit launch override must win over persisted UI state so harnesses
    // and managed launches cannot silently connect to a user's saved daemon.
    socketPath =
      environment[NativeProcessConstants.socketEnvironmentKey]
      ?? userDefaults.string(forKey: NativeProcessConstants.socketDefaultsKey)
      ?? NativeProcessConstants.defaultSocketPath(environment: environment)
      ?? ""
    if validatedSocketPath == nil {
      connectionStatus = .notConfigured
    }
  }

  var validatedSocketPath: String? {
    let trimmed = socketPath.trimmingCharacters(in: .whitespacesAndNewlines)
    guard trimmed.hasPrefix("/") else {
      return nil
    }
    return trimmed
  }

  func save() {
    guard let path = validatedSocketPath else {
      connectionStatus = .failed("Enter an absolute local Unix-socket path.")
      return
    }
    userDefaults.set(path, forKey: NativeProcessConstants.socketDefaultsKey)
    socketPath = path
    connectionStatus = .unknown
  }

  func test(
    using service: (any SignalboxProcessServiceProtocol)?,
    expectedSocketPath: String? = nil
  ) async {
    guard let service else {
      connectionStatus = .failed(remoteTransportGateMessage)
      return
    }
    do {
      try await service.testConnection()
      // The probe suspends while Settings remains editable. A result for an
      // endpoint the user has since replaced must never bless the new value.
      guard expectedSocketPath.map({ validatedSocketPath == $0 }) ?? true else {
        connectionStatus = .unknown
        return
      }
      connectionStatus = .connected
      if let path = expectedSocketPath ?? validatedSocketPath {
        userDefaults.set(path, forKey: NativeProcessConstants.socketDefaultsKey)
      }
    } catch {
      // Failure has the same endpoint-generation obligation as success.
      guard expectedSocketPath.map({ validatedSocketPath == $0 }) ?? true else {
        connectionStatus = .unknown
        return
      }
      connectionStatus = .failed(error.localizedDescription)
    }
  }

  func markNotConfigured() {
    connectionStatus = .notConfigured
  }

  func markConnectedForHarness() {
    connectionStatus = .connected
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

// This is a product gate, not a recoverable connection error: the admitted
// protocol has no remote endpoint or authentication shape to configure.
let remoteTransportGateMessage =
  "signalboxd currently exposes only a local Unix socket. Remote and mobile transport requires an owner-approved server design."
