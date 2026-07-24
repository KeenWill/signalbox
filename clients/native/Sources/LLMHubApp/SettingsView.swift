import SwiftUI

struct SettingsScreen: View {
    @EnvironmentObject private var coordinator: AppCoordinator
    @State private var isTesting = false
    @FocusState private var focusedField: SettingsFocusedField?

    var body: some View {
        #if os(macOS)
        macSettingsLayout
            .accessibilityIdentifier("settings-screen")
        #else
        NavigationStack {
            settingsForm
            .navigationTitle("Settings")
        }
        .accessibilityIdentifier("settings-screen")
        #endif
    }

    private var settingsForm: some View {
        Form {
            Section {
                hubURLField
                apiKeyField
                connectionStatusRow
            } header: {
                Text("Connection")
            } footer: {
                Text("The API key is stored in the system keychain and is never written to logs.")
            }

            Section {
                testConnectionButton
                saveSettingsButton
            }

            if case .failed(let message) = coordinator.settings.connectionStatus {
                Section("Error") {
                    connectionErrorText(message)
                }
            }

            Section("Client") {
                clientDiagnostics
            }
        }
        #if os(iOS)
        .scrollDismissesKeyboard(.interactively)
        #endif
    }

    #if os(macOS)
    private var macSettingsLayout: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 24) {
                VStack(alignment: .leading, spacing: 8) {
                    Text("Settings")
                        .font(.largeTitle.weight(.semibold))
                    Text("Connect this client to a native LLM Hub instance.")
                        .font(.body)
                        .foregroundStyle(.secondary)
                }

                HStack(alignment: .top, spacing: 20) {
                    VStack(alignment: .leading, spacing: 18) {
                        macPanelHeader("Connection", systemImage: "link")

                        VStack(alignment: .leading, spacing: 14) {
                            VStack(alignment: .leading, spacing: 6) {
                                Text("Hub URL")
                                    .font(.caption.weight(.semibold))
                                    .foregroundStyle(.secondary)
                                hubURLField
                                    .textFieldStyle(.roundedBorder)
                            }

                            VStack(alignment: .leading, spacing: 6) {
                                Text("API key")
                                    .font(.caption.weight(.semibold))
                                    .foregroundStyle(.secondary)
                                apiKeyField
                                    .textFieldStyle(.roundedBorder)
                            }
                        }

                        connectionStatusRow

                        if case .failed(let message) = coordinator.settings.connectionStatus {
                            connectionErrorText(message)
                                .padding(12)
                                .background(.red.opacity(0.08), in: RoundedRectangle(cornerRadius: 8))
                        }

                        HStack(spacing: 10) {
                            testConnectionButton
                                .buttonStyle(.borderedProminent)
                            saveSettingsButton
                                .buttonStyle(.bordered)
                        }
                    }
                    .padding(20)
                    .frame(maxWidth: 560, alignment: .leading)
                    .background(macPanelBackground)
                    .overlay(macPanelBorder)
                    .shadow(color: .black.opacity(0.035), radius: 12, y: 3)

                    VStack(alignment: .leading, spacing: 18) {
                        macPanelHeader("Client", systemImage: "desktopcomputer")
                        clientDiagnostics
                        Divider()
                        Label("Secrets stay in Keychain and are never emitted to logs.", systemImage: "key.fill")
                            .font(.callout)
                            .foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    .padding(20)
                    .frame(maxWidth: 340, alignment: .leading)
                    .background(macPanelBackground)
                    .overlay(macPanelBorder)
                    .shadow(color: .black.opacity(0.035), radius: 12, y: 3)
                }
            }
            .padding(28)
            .frame(maxWidth: 980, alignment: .leading)
        }
        .background(Color(nsColor: .windowBackgroundColor))
    }

    private func macPanelHeader(_ title: String, systemImage: String) -> some View {
        Label(title, systemImage: systemImage)
            .font(.title3.weight(.semibold))
    }

    private var macPanelBackground: some View {
        RoundedRectangle(cornerRadius: 12, style: .continuous)
            .fill(Color(nsColor: .textBackgroundColor))
    }

    private var macPanelBorder: some View {
        RoundedRectangle(cornerRadius: 12, style: .continuous)
            .strokeBorder(Color(nsColor: .separatorColor).opacity(0.55), lineWidth: 1)
    }
    #endif

    private var hubURLField: some View {
        TextField("Hub URL", text: $coordinator.settings.hubURLText)
            #if os(iOS)
            .keyboardType(.URL)
            .textInputAutocapitalization(.never)
            .autocorrectionDisabled()
            .submitLabel(.next)
            #endif
            .focused($focusedField, equals: .hubURL)
            .onSubmit {
                focusedField = .apiKey
            }
            .accessibilityIdentifier("hub-url-field")
    }

    private var apiKeyField: some View {
        SecureField("API key", text: $coordinator.settings.apiKey)
            #if os(iOS)
            .submitLabel(.done)
            .textContentType(.oneTimeCode)
            .textInputAutocapitalization(.never)
            .autocorrectionDisabled()
            #endif
            .focused($focusedField, equals: .apiKey)
            .onSubmit {
                focusedField = nil
            }
            .accessibilityIdentifier("api-key-field")
    }

    private var connectionStatusRow: some View {
        HStack {
            Label(coordinator.settings.connectionStatus.label, systemImage: statusIcon)
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

    private var testConnectionButton: some View {
        Button {
            focusedField = nil
            Task {
                isTesting = true
                await coordinator.testConnectionAndInstallClient()
                isTesting = false
            }
        } label: {
            Label("Test Connection", systemImage: "network")
        }
        .disabled(isTesting)
        .accessibilityIdentifier("test-connection-button")
    }

    private var saveSettingsButton: some View {
        Button {
            focusedField = nil
            coordinator.settings.save()
        } label: {
            Label("Save Settings", systemImage: "externaldrive.badge.checkmark")
        }
        .accessibilityIdentifier("save-settings-button")
    }

    private var clientDiagnostics: some View {
        Group {
            LabeledContent("Mode", value: coordinator.isMockMode ? "Mock fixtures" : "Native hub API")
            LabeledContent("Core API", value: "REST + WebSocket")
            LabeledContent("OpenAI facade", value: "Not used")
        }
    }

    private func connectionErrorText(_ message: String) -> some View {
        Text(message)
            .foregroundStyle(.red)
            .textSelection(.enabled)
            .accessibilityIdentifier("connection-error-text")
    }

    private var statusIcon: String {
        switch coordinator.settings.connectionStatus {
        case .connected:
            return "checkmark.circle.fill"
        case .failed:
            return "xmark.octagon.fill"
        case .notConfigured, .unknown:
            return "circle.dashed"
        }
    }

    private var statusColor: Color {
        switch coordinator.settings.connectionStatus {
        case .connected:
            return .green
        case .failed:
            return .red
        case .notConfigured, .unknown:
            return .secondary
        }
    }
}

private enum SettingsFocusedField: Hashable {
    case hubURL
    case apiKey
}
