#if os(macOS)
#if canImport(LLMHubModels)
import LLMHubModels
#endif
import AppKit
import Darwin
import SwiftUI

final class MacScreenshotExportAppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        Task {
            await MacScreenshotExporter.exportIfRequested(arguments: ProcessInfo.processInfo.arguments)
        }
    }
}

@MainActor
enum MacScreenshotExporter {
    private static var didStartExport = false
    private static let windowSizes = [
        MacScreenshotWindowSize(name: "compact", desktopSize: CGSize(width: 960, height: 640), artifactSize: CGSize(width: 760, height: 560)),
        MacScreenshotWindowSize(name: "regular", desktopSize: CGSize(width: 1280, height: 860), artifactSize: CGSize(width: 920, height: 680)),
        MacScreenshotWindowSize(name: "wide", desktopSize: CGSize(width: 1600, height: 1000), artifactSize: CGSize(width: 1120, height: 760))
    ]

    static func exportIfRequested(arguments: [String]) async {
        guard !didStartExport else {
            return
        }
        guard let optionIndex = arguments.firstIndex(of: "--export-macos-screenshots") else {
            return
        }
        let valueIndex = arguments.index(after: optionIndex)
        guard arguments.indices.contains(valueIndex) else {
            return
        }
        didStartExport = true

        let outputDirectory = URL(fileURLWithPath: arguments[valueIndex], isDirectory: true)
        do {
            try await exportAll(to: outputDirectory)
            NSApp.terminate(nil)
        } catch {
            FileHandle.standardError.write(Data("macOS screenshot export failed: \(error.localizedDescription)\n".utf8))
            Darwin.exit(EXIT_FAILURE)
        }
    }

    private static func exportAll(to outputDirectory: URL) async throws {
        try FileManager.default.createDirectory(at: outputDirectory, withIntermediateDirectories: true)

        for windowSize in windowSizes {
            let sizeDirectory = outputDirectory.appendingPathComponent(windowSize.name, isDirectory: true)
            try FileManager.default.createDirectory(at: sizeDirectory, withIntermediateDirectories: true)
            let specifications = try screenshotSpecifications(windowSize: windowSize)
            for specification in specifications {
                try await render(
                    specification.content(),
                    size: specification.size,
                    to: sizeDirectory.appendingPathComponent("\(specification.name).png")
                )
            }
        }
    }

    private static func screenshotSpecifications(windowSize: MacScreenshotWindowSize) throws -> [MacScreenshotSpecification] {
        let artifact = try sampleArtifact()
        return [
            rootSpecification(name: "setup", scenario: .setup, windowSize: windowSize),
            rootSpecification(name: "sessions", scenario: .sessions, windowSize: windowSize),
            newSessionSpecification(windowSize: windowSize),
            rootSpecification(name: "active-chat", scenario: .activeChat, windowSize: windowSize),
            rootSpecification(name: "markdown-basics", scenario: .markdownBasics, windowSize: windowSize),
            rootSpecification(name: "markdown-table", scenario: .markdownTable, windowSize: windowSize),
            rootSpecification(name: "markdown-code", scenario: .markdownCode, windowSize: windowSize),
            rootSpecification(name: "markdown-message", scenario: .markdownMessage, windowSize: windowSize),
            rootSpecification(name: "pending-approval", scenario: .pendingApproval, windowSize: windowSize),
            rootSpecification(name: "failed-tool", scenario: .failedTool, windowSize: windowSize),
            MacScreenshotSpecification(name: "artifact-preview", size: windowSize.artifactSize) {
                AnyView(ArtifactPreviewScreen(artifact: artifact))
            },
            rootSpecification(name: "runners", scenario: .runners, windowSize: windowSize),
            rootSpecification(name: "monitor", scenario: .monitor, windowSize: windowSize),
            rootSpecification(name: "settings", scenario: .settings, windowSize: windowSize),
            rootSpecification(name: "dark", scenario: .activeChat, windowSize: windowSize, colorScheme: .dark),
            rootSpecification(name: "large-type", scenario: .pendingApproval, windowSize: windowSize, dynamicTypeSize: .accessibility2)
        ]
    }

    private static func newSessionSpecification(windowSize: MacScreenshotWindowSize) -> MacScreenshotSpecification {
        MacScreenshotSpecification(name: "new-session", size: windowSize.desktopSize) {
            AnyView(MacNewSessionScreenshotView())
        }
    }

    private static func rootSpecification(
        name: String,
        scenario: ScreenshotScenario,
        windowSize: MacScreenshotWindowSize,
        colorScheme: ColorScheme? = nil,
        dynamicTypeSize: DynamicTypeSize = .large
    ) -> MacScreenshotSpecification {
        MacScreenshotSpecification(name: name, size: windowSize.desktopSize) {
            let coordinator = AppCoordinator(isMockMode: scenario.requiresMockService, screenshotScenario: scenario)
            var view = AnyView(
                RootView()
                    .environmentObject(coordinator)
                    .environment(\.dynamicTypeSize, dynamicTypeSize)
            )
            if let colorScheme {
                view = AnyView(view.environment(\.colorScheme, colorScheme))
            }
            return view
        }
    }

