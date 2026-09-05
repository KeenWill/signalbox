//! Model-configuration token-limit enforcement around provider execution.

use std::{collections::HashMap, future::Future};

use signalbox_application::{
    ClassifyOperatorFailure, ModelCallCapabilityPreparation, ModelCallProvider,
    OperatorFailureClass, PreparedModelOperation,
};
use signalbox_domain::{
    AuthorizedModelCall, CorrelatedModelCallTerminalObservation, FastMode,
    ModelCallTerminalObservation, ProviderReportedTokenUsage, ResolvedProviderTarget,
};
use signalbox_model_provider_runtime::RuntimeModelCatalog;
use signalbox_model_runtime::TokenUsage;

use crate::configuration::{HubModelConfiguration, ModelAdapter};

/// One provider capability paired with configuration-owned limits.
pub struct UsageLimitedCapability<C> {
    inner: C,
    limits: ConfiguredUsageLimits,
}

#[derive(Clone, Copy)]
struct ConfiguredUsageLimits {
    max_output_tokens: u64,
    context_window_tokens: u64,
    adapter: ModelAdapter,
}

#[derive(Clone, Copy)]
struct ReportedUsageLowerBound {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfiguredUsageLimitExcess {
    Output,
    Context,
}

impl ConfiguredUsageLimitExcess {
    const fn cause_code(self) -> &'static str {
        match self {
            Self::Output => "model_output_usage_exceeded_after_completion",
            Self::Context => "model_context_usage_exceeded_after_completion",
        }
    }
}

impl From<ProviderReportedTokenUsage> for ReportedUsageLowerBound {
    fn from(usage: ProviderReportedTokenUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens(),
            output_tokens: usage.output_tokens(),
            cache_creation_input_tokens: usage.cache_creation_input_tokens(),
            cache_read_input_tokens: usage.cache_read_input_tokens(),
        }
    }
}

impl From<TokenUsage> for ReportedUsageLowerBound {
    fn from(usage: TokenUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            cache_read_input_tokens: usage.cache_read_input_tokens,
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
    adapters: HashMap<String, ModelAdapter>,
}

impl<P> UsageLimitedModelCallProvider<P> {
    /// Binds the provider to the same immutable runtime model catalog.
    pub fn new(inner: P, configuration: &HubModelConfiguration) -> Self {
        Self {
            inner,
            models: configuration.runtime_model_catalog(),
            adapters: configuration.adapter_routes(),
        }
    }
}

fn configured_usage_limits(
    models: &RuntimeModelCatalog,
    adapters: &HashMap<String, ModelAdapter>,
    target: ResolvedProviderTarget,
    fast_mode: FastMode,
) -> Option<ConfiguredUsageLimits> {
    let selected = models.resolve(target)?;
    let definition = models.effective_definition(selected, fast_mode)?;
    let adapter = adapters.get(definition.provider_model()).copied()?;
    Some(ConfiguredUsageLimits {
        max_output_tokens: u64::from(definition.max_output_tokens()),
        context_window_tokens: u64::from(definition.context_window_tokens()),
        adapter,
    })
}

fn configured_usage_limit_excess(
    usage: impl Into<ReportedUsageLowerBound>,
    limits: ConfiguredUsageLimits,
) -> Option<ConfiguredUsageLimitExcess> {
    let usage = usage.into();
    if usage
        .output_tokens
        .is_some_and(|output| output > limits.max_output_tokens)
    {
        return Some(ConfiguredUsageLimitExcess::Output);
    }
    let input_tokens = if limits.adapter.reports_cache_inclusive_input() {
        usage.input_tokens.unwrap_or(0)
    } else {
        usage
            .input_tokens
            .unwrap_or(0)
            .saturating_add(usage.cache_creation_input_tokens.unwrap_or(0))
            .saturating_add(usage.cache_read_input_tokens.unwrap_or(0))
    };
    (input_tokens.saturating_add(usage.output_tokens.unwrap_or(0)) > limits.context_window_tokens)
        .then_some(ConfiguredUsageLimitExcess::Context)
}

/// Whether one call's stored input count already includes the cache axes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReportedInputCacheAxes {
    /// The stored input already counts cache creation and cache reads.
    Included,
    /// The cache axes are reported beside the input and add to it.
    Excluded,
}

