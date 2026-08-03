//! Dedicated model execution for delegated tool-approval decisions.

use std::{error::Error, fmt, future::Future, pin::Pin};

use serde_json::{Map, Value, value::RawValue};
use signalbox_domain::{
    DelegateApprovalRecommendation, DirectModelSelection, ModelCallId, ResolvedProviderTarget,
    SessionId, ToolDecisionRationale,
};
use signalbox_model_runtime::{
    CancellationSignal, CompletionFinish, ConversationMessage, CredentialReference, DeliveryMode,
    ModelOperation, ModelRuntime, ModelSettings, NoDomainConstraints, Observation,
    PreparationOutcome, ProviderReportedModel, RequestedTarget, ResolvedTarget,
    StructuredOutputContract, TerminalEvidence, TokenUsage, UnsentCause, decode_structured,
};

use crate::{ProviderTargetRelation, RuntimeModelCatalog, relate_provider_target};

const OUTPUT_NAME: &str = "tool_approval_decision";
const OUTPUT_DESCRIPTION: &str =
    "Decide whether this exact tool request may run and explain the decision.";
const OUTPUT_SCHEMA: &str = r#"{
  "type":"object",
  "properties":{
    "recommendation":{
      "type":"string",
      "enum":["approve","deny","escalate_to_human"]
    },
    "rationale":{"type":"string","minLength":1,"maxLength":4096}
  },
  "required":["recommendation","rationale"],
  "additionalProperties":false
}"#;

/// Exact inputs to one dedicated approval-judge model call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalJudgeModelRequest {
    /// Durable physical call identity.
    pub call: ModelCallId,
    /// Session whose parked tool request is judged.
    pub session: SessionId,
    /// Direct model selection frozen for this call.
    pub selection: DirectModelSelection,
    /// Exact resolved provider target.
    pub target: ResolvedProviderTarget,
    /// Non-secret credential reference pinned in durable call evidence.
    pub credential_reference: String,
    /// Daemon-owned safety instructions for the judge.
    pub system_prompt: String,
    /// Deterministic rendering of the exact parked request.
    pub rendered_request: String,
}

/// Exact successful result of one dedicated approval-judge call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalJudgeModelResult {
    /// Closed recommendation emitted by the judge.
    pub recommendation: DelegateApprovalRecommendation,
    /// Checked nonempty rationale emitted with the recommendation.
    pub rationale: ToolDecisionRationale,
    /// Provider-reported usage, independently optional by field.
    pub usage: TokenUsage,
}

/// One provider adapter capable of executing a dedicated approval-judge call.
pub trait ApprovalJudgeModel: fmt::Debug + Send + Sync {
    /// Executes the already-durably-authorized request exactly once.
    fn execute<'a>(
        &'a self,
        request: ApprovalJudgeModelRequest,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ApprovalJudgeModelResult, ApprovalJudgeModelError>>
                + Send
                + 'a,
        >,
    >;
}

impl<Model> ApprovalJudgeModel for std::sync::Arc<Model>
where
    Model: ApprovalJudgeModel + ?Sized,
{
    fn execute<'a>(
        &'a self,
        request: ApprovalJudgeModelRequest,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ApprovalJudgeModelResult, ApprovalJudgeModelError>>
                + Send
                + 'a,
        >,
    > {
        (**self).execute(request)
    }
}

/// Layer-1 runtime adapter for dedicated approval-judge calls.
#[derive(Clone, Debug)]
pub struct RuntimeApprovalJudgeModel<R> {
    runtime: R,
    models: RuntimeModelCatalog,
}

impl<R> RuntimeApprovalJudgeModel<R> {
    /// Supplies the runtime and immutable target mapping.
    pub const fn new(runtime: R, models: RuntimeModelCatalog) -> Self {
        Self { runtime, models }
    }
}

