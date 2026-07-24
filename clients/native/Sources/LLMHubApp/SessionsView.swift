#if canImport(LLMHubClient)
import LLMHubClient
#endif
#if canImport(LLMHubModels)
import LLMHubModels
#endif
import SwiftUI

struct SessionsScreen: View {
    @EnvironmentObject private var coordinator: AppCoordinator
    @StateObject private var viewModel: SessionListViewModel
    @State private var showCreateSheet = false
    @State private var selectedSessionID: HubSessionID?
    @State private var didPresentScreenshotCreateSheet = false

    init() {
        _viewModel = StateObject(wrappedValue: SessionListViewModel { nil })
    }

    var body: some View {
        NavigationStack {
            content
                .navigationTitle("Sessions")
                .toolbar {
                    ToolbarItem(placement: .primaryAction) {
                        Button {
                            showCreateSheet = true
                        } label: {
                            Label("Create Session", systemImage: "plus")
                        }
                        .accessibilityIdentifier("create-session-button")
                    }
                    ToolbarItem(placement: .automatic) {
                        Button {
                            Task { await viewModel.refresh() }
                        } label: {
                            Label("Refresh", systemImage: "arrow.clockwise")
                        }
                    }
                }
                .searchable(text: $viewModel.searchText, prompt: "Search sessions")
                .sheet(isPresented: $showCreateSheet) {
                    CreateSessionSheet(viewModel: viewModel) { session in
                        coordinator.selectedSessionID = session.id
                        selectedSessionID = session.id
                    }
                }
                .navigationDestination(item: $selectedSessionID) { sessionID in
                    if let session = viewModel.session(with: sessionID) {
                        SessionDetailScreen(session: session)
                    } else {
                        EmptyStateView(
                            systemImage: "questionmark.folder",
                            title: "Session unavailable",
                            message: "Refresh the session list and try again."
                        )
                    }
                }
                .task {
                    viewModel.replaceServiceProvider { coordinator.service }
                    await viewModel.refresh()
                    applyRequestedSessionSelection()
                    presentScreenshotCreateSheetIfNeeded()
                }
                .onReceive(NotificationCenter.default.publisher(for: .hubRefreshRequested)) { _ in
                    Task {
                        await viewModel.refresh()
                        applyRequestedSessionSelection()
                    }
                }
        }
    }

    private func presentScreenshotCreateSheetIfNeeded() {
        guard coordinator.screenshotScenario == .newSession, !didPresentScreenshotCreateSheet else {
            return
        }
        didPresentScreenshotCreateSheet = true
        showCreateSheet = true
    }

    private func applyRequestedSessionSelection() {
        guard selectedSessionID == nil, let requestedSessionID = coordinator.selectedSessionID else {
            return
        }
        if viewModel.session(with: requestedSessionID) != nil {
            selectedSessionID = requestedSessionID
        }
    }

    @ViewBuilder
    private var content: some View {
        if coordinator.service == nil {
            UnconfiguredHubView()
        } else {
            VStack(spacing: 0) {
                Picker("Session state", selection: $viewModel.showArchived) {
                    Text("Active").tag(false)
                    Text("Archived").tag(true)
                }
                .pickerStyle(.segmented)
                .padding([.horizontal, .top])
                .accessibilityIdentifier("session-state-picker")

                if let errorMessage = viewModel.errorMessage {
                    ErrorBanner(message: errorMessage)
                        .padding(.horizontal)
                        .padding(.top, 8)
                }

                if viewModel.visibleSessions.isEmpty && !viewModel.isLoading {
                    EmptyStateView(
                        systemImage: viewModel.showArchived ? "archivebox" : "bubble.left.and.bubble.right",
                        title: viewModel.showArchived ? "No archived sessions" : "No active sessions",
                        message: "Create a session from a template or refresh after connecting to the hub."
                    )
                } else {
                    List(viewModel.visibleSessions) { session in
                        Button {
                            coordinator.selectedSessionID = session.id
                            selectedSessionID = session.id
                        } label: {
                            SessionRowView(
                                session: session,
                                status: viewModel.statusesBySessionID[session.id] ?? HubSessionStatus(state: .idle)
                            )
                        }
                        .buttonStyle(.plain)
                        .swipeActions(edge: .trailing) {
                            Button {
                                Task { await viewModel.setArchived(!session.isArchived, sessionID: session.id) }
                            } label: {
                                Label(session.isArchived ? "Unarchive" : "Archive", systemImage: session.isArchived ? "tray.and.arrow.up" : "archivebox")
                            }
                        }
                        .accessibilityIdentifier("session-row-\(session.id.rawValue)")
                    }
                    .listStyle(.plain)
                    .accessibilityIdentifier("session-list")
                }
            }
        }
    }
}

