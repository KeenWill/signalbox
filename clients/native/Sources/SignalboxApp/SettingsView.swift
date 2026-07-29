import SwiftUI

struct SettingsScreen: View {
  @EnvironmentObject private var coordinator: AppCoordinator
  @State private var isTesting = false

  var body: some View {
    NavigationStack {
      Form {
        Section {
          #if os(macOS)
            TextField("Unix socket path", text: $coordinator.processSettings.socketPath)
              .accessibilityIdentifier("socket-path-field")
          #else
            Text(remoteTransportGateMessage)
              .foregroundStyle(.secondary)
          #endif
          statusRow
        } header: {
          Text("Process protocol")
        } footer: {
          Text(
            "signalboxd serves the single-version JSONL protocol over a local Unix socket. It defines no authentication field."
          )
        }

        #if os(macOS)
          Section {
            Button {
              Task {
                isTesting = true
                await coordinator.testConnectionAndInstallClient()
                isTesting = false
              }
            } label: {
              Label("Test Connection", systemImage: "cable.connector")
            }
            .disabled(isTesting)
            .accessibilityIdentifier("test-connection-button")

            Button {
              coordinator.saveProcessSocketPath()
            } label: {
              Label("Save Socket Path", systemImage: "externaldrive.badge.checkmark")
            }
            .disabled(coordinator.isMockMode)
            .accessibilityIdentifier("save-settings-button")
          }
        #endif

        if case .failed(let message) = coordinator.processSettings.connectionStatus {
          Section("Connection") {
            Text(message)
              .foregroundStyle(.red)
              .textSelection(.enabled)
              .accessibilityIdentifier("connection-error-text")
          }
        }

        Section("Client") {
          LabeledContent(
            "Mode",
            value: coordinator.isMockMode ? "v18 JSONL harness" : "Local Unix socket"
          )
          LabeledContent("Wire", value: "Process protocol v18")
            .accessibilityIdentifier("wire-diagnostic")
          LabeledContent("Authentication", value: "Not defined")
          LabeledContent("Remote transport", value: "Owner design gate")
            .accessibilityIdentifier("remote-transport-diagnostic")
        }
      }
      .navigationTitle("Settings")
    }
    .accessibilityIdentifier("settings-screen")
  }

  private var statusRow: some View {
    HStack {
      Label(
        coordinator.processSettings.connectionStatus.label,
        systemImage: statusIcon
      )
      .foregroundStyle(statusColor)
      .font(.callout.weight(.semibold))
      Spacer()
      if isTesting {
        ProgressView()
          .controlSize(.small)
      }
    }
    .accessibilityIdentifier("connection-status")
  }

  private var statusIcon: String {
    switch coordinator.processSettings.connectionStatus {
    case .connected:
      return "checkmark.circle.fill"
    case .failed:
      return "xmark.octagon.fill"
    case .notConfigured, .unknown:
      return "circle.dashed"
    }
  }

  private var statusColor: Color {
    switch coordinator.processSettings.connectionStatus {
    case .connected:
      return .green
    case .failed:
      return .red
    case .notConfigured, .unknown:
      return .secondary
    }
  }
}
