//! Runtime composition from one accepted model-catalog snapshot per operation.

use std::{future::Future, sync::Arc, time::Duration};

use signalbox_application::{EligibilityPass, SchedulerPassExpiryHandler};
use signalbox_domain::{SessionId, TurnId};
use signalbox_model_provider_runtime::{
    ContextCompactionModel, ContextCompactionModelError, ContextCompactionModelRequest,
    ContextCompactionModelResult, RuntimeContextCompactionModel,
};
use signalbox_model_runtime::CredentialReference;
use signalbox_model_runtime_anthropic::{
    AnthropicConfig, AnthropicConstructionError, AnthropicRuntime,
};
use signalbox_model_runtime_openai::{OpenAiConfig, OpenAiConstructionError, OpenAiRuntime};

use crate::{
    FileCredentialAccess, HubModelConfiguration, configuration::ModelAdapter,
    configuration_reload::ConfigurationReload, model_adapter::ConfiguredModelRuntime,
};

type Runtime = ConfiguredModelRuntime<
    AnthropicRuntime<FileCredentialAccess>,
    OpenAiRuntime<FileCredentialAccess>,
>;

/// Startup-only transport bounds used when composing an admitted catalog.
#[derive(Clone, Copy, Debug)]
pub struct ModelRuntimeFactory {
    exchange_timeout: Option<Duration>,
    post_kill_reap_bound: Option<Duration>,
    native_message_limit: Option<usize>,
}

#[derive(Clone, Copy, Debug)]
pub struct ModelRuntimeBuildError(&'static str);

impl ModelRuntimeBuildError {
    /// Returns the closed cause code without adapter-owned detail strings.
    pub const fn cause_code(self) -> &'static str {
        self.0
    }
}

impl From<AnthropicConstructionError> for ModelRuntimeBuildError {
    fn from(error: AnthropicConstructionError) -> Self {
        Self(match error {
            AnthropicConstructionError::InvalidBaseUrl { .. } => "anthropic_invalid_base_url",
            AnthropicConstructionError::InvalidVersion => "anthropic_invalid_version",
            AnthropicConstructionError::InvalidExchangeTimeout => "anthropic_invalid_timeout",
            AnthropicConstructionError::InvalidSseRecordLimit => "anthropic_invalid_record_limit",
            AnthropicConstructionError::ClientConstruction { .. } => {
                "anthropic_client_construction"
            }
        })
    }
}

impl From<OpenAiConstructionError> for ModelRuntimeBuildError {
    fn from(error: OpenAiConstructionError) -> Self {
        Self(match error {
            OpenAiConstructionError::InvalidBaseUrl { .. } => "openai_invalid_base_url",
            OpenAiConstructionError::InvalidExchangeTimeout => "openai_invalid_timeout",
            OpenAiConstructionError::InvalidSseRecordLimit => "openai_invalid_record_limit",
            OpenAiConstructionError::ClientConstruction { .. } => "openai_client_construction",
        })
    }
}

impl ModelRuntimeFactory {
    pub fn new(
        exchange_timeout: Option<Duration>,
        post_kill_reap_bound: Option<Duration>,
        native_message_limit: Option<usize>,
    ) -> Self {
        Self {
            exchange_timeout,
            post_kill_reap_bound,
            native_message_limit,
        }
    }

    /// Constructs adapters without provider I/O using this snapshot's routes and capabilities.
    pub fn build(&self, models: &HubModelConfiguration) -> Result<Runtime, ModelRuntimeBuildError> {
        let credentials =
            |adapter| {
                FileCredentialAccess::from_files(models.file_credential_profiles(adapter).map(
                    |(reference, path)| (CredentialReference::new(reference), path.to_path_buf()),
                ))
            };
        let anthropic = if models.uses_anthropic_adapter() {
            let mut config = AnthropicConfig::new(self.native_message_limit);
            config.exchange_timeout = self.exchange_timeout;
            config.model_capabilities = models.runtime_model_capability_catalog();
            Some(
                AnthropicRuntime::new(config, credentials(ModelAdapter::Anthropic))
                    .map_err(ModelRuntimeBuildError::from)?,
            )
        } else {
            None
        };
        let openai = if models.uses_openai_adapter() {
            let mut config = OpenAiConfig::new(self.native_message_limit);
            config.exchange_timeout = self.exchange_timeout;
            config.model_capabilities = models.runtime_model_capability_catalog();
            Some(
                OpenAiRuntime::new(config, credentials(ModelAdapter::OpenAi))
                    .map_err(ModelRuntimeBuildError::from)?,
            )
        } else {
            None
        };
        ConfiguredModelRuntime::new(
            anthropic,
            openai,
            models,
            self.exchange_timeout,
            self.post_kill_reap_bound,
            self.native_message_limit,
        )
        .map_err(|error| ModelRuntimeBuildError(error.cause_code()))
    }
}

