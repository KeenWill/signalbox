#if canImport(LLMHubModels)
import LLMHubModels
#endif

struct PromptContextSummary: Equatable {
    let projectContextConsidered: Bool
    let truncated: Bool
    let guidanceDocumentCount: Int
    let selectedSkillCardCount: Int
    let selectedSkillDocumentCount: Int
    let selectedAgentProfileCardCount: Int
    let modelAlias: String?
    let runnerID: String?
    let workspaceDir: String?
    let projectWorkspaceDir: String?
    let linkedWorkspacePath: String?
    let enabledToolCount: Int
}

extension HubArtifact {
    var promptContextSummary: PromptContextSummary? {
        guard
            isPromptContextArtifact,
            let projectContextConsidered = metadata.bool(forKey: "project_context_considered"),
            let truncated = metadata.bool(forKey: "truncated"),
            let guidanceDocumentCount = metadata.arrayCount(forKey: "guidance_documents"),
            let selectedSkillCardCount = metadata.int(forKey: "selected_skill_card_count"),
            let selectedSkillDocumentCount = metadata.int(forKey: "selected_skill_document_count"),
            let selectedAgentProfileCardCount = metadata.int(forKey: "selected_agent_profile_card_count"),
            let runtimeContext = metadata.object(forKey: "runtime_context"),
            let enabledToolCount = runtimeContext.arrayCount(forKey: "enabled_tool_names")
        else { return nil }

        return PromptContextSummary(
            projectContextConsidered: projectContextConsidered,
            truncated: truncated,
            guidanceDocumentCount: guidanceDocumentCount,
            selectedSkillCardCount: selectedSkillCardCount,
            selectedSkillDocumentCount: selectedSkillDocumentCount,
            selectedAgentProfileCardCount: selectedAgentProfileCardCount,
            modelAlias: runtimeContext.string(forKey: "model_alias"),
            runnerID: runtimeContext.string(forKey: "runner_id"),
            workspaceDir: runtimeContext.string(forKey: "workspace_dir"),
            projectWorkspaceDir: runtimeContext.string(forKey: "project_workspace_dir"),
            linkedWorkspacePath: runtimeContext.string(forKey: "linked_workspace_path"),
            enabledToolCount: enabledToolCount
        )
    }
}

private extension Dictionary where Key == String, Value == HubJSONValue {
    func object(forKey key: String) -> [String: HubJSONValue]? {
        guard case .object(let object) = self[key] else { return nil }
        return object
    }

    func arrayCount(forKey key: String) -> Int? {
        guard case .array(let array) = self[key] else { return nil }
        return array.count
    }

    func string(forKey key: String) -> String? {
        guard case .string(let string) = self[key] else { return nil }
        return string
    }

    func bool(forKey key: String) -> Bool? {
        guard case .bool(let bool) = self[key] else { return nil }
        return bool
    }

    func int(forKey key: String) -> Int? {
        guard case .number(let number) = self[key] else { return nil }
        guard number.isFinite, number >= 0, number.rounded(.towardZero) == number else {
            return nil
        }
        guard number <= Double(Int.max) else { return nil }
        return Int(number)
    }
}