impl ReportedInputCacheAxes {
    /// Names the axis the durable usage read answers as a stored boolean.
    pub(crate) const fn from_includes_cache_tokens(includes_cache_tokens: bool) -> Self {
        if includes_cache_tokens {
            Self::Included
        } else {
            Self::Excluded
        }
    }
}

/// Whether one call's reported input is still model-visible for the next call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReportedInputRetention {
    /// The next request resends the transcript prefix this input counted.
    Retained(Option<u64>),
    /// A summary replaced the source this input counted, so the next request
    /// carries that summary instead of the counted material.
    Replaced,
}

impl ReportedInputRetention {
    /// Names the axis the durable usage read answers as a stored boolean.
    pub(crate) const fn from_retained(retained: bool, retained_input_tokens: Option<u64>) -> Self {
        if retained {
            Self::Retained(retained_input_tokens)
        } else {
            Self::Replaced
        }
    }
}

/// Whether one call's reported output became model-visible assistant transcript.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReportedOutputRetention {
    /// Completion kept the output as transcript the next request carries.
    Retained,
    /// Another terminal disposition left no assistant transcript behind.
    Discarded,
}

impl ReportedOutputRetention {
    /// Names the axis the durable usage read answers as a stored boolean.
    pub(crate) const fn from_retained(retained: bool) -> Self {
        if retained {
            Self::Retained
        } else {
            Self::Discarded
        }
    }
}

/// Whether one terminal call's reported usage proves the next un-compacted
/// call cannot retain the configured output reservation.
///
/// Later transcript entries can only increase the next input, so this is
/// deliberately a lower-bound trigger rather than an estimate of the
/// prospective request. A completed call's output also becomes part of that
/// next input; output reported by another terminal disposition did not become
/// assistant transcript and is excluded from the lower bound.
///
/// A dedicated compaction call's reported input is likewise excluded: it counts
/// the source text the summary replaced, which the next request no longer
/// carries. What that call retains is its summary output plus the unsummarized
/// content the projected-content allowance measures.
pub(crate) fn reported_usage_requires_compaction(
    usage: ProviderReportedTokenUsage,
    cache_axes: ReportedInputCacheAxes,
    input: ReportedInputRetention,
    output: ReportedOutputRetention,
    projected_unreported_content_bytes: u64,
    max_output_tokens: u64,
    context_window_tokens: u64,
) -> bool {
    let Some(input_tokens) = usage.input_tokens() else {
        return false;
    };
    let input_tokens = match cache_axes {
        ReportedInputCacheAxes::Included => input_tokens,
        ReportedInputCacheAxes::Excluded => input_tokens
            .saturating_add(usage.cache_creation_input_tokens().unwrap_or(0))
            .saturating_add(usage.cache_read_input_tokens().unwrap_or(0)),
    };
    let input_tokens = match input {
        ReportedInputRetention::Retained(Some(retained_input_tokens)) => retained_input_tokens,
        ReportedInputRetention::Retained(None) => input_tokens,
        ReportedInputRetention::Replaced => 0,
    };
    input_tokens
        .saturating_add(match output {
            ReportedOutputRetention::Retained => usage.output_tokens().unwrap_or(0),
            ReportedOutputRetention::Discarded => 0,
        })
        // CLI-backed adapters expose no tokenizer-only operation. UTF-8
        // bytes for model-visible transcript additions after the reported
        // input therefore form a deliberately conservative token allowance.
        .saturating_add(projected_unreported_content_bytes)
        .saturating_add(max_output_tokens)
        > context_window_tokens
}

/// Classifies dedicated-compaction usage against immutable model limits.
pub fn context_compaction_usage_exceeds_configured_limits(
    configuration: &HubModelConfiguration,
    target: ResolvedProviderTarget,
    usage: TokenUsage,
) -> Option<bool> {
    dedicated_model_usage_exceeds_configured_limits(configuration, target, usage)
}

/// Classifies dedicated approval-judge usage against immutable model limits.
pub fn approval_judge_usage_exceeds_configured_limits(
    configuration: &HubModelConfiguration,
    target: ResolvedProviderTarget,
    usage: TokenUsage,
) -> Option<bool> {
    dedicated_model_usage_exceeds_configured_limits(configuration, target, usage)
}

