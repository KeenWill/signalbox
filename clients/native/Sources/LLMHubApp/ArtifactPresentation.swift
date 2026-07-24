import Foundation

#if canImport(LLMHubModels)
import LLMHubModels
#endif

extension HubArtifact {
    var isPromptContextArtifact: Bool {
        kind == "prompt_context"
    }

    var presentationTitle: String {
        if isPromptContextArtifact {
            return "Prompt Context"
        }
        let trimmedTitle = title.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmedTitle.isEmpty ? kind : trimmedTitle
    }

    var presentationKindLabel: String {
        switch kind {
        case "prompt_context":
            return "Prompt context"
        case "report":
            return "Report"
        case "system_prompt":
            return "System prompt"
        default:
            return kind
                .replacingOccurrences(of: "_", with: " ")
                .replacingOccurrences(of: "-", with: " ")
                .capitalized
        }
    }

    var presentationSubtitle: String {
        "\(presentationKindLabel)  \(mimeType ?? "unknown type")"
    }

    var presentationSystemImageName: String {
        switch kind {
        case "prompt_context":
            return "brain.head.profile"
        case "system_prompt":
            return "text.quote"
        case "report":
            return "doc.text"
        default:
            return "shippingbox"
        }
    }

    var presentationSystemImage: String {
        presentationSystemImageName
    }

    var presentationAccessibilityLabel: String {
        "\(presentationTitle), \(presentationSubtitle)"
    }

    var accessibilitySummary: String {
        presentationAccessibilityLabel
    }

    var previewNavigationTitle: String {
        isPromptContextArtifact ? "Prompt Context" : "Artifact"
    }

    var prefersMonospacedPreview: Bool {
        isPromptContextArtifact
            || mimeType == "application/json"
            || mimeType == "application/x-ndjson"
            || kind == "system_prompt"
            || contentText?.contains("```") == true
    }

    var usesMonospacedPreviewText: Bool {
        prefersMonospacedPreview
    }
}
