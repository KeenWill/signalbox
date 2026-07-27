#if os(macOS)
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
        MacScreenshotWindowSize(name: "compact", desktopSize: CGSize(width: 960, height: 640)),
        MacScreenshotWindowSize(name: "regular", desktopSize: CGSize(width: 1280, height: 860)),
        MacScreenshotWindowSize(name: "wide", desktopSize: CGSize(width: 1600, height: 1000))
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
            let specifications = screenshotSpecifications(windowSize: windowSize)
            for specification in specifications {
                try await render(
                    specification.content(),
                    presentation: specification.presentation,
                    size: specification.size,
                    to: sizeDirectory.appendingPathComponent("\(specification.name).png")
                )
            }
        }
    }

    private static func screenshotSpecifications(windowSize: MacScreenshotWindowSize) -> [MacScreenshotSpecification] {
        return [
            rootSpecification(name: "setup", scenario: .setup, windowSize: windowSize),
            rootSpecification(name: "sessions", scenario: .sessions, windowSize: windowSize),
            rootSpecification(
                name: "new-session",
                scenario: .newSession,
                windowSize: windowSize,
                presentation: .attachedSheet
            ),
            rootSpecification(name: "active-chat", scenario: .activeChat, windowSize: windowSize),
            rootSpecification(name: "markdown-basics", scenario: .markdownBasics, windowSize: windowSize),
            rootSpecification(name: "markdown-table", scenario: .markdownTable, windowSize: windowSize),
            rootSpecification(name: "markdown-code", scenario: .markdownCode, windowSize: windowSize),
            rootSpecification(name: "markdown-message", scenario: .markdownMessage, windowSize: windowSize),
            rootSpecification(name: "pending-approval", scenario: .pendingApproval, windowSize: windowSize),
            rootSpecification(name: "completed-tool", scenario: .completedTool, windowSize: windowSize),
            rootSpecification(name: "failed-tool", scenario: .failedTool, windowSize: windowSize),
            rootSpecification(
                name: "artifact-preview",
                scenario: .artifactPreview,
                windowSize: windowSize,
                presentation: .attachedSheet
            ),
            rootSpecification(name: "runners", scenario: .runners, windowSize: windowSize),
            rootSpecification(name: "monitor", scenario: .monitor, windowSize: windowSize),
            rootSpecification(name: "settings", scenario: .settings, windowSize: windowSize),
            rootSpecification(name: "dark", scenario: .activeChat, windowSize: windowSize, colorScheme: .dark),
            rootSpecification(name: "large-type", scenario: .pendingApproval, windowSize: windowSize, dynamicTypeSize: .accessibility2)
        ]
    }

    private static func rootSpecification(
        name: String,
        scenario: ScreenshotScenario,
        windowSize: MacScreenshotWindowSize,
        colorScheme: ColorScheme? = nil,
        dynamicTypeSize: DynamicTypeSize = .large,
        presentation: MacScreenshotPresentation = .content
    ) -> MacScreenshotSpecification {
        MacScreenshotSpecification(
            name: name,
            size: windowSize.desktopSize,
            presentation: presentation
        ) {
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

    private static func render(
        _ content: AnyView,
        presentation: MacScreenshotPresentation,
        size: CGSize,
        to outputURL: URL
    ) async throws {
        let window = NSWindow(
            contentRect: CGRect(origin: .zero, size: size),
            styleMask: [.titled, .closable, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = "Signalbox Native Screenshot"
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

        let bitmap = try bitmap(of: hostingView)
        try compositePresentation(
            presentation,
            from: window,
            onto: bitmap,
            relativeTo: hostingView
        )
        guard let pngData = bitmap.representation(using: .png, properties: [:]) else {
            throw MacScreenshotExportError.pngEncodingFailed
        }
        try pngData.write(to: outputURL, options: .atomic)
        if let attachedSheet = window.attachedSheet {
            window.endSheet(attachedSheet)
        }
        window.orderOut(nil)
    }

    private static func bitmap(of view: NSView) throws -> NSBitmapImageRep {
        view.layoutSubtreeIfNeeded()
        guard let bitmap = view.bitmapImageRepForCachingDisplay(in: view.bounds) else {
            throw MacScreenshotExportError.bitmapCreationFailed
        }
        view.cacheDisplay(in: view.bounds, to: bitmap)
        return bitmap
    }

    private static func compositePresentation(
        _ presentation: MacScreenshotPresentation,
        from window: NSWindow,
        onto destination: NSBitmapImageRep,
        relativeTo hostingView: NSView
    ) throws {
        switch presentation {
        case .content:
            guard window.attachedSheet == nil else {
                throw MacScreenshotExportError.unexpectedAttachedSheet
            }
        case .attachedSheet:
            guard
                let sheet = window.attachedSheet,
                sheet.isVisible,
                let contentView = sheet.contentView
            else {
                throw MacScreenshotExportError.expectedAttachedSheetMissing
            }
            let sheetView = contentView.superview ?? contentView
            let sheetBitmap = try bitmap(of: sheetView)
            let hostingFrame = frameInScreen(of: hostingView, in: window)
            let sheetFrame = frameInScreen(of: sheetView, in: sheet)
            let destinationFrame = sheetFrame.offsetBy(
                dx: -hostingFrame.minX,
                dy: -hostingFrame.minY
            )
            guard let context = NSGraphicsContext(bitmapImageRep: destination) else {
                throw MacScreenshotExportError.bitmapContextCreationFailed
            }
            let image = NSImage(size: sheetView.bounds.size)
            image.addRepresentation(sheetBitmap)
            NSGraphicsContext.saveGraphicsState()
            NSGraphicsContext.current = context
            image.draw(in: destinationFrame)
            context.flushGraphics()
            NSGraphicsContext.restoreGraphicsState()
        }
    }

    private static func frameInScreen(of view: NSView, in window: NSWindow) -> CGRect {
        window.convertToScreen(view.convert(view.bounds, to: nil))
    }

}

private struct MacScreenshotSpecification {
    let name: String
    let size: CGSize
    let presentation: MacScreenshotPresentation
    let content: @MainActor () -> AnyView
}

private enum MacScreenshotPresentation {
    case content
    case attachedSheet
}

private struct MacScreenshotWindowSize {
    let name: String
    let desktopSize: CGSize
}

private enum MacScreenshotExportError: LocalizedError {
    case bitmapCreationFailed
    case bitmapContextCreationFailed
    case expectedAttachedSheetMissing
    case pngEncodingFailed
    case unexpectedAttachedSheet

    var errorDescription: String? {
        switch self {
        case .bitmapCreationFailed:
            return "Could not create a bitmap from the hosted SwiftUI view."
        case .bitmapContextCreationFailed:
            return "Could not create a bitmap drawing context for the macOS screenshot."
        case .expectedAttachedSheetMissing:
            return "The macOS screenshot scenario did not present its expected attached sheet."
        case .pngEncodingFailed:
            return "Could not encode the hosted SwiftUI view as PNG."
        case .unexpectedAttachedSheet:
            return "A content-only macOS screenshot scenario presented an unexpected attached sheet."
        }
    }
}
#endif
