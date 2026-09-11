//! Serial durable reloads and atomic request-catalog snapshots for process protocol.

use std::{
    path::PathBuf,
    sync::{Arc, RwLock},
};

use serde::{Deserialize, Serialize};
use signalbox_module_repo_watch_v2::{
    ReloadIntentInput, RepositoryRuleSet, RuleReconciliationAdmission, repository_rule_set_digest,
};
use signalbox_persistence::reload_configuration::{
    ReloadClaim, ReloadConfiguration, ReloadConfigurationRepository, ReloadIntent, ReloadLookup,
    ReloadPhase, ReloadRepositoryError, ReloadResult,
};
use tokio::sync::Mutex;

use crate::repo_watch_runtime::RepositoryWatchRuntime;
use crate::{HubModelConfiguration, SessionTemplateConfiguration};
use signalbox_persistence::convergence_sweep::PostgresConvergenceSweepStore;

// The design admits these catalog sections; all other model-document keys are startup-only.
const RELOADABLE_KEYS: [&str; 5] = [
    "models",
    "serving_targets",
    "aliases",
    "repository_watch",
    "credential_profiles",
];

/// Complete immutable pair observed by one admitted request.
#[derive(Clone, Debug)]
pub struct ConfigurationCatalogs {
    /// Checked model and alias configuration.
    pub models: Arc<HubModelConfiguration>,
    /// Checked templates, including retained prompt-file contents.
    pub templates: Arc<SessionTemplateConfiguration>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetainedSnapshot {
    model_catalog: String,
    session_templates: String,
}

impl ConfigurationCatalogs {
    fn retained(&self) -> Result<RetainedSnapshot, ReloadResult> {
        let mut model_catalog = self
            .models
            .source()
            .parse::<toml_edit::DocumentMut>()
            .map_err(|_| failure(ReloadPhase::Validate, "retained model source is invalid"))?;
        model_catalog
            .as_table_mut()
            .retain(|key, _| RELOADABLE_KEYS.contains(&key));
        Ok(RetainedSnapshot {
            model_catalog: model_catalog.to_string(),
            session_templates: self.templates.source().to_owned(),
        })
    }
}

/// Persistence failure classified by whether reload effects need recovery.
#[derive(Debug)]
pub enum ConfigurationReloadError {
    BeforeEffect(ReloadRepositoryError),
    RecoveryRequired(ReloadRepositoryError),
}

impl std::fmt::Display for ConfigurationReloadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BeforeEffect(error) | Self::RecoveryRequired(error) => {
                std::fmt::Display::fmt(error, formatter)
            }
        }
    }
}

impl std::error::Error for ConfigurationReloadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BeforeEffect(error) | Self::RecoveryRequired(error) => Some(error),
        }
    }
}

impl From<ReloadRepositoryError> for ConfigurationReloadError {
    fn from(error: ReloadRepositoryError) -> Self {
        Self::BeforeEffect(error)
    }
}

/// One daemon-wide reload mutex and one atomically replaced catalog pair.
#[derive(Clone)]
pub struct ConfigurationReload {
    current: Arc<RwLock<ConfigurationCatalogs>>,
    serial: Arc<Mutex<()>>,
    repository: ReloadConfigurationRepository,
    model_path: PathBuf,
    template_path: PathBuf,
    home: Option<PathBuf>,
    startup: toml::Table,
    watch: Option<RepositoryWatchRuntime>,
    runtime_factory: Option<crate::model_catalog_runtime::ModelRuntimeFactory>,
    github_tool_credential: Option<PathBuf>,
    integration_credentials: crate::FileCredentialAccess,
    convergence: PostgresConvergenceSweepStore,
}

impl std::fmt::Debug for ConfigurationReload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConfigurationReload")
            .finish_non_exhaustive()
    }
}

impl ConfigurationReload {
    pub(crate) fn repository_ingestion_measurements(
        &self,
    ) -> Vec<(
        signalbox_domain::RepositorySlug,
        signalbox_module_repo_watch_v2::measurements::IngestionMeasurements,
    )> {
        let catalogs = self.catalogs();
        catalogs
            .models
            .repository_watch()
            .into_iter()
            .flat_map(|configuration| configuration.repositories())
            .map(|repository| {
                let measurements = self
                    .watch
                    .as_ref()
                    .map(|watch| watch.ingestion_measurements(repository.repository()))
                    .unwrap_or_default();
                (repository.repository().clone(), measurements)
            })
            .collect()
    }
    pub(crate) async fn goal_github_client(
        &self,
        repository: &str,
    ) -> Result<signalbox_module_repo_watch_v2::github::GitHubClient, ()> {
        use signalbox_model_runtime::{CredentialAccess, CredentialReference};
        let catalogs = self.catalogs();
        if let Some(watched) = catalogs
            .models
            .repository_watch()
            .into_iter()
            .flat_map(|watch| watch.repositories())
            .find(|watched| watched.repository().as_str() == repository)
        {
            return crate::repo_watch_credentials::RepositoryWatchClientLoader::new(watched)
                .load()
                .await
                .map_err(|_| ());
        }
        let reference =
            CredentialReference::new(signalbox_tools_code_host::CODE_HOST_CREDENTIAL_REFERENCE);
        let credentials = match catalogs
            .models
            .github_credential_profile(reference.as_str())
        {
            Some(profile) => crate::FileCredentialAccess::from_github(profile, reference.clone()),
            None => crate::FileCredentialAccess::new(
                self.github_tool_credential.clone().ok_or(())?,
                reference.clone(),
            ),
        };
        if let Some(app) = credentials.github_app() {
            return crate::repo_watch_credentials::app_observation_client(
                "signalbox-goal-verification",
                app,
            )
            .map_err(|_| ());
        }
        let credential = credentials.resolve(&reference).await.map_err(|_| ())?;
        let token = std::str::from_utf8(credential.expose_bytes()).map_err(|_| ())?;
        signalbox_module_repo_watch_v2::github::GitHubClient::try_new(
            "signalbox-goal-verification",
            token,
        )
        .map_err(|_| ())
    }

