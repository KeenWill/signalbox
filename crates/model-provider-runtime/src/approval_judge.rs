//! Dedicated model execution for delegated tool-approval decisions.

use std::{error::Error, fmt, future::Future, pin::Pin, sync::Arc};

use serde_json::{Map, Value, value::RawValue};
use signalbox_application::ApprovalJudgeAuthorization;
use signalbox_domain::{
    DelegateApprovalRecommendation, DirectModelSelection, ModelCallId, ResolvedProviderTarget,
    ToolDecisionRationale, ToolRequest,
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

/// Returns a canonical rendering of the complete structured-output contract
/// every approval-judge call carries — name, description, and schema — so
/// measurement harnesses can fingerprint the exact acceptance criteria they
/// replay. U+0000 separators cannot occur inside any component.
#[must_use]
pub fn approval_judge_output_contract_text() -> String {
    format!(
        "{OUTPUT_NAME}\u{0}{OUTPUT_DESCRIPTION}\u{0}{OUTPUT_SCHEMA}\u{0}rationale_decoder=nonempty,max_utf8_bytes={},exclude_u+0000",
        ToolDecisionRationale::MAX_UTF8_BYTES
    )
}

/// Exact inputs to one dedicated approval-judge model call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalJudgeModelRequest {
    /// Exact parked request being judged.
    pub request: ToolRequest,
    /// Durable physical call identity.
    pub call: ModelCallId,
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
    /// Durable physical call identity returned with the decision.
    pub call: ModelCallId,
    /// Closed recommendation emitted by the judge.
    pub recommendation: DelegateApprovalRecommendation,
    /// Checked nonempty rationale emitted with the recommendation.
    pub rationale: ToolDecisionRationale,
    /// Model identity reported by the provider, when one was observable.
    pub reported_model: Option<ProviderReportedModel>,
    /// Provider-reported usage, independently optional by field.
    pub usage: TokenUsage,
}

/// One provider adapter capable of executing a dedicated approval-judge call.
pub trait ApprovalJudgeModel: fmt::Debug + Send + Sync {
    /// Prepares one provider capability without provider traffic.
    fn prepare<'a>(
        &'a self,
        request: ApprovalJudgeModelRequest,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<PreparedApprovalJudgeModelCall, ApprovalJudgeModelError>>
                + Send
                + 'a,
        >,
    >;
}

type ApprovalJudgeExecutionFuture = Pin<
    Box<
        dyn Future<Output = Result<ApprovalJudgeModelResult, ApprovalJudgeModelError>>
            + Send
            + 'static,
    >,
>;

/// One prepared, non-cloneable approval-judge provider capability.
pub struct PreparedApprovalJudgeModelCall {
    binding: PreparedApprovalJudgeBinding,
    execute: Box<dyn FnOnce() -> ApprovalJudgeExecutionFuture + Send>,
}

impl PreparedApprovalJudgeModelCall {
    fn new(
        binding: PreparedApprovalJudgeBinding,
        execute: impl FnOnce() -> ApprovalJudgeExecutionFuture + Send + 'static,
    ) -> Self {
        Self {
            binding,
            execute: Box::new(execute),
        }
    }