impl<R> ApprovalJudgeModel for RuntimeApprovalJudgeModel<R>
where
    R: ModelRuntime<ModelCallId> + fmt::Debug + Send + Sync,
    R::Prepared: Send,
{
    fn execute<'a>(
        &'a self,
        request: ApprovalJudgeModelRequest,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ApprovalJudgeModelResult, ApprovalJudgeModelError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let definition = self
                .models
                .resolve(request.target)
                .ok_or(ApprovalJudgeModelError::UnconfiguredTarget)?;
            let resolved = ResolvedTarget::new(definition.provider_model().to_owned());
            let contract = output_contract()?;
            let mut operation = ModelOperation::new(
                request.call,
                CredentialReference::new(request.credential_reference),
                RequestedTarget::new(render_selection(request.selection)),
                resolved.clone(),
                vec![ConversationMessage::user_text(request.rendered_request)],
                ModelSettings::new(definition.max_output_tokens()),
            );
            operation.system = Some(request.system_prompt);
            operation.output_contract = Some(contract.clone());
            operation.delivery = DeliveryMode::Buffered;
            let prepared = match self
                .runtime
                .prepare(operation, CancellationSignal::never())
                .await
            {
                PreparationOutcome::Prepared(prepared) => prepared,
                PreparationOutcome::Cancelled { .. } => {
                    return Err(ApprovalJudgeModelError::CancelledBeforeSend);
                }
                PreparationOutcome::Failed { .. } => {
                    return Err(ApprovalJudgeModelError::PreparationFailed);
                }
                PreparationOutcome::Defect { .. } => {
                    return Err(ApprovalJudgeModelError::PreparationDefect);
                }
            };
            let mut observations: Vec<Observation<ModelCallId>> = Vec::new();
            let report = self
                .runtime
                .execute(prepared, &mut observations, CancellationSignal::never())
                .await;
            let usage = terminal_usage(&report.evidence);
            if report.correlation != request.call {
                return Err(ApprovalJudgeModelError::CorrelationMismatch(usage));
            }
            require_observation_correlations(&observations, request.call, usage)?;
            for reported in observations.iter().filter_map(|observation| {
                let signalbox_model_runtime::ObservationFact::ProviderModelReported(reported) =
                    &observation.fact
                else {
                    return None;
                };
                Some(reported)
            }) {
                require_same_target(&resolved, reported, usage)?;
            }
            let reported_model = match &report.evidence {
                TerminalEvidence::Completed(evidence) => evidence.reported_model.as_ref(),
                TerminalEvidence::Refused(evidence) => evidence.reported_model.as_ref(),
                TerminalEvidence::ProviderError(evidence) => evidence.reported_model.as_ref(),
                TerminalEvidence::CancellationConfirmed(evidence) => {
                    evidence.reported_model.as_ref()
                }
                TerminalEvidence::BoundaryLoss(evidence) => evidence.reported_model.as_ref(),
                TerminalEvidence::ProvenUnsent(_) => None,
            };
            if let Some(reported) = reported_model {
                require_same_target(&resolved, reported, usage)?;
            }
            let completed = match report.evidence {
                TerminalEvidence::Completed(completed) => completed,
                TerminalEvidence::Refused(evidence) => {
                    return Err(ApprovalJudgeModelError::Refused(evidence.usage));
                }
                TerminalEvidence::ProviderError(evidence) => {
                    return Err(ApprovalJudgeModelError::ProviderError(evidence.usage));
                }
                TerminalEvidence::CancellationConfirmed(_) => {
                    return Err(ApprovalJudgeModelError::CancellationConfirmed);
                }
                TerminalEvidence::ProvenUnsent(evidence) => {
                    return Err(match evidence.cause {
                        UnsentCause::CancelledBeforeSend => {
                            ApprovalJudgeModelError::CancelledBeforeSend
                        }
                        UnsentCause::ConnectFailed(_)
                        | UnsentCause::SendIncompleteProvenUnacceptable(_) => {
                            ApprovalJudgeModelError::ProvenUnsent
                        }
                    });
                }
                TerminalEvidence::BoundaryLoss(evidence) => {
                    return Err(ApprovalJudgeModelError::BoundaryLoss(evidence.usage));
                }
            };
            if completed.finish != CompletionFinish::ToolUse {
                return Err(ApprovalJudgeModelError::IncompleteDecision(completed.usage));
            }
            let decoded: Value =
                decode_structured(&completed.content, &contract, &NoDomainConstraints)
                    .map_err(|_| ApprovalJudgeModelError::InvalidDecision(completed.usage))?;
            let (recommendation, rationale) = decode_decision(decoded)
                .map_err(|_| ApprovalJudgeModelError::InvalidDecision(completed.usage))?;
            Ok(ApprovalJudgeModelResult {
                recommendation,
                rationale,
                usage: completed.usage,
            })
        })
    }
}

