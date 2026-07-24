#if canImport(LLMHubClient)
import LLMHubClient
#endif
#if canImport(LLMHubModels)
import LLMHubModels
#endif
import SwiftUI

struct MonitorScreen: View {
    @EnvironmentObject private var coordinator: AppCoordinator
    @StateObject private var viewModel = OperationsViewModel { nil }

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    SectionHeader(
                        title: "Needs Attention",
                        subtitle: "Sessions waiting on approvals or failures."
                    )
                    if viewModel.needsAttention.isEmpty {
                        EmptyStateView(
                            systemImage: "checkmark.seal",
                            title: "No sessions need attention",
                            message: "Waiting approvals and failed turns will appear here."
                        )
                    } else {
                        ForEach(viewModel.needsAttention) { summary in
                            MonitorSummaryCard(summary: summary)
                                .accessibilityIdentifier("monitor-attention-\(summary.id.rawValue)")
                        }
                    }

                    SectionHeader(title: "All Sessions", subtitle: nil)
                    ForEach(viewModel.monitorSessions) { summary in
                        MonitorSummaryCard(summary: summary)
                    }
                }
                .padding()
                .frame(maxWidth: 980)
                .frame(maxWidth: .infinity)
            }
            .navigationTitle("Monitor")
            .toolbar {
                Button {
                    Task { await viewModel.refresh() }
                } label: {
                    Label("Refresh", systemImage: "arrow.clockwise")
                }
            }
            .task {
                viewModel.replaceServiceProvider { coordinator.service }
                await viewModel.refresh()
            }
            .onReceive(NotificationCenter.default.publisher(for: .hubRefreshRequested)) { _ in
                Task { await viewModel.refresh() }
            }
            .overlay {
                if coordinator.service == nil {
                    UnconfiguredHubView()
                }
            }
        }
        .accessibilityIdentifier("monitor-screen")
    }
}

struct RunnersScreen: View {
    @EnvironmentObject private var coordinator: AppCoordinator
    @StateObject private var viewModel = OperationsViewModel { nil }

    var body: some View {
        NavigationStack {
            List {
                ForEach(viewModel.runners) { runner in
                    RunnerCard(runner: runner)
                        .listRowSeparator(.hidden)
                        .accessibilityIdentifier("runner-\(runner.id.rawValue)")
                }
            }
            .listStyle(.plain)
            .navigationTitle("Runners")
            .toolbar {
                Button {
                    Task { await viewModel.refresh() }
                } label: {
                    Label("Refresh", systemImage: "arrow.clockwise")
                }
            }
            .task {
                viewModel.replaceServiceProvider { coordinator.service }
                await viewModel.refresh()
            }
            .onReceive(NotificationCenter.default.publisher(for: .hubRefreshRequested)) { _ in
                Task { await viewModel.refresh() }
            }
            .overlay {
                if coordinator.service == nil {
                    UnconfiguredHubView()
                } else if viewModel.runners.isEmpty {
                    EmptyStateView(systemImage: "server.rack", title: "No runners", message: "Runners appear after registering with the hub.")
                }
            }
        }
        .accessibilityIdentifier("runners-screen")
    }
}

struct TemplatesScreen: View {
    @EnvironmentObject private var coordinator: AppCoordinator
    @StateObject private var viewModel = OperationsViewModel { nil }

