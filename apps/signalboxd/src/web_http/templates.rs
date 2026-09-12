//! Browser views of the accepted reloadable session-template catalog.

use axum::{
    Extension, Json,
    extract::Path,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use signalbox_domain::{
    DangerousToolAutoApproval, ModelSelectionRequest, SessionTemplateName, ToolApprovalPosture,
};
use signalbox_tools_workflows::RegistrationNames;
use signalbox_web_contract::{
    WebModelSelection, WebPositiveU64, WebTemplateApprovalPosture, WebTemplateDetail,
    WebTemplateList, WebTemplateSourceKind, WebTemplateSummary, WebTemplateWorkflowGrant,
    WebTemplateWorkflowTool,
};
use toml_edit::{ArrayOfTables, DocumentMut, Item};

use super::application_error;
use crate::{
    ResolvedSessionTemplate,
    configuration_reload::{ConfigurationCatalogs, ConfigurationReload},
};

pub(super) async fn list(reload: Option<Extension<ConfigurationReload>>) -> Response {
    let Some(Extension(reload)) = reload else {
        return unavailable();
    };
    let catalogs = reload.catalogs();
    let templates = catalogs
        .templates
        .summaries()
        .map(|(name, _)| {
            catalogs
                .templates
                .resolve(name)
                .and_then(|template| summary(&catalogs, template))
        })
        .collect::<Option<Vec<_>>>();
    match templates {
        Some(templates) => Json(WebTemplateList { templates }).into_response(),
        None => unavailable(),
    }
}

pub(super) async fn detail(
    reload: Option<Extension<ConfigurationReload>>,
    Path(name): Path<String>,
) -> Response {
    let Some(Extension(reload)) = reload else {
        return unavailable();
    };
    let Ok(name) = SessionTemplateName::try_new(name) else {
        return application_error(
            StatusCode::BAD_REQUEST,
            "invalid_template_name",
            "Template name is invalid",
        );
    };
    let catalogs = reload.catalogs();
    let Some(template) = catalogs.templates.resolve(&name) else {
        return application_error(
            StatusCode::NOT_FOUND,
            "template_not_found",
            "Template was not found",
        );
    };
    match detail_dto(&catalogs, template) {
        Some(detail) => Json(detail).into_response(),
        None => unavailable(),
    }
}

fn unavailable() -> Response {
    application_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "templates_unavailable",
        "Templates are unavailable",
    )
}

fn summary(
    catalogs: &ConfigurationCatalogs,
    template: &ResolvedSessionTemplate,
) -> Option<WebTemplateSummary> {
    let defaults = template.defaults();
    let model = match defaults.model() {
        ModelSelectionRequest::Direct(selection) => WebModelSelection::Direct {
            selection_id: selection.as_uuid().to_string(),
        },
        ModelSelectionRequest::Alias(alias) => WebModelSelection::Alias {
            alias_id: alias.as_uuid().to_string(),
        },
    };
    let route = catalogs
        .models
        .resolve_session_model(defaults.model())
        .ok()?;
    let workflow_tools = template
        .workflow_tools()
        .0
        .iter()
        .map(|(operation, policy)| {
            let grant = match &policy.names {
                Some(RegistrationNames::All(_)) => WebTemplateWorkflowGrant::AllRegistrations,
                Some(RegistrationNames::Names(names)) => WebTemplateWorkflowGrant::Registrations {
                    names: names.clone(),
                },
                None => WebTemplateWorkflowGrant::Enabled {
                    enabled: policy.enabled == Some(true),
                },
            };
            let posture = match template.workflow_tools().posture(*operation) {
                ToolApprovalPosture::Auto => WebTemplateApprovalPosture::Auto,
                ToolApprovalPosture::Delegated => WebTemplateApprovalPosture::Delegated,
                ToolApprovalPosture::Human => WebTemplateApprovalPosture::Human,
            };
            WebTemplateWorkflowTool {
                name: operation.name().to_owned(),
                grant,
                posture,
            }
        })
        .collect();
    Some(WebTemplateSummary {
        name: template.provenance().name().as_str().to_owned(),
        digest: hex::encode(template.provenance().content_digest().as_bytes()),
        version: WebPositiveU64::from_nonzero(std::num::NonZeroU64::new(
            template.version().as_u64(),
        )?),
        model,
        model_label: catalogs
            .models
            .runtime_model_catalog()
            .resolve(route.target())?
            .provider_model()
            .to_owned(),
        dangerous_tool_auto_approval: defaults.dangerous_tool_auto_approval()
            == DangerousToolAutoApproval::ApproveAll,
        workflow_tools,
    })
}

