//! Bridge from the application-owned model-call port to a Layer-1 runtime.
//!
//! The layer boundary in docs/spec/runtime-substrate.md keeps runtime types
//! out of the domain and application crates. This crate is the outward
//! adapter: it translates one checked application operation, moves the
//! runtime's opaque one-shot capability across durable authorization, and
//! maps typed terminal evidence into the domain dispositions defined in
//! docs/spec/model-call-execution.md. It owns no retry, fallback, lifecycle,
//! or durable state.

use std::{collections::HashMap, error::Error, fmt, future::Future, sync::Arc};

use signalbox_application::{
    ClassifyOperatorFailure, ModelCallCapabilityPreparation, ModelCallProvider,
    ModelConversationMessage, OperatorFailureClass, PreparedModelOperation,
};
use signalbox_domain::{
    AssistantText, AuthorizedModelCall, ContextFrontierId, FrozenModelSelection, ModelCallId,
    ModelCallTerminalObservation, ResolvedProviderTarget, SessionId, TurnAttemptId, TurnId,
};
use signalbox_model_runtime::{
    AssistantPart, CancellationSignal, ConversationMessage, CredentialReference, ModelOperation,
    ModelRuntime, ModelSettings, Observation, ObservationFact, ObservationSink, PreparationOutcome,
    ProviderReportedModel, RequestedTarget, ResolvedTarget, TerminalEvidence, UnsentCause,
};

/// One exact provider-model spelling and baseline request limit for a durable
/// domain target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeModelDefinition {
    target: ResolvedProviderTarget,
    provider_model: String,
    max_output_tokens: u32,
}

impl RuntimeModelDefinition {
    /// Associates a durable target with one exact provider model spelling.
    pub fn try_new(
        target: ResolvedProviderTarget,
        provider_model: String,
        max_output_tokens: u32,
    ) -> Result<Self, RuntimeModelDefinitionError> {
        if provider_model.is_empty() || provider_model.trim() != provider_model {
            return Err(RuntimeModelDefinitionError::InvalidProviderModel);
        }
        if max_output_tokens == 0 {
            return Err(RuntimeModelDefinitionError::InvalidOutputLimit);
        }
        Ok(Self {
            target,
            provider_model,
            max_output_tokens,
        })
    }

    /// Returns the durable exact target represented by this mapping.
    pub const fn target(&self) -> ResolvedProviderTarget {
        self.target
    }

    /// Returns the exact provider-native model spelling.
    pub fn provider_model(&self) -> &str {
        &self.provider_model
    }

    /// Returns the required provider output-token ceiling.
    pub const fn max_output_tokens(&self) -> u32 {
        self.max_output_tokens
    }
}

/// A runtime delivery definition cannot construct a request-safe mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeModelDefinitionError {
    /// The provider model spelling was empty or padded.
    InvalidProviderModel,
    /// A provider request requires a positive output-token ceiling.
    InvalidOutputLimit,
}

impl fmt::Display for RuntimeModelDefinitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidProviderModel => "provider model spelling is empty or padded",
            Self::InvalidOutputLimit => "provider output-token limit is zero",
        })
    }
}

impl Error for RuntimeModelDefinitionError {}

/// Immutable runtime delivery mappings indexed by durable exact target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeModelCatalog {
    definitions: HashMap<ResolvedProviderTarget, RuntimeModelDefinition>,
}

impl RuntimeModelCatalog {
    /// Builds a catalog and rejects conflicting meanings for one target.
    pub fn try_from_definitions(
        definitions: impl IntoIterator<Item = RuntimeModelDefinition>,
    ) -> Result<Self, RuntimeModelCatalogError> {
        let mut by_target = HashMap::new();
        for definition in definitions {
            if let Some(existing) = by_target.get(&definition.target)
                && existing != &definition
            {
                return Err(RuntimeModelCatalogError::ConflictingTarget {
                    target: definition.target,
                });
            }
            by_target.insert(definition.target, definition);
        }
        Ok(Self {
            definitions: by_target,
        })
    }

    /// Looks up the exact runtime delivery mapping for a durable target.
    pub fn resolve(&self, target: ResolvedProviderTarget) -> Option<&RuntimeModelDefinition> {
        self.definitions.get(&target)
    }
}