    private static func render(_ content: AnyView, size: CGSize, to outputURL: URL) async throws {
        let window = NSWindow(
            contentRect: CGRect(origin: .zero, size: size),
            styleMask: [.titled, .closable, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = "LLM Hub Native Screenshot"
        window.backgroundColor = .windowBackgroundColor
        window.setFrame(CGRect(origin: CGPoint(x: 80, y: 80), size: size), display: true)

        let hostedContent = ZStack {
            Color(nsColor: .windowBackgroundColor)
                .ignoresSafeArea()
            content
        }
        .frame(width: size.width, height: size.height)
        let hostingView = NSHostingView(rootView: hostedContent)
        hostingView.frame = CGRect(origin: .zero, size: size)
        hostingView.wantsLayer = true
        hostingView.layer?.backgroundColor = NSColor.windowBackgroundColor.cgColor
        window.contentView = hostingView
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)

        try await Task.sleep(nanoseconds: 1_500_000_000)
        hostingView.layoutSubtreeIfNeeded()

        guard let bitmap = hostingView.bitmapImageRepForCachingDisplay(in: hostingView.bounds) else {
            throw MacScreenshotExportError.bitmapCreationFailed
        }
        hostingView.cacheDisplay(in: hostingView.bounds, to: bitmap)
        guard let pngData = bitmap.representation(using: .png, properties: [:]) else {
            throw MacScreenshotExportError.pngEncodingFailed
        }
        try pngData.write(to: outputURL, options: .atomic)
        window.orderOut(nil)
    }

    private static func sampleArtifact() throws -> HubArtifact {
        let decoder = HubJSONCoding.decoder()
        let fixture = try decoder.decode(MacArtifactFixture.self, from: Data(MockHubFixtures.initial.utf8))
        guard let artifact = fixture.artifactsBySession[MockHubFixtures.activeSessionID]?.first else {
            throw MacScreenshotExportError.missingArtifactFixture
        }
        return artifact
    }
}

private struct MacNewSessionScreenshotView: View {
    @StateObject private var coordinator = AppCoordinator(isMockMode: true, screenshotScenario: .sessions)

    var body: some View {
        ZStack {
            RootView()
                .environmentObject(coordinator)
                .disabled(true)

            Color.black.opacity(0.18)
                .ignoresSafeArea()

            MacNewSessionModalPreview()
                .frame(width: 560)
                .background(Color(nsColor: .windowBackgroundColor), in: RoundedRectangle(cornerRadius: 12))
                .overlay(
                    RoundedRectangle(cornerRadius: 12)
                        .strokeBorder(Color(nsColor: .separatorColor), lineWidth: 1)
                )
                .shadow(color: .black.opacity(0.22), radius: 28, y: 12)
        }
        .environment(\.dynamicTypeSize, .large)
    }
}

private struct MacNewSessionModalPreview: View {
    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text("New Session")
                .font(.title3.weight(.semibold))
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 22)
                .padding(.top, 20)
                .padding(.bottom, 16)

            Divider()

            VStack(alignment: .leading, spacing: 18) {
                NewSessionFormField(label: "Title") {
                    MacPreviewTextField(value: "Native session")
                }

                NewSessionFormField(label: "Template") {
                    Picker("Template", selection: .constant("general_chat")) {
                        Text("General Chat").tag("general_chat")
                        Text("Coder").tag("coder")
                    }
                    .labelsHidden()
                    .pickerStyle(.menu)
                }

                NewSessionFormField(label: "Runner") {
                    Picker("Runner", selection: .constant("local-runner")) {
                        Text("local-runner (laptop)").tag("local-runner")
                        Text("Auto placement").tag("auto")
                    }
                    .labelsHidden()
                    .pickerStyle(.menu)
                }

                NewSessionTemplateDetails(
                    description: "Broad assistant with safe tools.",
                    model: "claude-sonnet-latest",
                    tools: ["echo", "current_time", "save_report"]
                )
            }
            .padding(22)

            Divider()

            HStack(spacing: 10) {
                Spacer()
                Button("Cancel") {}
                    .keyboardShortcut(.cancelAction)
                Button("Create") {}
                    .buttonStyle(.borderedProminent)
                    .keyboardShortcut(.defaultAction)
            }
            .padding(.horizontal, 22)
            .padding(.vertical, 15)
        }
    }
}

private struct MacPreviewTextField: View {
    let value: String

    var body: some View {
        Text(value)
            .font(.body)
            .foregroundStyle(.primary)
            .padding(.horizontal, 8)
            .frame(height: 28)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Color(nsColor: .textBackgroundColor), in: RoundedRectangle(cornerRadius: 6))
            .overlay(
                RoundedRectangle(cornerRadius: 6)
                    .strokeBorder(Color.secondary.opacity(0.22), lineWidth: 1)
            )
    }
}

private struct MacScreenshotSpecification {
    let name: String
    let size: CGSize
    let content: @MainActor () -> AnyView
}

private struct MacScreenshotWindowSize {
    let name: String
    let desktopSize: CGSize
    let artifactSize: CGSize
}

private struct MacArtifactFixture: Decodable {
    let artifactsBySession: [String: [HubArtifact]]

    private enum CodingKeys: String, CodingKey {
        case artifactsBySession = "artifacts_by_session"
    }
}

private enum MacScreenshotExportError: LocalizedError {
    case bitmapCreationFailed
    case pngEncodingFailed
    case missingArtifactFixture

    var errorDescription: String? {
        switch self {
        case .bitmapCreationFailed:
            return "Could not create a bitmap from the hosted SwiftUI view."
        case .pngEncodingFailed:
            return "Could not encode the hosted SwiftUI view as PNG."
        case .missingArtifactFixture:
            return "The mock artifact fixture is missing."
        }
    }
}
#endif