fn output_contract() -> Result<StructuredOutputContract, ApprovalJudgeModelError> {
    let schema = RawValue::from_string(String::from(OUTPUT_SCHEMA))
        .map_err(|_| ApprovalJudgeModelError::InvalidContract)?;
    Ok(StructuredOutputContract {
        name: signalbox_model_runtime::ToolName::new(OUTPUT_NAME),
        description: String::from(OUTPUT_DESCRIPTION),
        schema,
    })
}

fn decode_decision(
    value: Value,
) -> Result<(DelegateApprovalRecommendation, ToolDecisionRationale), InvalidDecision> {
    let Value::Object(mut fields) = value else {
        return Err(InvalidDecision);
    };
    if fields.len() != 2 {
        return Err(InvalidDecision);
    }
    let recommendation = take_string(&mut fields, "recommendation")?;
    let rationale = take_string(&mut fields, "rationale")?;
    let recommendation = match recommendation.as_str() {
        "approve" => DelegateApprovalRecommendation::Approve,
        "deny" => DelegateApprovalRecommendation::Deny,
        "escalate_to_human" => DelegateApprovalRecommendation::EscalateToHuman,
        _ => return Err(InvalidDecision),
    };
    let rationale = ToolDecisionRationale::try_new(rationale).map_err(|_| InvalidDecision)?;
    Ok((recommendation, rationale))
}

