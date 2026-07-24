import SwiftUI

struct RootView: View {
    @EnvironmentObject private var coordinator: AppCoordinator
    @Environment(\.horizontalSizeClass) private var horizontalSizeClass

    var body: some View {
        Group {
            #if os(macOS)
            macDesktopLayout
            #else
            if horizontalSizeClass == .compact {
                compactLayout
            } else {
                desktopLayout
            }
            #endif
        }
        .accessibilityIdentifier("llm-hub-root")
    }

    private var compactLayout: some View {
        TabView(selection: $coordinator.selectedSection) {
            SessionsScreen()
                .tabItem { Label("Sessions", systemImage: AppSection.sessions.systemImage) }
                .tag(AppSection.sessions)
            MonitorScreen()
                .tabItem { Label("Monitor", systemImage: AppSection.monitor.systemImage) }
                .tag(AppSection.monitor)
            RunnersScreen()
                .tabItem { Label("Runners", systemImage: AppSection.runners.systemImage) }
                .tag(AppSection.runners)
            SettingsScreen()
                .tabItem { Label("Settings", systemImage: AppSection.settings.systemImage) }
                .tag(AppSection.settings)
        }
    }

    #if os(macOS)
    private var macDesktopLayout: some View {
        HStack(spacing: 0) {
            VStack(alignment: .leading, spacing: 16) {
                Text("LLM Hub")
                    .font(.title2.weight(.bold))
                    .padding(.horizontal, 14)
                    .padding(.top, 18)

                VStack(spacing: 4) {
                    ForEach(AppSection.allCases) { section in
                        macSidebarButton(for: section)
                    }
                }
                .padding(.horizontal, 10)

                Spacer()
            }
            .frame(width: 240)
            .background(Color(nsColor: .controlBackgroundColor))

            Divider()

            Group {
                switch coordinator.selectedSection {
                case .sessions:
                    SessionsScreen()
                case .monitor:
                    MonitorScreen()
                case .runners:
                    RunnersScreen()
                case .templates:
                    TemplatesScreen()
                case .settings:
                    SettingsScreen()
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(Color(nsColor: .windowBackgroundColor))
        }
    }

    private func macSidebarButton(for section: AppSection) -> some View {
        let isSelected = section == coordinator.selectedSection
        return Button {
            coordinator.selectedSection = section
        } label: {
            Label(section.title, systemImage: section.systemImage)
                .font(.headline)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 12)
                .padding(.vertical, 10)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .foregroundStyle(isSelected ? Color.accentColor : Color.primary)
        .background(
            isSelected ? Color.accentColor.opacity(0.14) : Color.clear,
            in: RoundedRectangle(cornerRadius: 8)
        )
        .accessibilityIdentifier("sidebar-\(section.rawValue)")
    }
    #endif

    private var desktopLayout: some View {
        NavigationSplitView {
            List(AppSection.allCases) { section in
                Button {
                    coordinator.selectedSection = section
                } label: {
                    Label(section.title, systemImage: section.systemImage)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .listRowBackground(section == coordinator.selectedSection ? Color.accentColor.opacity(0.14) : Color.clear)
                .accessibilityIdentifier("sidebar-\(section.rawValue)")
            }
            .navigationTitle("LLM Hub")
            .frame(minWidth: 220)
        } detail: {
            switch coordinator.selectedSection {
            case .sessions:
                SessionsScreen()
            case .monitor:
                MonitorScreen()
            case .runners:
                RunnersScreen()
            case .templates:
                TemplatesScreen()
            case .settings:
                SettingsScreen()
            }
        }
        .navigationSplitViewStyle(.balanced)
    }
}

struct UnconfiguredHubView: View {
    var body: some View {
        ContentUnavailableView {
            Label("Connect to LLM Hub", systemImage: "link.badge.plus")
        } description: {
            Text("Configure the hub URL and API key in Settings.")
        }
        .accessibilityIdentifier("setup-no-connection")
    }
}
