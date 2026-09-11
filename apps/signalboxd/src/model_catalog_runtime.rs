//! Runtime composition from one accepted model-catalog snapshot per operation.

use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

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
#[derive(Clone, Debug)]
pub struct ModelRuntimeFactory {
    exchange_timeout: Option<Duration>,
    post_kill_reap_bound: Option<Duration>,
    native_message_limit: Option<usize>,
    oauth_delivery: Option<(
        Arc<dyn signalbox_model_runtime_codex_cli::OauthCredentialProvider>,
        Arc<signalbox_model_runtime_codex_cli::OauthCredentialRoot>,
    )>,
    codex_cli_unavailable_cause: Option<&'static str>,
}

#[derive(Clone, Copy, Debug)]
pub struct ModelRuntimeBuildError(&'static str);

impl ModelRuntimeBuildError {
    /// Returns the closed cause code without adapter-owned detail strings.
    pub const fn cause_code(self) -> &'static str {
        self.0
    }
}

impl From<crate::configuration::HubModelConfigurationError> for ModelRuntimeBuildError {
    fn from(_: crate::configuration::HubModelConfigurationError) -> Self {
        Self("continuation_request_measurement")
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
            oauth_delivery: None,
            codex_cli_unavailable_cause: None,
        }
    }

    /// Shares startup OAuth delivery with every catalog-composed adapter.
    pub fn with_oauth_delivery(
        mut self,
        provider: Arc<dyn signalbox_model_runtime_codex_cli::OauthCredentialProvider>,
        root: Arc<signalbox_model_runtime_codex_cli::OauthCredentialRoot>,
    ) -> Self {
        self.oauth_delivery = Some((provider, root));
        self
    }

    /// Retains a startup probe cause while omitting the Codex adapter.
    pub fn with_codex_cli_unavailable(mut self, cause: &'static str) -> Self {
        self.codex_cli_unavailable_cause = Some(cause);
        self
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
        let runtime = ConfiguredModelRuntime::new(
            anthropic,
            openai,
            models,
            self.exchange_timeout,
            self.post_kill_reap_bound,
            self.native_message_limit,
        )
        .map_err(|error| ModelRuntimeBuildError(error.cause_code()))?;
        let runtime = match self.codex_cli_unavailable_cause {
            Some(cause) => runtime.with_codex_cli_unavailable(cause),
            None => runtime,
        };
        Ok(match &self.oauth_delivery {
            Some((provider, root)) => runtime.with_oauth_delivery(provider.clone(), root.clone()),
            None => runtime,
        })
    }
}

/// Composes an execution pass from the complete catalog at admission.
#[derive(Clone)]
pub struct CatalogEligibilityPass<F> {
    catalogs: ConfigurationReload,
    compose: F,
    expiry_handlers: Arc<CatalogExpiryHandlers>,
}

impl<F> CatalogEligibilityPass<F> {
    pub fn new(catalogs: ConfigurationReload, compose: F) -> Self {
        Self {
            catalogs,
            compose,
            expiry_handlers: Arc::default(),
        }
    }
}

#[derive(Debug, Default)]
struct CatalogExpiryHandlers(Mutex<HashMap<SessionId, Arc<dyn SchedulerPassExpiryHandler>>>);

impl SchedulerPassExpiryHandler for CatalogExpiryHandlers {
    fn occupancy_expired(&self, session: SessionId) {
        let handler = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session)
            .cloned();
        if let Some(handler) = handler {
            handler.occupancy_expired(session);
        }
    }
}

struct CatalogExpiryRegistration {
    handlers: Arc<CatalogExpiryHandlers>,
    session: SessionId,
}

impl Drop for CatalogExpiryRegistration {
    fn drop(&mut self) {
        self.handlers
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.session);
    }
}

#[derive(Debug)]
pub enum CatalogPassError<E> {
    Configuration(ModelRuntimeBuildError),
    Pass(E),
}