/// Two deployment definitions assigned conflicting meanings to one target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeModelCatalogError {
    /// One target named distinct provider spellings or output limits.
    ConflictingTarget {
        /// The target whose immutable meaning conflicted.
        target: ResolvedProviderTarget,
    },
}

impl fmt::Display for RuntimeModelCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("runtime model catalog contains a conflicting target")
    }
}

impl Error for RuntimeModelCatalogError {}

#[derive(Clone, Copy)]
struct PreparedBinding {
    session: SessionId,
    turn: TurnId,
    attempt: TurnAttemptId,
    call: ModelCallId,
    selection: FrozenModelSelection,
    target: ResolvedProviderTarget,
    frontier: ContextFrontierId,
}

impl PreparedBinding {
    fn matches(&self, authorized: &AuthorizedModelCall) -> bool {
        self.session == authorized.session()
            && self.turn == authorized.turn()
            && self.attempt == authorized.attempt().id()
            && self.call == authorized.call().id()
            && self.selection == authorized.call().selection()
            && self.target == authorized.call().target()
            && self.frontier == authorized.call().frontier().snapshot()
    }
}

/// Opaque runtime capability plus the application facts it was prepared from.
pub struct RuntimeModelCallCapability<Prepared> {
    prepared: Prepared,
    binding: PreparedBinding,
    provider_model: String,
}

/// Sanitized adapter defect; provider response text and credentials are never
/// retained in this error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeModelCallProviderError {
    /// A durably resolved target had no matching runtime mapping.
    UnconfiguredTarget,
    /// Runtime preparation reported a local adapter defect.
    PreparationDefect,
    /// The runtime returned a different caller-owned correlation identity.
    CorrelationMismatch,
    /// Durable authorization did not match the prepared one-shot request.
    AuthorizationMismatch,
    /// A runtime observation did not carry the caller-owned call identity.
    ObservationCorrelationMismatch,
    /// A provider-reported model mismatch cannot yet form durable evidence.
    UnrepresentableProviderTargetMismatch,
    /// Definitive response material is outside the first text-only slice.
    UnsupportedCompletionMaterial,
    /// A runtime text part cannot construct exact domain assistant text.
    InvalidAssistantText,
}

impl fmt::Display for RuntimeModelCallProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnconfiguredTarget => "resolved model target has no runtime mapping",
            Self::PreparationDefect => "model runtime preparation reported a defect",
            Self::CorrelationMismatch => "model runtime returned a different correlation",
            Self::AuthorizationMismatch => {
                "authorized model call differs from the prepared capability"
            }
            Self::ObservationCorrelationMismatch => {
                "model runtime observation carried a different correlation"
            }
            Self::UnrepresentableProviderTargetMismatch => {
                "provider target mismatch cannot be represented durably"
            }
            Self::UnsupportedCompletionMaterial => {
                "provider completion contains unsupported assistant material"
            }
            Self::InvalidAssistantText => "provider completion contains invalid assistant text",
        })
    }
}

impl Error for RuntimeModelCallProviderError {}

impl ClassifyOperatorFailure for RuntimeModelCallProviderError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        OperatorFailureClass::CallerOrHubBug
    }
}

/// Application-port adapter over one provider-neutral model runtime.
pub struct RuntimeModelCallProvider<R> {
    runtime: Arc<R>,
    models: RuntimeModelCatalog,
}

struct AcceptanceObservations<AcceptancePossible, Correlation> {
    expected_correlation: Correlation,
    correlation_mismatch: bool,
    acceptance_possible: Option<AcceptancePossible>,
    observations: Vec<Observation<Correlation>>,
}

impl<AcceptancePossible, Correlation> ObservationSink<Correlation>
    for AcceptanceObservations<AcceptancePossible, Correlation>
where
    AcceptancePossible: FnOnce(),
    Correlation: PartialEq,
{
    fn observe(&mut self, observation: Observation<Correlation>) {
        if observation.correlation != self.expected_correlation {
            self.correlation_mismatch = true;
            self.observations.push(observation);
            return;
        }
        if matches!(&observation.fact, ObservationFact::SendCommenced)
            && let Some(acceptance_possible) = self.acceptance_possible.take()
        {
            acceptance_possible();
        }
        self.observations.push(observation);
    }
}