    /// Consumes this capability and its exact durable authorization once.
    pub fn execute<Authorization>(
        self,
        authorization: Authorization,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ApprovalJudgeModelResult, ApprovalJudgeModelError>>
                + Send
                + 'static,
        >,
    >
    where
        Authorization: ApprovalJudgeAuthorization,
    {
        if !self.binding.matches(&authorization) {
            return Box::pin(async { Err(ApprovalJudgeModelError::AuthorizationMismatch) });
        }
        (self.execute)()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PreparedApprovalJudgeBinding {
    request: ToolRequest,
    call: ModelCallId,
    selection: DirectModelSelection,
    target: ResolvedProviderTarget,
    credential_reference: String,
}

impl PreparedApprovalJudgeBinding {
    fn matches(&self, authorization: &impl ApprovalJudgeAuthorization) -> bool {
        &self.request == authorization.request()
            && self.call == authorization.call()
            && self.selection == authorization.selection()
            && self.target == authorization.target()
            && self.credential_reference == authorization.credential_reference()
    }
}

impl fmt::Debug for PreparedApprovalJudgeModelCall {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PreparedApprovalJudgeModelCall")
    }
}

impl<Model> ApprovalJudgeModel for std::sync::Arc<Model>
where
    Model: ApprovalJudgeModel + ?Sized,
{
    fn prepare<'a>(
        &'a self,
        request: ApprovalJudgeModelRequest,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<PreparedApprovalJudgeModelCall, ApprovalJudgeModelError>>
                + Send
                + 'a,
        >,
    > {
        (**self).prepare(request)
    }
}

/// Layer-1 runtime adapter for dedicated approval-judge calls.
#[derive(Clone, Debug)]
pub struct RuntimeApprovalJudgeModel<R> {
    runtime: Arc<R>,
    models: RuntimeModelCatalog,
}

impl<R> RuntimeApprovalJudgeModel<R> {
    /// Supplies the runtime and immutable target mapping.
    pub fn new(runtime: R, models: RuntimeModelCatalog) -> Self {
        Self {
            runtime: Arc::new(runtime),
            models,
        }
    }
}

impl<R> ApprovalJudgeModel for RuntimeApprovalJudgeModel<R>
where
    R: ModelRuntime<ModelCallId> + fmt::Debug + Send + Sync + 'static,
    R::Prepared: Send + 'static,
{
    fn prepare<'a>(
        &'a self,
        request: ApprovalJudgeModelRequest,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<PreparedApprovalJudgeModelCall, ApprovalJudgeModelError>>
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
            let binding = PreparedApprovalJudgeBinding {
                request: request.request,
                call: request.call,
                selection: request.selection,
                target: request.target,
                credential_reference: request.credential_reference.clone(),
            };
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
            let preparation = self
                .runtime
                .prepare(operation, CancellationSignal::never())
                .await;
            require_preparation_correlation(&preparation, request.call)?;
            let prepared = match preparation {
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
            let runtime = Arc::clone(&self.runtime);
            let call = request.call;
            Ok(PreparedApprovalJudgeModelCall::new(binding, move || {
                Box::pin(execute_prepared(
                    runtime, prepared, resolved, contract, call,
                ))
            }))
        })
    }
}

async fn execute_prepared<R>(
    runtime: Arc<R>,
    prepared: R::Prepared,
    resolved: ResolvedTarget,
    contract: StructuredOutputContract,
    call: ModelCallId,
) -> Result<ApprovalJudgeModelResult, ApprovalJudgeModelError>
where
    R: ModelRuntime<ModelCallId> + Send,
    R::Prepared: Send,
{
    let mut observations: Vec<Observation<ModelCallId>> = Vec::new();
    let report = runtime
        .execute(prepared, &mut observations, CancellationSignal::never())
        .await;
    require_terminal_correlation(report.correlation, call)?;
    let usage = terminal_usage(&report.evidence);
    require_observation_correlations(&observations, call, usage)?;
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
        TerminalEvidence::CompletedWithProviderCompaction { completion, .. } => {
            completion.reported_model.as_ref()
        }
        TerminalEvidence::Refused(evidence) => evidence.reported_model.as_ref(),
        TerminalEvidence::ProviderError(evidence) => evidence.reported_model.as_ref(),
        TerminalEvidence::CancellationConfirmed(evidence) => evidence.reported_model.as_ref(),
        TerminalEvidence::BoundaryLoss(evidence) => evidence.reported_model.as_ref(),
        TerminalEvidence::ProvenUnsent(_) => None,
    }
    .cloned();
    if let Some(reported) = &reported_model {
        require_same_target(&resolved, reported, usage)?;
    }
    let completed = match report.evidence {
        TerminalEvidence::Completed(completed) => completed,
        TerminalEvidence::CompletedWithProviderCompaction { completion, .. } => {
            return Err(ApprovalJudgeModelError::InvalidDecision(completion.usage));
        }
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
                UnsentCause::CancelledBeforeSend => ApprovalJudgeModelError::CancelledBeforeSend,
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
    let decoded: Value = decode_structured(&completed.content, &contract, &NoDomainConstraints)
        .map_err(|_| ApprovalJudgeModelError::InvalidDecision(completed.usage))?;
    let (recommendation, rationale) = decode_decision(decoded)
        .map_err(|_| ApprovalJudgeModelError::InvalidDecision(completed.usage))?;
    Ok(ApprovalJudgeModelResult {
        call,
        recommendation,
        rationale,
        reported_model,
        usage: completed.usage,
    })
}

fn require_preparation_correlation<P>(
    preparation: &PreparationOutcome<ModelCallId, P>,
    expected: ModelCallId,
) -> Result<(), ApprovalJudgeModelError> {
    let correlation = match preparation {
        PreparationOutcome::Prepared(_) => return Ok(()),
        PreparationOutcome::Cancelled { correlation }
        | PreparationOutcome::Failed { correlation, .. }
        | PreparationOutcome::Defect { correlation, .. } => correlation,
    };
    (*correlation == expected)
        .then_some(())
        .ok_or(ApprovalJudgeModelError::PreparationCorrelationMismatch)
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
        TerminalEvidence::CompletedWithProviderCompaction { completion, .. } => completion.usage,
        TerminalEvidence::Refused(value) => value.usage,
        TerminalEvidence::ProviderError(value) => value.usage,
        TerminalEvidence::BoundaryLoss(value) => value.usage,
        TerminalEvidence::CancellationConfirmed(_) | TerminalEvidence::ProvenUnsent(_) => {
            TokenUsage::default()
        }
    }
}