private struct SessionRowView: View {
    let session: HubSessionMetadata
    let status: HubSessionStatus

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: status.state == .waitingForConfirmation ? "exclamationmark.triangle.fill" : (session.runnerID == nil ? "point.3.connected.trianglepath.dotted" : "terminal"))
                .font(.title3.weight(.semibold))
                .foregroundStyle(status.state == .waitingForConfirmation ? .orange : .accentColor)
                .frame(width: 30)

            VStack(alignment: .leading, spacing: 6) {
                Text(session.displayTitle)
                    .font(.headline)
                    .lineLimit(2)
                HStack(spacing: 8) {
                    if let runnerID = session.runnerID {
                        Label(runnerID.rawValue, systemImage: "server.rack")
                    } else {
                        Label("Auto placement", systemImage: "shuffle")
                    }
                    Text(session.modelAlias)
                }
                .font(.caption)
                .foregroundStyle(.secondary)

                if !session.tags.isEmpty {
                    Text(session.tags.prefix(4).joined(separator: "  "))
                        .font(.caption2.weight(.semibold))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
            }

            Spacer(minLength: 8)

            StatusBadge(status: status)
        }
        .padding(.vertical, 6)
    }
}

struct CreateSessionSheet: View {
    @ObservedObject var viewModel: SessionListViewModel
    let onCreated: (HubSessionMetadata) -> Void

    @Environment(\.dismiss) private var dismiss
    @State private var selectedTemplateID: HubTemplateID?
    @State private var selectedRunnerID: HubRunnerID?
    @State private var title = "Native session"
    @State private var guidanceTargetPath = ""
    @State private var linkedWorkspacePath = ""

    var body: some View {
        content
            .onAppear {
                selectDefaultsIfNeeded()
            }
            .onChange(of: viewModel.templates.map { $0.id.rawValue }) { _, _ in
                selectDefaultsIfNeeded()
            }
            .onChange(of: viewModel.runners.map { $0.id.rawValue }) { _, _ in
                selectDefaultsIfNeeded()
            }
    }

    @ViewBuilder
    private var content: some View {
        #if os(macOS)
        macContent
        #else
        mobileContent
        #endif
    }