impl<F, P> EligibilityPass for CatalogEligibilityPass<F>
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
        Some(self.expiry_handlers.clone())
    }
    fn run(
        &mut self,
        session: SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let pass = (self.compose)(&self.catalogs.catalogs().models);
        let handlers = self.expiry_handlers.clone();
        async move {
            let mut pass = pass.map_err(CatalogPassError::Configuration)?;
            let _registration = pass.occupancy_expiry_handler().map(|handler| {
                handlers
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(session, handler);
                CatalogExpiryRegistration { handlers, session }
            });
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

    type ExpiryHandoffs = Arc<Mutex<Vec<(SessionId, Option<TurnId>)>>>;

    #[derive(Debug)]
    struct TrackedExpiry {
        turn: Mutex<Option<TurnId>>,
        handoffs: ExpiryHandoffs,
    }

    impl SchedulerPassExpiryHandler for TrackedExpiry {
        fn occupancy_expired(&self, session: SessionId) {
            self.handoffs
                .lock()
                .expect("handoffs")
                .push((session, *self.turn.lock().expect("turn")));
        }
    }

    struct TrackingPass {
        handler: Arc<TrackedExpiry>,
        turn: TurnId,
        started: Arc<tokio::sync::Notify>,
        complete: Arc<tokio::sync::Notify>,
    }

    impl EligibilityPass for TrackingPass {
        type Error = std::convert::Infallible;

        fn occupancy_expiry_handler(&self) -> Option<Arc<dyn SchedulerPassExpiryHandler>> {
            Some(self.handler.clone())
        }

        fn run(
            &mut self,
            _session: SessionId,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
            let handler = self.handler.clone();
            let turn = self.turn;
            let started = self.started.clone();
            let complete = self.complete.clone();
            async move {
                *handler.turn.lock().expect("turn") = Some(turn);
                started.notify_one();
                complete.notified().await;
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn occupancy_expiry_tracks_each_composed_pass_until_completion_or_cancellation() {
        let models = crate::configuration::checked_in_example_configuration().expect("models");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@localhost/unused")
            .expect("lazy pool");
        let catalogs = ConfigurationReload::new(
            pool,
            models,
            crate::SessionTemplateConfiguration::default(),
            "/unused/models.toml".into(),
            "/unused/templates.toml".into(),
            None,
        )
        .expect("catalogs");
        // Distinct fixture identities expose cross-session or baseline-handler routing.
        let first_session = SessionId::from_uuid(uuid::Uuid::from_u128(1));
        let second_session = SessionId::from_uuid(uuid::Uuid::from_u128(2));
        let first_turn = TurnId::from_uuid(uuid::Uuid::from_u128(3));
        let second_turn = TurnId::from_uuid(uuid::Uuid::from_u128(4));
        let handoffs = Arc::new(Mutex::new(Vec::new()));
        let first_started = Arc::new(tokio::sync::Notify::new());
        let second_started = Arc::new(tokio::sync::Notify::new());
        let second_complete = Arc::new(tokio::sync::Notify::new());
        let passes = Mutex::new(std::collections::VecDeque::from([
            TrackingPass {
                handler: Arc::new(TrackedExpiry {
                    turn: Mutex::new(None),
                    handoffs: handoffs.clone(),
                }),
                turn: first_turn,
                started: first_started.clone(),
                complete: Arc::default(),
            },
            TrackingPass {
                handler: Arc::new(TrackedExpiry {
                    turn: Mutex::new(None),
                    handoffs: handoffs.clone(),
                }),
                turn: second_turn,
                started: second_started.clone(),
                complete: second_complete.clone(),
            },
        ]));
        let mut pass = CatalogEligibilityPass::new(catalogs, move |_: &HubModelConfiguration| {
            Ok(passes
                .lock()
                .expect("passes")
                .pop_front()
                .expect("composed pass"))
        });
        // Scheduler admission captures expiry before invoking run.
        let first_expiry = pass.occupancy_expiry_handler().expect("expiry handler");
        let first = tokio::spawn(pass.run(first_session));
        first_started.notified().await;
        let second_expiry = pass.occupancy_expiry_handler().expect("expiry handler");
        let second = tokio::spawn(pass.run(second_session));
        second_started.notified().await;
        first_expiry.occupancy_expired(first_session);
        second_expiry.occupancy_expired(second_session);
        assert_eq!(
            *handoffs.lock().expect("handoffs"),
            [
                (first_session, Some(first_turn)),
                (second_session, Some(second_turn)),
            ]
        );
        second_complete.notify_one();
        second.await.expect("second task").expect("second pass");
        first.abort();
        assert!(
            first
                .await
                .expect_err("first pass cancelled")
                .is_cancelled()
        );
        first_expiry.occupancy_expired(first_session);
        second_expiry.occupancy_expired(second_session);
        assert_eq!(
            *handoffs.lock().expect("handoffs"),
            [
                (first_session, Some(first_turn)),
                (second_session, Some(second_turn)),
            ]
        );
    }

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
