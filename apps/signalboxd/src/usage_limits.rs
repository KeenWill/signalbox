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
    limits: ConfiguredUsageLimits,
}

#[derive(Clone, Copy)]
struct ConfiguredUsageLimits {
    max_output_tokens: u64,
    context_window_tokens: u64,
}

#[derive(Clone, Copy)]
struct ReportedUsageLowerBound {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

impl From<ProviderReportedTokenUsage> for ReportedUsageLowerBound {
    fn from(usage: ProviderReportedTokenUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens(),
            output_tokens: usage.output_tokens(),
        }
    }
}

impl From<TokenUsage> for ReportedUsageLowerBound {
    fn from(usage: TokenUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
        }
    }
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

    fn operator_failure_cause_code(&self) -> &'static str {
        match self {
            Self::Provider(error) => error.operator_failure_cause_code(),
            Self::UnconfiguredTarget => "usage_limit_unconfigured_target",
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
    usage: impl Into<ReportedUsageLowerBound>,
    limits: ConfiguredUsageLimits,
) -> bool {
    let usage = usage.into();
    if usage
        .output_tokens
        .is_some_and(|output| output > limits.max_output_tokens)
    {
        return true;
    }
    usage
        .input_tokens
        .unwrap_or(0)
        .saturating_add(usage.output_tokens.unwrap_or(0))
        > limits.context_window_tokens
}

/// Classifies dedicated-compaction usage against immutable model limits.
pub fn context_compaction_usage_exceeds_configured_limits(
    models: &RuntimeModelCatalog,
    target: ResolvedProviderTarget,
    usage: TokenUsage,
) -> Option<bool> {
    let definition = models.resolve(target)?;
    Some(exceeds_configured_limits(
        usage,
        ConfiguredUsageLimits {
            max_output_tokens: u64::from(definition.max_output_tokens()),
            context_window_tokens: u64::from(definition.context_window_tokens()),
        },
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
        let limits = ConfiguredUsageLimits {
            max_output_tokens: u64::from(definition.max_output_tokens()),
            context_window_tokens: u64::from(definition.context_window_tokens()),
        };
        self.inner
            .prepare_capability(operation, cancellation)
            .await
            .map_err(UsageLimitedProviderError::Provider)
            .map(|outcome| match outcome {
                ModelCallCapabilityPreparation::Ready(inner) => {
                    ModelCallCapabilityPreparation::Ready(UsageLimitedCapability { inner, limits })
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
        if completed && exceeds_configured_limits(observation.usage(), capability.limits) {
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
    use signalbox_application::{ClassifyOperatorFailure, OperatorFailureClass};
    use signalbox_domain::ProviderReportedTokenUsage;

    use super::{ConfiguredUsageLimits, UsageLimitedProviderError, exceeds_configured_limits};

    #[derive(Debug)]
    struct FixtureProviderError;

    impl ClassifyOperatorFailure for FixtureProviderError {
        fn operator_failure_class(&self) -> OperatorFailureClass {
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            }
        }

        fn operator_failure_cause_code(&self) -> &'static str {
            "fixture_provider"
        }
    }

    #[test]
    fn reported_output_usage_is_checked_against_the_model_output_limit() {
        let at_limit = ProviderReportedTokenUsage::unreported().with_output_tokens(Some(20));
        let output_exceeded = ProviderReportedTokenUsage::unreported().with_output_tokens(Some(21));
        let limits = ConfiguredUsageLimits {
            max_output_tokens: 20,
            context_window_tokens: 100,
        };

        assert!(!exceeds_configured_limits(at_limit, limits));
        assert!(exceeds_configured_limits(output_exceeded, limits));
    }

    #[test]
    fn reported_usage_is_checked_against_the_model_context_limit() {
        let at_limit = ProviderReportedTokenUsage::unreported()
            .with_input_tokens(Some(80))
            .with_output_tokens(Some(20));
        let context_exceeded = ProviderReportedTokenUsage::unreported()
            .with_input_tokens(Some(81))
            .with_output_tokens(Some(20));
        let limits = ConfiguredUsageLimits {
            max_output_tokens: 50,
            context_window_tokens: 100,
        };

        assert!(!exceeds_configured_limits(at_limit, limits));
        assert!(exceeds_configured_limits(context_exceeded, limits));
    }

    #[test]
    fn absent_usage_fields_do_not_invent_adapter_counts() {
        let limits = ConfiguredUsageLimits {
            max_output_tokens: 20,
            context_window_tokens: 100,
        };

        assert!(!exceeds_configured_limits(
            ProviderReportedTokenUsage::unreported(),
            limits,
        ));
    }

    #[test]
    fn provider_errors_retain_their_operator_cause_code() {
        let source = FixtureProviderError;
        let expected_cause = source.operator_failure_cause_code();
        let error = UsageLimitedProviderError::Provider(source);

        assert_eq!(error.operator_failure_cause_code(), expected_cause);
    }

    #[test]
    fn unconfigured_targets_have_a_dedicated_operator_cause_code() {
        let error = UsageLimitedProviderError::<FixtureProviderError>::UnconfiguredTarget;

        assert_eq!(
            error.operator_failure_cause_code(),
            "usage_limit_unconfigured_target"
        );
    }
}
