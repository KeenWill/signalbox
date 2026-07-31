//! Model-configuration token-limit enforcement around provider execution.

use std::{fmt, future::Future, pin::Pin};

use signalbox_application::{
    ClassifyOperatorFailure, ModelCallCapabilityPreparation, ModelCallProvider,
    OperatorFailureClass, PreparedModelOperation,
};
use signalbox_domain::{
    AuthorizedModelCall, CorrelatedModelCallTerminalObservation, ModelCallTerminalObservation,
    ProviderReportedTokenUsage,
};
use signalbox_model_provider_runtime::{
    ContextCompactionModel, ContextCompactionModelError, ContextCompactionModelRequest,
    ContextCompactionModelResult, RuntimeModelCatalog,
};
use signalbox_model_runtime::TokenUsage;

/// One provider capability paired with configuration-owned limits.
pub struct UsageLimitedCapability<C> {
    inner: C,
    max_output_tokens: u64,
    context_window_tokens: u64,
}

/// Sanitized failure from the provider or a daemon composition defect.
#[derive(Debug)]
pub enum UsageLimitedProviderError<E> {
    /// The underlying provider bridge failed.
    Provider(E),
    /// The operation target had no model configuration.
    UnconfiguredTarget,
}

impl<E> ClassifyOperatorFailure for UsageLimitedProviderError<E>
where
    E: ClassifyOperatorFailure,
{
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Provider(error) => error.operator_failure_class(),
            Self::UnconfiguredTarget => OperatorFailureClass::CallerOrHubBug,
        }
    }
}

/// Provider wrapper enforcing only limits stated in immutable model config.
#[derive(Clone, Debug)]
pub struct UsageLimitedModelCallProvider<P> {
    inner: P,
    models: RuntimeModelCatalog,
}

impl<P> UsageLimitedModelCallProvider<P> {
    /// Binds the provider to the same immutable runtime model catalog.
    pub fn new(inner: P, models: RuntimeModelCatalog) -> Self {
        Self { inner, models }
    }
}

fn exceeds_configured_limits(
    usage: ProviderReportedTokenUsage,
    max_output_tokens: u64,
    context_window_tokens: u64,
) -> bool {
    if usage
        .output_tokens()
        .is_some_and(|output| output > max_output_tokens)
    {
        return true;
    }
    usage
        .input_tokens()
        .unwrap_or(0)
        .saturating_add(usage.output_tokens().unwrap_or(0))
        > context_window_tokens
}

fn runtime_usage_exceeds_configured_limits(
    usage: TokenUsage,
    max_output_tokens: u64,
    context_window_tokens: u64,
) -> bool {
    usage
        .output_tokens
        .is_some_and(|output| output > max_output_tokens)
        || usage
            .input_tokens
            .unwrap_or(0)
            .saturating_add(usage.output_tokens.unwrap_or(0))
            > context_window_tokens
}

/// Dedicated-compaction wrapper enforcing the same immutable model limits.
#[derive(Clone, Debug)]
pub struct UsageLimitedContextCompactionModel<M> {
    inner: M,
    models: RuntimeModelCatalog,
}

impl<M> UsageLimitedContextCompactionModel<M> {
    /// Binds dedicated compaction to the same immutable runtime catalog.
    pub fn new(inner: M, models: RuntimeModelCatalog) -> Self {
        Self { inner, models }
    }
}

impl<M> ContextCompactionModel for UsageLimitedContextCompactionModel<M>
where
    M: ContextCompactionModel + fmt::Debug + Send + Sync,
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
            let max_output_tokens = u64::from(definition.max_output_tokens());
            let context_window_tokens = u64::from(definition.context_window_tokens());
            let result = self.inner.execute(request).await?;
            if runtime_usage_exceeds_configured_limits(
                result.usage,
                max_output_tokens,
                context_window_tokens,
            ) {
                return Err(ContextCompactionModelError::ProviderError);
            }
            Ok(result)
        })
    }
}

impl<P> ModelCallProvider for UsageLimitedModelCallProvider<P>
where
    P: ModelCallProvider + Send,
    P::Capability: Send,
{
    type Capability = UsageLimitedCapability<P::Capability>;
    type Error = UsageLimitedProviderError<P::Error>;

    async fn prepare_capability<Cancellation>(
        &mut self,
        operation: PreparedModelOperation,
        cancellation: Cancellation,
    ) -> Result<ModelCallCapabilityPreparation<Self::Capability>, Self::Error>
    where
        Cancellation: Future<Output = ()> + Send + 'static,
    {
        let definition = self
            .models
            .resolve(operation.request().call().target())
            .ok_or(UsageLimitedProviderError::UnconfiguredTarget)?;
        let max_output_tokens = u64::from(definition.max_output_tokens());
        let context_window_tokens = u64::from(definition.context_window_tokens());
        self.inner
            .prepare_capability(operation, cancellation)
            .await
            .map_err(UsageLimitedProviderError::Provider)
            .map(|outcome| match outcome {
                ModelCallCapabilityPreparation::Ready(inner) => {
                    ModelCallCapabilityPreparation::Ready(UsageLimitedCapability {
                        inner,
                        max_output_tokens,
                        context_window_tokens,
                    })
                }
                ModelCallCapabilityPreparation::Cancelled => {
                    ModelCallCapabilityPreparation::Cancelled
                }
                ModelCallCapabilityPreparation::KnownFailure => {
                    ModelCallCapabilityPreparation::KnownFailure
                }
            })
    }

    async fn invoke<AcceptancePossible, Cancellation>(
        &mut self,
        authorized: AuthorizedModelCall,
        capability: Self::Capability,
        acceptance_possible: AcceptancePossible,
        cancellation: Cancellation,
    ) -> Result<CorrelatedModelCallTerminalObservation, Self::Error>
    where
        AcceptancePossible: FnOnce() + Send,
        Cancellation: Future<Output = ()> + Send + 'static,
    {
        let observation = self
            .inner
            .invoke(
                authorized,
                capability.inner,
                acceptance_possible,
                cancellation,
            )
            .await
            .map_err(UsageLimitedProviderError::Provider)?;
        let completed = matches!(
            observation.observation(),
            ModelCallTerminalObservation::Completed { .. }
                | ModelCallTerminalObservation::CompletedWithTools { .. }
        );
        if completed
            && exceeds_configured_limits(
                observation.usage(),
                capability.max_output_tokens,
                capability.context_window_tokens,
            )
        {
            return Ok(observation
                .correlation()
                .bind_terminal_observation_with_usage(
                    ModelCallTerminalObservation::KnownFailed,
                    observation.usage(),
                ));
        }
        Ok(observation)
    }
}

