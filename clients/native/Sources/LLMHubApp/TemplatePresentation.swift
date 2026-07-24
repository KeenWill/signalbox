#if canImport(LLMHubModels)
import LLMHubModels
#endif

extension HubTemplate {
    var visibleDerivativeStatus: HubTemplateDerivativeStatus? {
        guard let derivativeStatus, derivativeStatus != .notDerived else { return nil }
        return derivativeStatus
    }

    var versionSummary: String? {
        switch (templateVersion, promptVersion) {
        case (.some(let templateVersion), .some(let promptVersion)):
            return "template \(templateVersion), prompt \(promptVersion)"
        case (.some(let templateVersion), .none):
            return "template \(templateVersion)"
        case (.none, .some(let promptVersion)):
            return "prompt \(promptVersion)"
        case (.none, .none):
            return nil
        }
    }

    var derivativeAdvisoryText: String? {
        guard let status = visibleDerivativeStatus else { return nil }
        let source = derivedFromTemplateID?.rawValue ?? "source template"
        switch status {
        case .current:
            return "Derived from \(source); source is current."
        case .stale:
            return sourceVersionAdvisory(
                prefix: "Derived from \(source); source has changed",
                templateVersion: currentSourceTemplateVersion,
                promptVersion: currentSourceTemplatePromptVersion
            )
        case .sourceMissing:
            return "Derived from \(source), but the source template is missing."
        case .sourceArchived:
            return "Derived from \(source), but the source template is archived."
        case .unknown(let value):
            return "Derived template status: \(value)."
        case .notDerived:
            return nil
        }
    }

    private func sourceVersionAdvisory(
        prefix: String,
        templateVersion: String?,
        promptVersion: String?
    ) -> String {
        switch (templateVersion, promptVersion) {
        case (.some(let templateVersion), .some(let promptVersion)):
            return "\(prefix) to template \(templateVersion), prompt \(promptVersion)."
        case (.some(let templateVersion), .none):
            return "\(prefix) to template \(templateVersion)."
        case (.none, .some(let promptVersion)):
            return "\(prefix) to prompt \(promptVersion)."
        case (.none, .none):
            return "\(prefix)."
        }
    }
}