    pub(crate) async fn goal_dispatch_authority(
        &self,
        session: signalbox_domain::SessionId,
    ) -> Result<
        Option<signalbox_application::ApprovalJudgeDispatchAuthority>,
        signalbox_module_repo_watch_v2::StoreError,
    > {
        match &self.watch {
            Some(watch) => watch.approval_judge_authority(session).await,
            None => Ok(None),
        }
    }

    pub(crate) async fn repository_watch_origin(
        &self,
        session: signalbox_domain::SessionId,
    ) -> Result<
        Option<signalbox_module_repo_watch_v2::RetainedDispatchAction>,
        signalbox_module_repo_watch_v2::StoreError,
    > {
        match &self.watch {
            Some(watch) => watch.session_origin(session).await,
            None => Ok(None),
        }
    }

    /// Retains the exact startup catalogs and the configured file locations.
    pub fn new(
        pool: sqlx::PgPool,
        models: HubModelConfiguration,
        templates: SessionTemplateConfiguration,
        model_path: PathBuf,
        template_path: PathBuf,
        home: Option<PathBuf>,
    ) -> Result<Self, ReloadResult> {
        let startup = startup_sections(&models)?;
        Ok(Self {
            current: Arc::new(RwLock::new(ConfigurationCatalogs {
                models: Arc::new(models),
                templates: Arc::new(templates),
            })),
            serial: Arc::new(Mutex::new(())),
            repository: ReloadConfigurationRepository::new(pool.clone()),
            convergence: PostgresConvergenceSweepStore::new(pool),
            watch: None,
            runtime_factory: None,
            github_tool_credential: None,
            integration_credentials: crate::FileCredentialAccess::from_files([]),
            model_path,
            template_path,
            home,
            startup,
        })
    }

    /// Validates execution composition before admitting a replacement catalog.
    pub fn with_runtime_factory(
        mut self,
        factory: crate::model_catalog_runtime::ModelRuntimeFactory,
    ) -> Self {
        self.runtime_factory = Some(factory);
        self
    }

    pub(crate) fn compaction_model(
        &self,
        models: Arc<HubModelConfiguration>,
    ) -> Option<Arc<dyn signalbox_model_provider_runtime::ContextCompactionModel>> {
        self.runtime_factory.as_ref().map(|factory| {
            Arc::new(
                crate::model_catalog_runtime::CatalogContextCompactionModel::new(
                    models,
                    factory.clone(),
                ),
            ) as _
        })
    }

    pub fn with_github_tool_credential(mut self, path: PathBuf) -> Self {
        self.github_tool_credential = Some(path);
        self
    }

    /// Rechecks the startup integration credential files on each reload.
    pub fn with_integration_credentials(
        mut self,
        credentials: crate::FileCredentialAccess,
    ) -> Self {
        self.integration_credentials = credentials;
        self
    }

    fn validate_runtime(&self, catalogs: &ConfigurationCatalogs) -> Result<(), ReloadResult> {
        catalogs
            .models
            .validate_credential_files()
            .and_then(|()| self.integration_credentials.validate())
            .map_err(|error| failure(ReloadPhase::Validate, &error.to_string()))?;
        let reference = signalbox_model_runtime::CredentialReference::new(
            signalbox_tools_code_host::CODE_HOST_CREDENTIAL_REFERENCE,
        );
        let github = match catalogs
            .models
            .github_credential_profile(reference.as_str())
        {
            Some(profile) => Some(crate::FileCredentialAccess::from_github(profile, reference)),
            None => self
                .github_tool_credential
                .as_ref()
                .map(|path| crate::FileCredentialAccess::new(path.clone(), reference)),
        };
        if let Some(github) = github {
            github
                .validate()
                .map_err(|error| failure(ReloadPhase::Validate, &error.to_string()))?;
        }
        if let Some(tool_credential) = &self.github_tool_credential
            && catalogs
                .models
                .github_tool_credential_conflicts(tool_credential)
        {
            return Err(failure(
                ReloadPhase::Validate,
                "repository-watch credential conflicts with GitHub tool credential",
            ));
        }
        if let Some(factory) = &self.runtime_factory {
            factory
                .build(&catalogs.models)
                .map_err(|_| failure(ReloadPhase::Validate, "model runtime composition failed"))?;
        }
        Ok(())
    }

