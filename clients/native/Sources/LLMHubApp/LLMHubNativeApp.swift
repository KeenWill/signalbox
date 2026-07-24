import SwiftUI

@main
struct LLMHubNativeApp: App {
    private static let launchArguments = ProcessInfo.processInfo.arguments
    private static let launchScreenshotScenario = ScreenshotScenario.parse(arguments: launchArguments)
    private static let launchColorScheme = ScreenshotPresentation.parseColorScheme(arguments: launchArguments)
    private static let launchDynamicTypeSize = ScreenshotPresentation.parseDynamicTypeSize(arguments: launchArguments)

    #if os(macOS)
    @NSApplicationDelegateAdaptor(MacScreenshotExportAppDelegate.self) private var macScreenshotExportAppDelegate
    #endif
    @StateObject private var coordinator = AppCoordinator(
        isMockMode: launchArguments.contains("--mock-hub"),
        screenshotScenario: launchScreenshotScenario,
        resetPersistedSettings: launchArguments.contains("--reset-hub-settings")
    )

    var body: some Scene {
        WindowGroup {
            RootView()
                .environmentObject(coordinator)
                .modifier(
                    ScreenshotPresentationModifier(
                        colorScheme: Self.launchColorScheme,
                        dynamicTypeSize: Self.launchDynamicTypeSize
                    )
                )
        }
        .commands {
            CommandGroup(after: .newItem) {
                Button("Refresh") {
                    NotificationCenter.default.post(name: .hubRefreshRequested, object: nil)
                }
                .keyboardShortcut("r", modifiers: [.command])
            }
        }
        #if os(macOS)
        Settings {
            SettingsScreen()
                .environmentObject(coordinator)
                .frame(minWidth: 460, minHeight: 360)
        }
        #endif
    }
}

private struct ScreenshotPresentationModifier: ViewModifier {
    let colorScheme: ColorScheme?
    let dynamicTypeSize: DynamicTypeSize?

    func body(content: Content) -> some View {
        var presentedContent = AnyView(content)
        if let colorScheme {
            presentedContent = AnyView(presentedContent.environment(\.colorScheme, colorScheme))
        }
        if let dynamicTypeSize {
            presentedContent = AnyView(presentedContent.environment(\.dynamicTypeSize, dynamicTypeSize))
        }
        return presentedContent
    }
}

private enum ScreenshotPresentation {
    static func parseColorScheme(arguments: [String]) -> ColorScheme? {
        guard argumentValue(after: "--screenshot-color-scheme", in: arguments) == "dark" else {
            return nil
        }
        return .dark
    }

    static func parseDynamicTypeSize(arguments: [String]) -> DynamicTypeSize? {
        switch argumentValue(after: "--screenshot-dynamic-type", in: arguments) {
        case "accessibility2":
            return .accessibility2
        case "accessibility3":
            return .accessibility3
        default:
            return nil
        }
    }

    private static func argumentValue(after option: String, in arguments: [String]) -> String? {
        guard let optionIndex = arguments.firstIndex(of: option) else {
            return nil
        }
        let valueIndex = arguments.index(after: optionIndex)
        guard arguments.indices.contains(valueIndex) else {
            return nil
        }
        return arguments[valueIndex]
    }
}