fn take_string(
    fields: &mut Map<String, Value>,
    field: &'static str,
) -> Result<String, InvalidDecision> {
    match fields.remove(field) {
        Some(Value::String(value)) => Ok(value),
        Some(_) | None => Err(InvalidDecision),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InvalidDecision;

fn render_selection(selection: DirectModelSelection) -> String {
    format!("direct:{}", selection.into_uuid())
}

fn require_same_target(
    configured: &ResolvedTarget,
    reported: &ProviderReportedModel,
    usage: TokenUsage,
) -> Result<(), ApprovalJudgeModelError> {
    match relate_provider_target(configured, reported) {
        ProviderTargetRelation::Exact | ProviderTargetRelation::AliasConcretion => Ok(()),
        ProviderTargetRelation::DifferentLineage => {
            Err(ApprovalJudgeModelError::ProviderTargetSubstituted(usage))
        }
    }
}

fn terminal_usage(evidence: &TerminalEvidence) -> TokenUsage {
    match evidence {
        TerminalEvidence::Completed(value) => value.usage,
        TerminalEvidence::Refused(value) => value.usage,
        TerminalEvidence::ProviderError(value) => value.usage,
        TerminalEvidence::BoundaryLoss(value) => value.usage,
        TerminalEvidence::CancellationConfirmed(_) | TerminalEvidence::ProvenUnsent(_) => {
            TokenUsage::default()
        }
    }
}

fn require_observation_correlations(
    observations: &[Observation<ModelCallId>],
    expected: ModelCallId,
    usage: TokenUsage,
) -> Result<(), ApprovalJudgeModelError> {
    observations
        .iter()
        .all(|observation| observation.correlation == expected)
        .then_some(())
        .ok_or(ApprovalJudgeModelError::CorrelationMismatch(usage))
}

/// Sanitized failure of one dedicated approval-judge call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalJudgeModelError {
    /// The durable target has no matching runtime configuration.
    UnconfiguredTarget,
    /// The static structured-output contract could not be constructed.
    InvalidContract,
    /// Runtime preparation observed cancellation before send.
    CancelledBeforeSend,
    /// Credential or request preparation failed safely.
    PreparationFailed,
    /// Adapter request construction was defective.
    PreparationDefect,
    /// Runtime correlation differed from the durable call.
    CorrelationMismatch(TokenUsage),
    /// The provider returned an explicit refusal.
    Refused(TokenUsage),
    /// A complete, correlated provider error response was observed.
    ProviderError(TokenUsage),
    /// The provider definitively confirmed cancellation.
    CancellationConfirmed,
    /// The request provably never reached an acceptance-capable boundary.
    ProvenUnsent,
    /// Provider acceptance or completion remained uncertain.
    BoundaryLoss(TokenUsage),
    /// The provider reported a different model lineage.
    ProviderTargetSubstituted(TokenUsage),
    /// The completion stopped before a complete decision.
    IncompleteDecision(TokenUsage),
    /// The completion lacked exactly one valid typed decision.
    InvalidDecision(TokenUsage),
}

impl ApprovalJudgeModelError {
    /// Provider-reported usage observed before the call failed.
    pub const fn usage(self) -> TokenUsage {
        match self {
            Self::CorrelationMismatch(usage)
            | Self::Refused(usage)
            | Self::ProviderError(usage)
            | Self::BoundaryLoss(usage)
            | Self::ProviderTargetSubstituted(usage)
            | Self::IncompleteDecision(usage)
            | Self::InvalidDecision(usage) => usage,
            Self::UnconfiguredTarget
            | Self::InvalidContract
            | Self::CancelledBeforeSend
            | Self::PreparationFailed
            | Self::PreparationDefect
            | Self::CancellationConfirmed
            | Self::ProvenUnsent => TokenUsage {
                input_tokens: None,
                output_tokens: None,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
        }
    }
}

impl fmt::Display for ApprovalJudgeModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("approval judge model execution failed")
    }
}

impl Error for ApprovalJudgeModelError {}

#[cfg(test)]
mod tests {
    use signalbox_domain::{
        DelegateApprovalRecommendation, DirectModelSelection, ModelCallId, ProviderModelIdentity,
        ResolvedProviderTarget, SessionId,
    };
    use signalbox_model_runtime::{
        AssistantPart, CompletionEvidence, CompletionFinish, ExchangeFacts, Observation,
        ObservationFact, ProviderReportedModel, Script, ScriptedModel, TerminalEvidence,
        TokenUsage, ToolCallId, ToolCallProposal, ToolName,
    };
    use uuid::Uuid;

    use super::{
        ApprovalJudgeModel, ApprovalJudgeModelError, ApprovalJudgeModelRequest,
        RuntimeApprovalJudgeModel,
    };
    use crate::{RuntimeModelCatalog, RuntimeModelDefinition};

    const PROVIDER_MODEL: &str = "fixture-judge-model";
    const APPROVAL_RATIONALE: &str = "The exact read is bounded.";
    const INPUT_TOKENS: u64 = 11;
    const OUTPUT_TOKENS: u64 = 5;
    const CACHE_READ_INPUT_TOKENS: u64 = 3;