    /// Attaches the idle repository-watch supervisor before startup recovery.
    pub fn with_repository_watch(mut self, watch: RepositoryWatchRuntime) -> Self {
        self.watch = Some(watch);
        self
    }

    /// Delivers pending snapshots before ordinary on-disk rule activation or client admission.
    pub async fn recover(&self) -> Result<(), ReloadRepositoryError> {
        let _serial = self.serial.lock().await;
        let pending = self.repository.pending().await?;
        if pending.is_empty() {
            if let Some(watch) = &self.watch {
                let catalogs = self.catalogs();
                let prepared = watch.prepare_reload(catalogs.clone()).await.map_err(|_| {
                    ReloadRepositoryError::Corruption("reload worker preparation failed")
                })?;
                watch
                    .activate_startup(catalogs.models.repository_watch())
                    .await
                    .map_err(|_| {
                        ReloadRepositoryError::Corruption("startup rule activation failed")
                    })?;
                let restored = self.reconcile(&catalogs).await?;
                watch.install_reload(prepared).await.map_err(|_| {
                    ReloadRepositoryError::Corruption("reload worker installation failed")
                })?;
                watch.nudge_restored(restored).await;
            }
        } else {
            for (request, intent) in pending {
                let replacement = self.restore(&intent.replacement_snapshot).map_err(|_| {
                    ReloadRepositoryError::Corruption(
                        "retained reload is incompatible with startup configuration",
                    )
                })?;
                self.deliver(request, &intent, replacement, true).await?;
            }
        }
        Ok(())
    }

    fn restore(&self, snapshot: &str) -> Result<ConfigurationCatalogs, ReloadResult> {
        let catalogs = Self::startup_snapshot(self.catalogs().models.source(), snapshot)?;
        self.validate_runtime(&catalogs)?;
        Ok(catalogs)
    }

    /// Validates retained catalogs with only the startup-only members of the on-disk document.
    pub fn startup_snapshot(
        on_disk: &str,
        snapshot: &str,
    ) -> Result<ConfigurationCatalogs, ReloadResult> {
        let on_disk_models = HubModelConfiguration::parse(on_disk)
            .map_err(|error| failure(ReloadPhase::Validate, &error.to_string()))?;
        let snapshot: RetainedSnapshot = serde_json::from_str(snapshot)
            .map_err(|_| failure(ReloadPhase::Validate, "retained snapshot cannot be decoded"))?;
        let mut source = on_disk
            .parse::<toml_edit::DocumentMut>()
            .map_err(|_| failure(ReloadPhase::Validate, "startup source is invalid"))?;
        source
            .as_table_mut()
            .retain(|key, _| !RELOADABLE_KEYS.contains(&key));
        let reloadable = snapshot
            .model_catalog
            .parse::<toml_edit::DocumentMut>()
            .map_err(|_| failure(ReloadPhase::Validate, "retained model snapshot is invalid"))?;
        for (key, value) in reloadable.iter() {
            if !RELOADABLE_KEYS.contains(&key) {
                return Err(failure(
                    ReloadPhase::Validate,
                    "snapshot contains startup-only configuration",
                ));
            }
            source.insert(key, value.clone());
        }
        let models = HubModelConfiguration::parse(&source.to_string())
            .map_err(|error| failure(ReloadPhase::Validate, &error.to_string()))?;
        if startup_sections(&models)? != startup_sections(&on_disk_models)? {
            return Err(failure(
                ReloadPhase::Validate,
                "startup-only configuration differs",
            ));
        }
        let templates =
            SessionTemplateConfiguration::parse_snapshot(&snapshot.session_templates, &models)
                .map_err(|error| failure(ReloadPhase::Validate, &error.to_string()))?;
        let catalogs = ConfigurationCatalogs {
            models: Arc::new(models),
            templates: Arc::new(templates),
        };
        validate_catalogs(&catalogs)?;
        Ok(catalogs)
    }

    async fn reconcile(
        &self,
        catalogs: &ConfigurationCatalogs,
    ) -> Result<Vec<signalbox_domain::SessionId>, ReloadRepositoryError> {
        let targets = catalogs
            .models
            .repository_watch()
            .filter(|watch| watch.enabled() && watch.convergence_sweep().is_some())
            .into_iter()
            .flat_map(|watch| watch.repositories())
            .flat_map(|repository| {
                repository
                    .convergence_pull_requests()
                    .iter()
                    .map(|number| (repository.repository().clone(), *number))
            })
            .collect::<Vec<_>>();
        self.convergence
            .reconcile_configured_targets(&targets)
            .await
            .map_err(|_| {
                ReloadRepositoryError::Corruption("reload convergence reconciliation failed")
            })
    }

