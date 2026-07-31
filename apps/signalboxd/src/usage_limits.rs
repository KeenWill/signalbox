//! Model-configuration token-limit enforcement around provider execution.

use std::future::Future;

use signalbox_application::{
    ClassifyOperatorFailure, ModelCallCapabilityPreparation, ModelCallProvider,
    OperatorFailureClass, PreparedModelOperation,
};
use signalbox_domain::{
    AuthorizedModelCall, CorrelatedModelCallTerminalObservation, ModelCallTerminalObservation,
    ProviderReportedTokenUsage, ResolvedProviderTarget,
};
use signalbox_model_provider_runtime::RuntimeModelCatalog;
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

/// Classifies dedicated-compaction usage against immutable model limits.
pub fn context_compaction_usage_exceeds_configured_limits(
    models: &RuntimeModelCatalog,
    target: ResolvedProviderTarget,
    usage: TokenUsage,
) -> Option<bool> {
    let definition = models.resolve(target)?;
    Some(runtime_usage_exceeds_configured_limits(
        usage,
        u64::from(definition.max_output_tokens()),
        u64::from(definition.context_window_tokens()),
    ))
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
    use signalbox_domain::ProviderReportedTokenUsage;

    use super::exceeds_configured_limits;

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
}
