#if canImport(LLMHubClient)
import LLMHubClient
#endif
#if canImport(LLMHubModels)
import LLMHubModels
#endif
import SwiftUI

struct SessionDetailScreen: View {
    @EnvironmentObject private var coordinator: AppCoordinator
    @StateObject private var viewModel: SessionDetailViewModel
    @State private var selectedArtifact: HubArtifact?

    init(session: HubSessionMetadata) {
        _viewModel = StateObject(wrappedValue: SessionDetailViewModel(session: session) { nil })
    }

    var body: some View {
        VStack(spacing: 0) {
            header
                .padding()
                .background(.bar)

            if let errorMessage = viewModel.errorMessage {
                ErrorBanner(message: errorMessage)
                    .padding([.horizontal, .top])
            }

            if let promptContextArtifact = viewModel.latestPromptContextArtifact {
                PromptContextArtifactCard(artifact: promptContextArtifact) {
                    selectedArtifact = promptContextArtifact
                }
                .padding([.horizontal, .top])
            }

            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 12) {
                        ForEach(viewModel.timelineItems) { item in
                            timelineView(for: item)
                                .id(item.id)
                        }
                        if viewModel.timelineItems.isEmpty {
                            EmptyStateView(
                                systemImage: "text.bubble",
                                title: "No events yet",
                                message: "Send a message to start a turn on the hub."
                            )
                        }
                    }
                    .padding()
                    .frame(maxWidth: 960)
                    .frame(maxWidth: .infinity)
                }
                .onChange(of: viewModel.timelineItems.count) { _, _ in
                    if let lastID = viewModel.timelineItems.last?.id {
                        withAnimation(.easeOut(duration: 0.2)) {
                            proxy.scrollTo(lastID, anchor: .bottom)
                        }
                    }
                }
            }

            if !viewModel.artifacts.isEmpty {
                artifactStrip
                    .padding(.horizontal)
                    .padding(.bottom, 8)
            }

            composer
                .padding()
                .background(.bar)
        }
        .navigationTitle(viewModel.session.displayTitle)
        #if os(iOS)
        .navigationBarTitleDisplayMode(.inline)
        #endif
        .task {
            viewModel.replaceServiceProvider { coordinator.service }
            await viewModel.load()
            presentScreenshotArtifactIfNeeded()
            viewModel.connectStream()
        }
        .onDisappear {
            viewModel.disconnectStream()
        }
        .sheet(item: $selectedArtifact) { artifact in
            ArtifactPreviewScreen(artifact: artifact)
        }
    }

    private func presentScreenshotArtifactIfNeeded() {
        guard coordinator.screenshotScenario == .artifactPreview, selectedArtifact == nil else {
            return
        }
        selectedArtifact = viewModel.artifacts.first
    }

    private var header: some View {
        HStack(alignment: .center, spacing: 12) {
            VStack(alignment: .leading, spacing: 4) {
                Text(viewModel.session.displayTitle)
                    .font(.headline)
                    .lineLimit(1)
                Text(viewModel.session.runnerID?.rawValue ?? "Auto placement")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer()
            StatusBadge(status: viewModel.status)
            if viewModel.isStreaming {
                ProgressView()
                    .controlSize(.small)
            }
        }
    }

    private var artifactStrip: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 8) {
                ForEach(viewModel.artifacts) { artifact in
                    Button {
                        selectedArtifact = artifact
                    } label: {
                        Label(artifact.presentationTitle, systemImage: artifact.presentationSystemImageName)
                            .font(.caption.weight(.semibold))
                            .lineLimit(1)
                            .padding(.horizontal, 10)
                            .padding(.vertical, 8)
                            .background(.thinMaterial, in: RoundedRectangle(cornerRadius: 8))
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel(artifact.presentationAccessibilityLabel)
                    .accessibilityIdentifier("artifact-\(artifact.id.rawValue)")
                }
            }
        }
        .accessibilityIdentifier("artifact-strip")
    }

    private var composer: some View {
        HStack(alignment: .bottom, spacing: 10) {
            TextField("Message the agent", text: $viewModel.composerText, axis: .vertical)
                .textFieldStyle(.roundedBorder)
                .lineLimit(1...5)
                .accessibilityIdentifier("message-composer")
            Button {
                Task { await viewModel.sendMessage() }
            } label: {
                Image(systemName: "paperplane.fill")
                    .frame(width: 34, height: 34)
            }
            .buttonStyle(.borderedProminent)
            .disabled(viewModel.composerText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            .accessibilityLabel("Send")
            .accessibilityIdentifier("send-message-button")
        }
    }

    @ViewBuilder
    private func timelineView(for item: HubTimelineItem) -> some View {
        switch item {
        case .message(let message):
            MessageBubble(message: message)
        case .tool(let tool):
            ToolInvocationCard(
                tool: tool,
                onApprove: { Task { await viewModel.approve(invocationID: tool.invocationID) } },
                onDeny: { Task { await viewModel.deny(invocationID: tool.invocationID) } }
            )
        case .turnFailure(let failure):
            FailureCard(failure: failure)
        case .unknown(let unknown):
            UnknownEventCard(unknown: unknown)
        }
    }
}