    async fn deliver(
        &self,
        request: ReloadConfiguration,
        intent: &ReloadIntent,
        mut replacement: ConfigurationCatalogs,
        recovering: bool,
    ) -> Result<ReloadLookup, ReloadRepositoryError> {
        let prior = self.restore(&intent.prior_snapshot).map_err(|_| {
            ReloadRepositoryError::Corruption("prior reload snapshot cannot be restored")
        })?;
        let changed_profiles = changed_codex_homes(&prior.models, &replacement.models);
        Arc::make_mut(&mut replacement.models).reuse_github_credentials(&self.catalogs().models);
        let prepared_watch = if let Some(watch) = &self.watch {
            let prepared = match watch.prepare_reload(replacement.clone()).await {
                Ok(prepared) => prepared,
                Err(_) if recovering => {
                    return Err(ReloadRepositoryError::Corruption(
                        "reload worker preparation failed",
                    ));
                }
                Err(_) => {
                    let result = failure(
                        ReloadPhase::Install,
                        "repository-watch listener or worker preparation failed",
                    );
                    self.repository.finish(request, &result).await?;
                    return Ok(ReloadLookup::Recorded(result));
                }
            };
            let configuration = replacement
                .models
                .repository_watch()
                .filter(|watch| watch.enabled());
            let sets = configuration
                .into_iter()
                .flat_map(|watch| {
                    watch.repositories().iter().map(move |repository| {
                        RepositoryRuleSet::new(repository.repository(), watch.rules())
                    })
                })
                .collect::<Vec<_>>();
            let activation = watch
                .activate_reload(ReloadIntentInput {
                    command_id: request.command_id,
                    repositories: &sets,
                    rule_set_digest: intent.rule_set_digest,
                })
                .await
                .map_err(|_| ReloadRepositoryError::Corruption("reload rule activation failed"))?;
            if !matches!(activation, RuleReconciliationAdmission::Applied { .. }) {
                drop(prepared);
                let refusal = failure(
                    ReloadPhase::Activate,
                    "repository-watch rule revision was rejected",
                );
                let prior = match self.restore(&intent.prior_snapshot) {
                    Ok(prior) => prior,
                    Err(_) => {
                        self.repository.finish(request, &refusal).await?;
                        return Err(ReloadRepositoryError::Corruption(
                            "prior reload snapshot is incompatible with startup configuration",
                        ));
                    }
                };
                let prepared = watch.prepare_reload(prior.clone()).await.map_err(|_| {
                    ReloadRepositoryError::Corruption("prior reload worker preparation failed")
                })?;
                *self
                    .current
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = prior.clone();
                let restored = self.reconcile(&prior).await?;
                watch.install_reload(prepared).await.map_err(|_| {
                    ReloadRepositoryError::Corruption("reload worker installation failed")
                })?;
                watch.nudge_restored(restored).await;
                self.repository.finish(request, &refusal).await?;
                return Ok(ReloadLookup::Recorded(refusal));
            }
            Some((watch, prepared))
        } else {
            None
        };
        *self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = replacement.clone();
        if let Some((watch, prepared)) = prepared_watch {
            let restored = self.reconcile(&replacement).await?;
            watch.install_reload(prepared).await.map_err(|_| {
                ReloadRepositoryError::Corruption("reload worker installation failed")
            })?;
            watch.nudge_restored(restored).await;
        }
        self.repository
            .finish_profile_reload(request, &changed_profiles)
            .await?;
        Ok(ReloadLookup::Recorded(ReloadResult::Reloaded))
    }