    fn target() -> ResolvedProviderTarget {
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(7)))
    }

    fn catalog() -> RuntimeModelCatalog {
        RuntimeModelCatalog::try_from_definitions([RuntimeModelDefinition::try_new(
            target(),
            String::from(PROVIDER_MODEL),
            256,
            200_000,
        )
        .expect("the fixture definition states a request-safe mapping")])
        .expect("the fixture catalog names one target once")
    }

    fn request() -> ApprovalJudgeModelRequest {
        ApprovalJudgeModelRequest {
            call: ModelCallId::from_uuid(Uuid::from_u128(1)),
            session: SessionId::from_uuid(Uuid::from_u128(2)),
            selection: DirectModelSelection::from_uuid(Uuid::from_u128(3)),
            target: target(),
            credential_reference: String::from("fixture-judge-credential"),
            system_prompt: String::from("Judge only the supplied request."),
            rendered_request: String::from("{\"tool\":\"echo\",\"arguments\":{}}"),
        }
    }

    fn completion(arguments: &str) -> Script {
        completion_with_finish(arguments, CompletionFinish::ToolUse)
    }

    fn completion_with_finish(arguments: &str, finish: CompletionFinish) -> Script {
        Script::delivering(TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new(PROVIDER_MODEL)),
            finish,
            content: vec![AssistantPart::ToolCall(ToolCallProposal {
                id: ToolCallId::new("judge_call"),
                name: ToolName::new(super::OUTPUT_NAME),
                arguments_json: arguments.to_owned(),
            })],
            usage: reported_usage(),
        }))
    }

    const fn reported_usage() -> TokenUsage {
        TokenUsage {
            input_tokens: Some(INPUT_TOKENS),
            output_tokens: Some(OUTPUT_TOKENS),
            cache_creation_input_tokens: None,
            cache_read_input_tokens: Some(CACHE_READ_INPUT_TOKENS),
        }
    }

    fn approval_completion() -> Script {
        completion(
            &serde_json::json!({
                "recommendation": "approve",
                "rationale": APPROVAL_RATIONALE,
            })
            .to_string(),
        )
    }

    #[tokio::test]
    async fn typed_approval_result_preserves_all_evidence() {
        let model = RuntimeApprovalJudgeModel::new(
            ScriptedModel::<ModelCallId>::single(approval_completion()),
            catalog(),
        );

        let result = model
            .execute(request())
            .await
            .expect("the typed decision is admitted");

        assert_eq!(
            result.recommendation,
            DelegateApprovalRecommendation::Approve
        );
        assert_eq!(result.rationale.as_str(), APPROVAL_RATIONALE);
        assert_eq!(result.usage.input_tokens, Some(INPUT_TOKENS));
        assert_eq!(result.usage.output_tokens, Some(OUTPUT_TOKENS));
        assert_eq!(
            result.usage.cache_read_input_tokens,
            Some(CACHE_READ_INPUT_TOKENS)
        );
    }

    #[tokio::test]
    async fn unknown_recommendation_fails_closed() {
        let model = RuntimeApprovalJudgeModel::new(
            ScriptedModel::<ModelCallId>::single(completion(
                r#"{"recommendation":"maybe","rationale":"Unsure."}"#,
            )),
            catalog(),
        );

        let error = model
            .execute(request())
            .await
            .expect_err("an open recommendation vocabulary is rejected");

        assert_eq!(
            error,
            ApprovalJudgeModelError::InvalidDecision(reported_usage())
        );
    }

    #[tokio::test]
    async fn non_tool_finish_fails_closed_for_structured_judge_decision() {
        let model = RuntimeApprovalJudgeModel::new(
            ScriptedModel::<ModelCallId>::single(completion_with_finish(
                &serde_json::json!({
                    "recommendation": "approve",
                    "rationale": APPROVAL_RATIONALE,
                })
                .to_string(),
                CompletionFinish::EndTurn,
            )),
            catalog(),
        );

        let error = model
            .execute(request())
            .await
            .expect_err("a tool decision requires a tool-use finish");

        assert_eq!(
            error,
            ApprovalJudgeModelError::IncompleteDecision(reported_usage())
        );
    }

    #[test]
    fn mismatched_observation_correlation_fails_closed() {
        let expected = request().call;
        let observation = Observation {
            correlation: ModelCallId::from_uuid(Uuid::from_u128(99)),
            fact: ObservationFact::ProviderModelReported(ProviderReportedModel::new(
                PROVIDER_MODEL,
            )),
        };

        let error =
            super::require_observation_correlations(&[observation], expected, reported_usage())
                .expect_err("an unrelated observation is rejected before its facts are read");

        assert_eq!(
            error,
            ApprovalJudgeModelError::CorrelationMismatch(reported_usage())
        );
    }
}
