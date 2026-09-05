//! Dedicated model execution for append-only context summaries.

use std::{error::Error, fmt, future::Future, pin::Pin};

use signalbox_domain::{DirectModelSelection, ModelCallId, ResolvedProviderTarget, SessionId};
use signalbox_model_runtime::{
    AssistantPart, CancellationSignal, CompletionFinish, ConversationMessage, CredentialReference,
    DeliveryMode, ModelOperation, ModelRuntime, ModelSettings, Observation, PreparationOutcome,
    ProviderCompactionMode, ProviderReportedModel, RequestedTarget, ResolvedTarget,
    TerminalEvidence, TokenUsage, UnsentCause,
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
            operation.provider_compaction = ProviderCompactionMode::Suppressed;
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
                require_same_target(&resolved, reported)?;
            }
            let completed = match report.evidence {
                TerminalEvidence::Completed(completed) => completed,
                TerminalEvidence::Refused(_) => {
                    return Err(ContextCompactionModelError::Refused);
                }
                TerminalEvidence::ProviderError(_) => {
                    return Err(ContextCompactionModelError::ProviderError);
                }
                TerminalEvidence::CancellationConfirmed(_) => {
                    return Err(ContextCompactionModelError::CancellationConfirmed);
                }
                TerminalEvidence::ProvenUnsent(evidence) => {
                    return Err(match evidence.cause {
                        UnsentCause::CancelledBeforeSend => {
                            ContextCompactionModelError::CancelledBeforeSend
                        }
                        UnsentCause::ConnectFailed(_)
                        | UnsentCause::SendIncompleteProvenUnacceptable(_) => {
                            ContextCompactionModelError::ProvenUnsent
                        }
                    });
                }
                TerminalEvidence::BoundaryLoss(_) => {
                    return Err(ContextCompactionModelError::BoundaryLoss);
                }
            };
            if !matches!(
                completed.finish,
                CompletionFinish::EndTurn | CompletionFinish::StopSequence { .. }
            ) {
                return Err(ContextCompactionModelError::IncompleteSummary);
            }
            let mut summary = String::new();
            for part in completed.content {
                match part {
                    AssistantPart::Text(text) => summary.push_str(&text),
                    // The dedicated compaction request configures no thinking
                    // display, so a Claude 5-family completion carries the
                    // omitted-display empty thinking block by default: it holds
                    // only a provider replay signature and no summary material.
                    // It is dropped exactly as this crate's ordinary bridge
                    // drops it, since rejecting it would fail compaction on the
                    // default path. Thinking with actual text, redacted
                    // thinking, and tool calls still fail closed, because a
                    // summary must never silently omit returned material.
                    AssistantPart::Thinking { text, .. } if text.is_empty() => {}
                    AssistantPart::Thinking { .. }
                    | AssistantPart::RedactedThinking { .. }
                    | AssistantPart::ProviderCompaction { .. }
                    | AssistantPart::ToolCall(_)
                    | AssistantPart::SuppressedToolCall(_) => {
                        return Err(ContextCompactionModelError::NonTextSummary);
                    }
                }
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
    /// The provider returned an explicit refusal.
    Refused,
    /// A complete, correlated provider error response was observed.
    ProviderError,
    /// The provider definitively confirmed cancellation.
    CancellationConfirmed,
    /// The request provably never reached an acceptance-capable boundary.
    ProvenUnsent,
    /// Provider acceptance or completion remained uncertain.
    BoundaryLoss,
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

#[cfg(test)]
mod tests {
    use signalbox_domain::{
        DirectModelSelection, ModelCallId, ProviderModelIdentity, ResolvedProviderTarget, SessionId,
    };
    use signalbox_model_runtime::{
        AssistantPart, CompletionEvidence, CompletionFinish, ExchangeFacts, ProviderReportedModel,
        Script, ScriptedModel, TerminalEvidence, TokenUsage,
    };
    use uuid::Uuid;

    use super::{
        ContextCompactionModel, ContextCompactionModelError, ContextCompactionModelRequest,
        RuntimeContextCompactionModel,
    };
    use crate::{RuntimeModelCatalog, RuntimeModelDefinition};

    /// The exact provider-model spelling this fixture deployment configures.
    const PROVIDER_MODEL: &str = "fixture-compaction-model";

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

    fn request() -> ContextCompactionModelRequest {
        ContextCompactionModelRequest {
            call: ModelCallId::from_uuid(Uuid::from_u128(1)),
            session: SessionId::from_uuid(Uuid::from_u128(2)),
            selection: DirectModelSelection::from_uuid(Uuid::from_u128(3)),
            target: target(),
            credential_reference: String::from("fixture-compaction-credential"),
            system_prompt: String::from("Summarize the prior conversation faithfully."),
            rendered_range: String::from("[\"fixture summarized range\"]"),
        }
    }

    fn completion(content: Vec<AssistantPart>) -> Script {
        Script::delivering(TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new(PROVIDER_MODEL)),
            finish: CompletionFinish::EndTurn,
            content,
            usage: TokenUsage::unreported(),
        }))
    }

    fn compaction_model(
        content: Vec<AssistantPart>,
    ) -> RuntimeContextCompactionModel<ScriptedModel<ModelCallId>> {
        RuntimeContextCompactionModel::new(
            ScriptedModel::<ModelCallId>::single(completion(content)),
            catalog(),
        )
    }

    /// S02 / INV-014: the dedicated compaction call configures no thinking
    /// display, so a Claude 5-family completion carries the omitted-display
    /// empty thinking block by default. Folding the summary drops it exactly as
    /// the ordinary bridge does, instead of failing the default path closed and
    /// stalling the very turn automatic compaction exists to rescue.
    #[tokio::test]
    async fn s02_inv014_empty_thinking_part_is_dropped_from_a_compaction_summary() {
        let model = compaction_model(vec![
            AssistantPart::Thinking {
                text: String::new(),
                signature: Some(String::from("sig_synthetic_1")),
            },
            AssistantPart::Text(String::from("compacted fixture summary")),
        ]);

        let result = model
            .execute(request())
            .await
            .expect("an empty thinking part must not fail the summary closed");

        assert_eq!(result.summary, "compacted fixture summary");
    }

    /// S02 / INV-014: thinking with actual text still fails the summary closed,
    /// because accepting it would publish a summary that silently omits
    /// response material no durable representation can carry.
    #[tokio::test]
    async fn s02_inv014_nonempty_thinking_part_still_fails_the_summary_closed() {
        let model = compaction_model(vec![
            AssistantPart::Thinking {
                text: String::from("visible reasoning"),
                signature: Some(String::from("sig_synthetic_1")),
            },
            AssistantPart::Text(String::from("compacted fixture summary")),
        ]);

        assert_eq!(
            model.execute(request()).await,
            Err(ContextCompactionModelError::NonTextSummary)
        );
    }

    /// S02 / INV-014: redacted thinking carries withheld reasoning in opaque
    /// form and fails the summary closed for the same reason.
    #[tokio::test]
    async fn s02_inv014_redacted_thinking_part_still_fails_the_summary_closed() {
        let model = compaction_model(vec![
            AssistantPart::RedactedThinking {
                data: String::from("opaque-fixture-payload"),
            },
            AssistantPart::Text(String::from("compacted fixture summary")),
        ]);

        assert_eq!(
            model.execute(request()).await,
            Err(ContextCompactionModelError::NonTextSummary)
        );
    }
}