fn dedicated_model_usage_exceeds_configured_limits(
    configuration: &HubModelConfiguration,
    target: ResolvedProviderTarget,
    usage: TokenUsage,
) -> Option<bool> {
    let models = configuration.runtime_model_catalog();
    let definition = models.resolve(target)?;
    let adapter = configuration.adapter_for_provider_model(definition.provider_model())?;
    Some(
        configured_usage_limit_excess(
            usage,
            ConfiguredUsageLimits {
                max_output_tokens: u64::from(definition.max_output_tokens()),
                context_window_tokens: u64::from(definition.context_window_tokens()),
                adapter,
            },
        )
        .is_some(),
    )
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
        let limits = configured_usage_limits(
            &self.models,
            &self.adapters,
            operation.request().call().target(),
            operation.request().model_settings().effective().fast_mode(),
        )
        .ok_or(UsageLimitedProviderError::UnconfiguredTarget)?;
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
                ModelCallCapabilityPreparation::AttachmentFailure(failure) => {
                    ModelCallCapabilityPreparation::AttachmentFailure(failure)
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
        let session = authorized.session();
        let turn = authorized.turn();
        let call = authorized.call().id();
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
                | ModelCallTerminalObservation::CompletedWithProviderCompaction { .. }
                | ModelCallTerminalObservation::CompletedWithTools { .. }
        );
        if let Some(excess) = completed
            .then(|| configured_usage_limit_excess(observation.usage(), capability.limits))
            .flatten()
        {
            tracing::warn!(
                cause_code = excess.cause_code(),
                session_id = %session.as_uuid(),
                turn_id = %turn.as_uuid(),
                model_call_id = %call.as_uuid(),
                "completed model output exceeded a configured usage limit and was preserved"
            );
        }
        Ok(observation)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use signalbox_application::{ClassifyOperatorFailure, OperatorFailureClass};
    use signalbox_domain::{
        FastMode, ProviderModelIdentity, ProviderReportedTokenUsage, ResolvedProviderTarget,
    };
    use signalbox_model_provider_runtime::{RuntimeModelCatalog, RuntimeModelDefinition};
    use uuid::Uuid;

    use crate::configuration::ModelAdapter;

    use super::{
        ConfiguredUsageLimitExcess, ConfiguredUsageLimits, ReportedInputCacheAxes,
        ReportedInputRetention, ReportedOutputRetention, UsageLimitedProviderError,
        configured_usage_limit_excess, configured_usage_limits, reported_usage_requires_compaction,
    };

    #[derive(Debug)]
    struct FixtureProviderError;

    fn target(value: u128) -> ResolvedProviderTarget {
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(value)))
    }

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
            adapter: ModelAdapter::Anthropic,
        };

        assert_eq!(configured_usage_limit_excess(at_limit, limits), None);
        assert_eq!(
            configured_usage_limit_excess(output_exceeded, limits),
            Some(ConfiguredUsageLimitExcess::Output)
        );
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
            adapter: ModelAdapter::Anthropic,
        };

        assert_eq!(configured_usage_limit_excess(at_limit, limits), None);
        assert_eq!(
            configured_usage_limit_excess(context_exceeded, limits),
            Some(ConfiguredUsageLimitExcess::Context)
        );
    }

    #[test]
    fn anthropic_cache_input_is_counted_against_the_model_context_limit() {
        let at_limit = ProviderReportedTokenUsage::unreported()
            .with_input_tokens(Some(60))
            .with_output_tokens(Some(10))
            .with_cache_creation_input_tokens(Some(15))
            .with_cache_read_input_tokens(Some(15));
        let context_exceeded = ProviderReportedTokenUsage::unreported()
            .with_input_tokens(Some(60))
            .with_output_tokens(Some(10))
            .with_cache_creation_input_tokens(Some(15))
            .with_cache_read_input_tokens(Some(16));
        let limits = ConfiguredUsageLimits {
            max_output_tokens: 50,
            context_window_tokens: 100,
            adapter: ModelAdapter::Anthropic,
        };

        assert_eq!(configured_usage_limit_excess(at_limit, limits), None);
        assert_eq!(
            configured_usage_limit_excess(context_exceeded, limits),
            Some(ConfiguredUsageLimitExcess::Context)
        );
    }

    /// Claude Code reports the same cache-exclusive input shape the Anthropic
    /// API does, so its separately reported cache axes join the input total.
    #[test]
    fn claude_cache_input_is_counted_against_the_model_context_limit() {
        let at_limit = ProviderReportedTokenUsage::unreported()
            .with_input_tokens(Some(60))
            .with_output_tokens(Some(10))
            .with_cache_creation_input_tokens(Some(15))
            .with_cache_read_input_tokens(Some(15));
        let context_exceeded = ProviderReportedTokenUsage::unreported()
            .with_input_tokens(Some(60))
            .with_output_tokens(Some(10))
            .with_cache_creation_input_tokens(Some(15))
            .with_cache_read_input_tokens(Some(16));
        let limits = ConfiguredUsageLimits {
            max_output_tokens: 50,
            context_window_tokens: 100,
            adapter: ModelAdapter::ClaudeCli,
        };

        assert_eq!(configured_usage_limit_excess(at_limit, limits), None);
        assert_eq!(
            configured_usage_limit_excess(context_exceeded, limits),
            Some(ConfiguredUsageLimitExcess::Context)
        );
    }

    /// OpenAI's `prompt_tokens` already contains the cached prompt tokens it
    /// reports beside it, so adding the cache axes would double-count them.
    #[test]
    fn openai_cache_breakdowns_are_not_added_to_the_reported_input_total() {
        let usage = ProviderReportedTokenUsage::unreported()
            .with_input_tokens(Some(80))
            .with_output_tokens(Some(20))
            .with_cache_read_input_tokens(Some(40));
        let limits = ConfiguredUsageLimits {
            max_output_tokens: 50,
            context_window_tokens: 100,
            adapter: ModelAdapter::OpenAi,
        };

        assert_eq!(configured_usage_limit_excess(usage, limits), None);
    }

    #[test]
    fn codex_cache_breakdowns_are_not_added_to_the_reported_input_total() {
        let usage = ProviderReportedTokenUsage::unreported()
            .with_input_tokens(Some(80))
            .with_output_tokens(Some(20))
            .with_cache_creation_input_tokens(Some(15))
            .with_cache_read_input_tokens(Some(15));
        let limits = ConfiguredUsageLimits {
            max_output_tokens: 50,
            context_window_tokens: 100,
            adapter: ModelAdapter::CodexCli,
        };

        assert_eq!(configured_usage_limit_excess(usage, limits), None);
    }

    #[test]
    fn reported_usage_triggers_compaction_before_the_next_output_reservation() {
        let usage = ProviderReportedTokenUsage::unreported()
            .with_input_tokens(Some(80))
            .with_output_tokens(Some(5));

        assert!(reported_usage_requires_compaction(
            usage,
            ReportedInputCacheAxes::Included,
            ReportedInputRetention::Retained(None),
            ReportedOutputRetention::Retained,
            0,
            16,
            100
        ));
    }

    #[test]
    fn reported_usage_trigger_uses_the_stored_cache_semantics() {
        let usage = ProviderReportedTokenUsage::unreported()
            .with_input_tokens(Some(60))
            .with_output_tokens(Some(5))
            .with_cache_read_input_tokens(Some(20));

        assert!(!reported_usage_requires_compaction(
            usage,
            ReportedInputCacheAxes::Included,
            ReportedInputRetention::Retained(None),
            ReportedOutputRetention::Retained,
            0,
            15,
            100
        ));
        assert!(reported_usage_requires_compaction(
            usage,
            ReportedInputCacheAxes::Excluded,
            ReportedInputRetention::Retained(None),
            ReportedOutputRetention::Retained,
            0,
            16,
            100
        ));
    }

    #[test]
    fn reported_usage_trigger_does_not_invent_a_missing_input_count() {
        let usage = ProviderReportedTokenUsage::unreported().with_output_tokens(Some(100));

        assert!(!reported_usage_requires_compaction(
            usage,
            ReportedInputCacheAxes::Included,
            ReportedInputRetention::Retained(None),
            ReportedOutputRetention::Retained,
            0,
            100,
            100
        ));
    }

    #[test]
    fn reported_usage_excludes_output_that_did_not_enter_the_transcript() {
        let usage = ProviderReportedTokenUsage::unreported()
            .with_input_tokens(Some(80))
            .with_output_tokens(Some(10));

        assert!(!reported_usage_requires_compaction(
            usage,
            ReportedInputCacheAxes::Included,
            ReportedInputRetention::Retained(None),
            ReportedOutputRetention::Discarded,
            0,
            11,
            100
        ));
        assert!(reported_usage_requires_compaction(
            usage,
            ReportedInputCacheAxes::Included,
            ReportedInputRetention::Retained(None),
            ReportedOutputRetention::Retained,
            0,
            11,
            100
        ));
    }

    #[test]
    fn post_compaction_baseline_excludes_the_summarized_away_source() {
        // A dedicated compaction reports the pre-compaction source text it
        // summarized as input and the retained summary as output. That source is
        // exactly the material the summary removed from model visibility, so
        // only the summary and the retained content bound the next call.
        let compaction_usage = ProviderReportedTokenUsage::unreported()
            .with_input_tokens(Some(90))
            .with_output_tokens(Some(5));

        assert!(!reported_usage_requires_compaction(
            compaction_usage,
            ReportedInputCacheAxes::Included,
            ReportedInputRetention::Replaced,
            ReportedOutputRetention::Retained,
            4,
            10,
            100
        ));
    }

    #[test]
    fn reported_usage_includes_model_visible_content_appended_after_the_call() {
        let usage = ProviderReportedTokenUsage::unreported()
            .with_input_tokens(Some(60))
            .with_output_tokens(Some(5));

        assert!(!reported_usage_requires_compaction(
            usage,
            ReportedInputCacheAxes::Included,
            ReportedInputRetention::Retained(None),
            ReportedOutputRetention::Retained,
            0,
            10,
            100
        ));
        assert!(reported_usage_requires_compaction(
            usage,
            ReportedInputCacheAxes::Included,
            ReportedInputRetention::Retained(None),
            ReportedOutputRetention::Retained,
            26,
            10,
            100
        ));
    }

    #[test]
    fn provider_compaction_headroom_uses_retained_input_not_billed_iterations() {
        let billed = ProviderReportedTokenUsage::unreported()
            .with_input_tokens(Some(180))
            .with_output_tokens(Some(5));

        assert!(!reported_usage_requires_compaction(
            billed,
            ReportedInputCacheAxes::Excluded,
            ReportedInputRetention::Retained(Some(40)),
            ReportedOutputRetention::Retained,
            0,
            10,
            100
        ));
    }

    #[test]
    fn absent_usage_fields_do_not_invent_adapter_counts() {
        let limits = ConfiguredUsageLimits {
            max_output_tokens: 20,
            context_window_tokens: 100,
            adapter: ModelAdapter::Anthropic,
        };

        assert_eq!(
            configured_usage_limit_excess(ProviderReportedTokenUsage::unreported(), limits),
            None
        );
    }

    #[test]
    fn mapped_fast_serving_uses_the_serving_targets_limits() {
        let standard_model = "fixture-standard";
        let fast_model = "fixture-fast";
        let standard =
            RuntimeModelDefinition::try_new(target(1), String::from(standard_model), 50, 100)
                .expect("the standard fixture definition is valid")
                .with_fast_target(target(2));
        let fast = RuntimeModelDefinition::try_new(target(2), String::from(fast_model), 20, 60)
            .expect("the fast fixture definition is valid");
        let expected_output_limit = fast.max_output_tokens();
        let expected_context_limit = fast.context_window_tokens();
        let models = RuntimeModelCatalog::try_from_definitions([standard, fast])
            .expect("the mapped target is complete");
        let adapters = HashMap::from([
            (String::from(standard_model), ModelAdapter::Anthropic),
            (String::from(fast_model), ModelAdapter::Anthropic),
        ]);

        let limits = configured_usage_limits(&models, &adapters, target(1), FastMode::Enabled)
            .expect("mapped fast limits resolve");

        assert_eq!(limits.max_output_tokens, u64::from(expected_output_limit));
        assert_eq!(
            limits.context_window_tokens,
            u64::from(expected_context_limit)
        );
        assert_eq!(limits.adapter, adapters[fast_model]);
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