/// Composes an execution pass from the complete catalog at admission.
#[derive(Clone)]
pub struct CatalogEligibilityPass<F, P> {
    catalogs: ConfigurationReload,
    compose: F,
    baseline: P,
}

impl<F, P> CatalogEligibilityPass<F, P> {
    pub fn new(catalogs: ConfigurationReload, compose: F, baseline: P) -> Self {
        Self {
            catalogs,
            compose,
            baseline,
        }
    }
}

#[derive(Debug)]
pub enum CatalogPassError<E> {
    Configuration(ModelRuntimeBuildError),
    Pass(E),
}

impl<F, P> EligibilityPass for CatalogEligibilityPass<F, P>
where
    F: Fn(&HubModelConfiguration) -> Result<P, ModelRuntimeBuildError>,
    P: EligibilityPass + Send + 'static,
    P::Error: Send + 'static,
{
    type Error = CatalogPassError<P::Error>;

    fn failure_stage(error: &Self::Error) -> &'static str {
        match error {
            CatalogPassError::Configuration(_) => "model_configuration",
            CatalogPassError::Pass(error) => P::failure_stage(error),
        }
    }
    fn failure_turn(error: &Self::Error) -> Option<TurnId> {
        match error {
            CatalogPassError::Configuration(_) => None,
            CatalogPassError::Pass(error) => P::failure_turn(error),
        }
    }
    fn occupancy_expiry_handler(&self) -> Option<Arc<dyn SchedulerPassExpiryHandler>> {
        self.baseline.occupancy_expiry_handler()
    }
    fn run(
        &mut self,
        session: SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let pass = (self.compose)(&self.catalogs.catalogs().models);
        async move {
            let mut pass = pass.map_err(CatalogPassError::Configuration)?;
            pass.run(session).await.map_err(CatalogPassError::Pass)
        }
    }
}

/// Process-triggered compaction uses one current catalog for its complete execution.
#[derive(Clone, Debug)]
pub struct CatalogContextCompactionModel {
    models: Arc<HubModelConfiguration>,
    factory: ModelRuntimeFactory,
}

impl CatalogContextCompactionModel {
    pub fn new(models: Arc<HubModelConfiguration>, factory: ModelRuntimeFactory) -> Self {
        Self { models, factory }
    }
}

impl ContextCompactionModel for CatalogContextCompactionModel {
    fn execute<'a>(
        &'a self,
        request: ContextCompactionModelRequest,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = Result<ContextCompactionModelResult, ContextCompactionModelError>>
                + Send
                + 'a,
        >,
    > {
        let models = self.models.clone();
        Box::pin(async move {
            let runtime = self
                .factory
                .build(&models)
                .map_err(|_| ContextCompactionModelError::PreparationDefect)?;
            RuntimeContextCompactionModel::new(runtime, models.runtime_model_catalog())
                .execute(request)
                .await
        })
    }
}

impl std::fmt::Display for ModelRuntimeBuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.cause_code())
    }
}
impl std::error::Error for ModelRuntimeBuildError {}
impl<E: std::fmt::Display> std::fmt::Display for CatalogPassError<E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Configuration(error) => error.fmt(formatter),
            Self::Pass(error) => error.fmt(formatter),
        }
    }
}
impl<E: std::error::Error + 'static> std::error::Error for CatalogPassError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match self {
            Self::Configuration(error) => error,
            Self::Pass(error) => error,
        })
    }
}

impl<E: signalbox_application::ClassifyOperatorFailure>
    signalbox_application::ClassifyOperatorFailure for CatalogPassError<E>
{
    fn operator_failure_class(&self) -> signalbox_application::OperatorFailureClass {
        match self {
            Self::Configuration(_) => signalbox_application::OperatorFailureClass::CallerOrHubBug,
            Self::Pass(error) => error.operator_failure_class(),
        }
    }
    fn operator_failure_cause_code(&self) -> &'static str {
        match self {
            Self::Configuration(error) => error.cause_code(),
            Self::Pass(error) => error.operator_failure_cause_code(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_composition_preserves_the_adapter_timeout_cause() {
        let models = crate::configuration::checked_in_example_configuration().expect("models");
        let error = match ModelRuntimeFactory::new(Some(Duration::ZERO), None, None).build(&models)
        {
            Ok(_) => panic!("zero exchange timeout is invalid"),
            Err(error) => error,
        };
        assert_eq!(error.cause_code(), "anthropic_invalid_timeout");
    }
}