    /// Clones the complete pair under one short read lock.
    pub fn catalogs(&self) -> ConfigurationCatalogs {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Inspects replay before reading files, then holds serial admission through settlement.
    pub async fn reload(
        &self,
        request: ReloadConfiguration,
    ) -> Result<ReloadLookup, ConfigurationReloadError> {
        let found = self.repository.lookup(request).await?;
        if found != ReloadLookup::Unclaimed {
            return Ok(found);
        }
        let _serial = self.serial.lock().await;
        let found = self.repository.lookup(request).await?;
        if found != ReloadLookup::Unclaimed {
            return Ok(found);
        }
        // A failed install must be recovered before another replacement can overtake its intent.
        if !self.repository.pending().await?.is_empty() {
            return Ok(ReloadLookup::Pending);
        }
        let replacement = self.read_replacement();
        let (replacement, intent) = match replacement.and_then(|replacement| {
            let prior = self.catalogs().retained()?;
            let retained = replacement.retained()?;
            let watch = replacement
                .models
                .repository_watch()
                .filter(|watch| watch.enabled());
            let sets = watch
                .into_iter()
                .flat_map(|watch| {
                    watch.repositories().iter().map(move |repository| {
                        RepositoryRuleSet::new(repository.repository(), watch.rules())
                    })
                })
                .collect::<Vec<_>>();
            let digest = repository_rule_set_digest(&sets)
                .map_err(|_| failure(ReloadPhase::Validate, "rule snapshot cannot be encoded"))?;
            let intent = ReloadIntent {
                replacement_snapshot: serde_json::to_string(&retained).map_err(|_| {
                    failure(
                        ReloadPhase::Validate,
                        "replacement snapshot cannot be encoded",
                    )
                })?,
                prior_snapshot: serde_json::to_string(&prior).map_err(|_| {
                    failure(ReloadPhase::Validate, "prior snapshot cannot be encoded")
                })?,
                rule_set_digest: digest,
            };
            Ok((replacement, intent))
        }) {
            Ok(value) => value,
            Err(result) => {
                return match self.repository.claim(request, Err(&result)).await? {
                    ReloadClaim::Settled(outcome) => Ok(outcome),
                    ReloadClaim::Retained => Err(ConfigurationReloadError::RecoveryRequired(
                        ReloadRepositoryError::Corruption("rejection retained an install intent"),
                    )),
                };
            }
        };
        let claimed = self
            .repository
            .claim(request, Ok(&intent))
            .await
            .map_err(|error| {
                if matches!(error, ReloadRepositoryError::CommitAmbiguous(_)) {
                    ConfigurationReloadError::RecoveryRequired(error)
                } else {
                    error.into()
                }
            })?;
        if let ReloadClaim::Settled(outcome) = claimed {
            return Ok(outcome);
        }
        self.deliver(request, &intent, replacement, false)
            .await
            .map_err(ConfigurationReloadError::RecoveryRequired)
    }

    fn read_replacement(&self) -> Result<ConfigurationCatalogs, ReloadResult> {
        let models = HubModelConfiguration::read(&self.model_path).map_err(|error| {
            let phase = if matches!(error, crate::HubModelConfigurationError::Read) {
                ReloadPhase::Read
            } else {
                ReloadPhase::Validate
            };
            failure(phase, &error.to_string())
        })?;
        if startup_sections(&models)? != self.startup {
            return Err(failure(
                ReloadPhase::Validate,
                "startup-only configuration differs",
            ));
        }
        if self.watch.is_none()
            && models.repository_watch() != self.catalogs().models.repository_watch()
        {
            return Err(failure(
                ReloadPhase::Validate,
                "repository-watch configuration changes require restart",
            ));
        }
        let templates =
            SessionTemplateConfiguration::read(&self.template_path, || self.home.clone(), &models)
                .map_err(|error| {
                    let phase = if matches!(
                        error,
                        crate::SessionTemplateConfigurationError::ReadCatalog
                            | crate::SessionTemplateConfigurationError::ReadPrompt
                    ) {
                        ReloadPhase::Read
                    } else {
                        ReloadPhase::Validate
                    };
                    failure(phase, &error.to_string())
                })?;
        let catalogs = ConfigurationCatalogs {
            models: Arc::new(models),
            templates: Arc::new(templates),
        };
        validate_catalogs(&catalogs)?;
        self.validate_runtime(&catalogs)?;
        let current = self.catalogs();
        if self.watch.is_none()
            && current
                .models
                .repository_watch()
                .is_some_and(|watch| watch.enabled())
            && current.retained()? != catalogs.retained()?
        {
            return Err(failure(
                ReloadPhase::Validate,
                "catalog changes used by repository watch require activation",
            ));
        }
        Ok(catalogs)
    }
}

pub(crate) fn validate_catalogs(catalogs: &ConfigurationCatalogs) -> Result<(), ReloadResult> {
    let models = &catalogs.models;
    let templates = &catalogs.templates;
    if let Some(watch) = models.repository_watch() {
        watch
            .validate_convergence_template(templates.summaries().map(|(name, _)| name))
            .map_err(|_| failure(ReloadPhase::Validate, "convergence template is unavailable"))?;
        if watch.enabled()
            && watch
                .rules()
                .iter()
                .flat_map(|rule| rule.actions())
                .any(|action| templates.resolve(action.template()).is_none())
        {
            return Err(failure(
                ReloadPhase::Validate,
                "repository-watch rule template is unavailable",
            ));
        }
    }
    Ok(())
}

fn startup_sections(models: &HubModelConfiguration) -> Result<toml::Table, ReloadResult> {
    let mut source: toml::Table = toml::from_str(models.source())
        .map_err(|_| failure(ReloadPhase::Validate, "model source is invalid"))?;
    source.retain(|key, _| key == "credential_profiles" || !RELOADABLE_KEYS.contains(&key));
    if let Some(profiles) = source
        .get_mut("credential_profiles")
        .and_then(toml::Value::as_array_mut)
    {
        for profile in profiles {
            if profile.get("delivery").and_then(toml::Value::as_str) == Some("codex_home")
                && let Some(profile) = profile.as_table_mut()
            {
                profile.remove("codex_home");
            }
        }
    }
    Ok(source)
}

fn changed_codex_homes(
    prior: &HubModelConfiguration,
    replacement: &HubModelConfiguration,
) -> Vec<String> {
    replacement
        .credential_invocation_registrations()
        .into_iter()
        .filter_map(|(name, _)| {
            use crate::credential_pools::CredentialDelivery::CodexHome;
            match (
                prior.credential_profile(&name)?.delivery(),
                replacement.credential_profile(&name)?.delivery(),
            ) {
                (CodexHome { path: old, .. }, CodexHome { path: new, .. }) if old != new => {
                    Some(name)
                }
                _ => None,
            }
        })
        .collect()
}

fn failure(phase: ReloadPhase, reason: &str) -> ReloadResult {
    let mut sanitized = String::new();
    for character in reason.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if sanitized.len() + character.len_utf8()
            > signalbox_process_protocol::MAX_CONFIGURATION_RELOAD_REASON_BYTES
        {
            break;
        }
        sanitized.push(character);
    }
    if sanitized.trim().is_empty() {
        sanitized = "configuration reload failed".to_owned();
    }
    ReloadResult::Failed {
        phase,
        reason: sanitized,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_home_replacement_survives_retained_reload_without_reloading_other_profile_fields() {
        let homes = tempfile::tempdir().expect("synthetic profile homes");
        let before_home = homes.path().join("before");
        let after_home = homes.path().join("after");
        std::fs::create_dir(&before_home).expect("initial home directory");
        std::fs::create_dir(&after_home).expect("replacement home directory");
        let before_path = before_home.to_str().expect("UTF-8 fixture path");
        let after_path = after_home.to_str().expect("UTF-8 fixture path");
        let source = crate::configuration::checked_in_example_configuration()
            .expect("checked example")
            .source()
            .replace(
                "\ndelivery = \"ambient\"\n",
                &format!("\ndelivery = \"codex_home\"\ncodex_home = {before_path:?}\n"),
            );
        let replacement = source.replace(before_path, after_path);
        let before = HubModelConfiguration::parse(&source).expect("initial home catalog");
        let after = HubModelConfiguration::parse(&replacement).expect("replacement home catalog");
        assert_eq!(startup_sections(&before), startup_sections(&after));
        assert_eq!(changed_codex_homes(&before, &after), ["codex-ambient"]);
        assert!(changed_codex_homes(&after, &after).is_empty());
        let equivalent_path = homes.path().join("unused/../before");
        let equivalent = source.replace(before_path, equivalent_path.to_str().unwrap());
        let equivalent =
            HubModelConfiguration::parse(&equivalent).expect("equivalent normalized home");
        assert!(changed_codex_homes(&before, &equivalent).is_empty());
        let identity_change = replacement.replace("codex-ambient", "another-codex-profile");
        let identity_change =
            HubModelConfiguration::parse(&identity_change).expect("other profile");
        assert_ne!(
            startup_sections(&before),
            startup_sections(&identity_change)
        );
        let snapshot = ConfigurationCatalogs {
            models: Arc::new(after),
            templates: Arc::new(
                SessionTemplateConfiguration::parse_snapshot("version = 1", &before)
                    .expect("empty templates"),
            ),
        }
        .retained()
        .expect("retained replacement");
        let restored = ConfigurationReload::startup_snapshot(
            &source,
            &serde_json::to_string(&snapshot).unwrap(),
        )
        .expect("home change restores from durable intent");
        assert_eq!(
            changed_codex_homes(&before, &restored.models),
            ["codex-ambient"]
        );
    }

    #[test]
    fn failure_reasons_are_bounded_utf8_without_control_characters() {
        let oversized = format!(
            "\n{}",
            "é".repeat(signalbox_process_protocol::MAX_CONFIGURATION_RELOAD_REASON_BYTES)
        );
        for input in [oversized.as_str(), "", "\n\t\0"] {
            let ReloadResult::Failed { reason, .. } = failure(ReloadPhase::Validate, input) else {
                panic!("failure result");
            };
            assert!(!reason.trim().is_empty());
            assert!(
                reason.len() <= signalbox_process_protocol::MAX_CONFIGURATION_RELOAD_REASON_BYTES
            );
            assert!(!reason.chars().any(char::is_control));
            if input == oversized {
                assert!(reason.ends_with('é'));
            }
        }
    }

    fn fixture() -> (tempfile::TempDir, ConfigurationReload) {
        let directory = tempfile::tempdir().expect("fixture directory");
        let models =
            crate::configuration::checked_in_example_configuration().expect("example models");
        let mut source = models.source().to_owned();
        for (reference, path) in
            models.file_credential_profiles(crate::configuration::ModelAdapter::Anthropic)
        {
            let file = tempfile::NamedTempFile::new_in(directory.path())
                .expect("private model credential");
            let credential_path = directory.path().join(reference);
            file.persist(&credential_path).expect("retain credential");
            source = source.replace(
                path.to_str().expect("configured path"),
                credential_path.to_str().expect("fixture path"),
            );
        }
        let models = HubModelConfiguration::parse(&source).expect("fixture model credentials");
        let model_path = directory.path().join("models.toml");
        let template_path = directory.path().join("templates.toml");
        std::fs::write(&model_path, models.source()).expect("model file");
        std::fs::write(&template_path, "version = 1\n").expect("template file");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@localhost/unused")
            .expect("lazy pool");
        let reload = ConfigurationReload::new(
            pool,
            models,
            SessionTemplateConfiguration::default(),
            model_path,
            template_path,
            None,
        )
        .expect("reload composition");
        (directory, reload)
    }

    #[tokio::test]
    async fn reload_accepts_a_model_credential_with_permissive_mode() {
        let (directory, reload) = fixture();
        reload
            .read_replacement()
            .expect("initial credentials admitted");
        std::fs::set_permissions(
            directory.path().join("anthropic-overflow"),
            std::os::unix::fs::PermissionsExt::from_mode(0o644),
        )
        .expect("make unused profile public");
        reload
            .read_replacement()
            .expect("reload warns and reads the permissive credential");
    }

    #[tokio::test]
    async fn reload_rechecks_the_environment_github_token() {
        let (directory, reload) = fixture();
        let mut document = reload
            .catalogs()
            .models
            .source()
            .parse::<toml_edit::DocumentMut>()
            .expect("catalog");
        document["credential_profiles"]
            .as_array_of_tables_mut()
            .expect("profiles")
            .retain(|profile| {
                profile.get("adapter").and_then(toml_edit::Item::as_str) != Some("github")
            });
        std::fs::write(&reload.model_path, document.to_string()).expect("fallback catalog");
        let reload = ConfigurationReload::new(
            sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://unused:unused@localhost/unused")
                .expect("lazy pool"),
            HubModelConfiguration::parse(&document.to_string()).expect("fallback models"),
            SessionTemplateConfiguration::default(),
            reload.model_path,
            reload.template_path,
            None,
        )
        .expect("fallback startup configuration");
        let credential =
            tempfile::NamedTempFile::new_in(directory.path()).expect("private fallback token");
        let reload = reload.with_github_tool_credential(credential.path().to_path_buf());
        reload.read_replacement().expect("valid fallback admitted");
        credential
            .as_file()
            .set_len(65_537)
            .expect("oversize fallback");
        assert_eq!(
            reload
                .read_replacement()
                .expect_err("oversize fallback rejected"),
            failure(
                ReloadPhase::Validate,
                "credential reference `github-primary` could not be resolved: TooLarge"
            )
        );
        credential.as_file().set_len(0).expect("restore file size");
        std::fs::set_permissions(
            credential.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o644),
        )
        .expect("weaken permissions");
        assert_eq!(
            reload
                .read_replacement()
                .expect_err("public fallback rejected"),
            failure(
                ReloadPhase::Validate,
                "credential reference `github-primary` could not be resolved: InsecurePermissions"
            )
        );
        credential.close().expect("remove fallback");
        assert_eq!(
            reload
                .read_replacement()
                .expect_err("missing fallback rejected"),
            failure(
                ReloadPhase::Validate,
                "credential reference `github-primary` could not be resolved: Unavailable"
            )
        );
    }