fn detail_dto(
    catalogs: &ConfigurationCatalogs,
    template: &ResolvedSessionTemplate,
) -> Option<WebTemplateDetail> {
    let source = catalogs.templates.source().parse::<DocumentMut>().ok()?;
    let mut definition = DocumentMut::new();
    definition.insert("version", toml_edit::value(1));
    let table = source
        .get("templates")
        .and_then(Item::as_array_of_tables)
        .and_then(|tables| {
            tables.iter().find(|table| {
                table.get("name").and_then(Item::as_str)
                    == Some(template.provenance().name().as_str())
            })
        });
    let source_kind = if let Some(table) = table {
        let mut tables = ArrayOfTables::new();
        tables.push(table.clone());
        definition.insert("templates", Item::ArrayOfTables(tables));
        WebTemplateSourceKind::Template
    } else {
        definition.insert("review_library", source.get("review_library")?.clone());
        WebTemplateSourceKind::ReviewLibrary
    };
    Some(WebTemplateDetail {
        summary: summary(catalogs, template)?,
        system_prompt: template.defaults().system_prompt()?.as_str().to_owned(),
        source_kind,
        definition_toml: definition.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HubModelConfiguration, SessionTemplateConfiguration};
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt as _;
    use std::{path::PathBuf, sync::Arc};
    use tower::ServiceExt as _;

    // Arbitrary stable identities for the synthetic model catalog.
    const SELECTION_ID: &str = "10000000-0000-4000-8000-000000000001";
    const TARGET_ID: &str = "20000000-0000-4000-8000-000000000002";
    const ALIAS_ID: &str = "30000000-0000-4000-8000-000000000003";
    const PROMPT: &str = "Inspect the change.";

    fn models() -> HubModelConfiguration {
        HubModelConfiguration::parse_test_fixture(&format!(
            r#"
version = 1

[[credential_profiles]]
name = "anthropic-primary"
adapter = "anthropic"
billing_kind = "api_metered"
delivery = "file"
file = "/run/secrets/anthropic-primary"

[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{{ profile = "anthropic-primary", priority = 1 }}]


[[adapter_mappings]]
model_family = "anthropic"
adapter = "anthropic"
credential_pool = "anthropic-main"

[compaction]
prompt = "Summarize the prior conversation faithfully for continuation."

[[models]]
selection_id = "{SELECTION_ID}"
target_id = "{TARGET_ID}"
model_family = "anthropic"
provider_model = "synthetic-model"
max_output_tokens = 1024
context_window_tokens = 200000

[[aliases]]
alias_id = "{ALIAS_ID}"
selection_id = "{SELECTION_ID}"
"#,
        ))
        .expect("synthetic model fixture is valid")
    }

    fn catalogs() -> ConfigurationCatalogs {
        let models = models();
        let templates = SessionTemplateConfiguration::parse_snapshot(
            &format!(
                r#"
version = 1
[[templates]]
name = "reviewer"
version = 7
alias = "{ALIAS_ID}"
system_prompt = "{PROMPT}"
dangerous_tool_auto_approval = false
[templates.workflow_tools.start]
names = ["build"]
posture = "human"
[templates.workflow_tools.list]
enabled = true
"#
            ),
            &models,
        )
        .expect("valid template fixture");
        ConfigurationCatalogs {
            models: Arc::new(models),
            templates: Arc::new(templates),
        }
    }

    fn router(catalogs: ConfigurationCatalogs) -> axum::Router {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@localhost/unused")
            .expect("unused pool");
        let reload = ConfigurationReload::new(
            pool,
            (*catalogs.models).clone(),
            (*catalogs.templates).clone(),
            PathBuf::from("unused-models"),
            PathBuf::from("unused-templates"),
            None,
        )
        .expect("reload fixture");
        super::super::production_router(None, None, None, None, None, None, None)
            .layer(Extension(reload))
    }

    async fn get(router: axum::Router, path: &str, host: &str) -> Response {
        router
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("host", host)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response")
    }

    #[tokio::test]
    async fn list_exposes_loaded_workflow_grants_with_effective_postures() {
        let response = get(router(catalogs()), "/api/templates", "localhost").await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let list: WebTemplateList = serde_json::from_slice(&bytes).expect("template list");
        let template = &list.templates[0];
        assert_eq!(template.name, "reviewer");
        assert_eq!(
            template.workflow_tools,
            vec![
                WebTemplateWorkflowTool {
                    name: "workflow_list".into(),
                    grant: WebTemplateWorkflowGrant::Enabled { enabled: true },
                    posture: WebTemplateApprovalPosture::Auto
                },
                WebTemplateWorkflowTool {
                    name: "workflow_start".into(),
                    grant: WebTemplateWorkflowGrant::Registrations {
                        names: vec!["build".into()]
                    },
                    posture: WebTemplateApprovalPosture::Human
                },
            ]
        );
    }

    #[tokio::test]
    async fn detail_source_reconstitutes_the_loaded_definition() {
        let catalogs = catalogs();
        let response = get(
            router(catalogs.clone()),
            "/api/templates/reviewer",
            "localhost",
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let detail: WebTemplateDetail = serde_json::from_slice(&bytes).expect("detail");
        assert_eq!(detail.system_prompt, PROMPT);
        let restored =
            SessionTemplateConfiguration::parse_snapshot(&detail.definition_toml, &catalogs.models)
                .expect("returned source loads");
        let name = SessionTemplateName::try_new(detail.summary.name).expect("name");
        assert_eq!(restored.resolve(&name), catalogs.templates.resolve(&name));
    }

    #[test]
    fn generated_template_detail_contains_its_shared_review_library() {
        let models = models();
        let source = format!(
            r#"
version = 1
[review_library]
source_version = 3
concern_set_version = "test-concerns"
alias = "{ALIAS_ID}"
dangerous_tool_auto_approval = false
shared_header = "Review this change."
import_body = "Import evidence."
judgment_body = "Judge findings."
repair_body = "Repair findings."
publication_body = "Publish results."
[review_library.concerns]
correctness = "Find defects."
"#
        );
        let templates =
            SessionTemplateConfiguration::parse_snapshot(&source, &models).expect("review library");
        let catalogs = ConfigurationCatalogs {
            models: Arc::new(models),
            templates: Arc::new(templates),
        };
        let name = SessionTemplateName::try_new("review-import".into()).expect("generated name");
        let original = catalogs
            .templates
            .resolve(&name)
            .expect("generated template");
        let detail = detail_dto(&catalogs, original).expect("generated detail");
        assert_eq!(detail.source_kind, WebTemplateSourceKind::ReviewLibrary);
        let restored =
            SessionTemplateConfiguration::parse_snapshot(&detail.definition_toml, &catalogs.models)
                .expect("shared source loads");
        assert_eq!(restored.resolve(&name), Some(original));
    }

    #[test]
    fn detail_uses_retained_prompt_contents_after_the_file_changes() {
        let models = models();
        let directory = tempfile::tempdir().expect("fixture directory");
        let prompt_path = directory.path().join("prompt.txt");
        std::fs::write(&prompt_path, PROMPT).expect("prompt");
        let catalog_path = directory.path().join("templates.toml");
        std::fs::write(
            &catalog_path,
            format!(
                r#"
version = 1
[[templates]]
name = "reviewer"
version = 1
alias = "{ALIAS_ID}"
system_prompt_file = "prompt.txt"
dangerous_tool_auto_approval = false
"#
            ),
        )
        .expect("catalog");
        let templates = SessionTemplateConfiguration::read(&catalog_path, || None, &models)
            .expect("accepted catalog");
        std::fs::remove_file(prompt_path).expect("remove external prompt");
        let catalogs = ConfigurationCatalogs {
            models: Arc::new(models),
            templates: Arc::new(templates),
        };
        let name = SessionTemplateName::try_new("reviewer".into()).expect("name");
        let original = catalogs.templates.resolve(&name).expect("template");
        let detail = detail_dto(&catalogs, original).expect("retained detail");
        assert_eq!(detail.system_prompt, PROMPT);
        let restored =
            SessionTemplateConfiguration::parse_snapshot(&detail.definition_toml, &catalogs.models)
                .expect("source needs no prompt file");
        assert_eq!(restored.resolve(&name), Some(original));
    }

    #[tokio::test]
    async fn unknown_template_returns_not_found() {
        assert_eq!(
            get(router(catalogs()), "/api/templates/missing", "localhost")
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn template_reads_reject_rebound_host() {
        assert_eq!(
            get(router(catalogs()), "/api/templates", "attacker.example")
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn absent_reload_service_does_not_claim_an_empty_catalog() {
        let response = get(
            super::super::production_router(None, None, None, None, None, None, None),
            "/api/templates",
            "localhost",
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
