//! Dedicated model execution for append-only context summaries.

use std::{error::Error, fmt, future::Future, pin::Pin};

use signalbox_domain::{DirectModelSelection, ModelCallId, ResolvedProviderTarget, SessionId};
use signalbox_model_runtime::{
    AssistantPart, CancellationSignal, CompletionFinish, ConversationMessage, CredentialReference,
    DeliveryMode, ModelOperation, ModelRuntime, ModelSettings, Observation, PreparationOutcome,
    ProviderReportedModel, RequestedTarget, ResolvedTarget, TerminalEvidence, TokenUsage,
};

use crate::{ProviderTargetRelation, RuntimeModelCatalog, relate_provider_target};

/// Exact inputs to one dedicated summary-producing model call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextCompactionModelRequest {
    /// Durable physical call identity.
    pub call: ModelCallId,
    /// Session whose complete frontier supplied the range.
    pub session: SessionId,
    /// Current direct model selection frozen for this call.
    pub selection: DirectModelSelection,
    /// Exact resolved provider target.
    pub target: ResolvedProviderTarget,
    /// Non-secret credential reference pinned in durable call evidence.
    pub credential_reference: String,
    /// Deployment-configured compaction system prompt.
    pub system_prompt: String,
    /// Deterministic rendering of the exact summarized range.
    pub rendered_range: String,
}

/// Exact successful result of one dedicated summary call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextCompactionModelResult {
    /// Nonempty plain-text summary content.
    pub summary: String,
    /// Provider-reported usage, independently optional by field.
    pub usage: TokenUsage,
}

/// One provider adapter capable of executing a dedicated compaction call.
pub trait ContextCompactionModel: fmt::Debug + Send + Sync {
    /// Executes the already-durably-authorized request exactly once.
    fn execute<'a>(
        &'a self,
        request: ContextCompactionModelRequest,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ContextCompactionModelResult, ContextCompactionModelError>>
                + Send
                + 'a,
        >,
    >;
}

impl<Model> ContextCompactionModel for std::sync::Arc<Model>
where
    Model: ContextCompactionModel + ?Sized,
{
    fn execute<'a>(
        &'a self,
        request: ContextCompactionModelRequest,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ContextCompactionModelResult, ContextCompactionModelError>>
                + Send
                + 'a,
        >,
    > {
        (**self).execute(request)
    }
}

/// Layer-1 runtime adapter for dedicated context-summary calls.
#[derive(Clone, Debug)]
pub struct RuntimeContextCompactionModel<R> {
    runtime: R,
    models: RuntimeModelCatalog,
}

impl<R> RuntimeContextCompactionModel<R> {
    /// Supplies the runtime and immutable target mapping.
    pub const fn new(runtime: R, models: RuntimeModelCatalog) -> Self {
        Self { runtime, models }
    }
}

impl<R> ContextCompactionModel for RuntimeContextCompactionModel<R>
where
    R: ModelRuntime<ModelCallId> + fmt::Debug + Send + Sync,
    R::Prepared: Send,
{
    fn execute<'a>(
        &'a self,
        request: ContextCompactionModelRequest,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ContextCompactionModelResult, ContextCompactionModelError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let definition = self
                .models
                .resolve(request.target)
                .ok_or(ContextCompactionModelError::UnconfiguredTarget)?;
            let resolved = ResolvedTarget::new(definition.provider_model().to_owned());
            let mut operation = ModelOperation::new(
                request.call,
                CredentialReference::new(request.credential_reference),
                RequestedTarget::new(render_selection(request.selection)),
                resolved.clone(),
                vec![ConversationMessage::user_text(request.rendered_range)],
                ModelSettings::new(definition.max_output_tokens()),
            );
            operation.system = Some(request.system_prompt);
            operation.delivery = DeliveryMode::Buffered;
            let prepared = match self
                .runtime
                .prepare(operation, CancellationSignal::never())
                .await
            {
                PreparationOutcome::Prepared(prepared) => prepared,
                PreparationOutcome::Cancelled { .. } => {
                    return Err(ContextCompactionModelError::CancelledBeforeSend);
                }
                PreparationOutcome::Failed { .. } => {
                    return Err(ContextCompactionModelError::PreparationFailed);
                }
                PreparationOutcome::Defect { .. } => {
                    return Err(ContextCompactionModelError::PreparationDefect);
                }
            };
            let mut observations: Vec<Observation<ModelCallId>> = Vec::new();
            let report = self
                .runtime
                .execute(prepared, &mut observations, CancellationSignal::never())
                .await;
            if report.correlation != request.call {
                return Err(ContextCompactionModelError::CorrelationMismatch);
            }
            for reported in observations.iter().filter_map(|observation| {
                let signalbox_model_runtime::ObservationFact::ProviderModelReported(reported) =
                    &observation.fact
                else {
                    return None;
                };
                Some(reported)
            }) {
                require_same_target(&resolved, reported)?;
            }
            let TerminalEvidence::Completed(completed) = report.evidence else {
                return Err(ContextCompactionModelError::NotCompleted);
            };
            if let Some(reported) = completed.reported_model.as_ref() {
                require_same_target(&resolved, reported)?;
            }
            if !matches!(
                completed.finish,
                CompletionFinish::EndTurn | CompletionFinish::StopSequence { .. }
            ) {
                return Err(ContextCompactionModelError::IncompleteSummary);
            }
            let mut summary = String::new();
            for part in completed.content {
                let AssistantPart::Text(text) = part else {
                    return Err(ContextCompactionModelError::NonTextSummary);
                };
                summary.push_str(&text);
            }
            if summary.is_empty() || summary.contains('\0') {
                return Err(ContextCompactionModelError::InvalidSummary);
            }
            Ok(ContextCompactionModelResult {
                summary,
                usage: completed.usage,
            })
        })
    }
}

fn render_selection(selection: DirectModelSelection) -> String {
    format!("direct:{}", selection.into_uuid())
}

fn require_same_target(
    configured: &ResolvedTarget,
    reported: &ProviderReportedModel,
) -> Result<(), ContextCompactionModelError> {
    match relate_provider_target(configured, reported) {
        ProviderTargetRelation::Exact | ProviderTargetRelation::AliasConcretion => Ok(()),
        ProviderTargetRelation::DifferentLineage => {
            Err(ContextCompactionModelError::ProviderTargetSubstituted)
        }
    }
}

/// Sanitized failure of one dedicated summary call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextCompactionModelError {
    /// The durable target has no runtime mapping.
    UnconfiguredTarget,
    /// Runtime preparation observed cancellation before send.
    CancelledBeforeSend,
    /// Credential or request preparation failed safely.
    PreparationFailed,
    /// Adapter request construction was defective.
    PreparationDefect,
    /// Runtime correlation differed from the durable call.
    CorrelationMismatch,
    /// The provider did not return completed evidence.
    NotCompleted,
    /// The provider reported a different model lineage.
    ProviderTargetSubstituted,
    /// The completion stopped before a complete summary.
    IncompleteSummary,
    /// Completion material was not plain text.
    NonTextSummary,
    /// Summary text was empty or contained U+0000.
    InvalidSummary,
}

impl fmt::Display for ContextCompactionModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("context compaction model execution failed")
    }
}

impl Error for ContextCompactionModelError {}