    #[tokio::test]
    async fn app_profile_reload_does_not_admit_the_unused_environment_token() {
        let (directory, reload) = fixture();
        let reload = reload.with_github_tool_credential(directory.path().join("missing-fallback"));
        reload
            .read_replacement()
            .expect("App profile does not use the missing fallback or read its key at reload");
    }

    #[tokio::test]
    async fn reload_rechecks_integration_credential_files() {
        let (directory, reload) = fixture();
        let credential = tempfile::NamedTempFile::new_in(directory.path())
            .expect("private integration credential");
        let reference = signalbox_model_runtime::CredentialReference::new(
            signalbox_tools_web::BRAVE_SEARCH_CREDENTIAL_REFERENCE,
        );
        let reload = reload.with_integration_credentials(crate::FileCredentialAccess::new(
            credential.path().to_path_buf(),
            reference.clone(),
        ));
        reload
            .read_replacement()
            .expect("initial credentials admitted");
        credential
            .as_file()
            .set_len(65_537)
            .expect("oversized integration credential");
        assert_eq!(
            reload
                .read_replacement()
                .expect_err("reload rejects oversized credential"),
            failure(
                ReloadPhase::Validate,
                &format!("credential reference `{reference}` could not be resolved: TooLarge")
            )
        );
    }