    private var mobileContent: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("Title", text: $title)
                        .accessibilityIdentifier("new-session-title")
                    Picker("Template", selection: $selectedTemplateID) {
                        Text("Explicit prompt").tag(Optional<HubTemplateID>.none)
                        ForEach(viewModel.templates) { template in
                            Text(template.title).tag(Optional(template.id))
                        }
                    }
                    .accessibilityIdentifier("template-picker")
                    Picker("Runner", selection: $selectedRunnerID) {
                        Text("Auto placement").tag(Optional<HubRunnerID>.none)
                        ForEach(viewModel.runners) { runner in
                            Text("\(runner.id.rawValue) (\(runner.environmentTag))").tag(Optional(runner.id))
                        }
                    }
                    .accessibilityIdentifier("runner-picker")
                    guidanceTargetPathField
                    TextField("Workspace path", text: $linkedWorkspacePath)
                        .accessibilityIdentifier("linked-workspace-path")
                }

                if let template = selectedTemplate {
                    Section("Template Details") {
                        NewSessionTemplateDetails(
                            description: template.description ?? "No description",
                            model: template.defaultModelAlias ?? "Hub default",
                            tools: template.defaultTools ?? ["runner catalog"],
                            showsTitle: false,
                            usesCard: false
                        )
                    }
                }
            }
            .navigationTitle("New Session")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Create") {
                        createSession()
                    }
                    .accessibilityIdentifier("confirm-create-session")
                    .disabled(!canCreateSession)
                }
            }
        }
        .frame(minWidth: 420, minHeight: 420)
    }

    #if os(macOS)
    private var macContent: some View {
        VStack(spacing: 0) {
            Text("New Session")
                .font(.title3.weight(.semibold))
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 24)
                .padding(.top, 22)
                .padding(.bottom, 16)

            Divider()

            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    NewSessionFormField(label: "Title") {
                        TextField("Title", text: $title)
                            .textFieldStyle(.roundedBorder)
                            .accessibilityIdentifier("new-session-title")
                    }

                    NewSessionFormField(label: "Template") {
                        Picker("Template", selection: $selectedTemplateID) {
                            Text("Explicit prompt").tag(Optional<HubTemplateID>.none)
                            ForEach(viewModel.templates) { template in
                                Text(template.title).tag(Optional(template.id))
                            }
                        }
                        .labelsHidden()
                        .pickerStyle(.menu)
                        .accessibilityIdentifier("template-picker")
                    }

                    NewSessionFormField(label: "Runner") {
                        Picker("Runner", selection: $selectedRunnerID) {
                            Text("Auto placement").tag(Optional<HubRunnerID>.none)
                            ForEach(viewModel.runners) { runner in
                                Text("\(runner.id.rawValue) (\(runner.environmentTag))").tag(Optional(runner.id))
                            }
                        }
                        .labelsHidden()
                        .pickerStyle(.menu)
                        .accessibilityIdentifier("runner-picker")
                    }

                    NewSessionFormField(label: "Guidance target path") {
                        guidanceTargetPathField
                            .textFieldStyle(.roundedBorder)
                    }

                    NewSessionFormField(label: "Workspace Path") {
                        TextField("projects/llm_hub", text: $linkedWorkspacePath)
                            .textFieldStyle(.roundedBorder)
                            .accessibilityIdentifier("linked-workspace-path")
                    }

                    if let template = selectedTemplate {
                        NewSessionTemplateDetails(
                            description: template.description ?? "No description",
                            model: template.defaultModelAlias ?? "Hub default",
                            tools: template.defaultTools ?? ["runner catalog"]
                        )
                    }
                }
                .padding(24)
            }

            Divider()

            HStack(spacing: 10) {
                Spacer()
                Button("Cancel") { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button("Create") {
                    createSession()
                }
                .buttonStyle(.borderedProminent)
                .keyboardShortcut(.defaultAction)
                .disabled(!canCreateSession)
                .accessibilityIdentifier("confirm-create-session")
            }
            .padding(.horizontal, 24)
            .padding(.vertical, 16)
        }
        .frame(width: 560)
        .frame(minHeight: 440)
        .background(Color(nsColor: .windowBackgroundColor))
    }
    #endif

    private var selectedTemplate: HubTemplate? {
        viewModel.templates.first(where: { $0.id == selectedTemplateID })
    }

    @ViewBuilder
    private var guidanceTargetPathField: some View {
        #if os(iOS)
        TextField("Guidance target path", text: $guidanceTargetPath)
            .textInputAutocapitalization(.never)
            .autocorrectionDisabled()
            .accessibilityIdentifier("guidance-target-path")
        #else
        TextField("Guidance target path", text: $guidanceTargetPath)
            .autocorrectionDisabled()
            .accessibilityIdentifier("guidance-target-path")
        #endif
    }

    private var trimmedLinkedWorkspacePath: String? {
        let trimmed = linkedWorkspacePath.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? nil : trimmed
    }

    private var canCreateSession: Bool {
        !title.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    private func createSession() {
        Task {
            if let session = await viewModel.createSession(
                templateID: selectedTemplateID,
                runnerID: selectedRunnerID,
                title: title,
                guidanceTargetPath: guidanceTargetPath,
                linkedWorkspacePath: trimmedLinkedWorkspacePath
            ) {
                onCreated(session)
                dismiss()
            }
        }
    }

    private func selectDefaultsIfNeeded() {
        if selectedTemplateID == nil {
            selectedTemplateID = viewModel.templates.first?.id
        }
        if selectedRunnerID == nil {
            selectedRunnerID = viewModel.runners.first(where: { $0.status == .online })?.id
        }
    }
}