impl<R> RuntimeModelCallProvider<R> {
    /// Supplies the runtime and immutable target mapping.
    pub fn new(runtime: R, models: RuntimeModelCatalog) -> Self {
        Self {
            runtime: Arc::new(runtime),
            models,
        }
    }
}

impl<R> Clone for RuntimeModelCallProvider<R> {
    fn clone(&self) -> Self {
        Self {
            runtime: Arc::clone(&self.runtime),
            models: self.models.clone(),
        }
    }
}

impl<R> fmt::Debug for RuntimeModelCallProvider<R> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeModelCallProvider")
            .field("runtime", &"[provider runtime]")
            .field("models", &self.models)
            .finish()
    }
}

impl<R> ModelCallProvider for RuntimeModelCallProvider<R>
where
    R: ModelRuntime<ModelCallId> + Send + Sync,
{
    type Capability = RuntimeModelCallCapability<R::Prepared>;
    type Error = RuntimeModelCallProviderError;

    async fn prepare_capability<Cancellation>(
        &mut self,
        operation: PreparedModelOperation,
        cancellation: Cancellation,
    ) -> Result<ModelCallCapabilityPreparation<Self::Capability>, Self::Error>
    where
        Cancellation: Future<Output = ()> + Send + 'static,
    {
        let request = operation.request();
        let call = request.call();
        let credential =
            CredentialReference::new(operation.credential_reference().as_str().to_owned());
        let definition = self
            .models
            .resolve(call.target())
            .ok_or(RuntimeModelCallProviderError::UnconfiguredTarget)?;
        let correlation = call.id();
        let binding = PreparedBinding {
            session: request.session(),
            turn: request.turn(),
            attempt: request.attempt(),
            call: correlation,
            selection: call.selection(),
            target: call.target(),
            frontier: call.frontier().snapshot(),
        };
        let messages = operation
            .messages()
            .iter()
            .map(|message| match message {
                ModelConversationMessage::User { content, .. } => {
                    ConversationMessage::user_text(content.text().as_str())
                }
                ModelConversationMessage::Assistant { content, .. } => {
                    ConversationMessage::assistant_text(content.as_str())
                }
            })
            .collect();
        let runtime_operation = ModelOperation::new(
            correlation,
            credential,
            RequestedTarget::new(render_requested_target(call.selection())),
            ResolvedTarget::new(definition.provider_model().to_owned()),
            messages,
            ModelSettings::new(definition.max_output_tokens()),
        );
        let provider_model = definition.provider_model().to_owned();
        match self
            .runtime
            .prepare(runtime_operation, CancellationSignal::when(cancellation))
            .await
        {
            PreparationOutcome::Prepared(prepared) => Ok(ModelCallCapabilityPreparation::Ready(
                RuntimeModelCallCapability {
                    prepared,
                    binding,
                    provider_model,
                },
            )),
            PreparationOutcome::Cancelled {
                correlation: returned,
            } => {
                require_correlation(correlation, returned)?;
                Ok(ModelCallCapabilityPreparation::Cancelled)
            }
            PreparationOutcome::Failed {
                correlation: returned,
                ..
            } => {
                require_correlation(correlation, returned)?;
                Ok(ModelCallCapabilityPreparation::KnownFailure)
            }
            PreparationOutcome::Defect {
                correlation: returned,
                ..
            } => {
                require_correlation(correlation, returned)?;
                Err(RuntimeModelCallProviderError::PreparationDefect)
            }
        }
    }

    async fn invoke<AcceptancePossible, Cancellation>(
        &mut self,
        authorized: AuthorizedModelCall,
        capability: Self::Capability,
        acceptance_possible: AcceptancePossible,
        cancellation: Cancellation,
    ) -> Result<signalbox_domain::CorrelatedModelCallTerminalObservation, Self::Error>
    where
        AcceptancePossible: FnOnce() + Send,
        Cancellation: Future<Output = ()> + Send + 'static,
    {
        if !capability.binding.matches(&authorized) {
            return Err(RuntimeModelCallProviderError::AuthorizationMismatch);
        }
        let correlation = authorized.call().id();
        let mut observations = AcceptanceObservations {
            expected_correlation: correlation,
            correlation_mismatch: false,
            acceptance_possible: Some(acceptance_possible),
            observations: Vec::new(),
        };
        let report = self
            .runtime
            .execute(
                capability.prepared,
                &mut observations,
                CancellationSignal::when(cancellation),
            )
            .await;
        require_correlation(correlation, report.correlation)?;
        if observations.correlation_mismatch {
            return Err(RuntimeModelCallProviderError::ObservationCorrelationMismatch);
        }
        let observation = classify_terminal(
            report.evidence,
            &observations.observations,
            capability.provider_model.as_str(),
        )?;
        Ok(authorized
            .observation_correlation()
            .bind_terminal_observation(observation))
    }
}

