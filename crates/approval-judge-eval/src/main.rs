//! Offline operator CLI for replaying recorded approval-judge responses.

use std::{env, error::Error, fs, io};

use serde::Deserialize;
use signalbox_approval_judge_eval::{ApprovalDisposition, load_corpus, score_corpus};
use signalbox_domain::{
    DirectModelSelection, ModelCallId, ProviderModelIdentity, ResolvedProviderTarget,
};
use signalbox_model_provider_runtime::{
    RuntimeApprovalJudgeModel, RuntimeModelCatalog, RuntimeModelDefinition,
};
use signalbox_model_runtime::{
    AssistantPart, CompletionEvidence, CompletionFinish, ExchangeFacts, ProviderReportedModel,
    Script, ScriptedModel, TerminalEvidence, TokenUsage, ToolCallId, ToolCallProposal, ToolName,
};
use signalboxd::approval_judge_eval::ApprovalJudgeEvalBinding;
use uuid::Uuid;

const OFFLINE_PROVIDER_MODEL: &str = "offline-recorded-approval-judge";
// Arbitrary constructor parameter: scripted replay reports usage as
// unreported and enforces no output bound, so this only satisfies the model
// definition.
const OFFLINE_MAX_OUTPUT_TOKENS: u32 = 256;
// Arbitrary constructor parameter: the offline replay path never reads the
// context window, so this satisfies the model definition without enforcing
// any bound.
const OFFLINE_CONTEXT_WINDOW_TOKENS: u32 = 4_096;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OfflineResponseFile {
    responses: Vec<OfflineResponse>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OfflineResponse {
    disposition: ApprovalDisposition,
    rationale: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = env::args().skip(1);
    let corpus_path = arguments.next().ok_or_else(usage_error)?;
    let responses_path = arguments.next().ok_or_else(usage_error)?;
    if arguments.next().is_some() {
        return Err(usage_error().into());
    }

    let corpus = load_corpus(corpus_path)?;
    let response_bytes = fs::read(&responses_path).map_err(|source| {
        io::Error::new(
            source.kind(),
            format!("could not read offline responses {responses_path}: {source}"),
        )
    })?;
    let responses: OfflineResponseFile =
        serde_json::from_slice(&response_bytes).map_err(|source| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("offline responses {responses_path} are not valid response JSON: {source}"),
            )
        })?;
    if responses.responses.len() != corpus.cases.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "offline response count {} differs from corpus case count {}",
                responses.responses.len(),
                corpus.cases.len(),
            ),
        )
        .into());
    }
    let scripts = responses.responses.iter().map(response_script);
    let (model, binding) = offline_model(scripts)?;
    let scorecard = score_corpus(&model, &binding, &corpus).await?;
    println!("{}", serde_json::to_string_pretty(&scorecard)?);
    Ok(())
}

fn usage_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "usage: signalbox-approval-judge-eval <corpus.json> <offline-responses.json>",
    )
}

fn offline_model(
    scripts: impl IntoIterator<Item = Script>,
) -> Result<
    (
        RuntimeApprovalJudgeModel<ScriptedModel<ModelCallId>>,
        ApprovalJudgeEvalBinding,
    ),
    io::Error,
> {
    let target =
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(30)));
    let definition = RuntimeModelDefinition::try_new(
        target,
        String::from(OFFLINE_PROVIDER_MODEL),
        OFFLINE_MAX_OUTPUT_TOKENS,
        OFFLINE_CONTEXT_WINDOW_TOKENS,
    )
    .map_err(|error| io::Error::other(error.to_string()))?;
    let catalog = RuntimeModelCatalog::try_from_definitions([definition])
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok((
        RuntimeApprovalJudgeModel::new(ScriptedModel::following(scripts), catalog),
        ApprovalJudgeEvalBinding {
            selection: DirectModelSelection::from_uuid(Uuid::from_u128(31)),
            target,
            credential_reference: String::from("offline-recorded-response"),
        },
    ))
}

fn response_script(response: &OfflineResponse) -> Script {
    let arguments_json = serde_json::json!({
        "recommendation": response.disposition.as_str(),
        "rationale": response.rationale,
    })
    .to_string();
    Script::delivering(TerminalEvidence::Completed(CompletionEvidence {
        exchange: ExchangeFacts::default(),
        message_id: None,
        reported_model: Some(ProviderReportedModel::new(OFFLINE_PROVIDER_MODEL)),
        finish: CompletionFinish::ToolUse,
        content: vec![AssistantPart::ToolCall(ToolCallProposal {
            id: ToolCallId::new("offline_recorded_decision"),
            name: ToolName::new("tool_approval_decision"),
            arguments_json,
        })],
        usage: TokenUsage::unreported(),
    }))
}