    var body: some View {
        NavigationStack {
            List(viewModel.templates) { template in
                VStack(alignment: .leading, spacing: 8) {
                    HStack(alignment: .firstTextBaseline) {
                        Text(template.title)
                            .font(.headline)
                        Spacer()
                        if template.isBuiltin {
                            TemplatePill(label: "Built-in", systemImage: "shippingbox")
                        }
                        if let status = template.visibleDerivativeStatus {
                            TemplateDerivativeStatusBadge(status: status)
                        }
                    }
                    Text(template.description ?? "No description")
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                    if let advisory = template.derivativeAdvisoryText,
                       let status = template.visibleDerivativeStatus {
                        Label(advisory, systemImage: status.symbolName)
                            .font(.caption)
                            .foregroundStyle(status.tint)
                            .accessibilityIdentifier("template-derivative-advisory-\(template.id.rawValue)")
                    }
                    LabeledContent("Model", value: template.defaultModelAlias ?? "Hub default")
                    if let versionSummary = template.versionSummary {
                        LabeledContent("Version", value: versionSummary)
                    }
                    LabeledContent("Tools", value: (template.defaultTools ?? ["runner catalog"]).joined(separator: ", "))
                    if !template.defaultToolPermissionOverrides.isEmpty {
                        Text(template.defaultToolPermissionOverrides.map { "\($0.key): \($0.value.label)" }.sorted().joined(separator: ", "))
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
                .padding(.vertical, 8)
                .accessibilityIdentifier("template-\(template.id.rawValue)")
            }
            .navigationTitle("Templates")
            .task {
                viewModel.replaceServiceProvider { coordinator.service }
                await viewModel.refresh()
            }
            .onReceive(NotificationCenter.default.publisher(for: .hubRefreshRequested)) { _ in
                Task { await viewModel.refresh() }
            }
            .overlay {
                if coordinator.service == nil {
                    UnconfiguredHubView()
                }
            }
        }
        .accessibilityIdentifier("templates-screen")
    }
}

private struct TemplatePill: View {
    let label: String
    let systemImage: String

    var body: some View {
        Label(label, systemImage: systemImage)
            .font(.caption.weight(.semibold))
            .foregroundStyle(.secondary)
            .labelStyle(.titleAndIcon)
    }
}

private struct TemplateDerivativeStatusBadge: View {
    let status: HubTemplateDerivativeStatus

    var body: some View {
        Label(status.label, systemImage: status.symbolName)
            .font(.caption.weight(.semibold))
            .foregroundStyle(status.tint)
            .labelStyle(.titleAndIcon)
    }
}

private extension HubTemplateDerivativeStatus {
    var symbolName: String {
        switch self {
        case .current:
            return "checkmark.seal"
        case .stale:
            return "exclamationmark.triangle"
        case .sourceMissing:
            return "questionmark.folder"
        case .sourceArchived:
            return "archivebox"
        case .notDerived:
            return "doc.text"
        case .unknown:
            return "questionmark.circle"
        }
    }

    var tint: Color {
        switch self {
        case .current:
            return .secondary
        case .stale, .sourceMissing, .sourceArchived, .unknown:
            return .orange
        case .notDerived:
            return .secondary
        }
    }
}

private struct MonitorSummaryCard: View {
    let summary: HubMonitorSessionSummary

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .top) {
                VStack(alignment: .leading, spacing: 3) {
                    Text(summary.metadata.displayTitle)
                        .font(.headline)
                    Text(summary.metadata.runnerID?.rawValue ?? "Auto runner")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                StatusBadge(status: summary.status)
            }
            HStack(spacing: 14) {
                Label("\(summary.messageCount) messages", systemImage: "text.bubble")
                Label("\(summary.toolInvocationCount) tools", systemImage: "wrench.and.screwdriver")
                Label("\(summary.artifactCount) artifacts", systemImage: "shippingbox")
                if summary.taskSummary.total > 0 {
                    Label("\(summary.taskSummary.done)/\(summary.taskSummary.total) tasks", systemImage: "checklist")
                }
                RunnerStatusBadge(status: summary.runnerStatus)
            }
            .font(.caption)
            .foregroundStyle(.secondary)
        }
        .padding(14)
        .background(.thinMaterial, in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(.primary.opacity(0.08)))
    }
}

private struct RunnerCard: View {
    let runner: HubRunner

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                VStack(alignment: .leading, spacing: 3) {
                    Text(runner.id.rawValue)
                        .font(.headline)
                    Text(runner.environmentTag)
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.secondary)
                }
                Spacer()
                RunnerStatusBadge(status: runner.status)
            }
            if runner.toolCatalog.isEmpty {
                Text("No advertised tools")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                LazyVGrid(columns: [GridItem(.adaptive(minimum: 150), spacing: 8)], alignment: .leading, spacing: 8) {
                    ForEach(runner.toolCatalog) { tool in
                        HStack(spacing: 6) {
                            Image(systemName: tool.defaultPermission == .confirm ? "hand.raised" : "bolt")
                            Text(tool.name)
                                .lineLimit(1)
                            Spacer()
                            Text(tool.defaultPermission.label)
                                .foregroundStyle(.secondary)
                        }
                        .font(.caption)
                        .padding(.horizontal, 8)
                        .padding(.vertical, 6)
                        .background(.secondary.opacity(0.08), in: RoundedRectangle(cornerRadius: 6))
                    }
                }
            }
        }
        .padding(14)
        .background(.thinMaterial, in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(.primary.opacity(0.08)))
        .accessibilityElement(children: .combine)
        .accessibilityValue(runner.status.rawValue)
    }
}

extension OperationsViewModel {
    func replaceServiceProvider(_ provider: @escaping () -> (any HubClientProtocol)?) {
        self.setServiceProvider(provider)
    }
}