#[cfg(test)]
mod tests {
    use std::{future::Future, pin::Pin};

    use signalbox_domain::{
        DirectModelSelection, ModelCallId, ProviderModelIdentity, ProviderReportedTokenUsage,
        ResolvedProviderTarget, SessionId,
    };
    use signalbox_model_provider_runtime::{
        ContextCompactionModel, ContextCompactionModelError, ContextCompactionModelRequest,
        ContextCompactionModelResult,
    };
    use signalbox_model_runtime::TokenUsage;
    use uuid::Uuid;

    use crate::configuration::HubModelConfiguration;

    use super::{UsageLimitedContextCompactionModel, exceeds_configured_limits};

    #[derive(Clone, Debug)]
    struct FixedCompactionModel {
        result: ContextCompactionModelResult,
    }

    impl ContextCompactionModel for FixedCompactionModel {
        fn execute<'a>(
            &'a self,
            _request: ContextCompactionModelRequest,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = Result<ContextCompactionModelResult, ContextCompactionModelError>,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async move { Ok(self.result.clone()) })
        }
    }

    #[test]
    fn reported_output_usage_is_checked_against_the_model_output_limit() {
        let at_limit = ProviderReportedTokenUsage::unreported().with_output_tokens(Some(20));
        let output_exceeded = ProviderReportedTokenUsage::unreported().with_output_tokens(Some(21));

        assert!(!exceeds_configured_limits(at_limit, 20, 100));
        assert!(exceeds_configured_limits(output_exceeded, 20, 100));
    }

    #[test]
    fn reported_usage_is_checked_against_the_model_context_limit() {
        let at_limit = ProviderReportedTokenUsage::unreported()
            .with_input_tokens(Some(80))
            .with_output_tokens(Some(20));
        let context_exceeded = ProviderReportedTokenUsage::unreported()
            .with_input_tokens(Some(81))
            .with_output_tokens(Some(20));

        assert!(!exceeds_configured_limits(at_limit, 50, 100));
        assert!(exceeds_configured_limits(context_exceeded, 50, 100));
    }

    #[test]
    fn absent_usage_fields_do_not_invent_adapter_counts() {
        assert!(!exceeds_configured_limits(
            ProviderReportedTokenUsage::unreported(),
            20,
            100
        ));
    }

    #[tokio::test]
    async fn dedicated_compaction_rejects_adapter_usage_over_configured_limits() {
        let configuration = HubModelConfiguration::parse(
            r#"
version = 1

[[adapter_mappings]]
model_family = "anthropic"
adapter = "anthropic"
credential_profile = "anthropic-primary"

[compaction]
prompt = "Summarize."

[[models]]
selection_id = "10000000-0000-4000-8000-000000000001"
target_id = "20000000-0000-4000-8000-000000000001"
model_family = "anthropic"
provider_model = "claude-example"
max_output_tokens = 20
context_window_tokens = 100
"#,
        )
        .expect("fixture model configuration is valid");
        let model = UsageLimitedContextCompactionModel::new(
            FixedCompactionModel {
                result: ContextCompactionModelResult {
                    summary: String::from("summary"),
                    usage: TokenUsage {
                        input_tokens: Some(80),
                        output_tokens: Some(21),
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: None,
                    },
                },
            },
            configuration.runtime_model_catalog(),
        );
        let result = model
            .execute(ContextCompactionModelRequest {
                call: ModelCallId::from_uuid(Uuid::from_u128(1)),
                session: SessionId::from_uuid(Uuid::from_u128(2)),
                selection: DirectModelSelection::from_uuid(Uuid::from_u128(
                    0x10000000000040008000000000000001,
                )),
                target: ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
                    Uuid::from_u128(0x20000000000040008000000000000001),
                )),
                credential_reference: String::from("anthropic-primary"),
                system_prompt: String::from("Summarize."),
                rendered_range: String::from("history"),
            })
            .await;

        assert_eq!(result, Err(ContextCompactionModelError::ProviderError));
    }
}