private struct PromptContextArtifactCard: View {
    let artifact: HubArtifact
    let onOpen: () -> Void

    private let metricColumns = [GridItem(.adaptive(minimum: 86), spacing: 8, alignment: .leading)]

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                Label("Prompt Context", systemImage: "brain.head.profile")
                    .font(.subheadline.weight(.semibold))
                Spacer()
                if artifact.promptContextSummary?.truncated == true {
                    Label("Truncated", systemImage: "exclamationmark.triangle.fill")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.orange)
                }
                Button(action: onOpen) {
                    Label("Open", systemImage: "arrow.up.forward.square")
                }
                .buttonStyle(.borderless)
                .accessibilityIdentifier("prompt-context-open")
            }

            if let summary = artifact.promptContextSummary {
                LazyVGrid(columns: metricColumns, alignment: .leading, spacing: 8) {
                    PromptContextMetric(label: "Guidance", value: summary.guidanceDocumentCount)
                    PromptContextMetric(label: "Skills", value: summary.selectedSkillCardCount)
                    PromptContextMetric(label: "Loaded", value: summary.selectedSkillDocumentCount)
                    PromptContextMetric(label: "Profiles", value: summary.selectedAgentProfileCardCount)
                    PromptContextMetric(label: "Tools", value: summary.enabledToolCount)
                }
                if let runtimeDescription = runtimeDescription(for: summary) {
                    Text(runtimeDescription)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                }
                if let workspaceDescription = workspaceDescription(for: summary) {
                    Text(workspaceDescription)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                }
            } else {
                Text("Prompt context metadata is unavailable; open the artifact to inspect the report.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(12)
        .frame(maxWidth: 960, alignment: .leading)
        .background(.secondary.opacity(0.08), in: RoundedRectangle(cornerRadius: 8))
        .overlay(
            RoundedRectangle(cornerRadius: 8)
                .strokeBorder(.primary.opacity(0.08))
        )
        .accessibilityIdentifier("prompt-context-card")
    }

    private func runtimeDescription(for summary: PromptContextSummary) -> String? {
        var parts: [String] = []
        if let modelAlias = summary.modelAlias, !modelAlias.isEmpty {
            parts.append(modelAlias)
        }
        if let runnerID = summary.runnerID, !runnerID.isEmpty {
            parts.append(runnerID)
        }
        return parts.isEmpty ? nil : parts.joined(separator: " - ")
    }

    private func workspaceDescription(for summary: PromptContextSummary) -> String? {
        var parts: [String] = []
        if let workspaceDir = summary.workspaceDir, !workspaceDir.isEmpty {
            parts.append("Session: \(workspaceDir)")
        }
        if let projectWorkspaceDir = summary.projectWorkspaceDir, !projectWorkspaceDir.isEmpty {
            parts.append("Project: \(projectWorkspaceDir)")
        }
        if let linkedWorkspacePath = summary.linkedWorkspacePath, !linkedWorkspacePath.isEmpty {
            parts.append("Linked: \(linkedWorkspacePath)")
        }
        return parts.isEmpty ? nil : parts.joined(separator: " - ")
    }
}

private struct PromptContextMetric: View {
    let label: String
    let value: Int

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(value.formatted())
                .font(.headline.monospacedDigit())
            Text(label)
                .font(.caption2.weight(.semibold))
                .foregroundStyle(.secondary)
                .lineLimit(1)
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel("\(label): \(value)")
    }
}

extension SessionDetailViewModel {
    func replaceServiceProvider(_ provider: @escaping () -> (any HubClientProtocol)?) {
        self.setServiceProvider(provider)
    }
}