    #[tokio::test]
    async fn lookup_database_failure_does_not_require_recovery() {
        let (_directory, mut reload) = fixture();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@localhost/unused")
            .expect("lazy pool");
        pool.close().await;
        reload.repository = ReloadConfigurationRepository::new(pool);
        let error = reload
            .reload(ReloadConfiguration {
                command_id: signalbox_domain::DurableCommandId::from_uuid(uuid::Uuid::now_v7()),
            })
            .await
            .expect_err("closed pool rejects lookup");
        assert!(matches!(
            error,
            ConfigurationReloadError::BeforeEffect(ReloadRepositoryError::Database(
                sqlx::Error::PoolClosed
            ))
        ));
    }

    #[tokio::test]
    async fn replacement_accepts_catalog_edits_but_refuses_startup_changes() {
        let (_directory, reload) = fixture();
        let mut document = reload
            .catalogs()
            .models
            .source()
            .parse::<toml_edit::DocumentMut>()
            .expect("source");
        document.remove("aliases");
        std::fs::write(&reload.model_path, document.to_string()).expect("replacement");
        let replacement = reload
            .read_replacement()
            .expect("catalog edit is reloadable");
        assert_eq!(replacement.models.model_aliases().count(), 0);
        assert!(reload.catalogs().models.model_aliases().count() > 0);
        document
            .get_mut("compaction")
            .expect("compaction")
            .as_table_mut()
            .expect("table")
            .insert("prompt", toml_edit::value("changed startup prompt"));
        std::fs::write(&reload.model_path, document.to_string()).expect("replacement");
        assert_eq!(
            reload.read_replacement().expect_err("startup changes fail"),
            failure(ReloadPhase::Validate, "startup-only configuration differs")
        );
    }

    #[tokio::test]
    async fn replacement_refuses_repository_watch_edits_before_installation() {
        let (_directory, reload) = fixture();
        // Repository identity, interval, and unread credential path are arbitrary fixture data.
        let source = format!(
            r#"{}
[repository_watch]
version = 1
enabled = false
signal_reviewers = []
[[repository_watch.repositories]]
repository = "example/reload"
poll_interval_seconds = 60
credential_file = "/unused/reload-token"
"#,
            reload.catalogs().models.source()
        );
        HubModelConfiguration::parse(&source).expect("valid repository-watch replacement");
        std::fs::write(&reload.model_path, source).expect("replacement");
        assert_eq!(
            reload
                .read_replacement()
                .expect_err("watch edits require activation"),
            failure(
                ReloadPhase::Validate,
                "repository-watch configuration changes require restart"
            ),
        );
        assert!(reload.catalogs().models.repository_watch().is_none());
    }