fn require_correlation(
    expected: ModelCallId,
    returned: ModelCallId,
) -> Result<(), RuntimeModelCallProviderError> {
    if expected == returned {
        Ok(())
    } else {
        Err(RuntimeModelCallProviderError::CorrelationMismatch)
    }
}

fn render_requested_target(selection: FrozenModelSelection) -> String {
    match selection {
        FrozenModelSelection::Direct(direct) => format!("direct:{}", direct.into_uuid()),
        FrozenModelSelection::FrozenAlias { alias, definition } => format!(
            "alias:{}@direct:{}",
            alias.into_uuid(),
            definition.selected().into_uuid()
        ),
    }
}

fn classify_terminal(
    evidence: TerminalEvidence,
    observations: &[Observation<ModelCallId>],
    expected_provider_model: &str,
) -> Result<ModelCallTerminalObservation, RuntimeModelCallProviderError> {
    if observations.iter().any(|observation| {
        matches!(
            &observation.fact,
            ObservationFact::ProviderModelReported(reported)
                if reported.as_str() != expected_provider_model
        )
    }) || reported_model(&evidence)
        .is_some_and(|reported| reported.as_str() != expected_provider_model)
    {
        // docs/spec/model-call-execution.md forbids collapsing mismatch
        // evidence into an ordinary provider failure. Provider identity
        // normalization and its durable provenance schema remain an
        // owner-gated open question, so fail the adapter stage closed
        // instead of committing the wrong lifecycle.
        return Err(RuntimeModelCallProviderError::UnrepresentableProviderTargetMismatch);
    }

    match evidence {
        TerminalEvidence::Completed(completion) => {
            let mut assistant_text = Vec::new();
            for part in completion.content {
                match part {
                    AssistantPart::Text(text) if text.is_empty() => {}
                    AssistantPart::Text(text) => assistant_text.push(
                        AssistantText::try_new(text)
                            .map_err(|_| RuntimeModelCallProviderError::InvalidAssistantText)?,
                    ),
                    AssistantPart::Thinking { .. }
                    | AssistantPart::RedactedThinking { .. }
                    | AssistantPart::ToolCall(_) => {
                        return Err(RuntimeModelCallProviderError::UnsupportedCompletionMaterial);
                    }
                }
            }
            Ok(ModelCallTerminalObservation::Completed { assistant_text })
        }
        TerminalEvidence::Refused(_) => Ok(ModelCallTerminalObservation::Refused),
        TerminalEvidence::ProviderError(_)
        | TerminalEvidence::ProvenUnsent(signalbox_model_runtime::ProvenUnsentEvidence {
            cause: UnsentCause::ConnectFailed(_) | UnsentCause::SendIncompleteProvenUnacceptable(_),
        }) => Ok(ModelCallTerminalObservation::KnownFailed),
        TerminalEvidence::ProvenUnsent(signalbox_model_runtime::ProvenUnsentEvidence {
            cause: UnsentCause::CancelledBeforeSend,
        }) => Ok(ModelCallTerminalObservation::Cancelled),
        TerminalEvidence::CancellationConfirmed(_) => Ok(ModelCallTerminalObservation::Cancelled),
        TerminalEvidence::BoundaryLoss(_) => Ok(ModelCallTerminalObservation::Ambiguous),
    }
}