struct NewSessionFormField<Content: View>: View {
    let label: String
    @ViewBuilder let content: () -> Content

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            Text(label)
                .font(.callout.weight(.semibold))
            content()
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

struct NewSessionTemplateDetails: View {
    let description: String
    let model: String
    let tools: [String]
    let showsTitle: Bool
    let usesCard: Bool

    init(
        description: String,
        model: String,
        tools: [String],
        showsTitle: Bool = true,
        usesCard: Bool = true
    ) {
        self.description = description
        self.model = model
        self.tools = tools
        self.showsTitle = showsTitle
        self.usesCard = usesCard
    }

    var body: some View {
        Group {
            if usesCard {
                detailsContent
                    .padding(14)
                    .background(Color.secondary.opacity(0.08), in: RoundedRectangle(cornerRadius: 10))
                    .overlay(
                        RoundedRectangle(cornerRadius: 10)
                            .strokeBorder(Color.secondary.opacity(0.16), lineWidth: 1)
                    )
            } else {
                detailsContent
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private var detailsContent: some View {
        VStack(alignment: .leading, spacing: 14) {
            if showsTitle {
                Text("Template Details")
                    .font(.headline)
            }

            Text(description)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)

            VStack(alignment: .leading, spacing: 12) {
                NewSessionMetadataRow(label: "Model") {
                    Text(model)
                        .font(.body.monospaced())
                        .textSelection(.enabled)
                }

                NewSessionMetadataRow(label: "Tools") {
                    ToolBadgeFlowLayout(horizontalSpacing: 8, verticalSpacing: 8) {
                        ForEach(tools, id: \.self) { tool in
                            ToolNameBadge(name: tool)
                        }
                    }
                }
            }
        }
    }
}

struct NewSessionMetadataRow<Content: View>: View {
    let label: String
    @ViewBuilder let content: () -> Content

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(label)
                .font(.subheadline.weight(.semibold))
            content()
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

struct ToolBadgeFlowLayout: Layout {
    let horizontalSpacing: CGFloat
    let verticalSpacing: CGFloat

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let maximumWidth = proposal.width ?? .greatestFiniteMagnitude
        var currentRowWidth: CGFloat = 0
        var currentRowHeight: CGFloat = 0
        var measuredWidth: CGFloat = 0
        var measuredHeight: CGFloat = 0

        for subview in subviews {
            let size = subview.sizeThatFits(.unspecified)
            let candidateWidth = currentRowWidth == 0 ? size.width : currentRowWidth + horizontalSpacing + size.width

            if currentRowWidth > 0 && candidateWidth > maximumWidth {
                measuredWidth = max(measuredWidth, currentRowWidth)
                measuredHeight += currentRowHeight + verticalSpacing
                currentRowWidth = size.width
                currentRowHeight = size.height
            } else {
                currentRowWidth = candidateWidth
                currentRowHeight = max(currentRowHeight, size.height)
            }
        }

        measuredWidth = max(measuredWidth, currentRowWidth)
        measuredHeight += currentRowHeight
        return CGSize(width: measuredWidth, height: measuredHeight)
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        var nextX = bounds.minX
        var nextY = bounds.minY
        var currentRowHeight: CGFloat = 0

        for subview in subviews {
            let size = subview.sizeThatFits(.unspecified)
            if nextX > bounds.minX && nextX + size.width > bounds.maxX {
                nextX = bounds.minX
                nextY += currentRowHeight + verticalSpacing
                currentRowHeight = 0
            }

            subview.place(
                at: CGPoint(x: nextX, y: nextY),
                proposal: ProposedViewSize(size)
            )
            nextX += size.width + horizontalSpacing
            currentRowHeight = max(currentRowHeight, size.height)
        }
    }
}

struct ToolNameBadge: View {
    let name: String

    var body: some View {
        Text(name)
            .font(.caption.weight(.semibold))
            .lineLimit(1)
            .minimumScaleFactor(0.82)
            .padding(.horizontal, 9)
            .padding(.vertical, 5)
            .background(Color.accentColor.opacity(0.12), in: Capsule())
            .overlay(
                Capsule()
                    .strokeBorder(Color.accentColor.opacity(0.26), lineWidth: 1)
            )
    }
}