fn require_terminal_correlation(
    observed: ModelCallId,
    expected: ModelCallId,
) -> Result<(), ApprovalJudgeModelError> {
    (observed == expected)
        .then_some(())
        .ok_or(ApprovalJudgeModelError::CorrelationMismatch(
            TokenUsage::default(),
        ))
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
    /// Durable authorization did not match the prepared one-shot request.
    AuthorizationMismatch,
    /// Runtime preparation returned another operation's correlation before send.
    PreparationCorrelationMismatch,
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
            | Self::AuthorizationMismatch
            | Self::PreparationCorrelationMismatch
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
        match self {
            Self::UnconfiguredTarget => {
                formatter.write_str("approval judge model target is not configured")
            }
            Self::InvalidContract => {
                formatter.write_str("approval judge structured-output contract is invalid")
            }
            Self::CancelledBeforeSend => {
                formatter.write_str("approval judge model call was cancelled before send")
            }
            Self::PreparationFailed => {
                formatter.write_str("approval judge model call preparation failed")
            }
            Self::PreparationDefect => {
                formatter.write_str("approval judge model call preparation was defective")
            }
            Self::AuthorizationMismatch => {
                formatter.write_str("approval judge model authorization did not match preparation")
            }
            Self::PreparationCorrelationMismatch => formatter
                .write_str("approval judge model call preparation returned another correlation"),
            Self::CorrelationMismatch(usage) => write!(
                formatter,
                "approval judge model call returned another correlation; usage={usage:?}"
            ),
            Self::Refused(usage) => write!(
                formatter,
                "approval judge model call was refused; usage={usage:?}"
            ),
            Self::ProviderError(usage) => write!(
                formatter,
                "approval judge model call returned a provider error; usage={usage:?}"
            ),
            Self::CancellationConfirmed => {
                formatter.write_str("approval judge model call cancellation was confirmed")
            }
            Self::ProvenUnsent => {
                formatter.write_str("approval judge model call was proven unsent")
            }
            Self::BoundaryLoss(usage) => write!(
                formatter,
                "approval judge model call lost its provider boundary; usage={usage:?}"
            ),
            Self::ProviderTargetSubstituted(usage) => write!(
                formatter,
                "approval judge model call reported another model lineage; usage={usage:?}"
            ),
            Self::IncompleteDecision(usage) => write!(
                formatter,
                "approval judge model call returned an incomplete decision; usage={usage:?}"
            ),
            Self::InvalidDecision(usage) => write!(
                formatter,
                "approval judge model call returned an invalid decision; usage={usage:?}"
            ),
        }
    }
}

impl Error for ApprovalJudgeModelError {}