fn reported_model(evidence: &TerminalEvidence) -> Option<&ProviderReportedModel> {
    match evidence {
        TerminalEvidence::Completed(value) => value.reported_model.as_ref(),
        TerminalEvidence::Refused(value) => value.reported_model.as_ref(),
        TerminalEvidence::ProviderError(value) => value.reported_model.as_ref(),
        TerminalEvidence::CancellationConfirmed(value) => value.reported_model.as_ref(),
        TerminalEvidence::BoundaryLoss(value) => value.reported_model.as_ref(),
        TerminalEvidence::ProvenUnsent(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use signalbox_domain::{ModelCallId, ModelCallTerminalObservation, ProviderModelIdentity};
    use signalbox_model_runtime::{
        AssistantPart, BoundaryLossEvidence, CancellationConfirmedEvidence, CompletionEvidence,
        CompletionFinish, ExchangeFacts, LossCause, NativeErrorFacts, Observation, ObservationFact,
        ObservationSink, ProvenUnsentEvidence, ProviderErrorEvidence, ProviderErrorKind,
        ProviderReportedModel, RefusalEvidence, TerminalEvidence, TokenUsage, TransportFacts,
        UnsentCause,
    };
    use uuid::Uuid;

    use super::{
        AcceptanceObservations, RuntimeModelCatalog, RuntimeModelCatalogError,
        RuntimeModelDefinition, classify_terminal,
    };
    use signalbox_domain::ResolvedProviderTarget;

    fn call() -> ModelCallId {
        ModelCallId::from_uuid(Uuid::from_u128(1))
    }

    fn target(value: u128) -> ResolvedProviderTarget {
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(value)))
    }

    fn completion(model: &str, content: Vec<AssistantPart>) -> TerminalEvidence {
        TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new(model)),
            finish: CompletionFinish::EndTurn,
            content,
            usage: TokenUsage::unreported(),
        })
    }

    /// INV-026: the application-owned dispatch permit is released exactly
    /// when the runtime first reports that provider acceptance is possible.
    #[test]
    fn inv026_send_commenced_releases_acceptance_callback_once() {
        let release_count = Arc::new(AtomicUsize::new(0));
        let callback_count = Arc::clone(&release_count);
        let mut sink = AcceptanceObservations {
            expected_correlation: call(),
            correlation_mismatch: false,
            acceptance_possible: Some(move || {
                callback_count.fetch_add(1, Ordering::SeqCst);
            }),
            observations: Vec::new(),
        };

        sink.observe(Observation {
            correlation: call(),
            fact: ObservationFact::ProviderModelReported(ProviderReportedModel::new("model-exact")),
        });
        assert_eq!(release_count.load(Ordering::SeqCst), 0);

        sink.observe(Observation {
            correlation: call(),
            fact: ObservationFact::SendCommenced,
        });
        sink.observe(Observation {
            correlation: call(),
            fact: ObservationFact::SendCommenced,
        });

        assert_eq!(release_count.load(Ordering::SeqCst), 1);
        assert_eq!(sink.observations.len(), 3);
    }

    /// INV-026: cross-wired acceptance evidence cannot release another
    /// attempt's dispatch/stop gate.
    #[test]
    fn inv026_cross_wired_send_commenced_retains_acceptance_callback() {
        let release_count = Arc::new(AtomicUsize::new(0));
        let callback_count = Arc::clone(&release_count);
        let mut sink = AcceptanceObservations {
            expected_correlation: call(),
            correlation_mismatch: false,
            acceptance_possible: Some(move || {
                callback_count.fetch_add(1, Ordering::SeqCst);
            }),
            observations: Vec::new(),
        };

        sink.observe(Observation {
            correlation: ModelCallId::from_uuid(Uuid::from_u128(2)),
            fact: ObservationFact::SendCommenced,
        });

        assert_eq!(release_count.load(Ordering::SeqCst), 0);
        assert!(sink.correlation_mismatch);
        assert!(sink.acceptance_possible.is_some());
    }

    /// S02 / INV-014 / INV-025: runtime terminal evidence maps to the exact
    /// physical disposition without retryability or error-string inference.
    #[test]
    fn s02_inv014_inv025_terminal_evidence_classification_is_total() {
        let exchange = ExchangeFacts::default();
        assert_eq!(
            classify_terminal(
                TerminalEvidence::Refused(RefusalEvidence {
                    exchange: exchange.clone(),
                    message_id: None,
                    reported_model: None,
                    content: Vec::new(),
                    usage: TokenUsage::unreported(),
                }),
                &[],
                "model-exact",
            )
            .expect("typed refusal evidence is supported"),
            ModelCallTerminalObservation::Refused
        );
        assert_eq!(
            classify_terminal(
                TerminalEvidence::ProviderError(ProviderErrorEvidence {
                    exchange: exchange.clone(),
                    reported_model: None,
                    kind: ProviderErrorKind::RateLimited,
                    native: NativeErrorFacts::default(),
                    usage: TokenUsage::unreported(),
                }),
                &[],
                "model-exact",
            )
            .expect("typed provider-error evidence is supported"),
            ModelCallTerminalObservation::KnownFailed
        );
        assert_eq!(
            classify_terminal(
                TerminalEvidence::CancellationConfirmed(CancellationConfirmedEvidence {
                    exchange: exchange.clone(),
                    reported_model: None,
                    native: NativeErrorFacts::default(),
                }),
                &[],
                "model-exact",
            )
            .expect("typed cancellation evidence is supported"),
            ModelCallTerminalObservation::Cancelled
        );
        assert_eq!(
            classify_terminal(
                TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
                    cause: UnsentCause::ConnectFailed(TransportFacts {
                        detail: String::from("safe typed fixture"),
                    }),
                }),
                &[],
                "model-exact",
            )
            .expect("typed non-acceptance evidence is supported"),
            ModelCallTerminalObservation::KnownFailed
        );
        assert_eq!(
            classify_terminal(
                TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
                    cause: UnsentCause::CancelledBeforeSend,
                }),
                &[],
                "model-exact",
            )
            .expect("pre-send cancellation evidence is supported"),
            ModelCallTerminalObservation::Cancelled
        );
        assert_eq!(
            classify_terminal(
                TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                    cause: LossCause::TransportFailed(TransportFacts {
                        detail: String::from("safe typed fixture"),
                    }),
                    exchange,
                    reported_model: None,
                    finish_reported: None,
                    usage: TokenUsage::unreported(),
                }),
                &[],
                "model-exact",
            )
            .expect("typed boundary-loss evidence is supported"),
            ModelCallTerminalObservation::Ambiguous
        );
    }

    /// S02 / INV-014: only exact text from a matching reported target becomes
    /// assistant content; empty blocks create no invalid empty entry.
    #[test]
    fn s02_inv014_matching_completion_preserves_text_parts() {
        assert_eq!(
            classify_terminal(
                completion(
                    "model-exact",
                    vec![
                        AssistantPart::Text(String::from("first")),
                        AssistantPart::Text(String::new()),
                        AssistantPart::Text(String::from("second")),
                    ],
                ),
                &[],
                "model-exact",
            )
            .expect("text-only completion is supported"),
            ModelCallTerminalObservation::Completed {
                assistant_text: vec![
                    signalbox_domain::AssistantText::try_new(String::from("first"))
                        .expect("fixture text is admitted"),
                    signalbox_domain::AssistantText::try_new(String::from("second"))
                        .expect("fixture text is admitted"),
                ],
            }
        );
    }

    /// INV-014: either early or terminal provider-model mismatch prevents
    /// response material from becoming authoritative.
    #[test]
    fn inv014_reported_target_mismatch_precedes_completion() {
        let early = vec![Observation {
            correlation: call(),
            fact: ObservationFact::ProviderModelReported(ProviderReportedModel::new("other")),
        }];
        assert_eq!(
            classify_terminal(
                completion(
                    "model-exact",
                    vec![AssistantPart::Text(String::from("hidden"))]
                ),
                &early,
                "model-exact",
            )
            .expect_err("unrepresentable mismatch must fail closed"),
            super::RuntimeModelCallProviderError::UnrepresentableProviderTargetMismatch
        );
        assert_eq!(
            classify_terminal(
                completion("other", vec![AssistantPart::Text(String::from("hidden"))]),
                &[],
                "model-exact",
            )
            .expect_err("unrepresentable mismatch must fail closed"),
            super::RuntimeModelCallProviderError::UnrepresentableProviderTargetMismatch
        );
    }

    #[test]
    fn conflicting_runtime_target_meaning_is_rejected() {
        assert_eq!(
            RuntimeModelCatalog::try_from_definitions([
                RuntimeModelDefinition::try_new(target(1), String::from("first"), 64)
                    .expect("fixture definition is valid"),
                RuntimeModelDefinition::try_new(target(1), String::from("second"), 64)
                    .expect("fixture definition is valid"),
            ]),
            Err(RuntimeModelCatalogError::ConflictingTarget { target: target(1) })
        );
    }
}