    fn fixture_with_repository_watch() -> (tempfile::TempDir, ConfigurationReload) {
        let (directory, reload) = fixture();
        let credential =
            tempfile::NamedTempFile::new_in(directory.path()).expect("private polling credential");
        let credential_path = directory.path().join("poll-token");
        credential
            .persist(&credential_path)
            .expect("retain polling credential");
        let source = format!(
            r#"{}
[repository_watch]
version = 1
enabled = true
signal_reviewers = []
[[repository_watch.repositories]]
repository = "example/reload"
poll_interval_seconds = 60
credential_file = "{}"
"#,
            reload.catalogs().models.source(),
            credential_path.display()
        );
        reload.current.write().expect("catalog lock").models =
            Arc::new(HubModelConfiguration::parse(&source).expect("watch configuration"));
        std::fs::write(&reload.model_path, source).expect("model file");
        reload
            .read_replacement()
            .expect("unchanged catalogs are allowed");
        (directory, reload)
    }

    #[tokio::test]
    async fn active_watch_refuses_model_edits_without_activation() {
        let (_directory, reload) = fixture_with_repository_watch();
        let mut source = reload
            .catalogs()
            .models
            .source()
            .parse::<toml_edit::DocumentMut>()
            .expect("source");
        source.remove("aliases");
        std::fs::write(&reload.model_path, source.to_string()).expect("model edit");
        assert_eq!(
            reload
                .read_replacement()
                .expect_err("watch models need activation"),
            failure(
                ReloadPhase::Validate,
                "catalog changes used by repository watch require activation"
            )
        );
    }

    #[tokio::test]
    async fn active_watch_refuses_template_edits_without_activation() {
        let (directory, reload) = fixture_with_repository_watch();
        let alias = reload
            .catalogs()
            .models
            .model_aliases()
            .next()
            .expect("alias")
            .0;
        std::fs::write(directory.path().join("watch-prompt.txt"), "watch prompt").expect("prompt");
        std::fs::write(&reload.template_path, format!("version = 1\n[[templates]]\nname = \"watch-template\"\nversion = 1\nalias = \"{}\"\nsystem_prompt_file = \"watch-prompt.txt\"\ndangerous_tool_auto_approval = false\n", alias.as_uuid())).expect("template edit");
        assert_eq!(
            reload
                .read_replacement()
                .expect_err("watch templates need activation"),
            failure(
                ReloadPhase::Validate,
                "catalog changes used by repository watch require activation"
            )
        );
    }

    #[tokio::test]
    async fn retained_templates_survive_prompt_file_changes() {
        let (directory, reload) = fixture();
        let alias = reload
            .catalogs()
            .models
            .model_aliases()
            .next()
            .expect("example alias")
            .0;
        let prompt_path = directory.path().join("prompt.txt");
        std::fs::write(&prompt_path, "accepted prompt").expect("prompt");
        std::fs::write(&reload.template_path, format!("version = 1\n[[templates]]\nname = \"reload-test\"\nversion = 1\nalias = \"{}\"\nsystem_prompt_file = \"prompt.txt\"\ndangerous_tool_auto_approval = false\n", alias.as_uuid())).expect("template");
        let replacement = reload.read_replacement().expect("replacement with prompt");
        let retained = replacement.retained().expect("retained snapshot");
        std::fs::write(&prompt_path, "changed prompt").expect("mutated source");
        let templates = SessionTemplateConfiguration::parse_snapshot(
            &retained.session_templates,
            &replacement.models,
        )
        .expect("retained templates");
        let name =
            signalbox_domain::SessionTemplateName::try_new("reload-test".to_owned()).expect("name");
        assert_eq!(
            templates
                .resolve(&name)
                .expect("template")
                .defaults()
                .system_prompt()
                .expect("prompt")
                .as_str(),
            "accepted prompt"
        );
        assert!(!retained.session_templates.contains("system_prompt_file"));
    }
    #[tokio::test]
    async fn startup_uses_retained_catalogs_and_rejects_invalid_startup_members() {
        let (_directory, reload) = fixture();
        let original = reload.catalogs();
        let snapshot =
            serde_json::to_string(&original.retained().expect("snapshot")).expect("JSON");
        let mut on_disk = original
            .models
            .source()
            .parse::<toml_edit::DocumentMut>()
            .expect("source");
        on_disk.remove("models");
        assert!(HubModelConfiguration::parse(&on_disk.to_string()).is_err());
        let restored = ConfigurationReload::startup_snapshot(&on_disk.to_string(), &snapshot)
            .expect("retained models supersede the changed catalog");
        assert_eq!(
            restored.models.model_aliases().count(),
            original.models.model_aliases().count()
        );
        on_disk.remove("version");
        assert!(ConfigurationReload::startup_snapshot(&on_disk.to_string(), &snapshot).is_err());
    }
}
