import Combine
import Foundation

#if canImport(SignalboxClient)
  import SignalboxClient
#endif

enum NativeProcessConstants {
  static let socketEnvironmentKey = "SIGNALBOX_SOCKET_PATH"
  static let socketDefaultsKey = "signalbox-process-socket-path"
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
    socketPath =
      environment[NativeProcessConstants.socketEnvironmentKey]
      ?? userDefaults.string(forKey: NativeProcessConstants.socketDefaultsKey)
      ?? ""
    if socketPath.isEmpty {
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
      guard expectedSocketPath.map({ validatedSocketPath == $0 }) ?? true else {
        connectionStatus = .unknown
        return
      }
      connectionStatus = .connected
      if let path = expectedSocketPath ?? validatedSocketPath {
        userDefaults.set(path, forKey: NativeProcessConstants.socketDefaultsKey)
      }
    } catch {
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

let remoteTransportGateMessage =
  "signalboxd currently exposes only a local Unix socket. Remote and mobile transport requires an owner-approved server design."
