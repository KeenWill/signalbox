//! Recorded provider answers executed through the deployed judge adapter.

use super::{JudgeBinding, RecordedResponse, effects::stable_digest};
use signalbox_domain::{ModelCallId, ProviderModelIdentity, ResolvedProviderTarget};
use signalbox_model_provider_runtime::{
    RuntimeApprovalJudgeModel, RuntimeModelCatalog, RuntimeModelDefinition,
};
use signalbox_model_runtime::{
    AssistantPart, CompletionEvidence, CompletionFinish, ExchangeFacts, ProviderReportedModel,
    Script, ScriptedModel, TerminalEvidence, TokenUsage, ToolCallId, ToolCallProposal, ToolName,
};

const PROVIDER_MODEL: &str = "offline-recorded-approval-judge";
// Synthetic identities distinguish recorded execution from configured provider routes.
const TARGET: u128 = 30;
const SELECTION: u128 = 31;

pub fn recorded_binding() -> JudgeBinding {
    JudgeBinding {
        selection: uuid::Uuid::from_u128(SELECTION).to_string(),
        target: uuid::Uuid::from_u128(TARGET).to_string(),
        credential_reference: "offline-recorded-response".into(),
        provider_model: PROVIDER_MODEL.into(),
        contract_digest: stable_digest(
            signalbox_model_provider_runtime::approval_judge_output_contract_text().as_bytes(),
        ),
        cache_accounting: "input_excludes_cache".into(),
    }
}

pub(super) fn model(
    response: &RecordedResponse,
) -> Result<RuntimeApprovalJudgeModel<ScriptedModel<ModelCallId>>, String> {
    let target = ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
        uuid::Uuid::from_u128(TARGET),
    ));
    // Scripted execution reports no usage; these constructor values enforce no bound.
    let definition = RuntimeModelDefinition::try_new(target, PROVIDER_MODEL.into(), 256, 4096)
        .map_err(|error| error.to_string())?;
    let catalog = RuntimeModelCatalog::try_from_definitions([definition])
        .map_err(|error| error.to_string())?;
    let script = Script::delivering(TerminalEvidence::Completed(CompletionEvidence {
        exchange: ExchangeFacts::default(),
        message_id: None,
        reported_model: Some(ProviderReportedModel::new(PROVIDER_MODEL)),
        finish: CompletionFinish::ToolUse,
        content: vec![AssistantPart::ToolCall(ToolCallProposal {
            id: ToolCallId::new("offline_recorded_decision"),
            name: ToolName::new("tool_approval_decision"),
            arguments_json: serde_json::json!({"recommendation": response.disposition.as_str(), "rationale": response.rationale}).to_string(),
        })],
        usage: TokenUsage::unreported(),
    }));
    Ok(RuntimeApprovalJudgeModel::new(
        ScriptedModel::following([script]),
        catalog,
    ))
}
