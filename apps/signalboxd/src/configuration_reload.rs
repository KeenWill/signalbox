//! Serial durable reloads and atomic request-catalog snapshots for process protocol.

use std::{
    path::PathBuf,
    sync::{Arc, RwLock},
};

use serde::{Deserialize, Serialize};
use signalbox_module_repo_watch_v2::{RepositoryRuleSet, repository_rule_set_digest};
use signalbox_persistence::reload_configuration::{
    ReloadClaim, ReloadConfiguration, ReloadConfigurationRepository, ReloadIntent, ReloadLookup,
    ReloadPhase, ReloadRepositoryError, ReloadResult,
};
use tokio::sync::Mutex;

use crate::{HubModelConfiguration, SessionTemplateConfiguration};

// The design admits these catalog sections; all other model-document keys are startup-only.
const RELOADABLE_KEYS: [&str; 4] = ["models", "serving_targets", "aliases", "repository_watch"];

/// Complete immutable pair observed by one admitted request.
#[derive(Clone, Debug)]
pub struct ConfigurationCatalogs {
    /// Checked model and alias configuration.
    pub models: Arc<HubModelConfiguration>,
    /// Checked templates, including retained prompt-file contents.
    pub templates: Arc<SessionTemplateConfiguration>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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

/// One daemon-wide reload mutex and one atomically replaced catalog pair.
#[derive(Clone, Debug)]
pub struct ConfigurationReload {
    current: Arc<RwLock<ConfigurationCatalogs>>,
    serial: Arc<Mutex<()>>,
    repository: ReloadConfigurationRepository,
    model_path: PathBuf,
    template_path: PathBuf,
    home: Option<PathBuf>,
    startup: toml::Table,
    runtime_factory: Option<crate::model_catalog_runtime::ModelRuntimeFactory>,
    github_tool_credential: Option<PathBuf>,
}

impl ConfigurationReload {
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
            repository: ReloadConfigurationRepository::new(pool),
            model_path,
            template_path,
            home,
            startup,
            runtime_factory: None,
            github_tool_credential: None,
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
        self.runtime_factory.map(|factory| {
            Arc::new(
                crate::model_catalog_runtime::CatalogContextCompactionModel::new(models, factory),
            ) as _
        })
    }

    pub fn with_github_tool_credential(mut self, path: PathBuf) -> Self {
        self.github_tool_credential = Some(path);
        self
    }

    fn validate_runtime(&self, catalogs: &ConfigurationCatalogs) -> Result<(), ReloadResult> {
        if let Some(tool_credential) = &self.github_tool_credential
            && catalogs.models.repository_watch().is_some_and(|watch| {
                watch.repositories().iter().any(|repository| {
                    crate::repo_watch_credentials::credential_files_conflict(
                        tool_credential,
                        repository.credential_file(),
                    )
                })
            })
        {
            return Err(failure(
                ReloadPhase::Validate,
                "repository-watch credential conflicts with GitHub tool credential",
            ));
        }
        if let Some(factory) = self.runtime_factory {
            factory
                .build(&catalogs.models)
                .map_err(|_| failure(ReloadPhase::Validate, "model runtime composition failed"))?;
        }
        Ok(())
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
    ) -> Result<ReloadLookup, ReloadRepositoryError> {
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
                    ReloadClaim::Retained => Err(ReloadRepositoryError::Corruption(
                        "rejection retained an install intent",
                    )),
                };
            }
        };
        let claimed = self.repository.claim(request, Ok(&intent)).await?;
        if let ReloadClaim::Settled(outcome) = claimed {
            return Ok(outcome);
        }
        *self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = replacement;
        self.repository
            .finish(request, &ReloadResult::Reloaded)
            .await?;
        Ok(ReloadLookup::Recorded(ReloadResult::Reloaded))
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
        if let Some(watch) = models.repository_watch() {
            watch
                .validate_convergence_template(templates.summaries().map(|(name, _)| name))
                .map_err(|_| {
                    failure(ReloadPhase::Validate, "convergence template is unavailable")
                })?;
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
        let catalogs = ConfigurationCatalogs {
            models: Arc::new(models),
            templates: Arc::new(templates),
        };
        self.validate_runtime(&catalogs)?;
        Ok(catalogs)
    }
}

fn startup_sections(models: &HubModelConfiguration) -> Result<toml::Table, ReloadResult> {
    let mut source: toml::Table = toml::from_str(models.source())
        .map_err(|_| failure(ReloadPhase::Validate, "model source is invalid"))?;
    source.retain(|key, _| !RELOADABLE_KEYS.contains(&key));
    Ok(source)
}

fn failure(phase: ReloadPhase, reason: &str) -> ReloadResult {
    ReloadResult::Failed {
        phase,
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, ConfigurationReload) {
        let directory = tempfile::tempdir().expect("fixture directory");
        let models =
            crate::configuration::checked_in_example_configuration().expect("example models");
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
}
