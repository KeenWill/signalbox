#if canImport(LLMHubModels)
import LLMHubModels
#endif
import SwiftUI

struct ArtifactPreviewScreen: View {
    let artifact: HubArtifact
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            VStack(alignment: .leading, spacing: 12) {
                VStack(alignment: .leading, spacing: 4) {
                    Label {
                        Text(artifact.presentationTitle)
                            .font(.title2.weight(.bold))
                    } icon: {
                        Image(systemName: artifact.presentationSystemImageName)
                            .foregroundStyle(.secondary)
                    }
                    Text(artifact.presentationSubtitle)
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.secondary)
                }

                if let content = artifact.contentText {
                    ScrollView {
                        Text(content)
                            .font(.system(.body, design: contentLooksLikeCode(content) ? .monospaced : .default))
                            .textSelection(.enabled)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .padding(12)
                    }
                    .background(.secondary.opacity(0.08), in: RoundedRectangle(cornerRadius: 8))
                    .accessibilityIdentifier("artifact-preview-text")
                } else if let path = artifact.path {
                    Label(path, systemImage: "folder")
                        .font(.callout)
                        .textSelection(.enabled)
                        .padding(12)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(.secondary.opacity(0.08), in: RoundedRectangle(cornerRadius: 8))
                } else {
                    EmptyStateView(
                        systemImage: "doc.questionmark",
                        title: "No preview",
                        message: "This artifact does not include inline text."
                    )
                }
            }
            .padding()
            .navigationTitle(artifact.previewNavigationTitle)
            #if os(iOS)
            .navigationBarTitleDisplayMode(.inline)
            #endif
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Done") { dismiss() }
                }
                #if os(iOS)
                if let content = artifact.contentText {
                    ToolbarItem(placement: .primaryAction) {
                        ShareLink(item: content) {
                            Image(systemName: "square.and.arrow.up")
                        }
                    }
                }
                #endif
            }
        }
        #if os(macOS)
        .frame(minWidth: 520, minHeight: 460)
        #endif
        .accessibilityIdentifier("artifact-preview")
    }

    private func contentLooksLikeCode(_ content: String) -> Bool {
        artifact.prefersMonospacedPreview || content.contains("```")
    }
}