#[cfg(test)]
mod tests {
    use signalbox_application::ApprovalJudgeAuthorization;
    use signalbox_domain::{
        DelegateApprovalRecommendation, DirectModelSelection, ModelCallId, NormalizedToolArguments,
        ProviderModelIdentity, ResolvedProviderTarget, SessionId, ToolRequest, ToolRequestId,
        ToolRequestOrdinal, ToolRequestReconstitutionInput, TurnId,
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
            request: ToolRequestReconstitutionInput::new(
                ToolRequestId::from_uuid(Uuid::from_u128(4)),
                SessionId::from_uuid(Uuid::from_u128(2)),
                TurnId::from_uuid(Uuid::from_u128(5)),
                ModelCallId::from_uuid(Uuid::from_u128(6)),
                ToolRequestOrdinal::from_u32(0),
                signalbox_domain::ToolName::try_new(String::from("echo"))
                    .expect("the fixture tool name is valid"),
                NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                    .expect("the fixture arguments are canonical"),
            )
            .into_request(),
            call: ModelCallId::from_uuid(Uuid::from_u128(1)),
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

    async fn execute(
        model: &impl ApprovalJudgeModel,
    ) -> Result<super::ApprovalJudgeModelResult, ApprovalJudgeModelError> {
        let request = request();
        let authorization = TestAuthorization::from_request(&request);
        model.prepare(request).await?.execute(authorization).await
    }

    struct TestAuthorization {
        request: ToolRequest,
        call: ModelCallId,
        selection: DirectModelSelection,
        target: ResolvedProviderTarget,
        credential_reference: String,
    }

    impl TestAuthorization {
        fn from_request(request: &ApprovalJudgeModelRequest) -> Self {
            Self {
                request: request.request.clone(),
                call: request.call,
                selection: request.selection,
                target: request.target,
                credential_reference: request.credential_reference.clone(),
            }
        }
    }

    impl ApprovalJudgeAuthorization for TestAuthorization {
        fn request(&self) -> &ToolRequest {
            &self.request
        }

        fn call(&self) -> ModelCallId {
            self.call
        }

        fn selection(&self) -> DirectModelSelection {
            self.selection
        }

        fn target(&self) -> ResolvedProviderTarget {
            self.target
        }

        fn credential_reference(&self) -> &str {
            &self.credential_reference
        }
    }

    #[tokio::test]
    async fn typed_approval_result_preserves_all_evidence() {
        let model = RuntimeApprovalJudgeModel::new(
            ScriptedModel::<ModelCallId>::single(approval_completion()),
            catalog(),
        );

        let result = execute(&model)
            .await
            .expect("the typed decision is admitted");

        assert_eq!(result.call, request().call);
        assert_eq!(
            result.recommendation,
            DelegateApprovalRecommendation::Approve
        );
        assert_eq!(result.rationale.as_str(), APPROVAL_RATIONALE);
        assert_eq!(
            result
                .reported_model
                .as_ref()
                .map(ProviderReportedModel::as_str),
            Some(PROVIDER_MODEL)
        );
        assert_eq!(result.usage.input_tokens, Some(INPUT_TOKENS));
        assert_eq!(result.usage.output_tokens, Some(OUTPUT_TOKENS));
        assert_eq!(
            result.usage.cache_read_input_tokens,
            Some(CACHE_READ_INPUT_TOKENS)
        );
    }

    #[tokio::test]
    async fn prepared_capability_rejects_another_authorized_judge_call() {
        let model = RuntimeApprovalJudgeModel::new(
            ScriptedModel::<ModelCallId>::single(approval_completion()),
            catalog(),
        );
        let request = request();
        let mut authorization = TestAuthorization::from_request(&request);
        authorization.call = ModelCallId::from_uuid(Uuid::from_u128(99));

        let error = model
            .prepare(request)
            .await
            .expect("the provider capability prepares locally")
            .execute(authorization)
            .await
            .expect_err("another call's authorization cannot enter the provider");

        assert_eq!(error, ApprovalJudgeModelError::AuthorizationMismatch);
    }

    #[tokio::test]
    async fn unknown_recommendation_fails_closed() {
        let model = RuntimeApprovalJudgeModel::new(
            ScriptedModel::<ModelCallId>::single(completion(
                r#"{"recommendation":"maybe","rationale":"Unsure."}"#,
            )),
            catalog(),
        );

        let error = execute(&model)
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

        let error = execute(&model)
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

    #[test]
    fn mismatched_terminal_correlation_does_not_attribute_usage() {
        let error = super::require_terminal_correlation(
            ModelCallId::from_uuid(Uuid::from_u128(99)),
            request().call,
        )
        .expect_err("an unrelated terminal report is rejected before its evidence is read");

        assert_eq!(error.usage(), TokenUsage::default());
    }

    #[test]
    fn mismatched_preparation_correlation_fails_closed_without_usage() {
        let preparation = signalbox_model_runtime::PreparationOutcome::<_, ()>::Cancelled {
            correlation: ModelCallId::from_uuid(Uuid::from_u128(99)),
        };

        let error = super::require_preparation_correlation(&preparation, request().call)
            .expect_err("an unrelated preparation outcome is rejected");

        assert_eq!(
            error,
            ApprovalJudgeModelError::PreparationCorrelationMismatch
        );
    }

    #[test]
    fn approval_judge_errors_display_distinct_failure_evidence() {
        assert_eq!(
            ApprovalJudgeModelError::PreparationCorrelationMismatch.to_string(),
            "approval judge model call preparation returned another correlation"
        );
        assert_eq!(
            ApprovalJudgeModelError::InvalidDecision(reported_usage()).to_string(),
            format!(
                "approval judge model call returned an invalid decision; usage={:?}",
                reported_usage()
            )
        );
    }

    #[test]
    fn output_contract_fingerprints_rationale_decoder_constraints() {
        let contract = super::approval_judge_output_contract_text();

        assert!(contract.contains(&format!(
            "max_utf8_bytes={}",
            signalbox_domain::ToolDecisionRationale::MAX_UTF8_BYTES
        )));
        assert!(contract.contains("exclude_u+0000"));
    }
}
