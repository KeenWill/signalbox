//! Catalog declaration and effect-class properties.

use signalbox_application::ToolCatalog;
use signalbox_domain::{NormalizedToolArguments, ToolEffectClass, ToolName, ToolPermissionDefault};
use signalbox_tools_workspace::LocalWorkspaceFileSystem;

use crate::catalog::LocalGitTools;
use crate::contracts::LocalToolKind;
use crate::names::{
    GIT_BRANCH_CREATE_NAME, GIT_BRANCH_SWITCH_NAME, GIT_CREATE_COMMIT_NAME, GIT_DIFF_NAME,
    GIT_LOG_NAME, GIT_STAGE_NAME, GIT_STATUS_NAME, LOCAL_GIT_TOOL_NAMES,
};
use crate::tests::support::{FIX_BRANCH, Fixture, Sha256Fixture, identity};

#[test]
fn sha256_catalog_admits_only_full_width_sha256_object_ids() {
    let fixture = Sha256Fixture::new();
    let catalog = LocalGitTools::try_new(LocalWorkspaceFileSystem, fixture.root(), identity())
        .expect("SHA-256 suite constructs")
        .into_parts()
        .0;
    let branch_create =
        ToolName::try_new(GIT_BRANCH_CREATE_NAME.to_owned()).expect("fixture name is admitted");
    let full_width = NormalizedToolArguments::try_from_provider_text(
        serde_json::json!({"name": FIX_BRANCH, "start": fixture.initial.to_string()}).to_string(),
    )
    .expect("full-width SHA-256 arguments normalize");
    let sha1_width = NormalizedToolArguments::try_from_provider_text(
        serde_json::json!({"name": FIX_BRANCH, "start": "0".repeat(40)}).to_string(),
    )
    .expect("SHA-1-width arguments normalize");

    assert_eq!(
        catalog.validate_arguments(&branch_create, &full_width),
        Ok(())
    );
    assert!(
        catalog
            .validate_arguments(&branch_create, &sha1_width)
            .is_err()
    );
}

#[test]
fn catalog_declares_every_local_verb_auto() {
    let fixture = Fixture::new();
    let catalog = LocalGitTools::try_new(LocalWorkspaceFileSystem, fixture.root(), identity())
        .expect("suite constructs")
        .into_parts()
        .0;

    let branch_create =
        ToolName::try_new(GIT_BRANCH_CREATE_NAME.to_owned()).expect("fixture name is admitted");
    let branch_switch =
        ToolName::try_new(GIT_BRANCH_SWITCH_NAME.to_owned()).expect("fixture name is admitted");
    let commit =
        ToolName::try_new(GIT_CREATE_COMMIT_NAME.to_owned()).expect("fixture name is admitted");
    let diff = ToolName::try_new(GIT_DIFF_NAME.to_owned()).expect("fixture name is admitted");
    let log = ToolName::try_new(GIT_LOG_NAME.to_owned()).expect("fixture name is admitted");
    let stage = ToolName::try_new(GIT_STAGE_NAME.to_owned()).expect("fixture name is admitted");
    let status = ToolName::try_new(GIT_STATUS_NAME.to_owned()).expect("fixture name is admitted");

    assert_eq!(
        catalog
            .definition(&branch_create)
            .expect("definition exists")
            .permission_default(),
        ToolPermissionDefault::Auto
    );
    assert_eq!(
        catalog
            .definition(&branch_switch)
            .expect("definition exists")
            .permission_default(),
        ToolPermissionDefault::Auto
    );
    assert_eq!(
        catalog
            .definition(&commit)
            .expect("definition exists")
            .permission_default(),
        ToolPermissionDefault::Auto
    );
    assert_eq!(
        catalog
            .definition(&diff)
            .expect("definition exists")
            .permission_default(),
        ToolPermissionDefault::Auto
    );
    assert_eq!(
        catalog
            .definition(&log)
            .expect("definition exists")
            .permission_default(),
        ToolPermissionDefault::Auto
    );
    assert_eq!(
        catalog
            .definition(&stage)
            .expect("definition exists")
            .permission_default(),
        ToolPermissionDefault::Auto
    );
    assert_eq!(
        catalog
            .definition(&status)
            .expect("definition exists")
            .permission_default(),
        ToolPermissionDefault::Auto
    );
}

#[test]
fn git_diff_schema_declares_its_closed_union_as_an_object() {
    let fixture = Fixture::new();
    let catalog = LocalGitTools::try_new(LocalWorkspaceFileSystem, fixture.root(), identity())
        .expect("suite constructs")
        .into_parts()
        .0;
    let diff = ToolName::try_new(GIT_DIFF_NAME.to_owned()).expect("fixture name is admitted");
    let schema: serde_json::Value = serde_json::from_str(
        catalog
            .definition(&diff)
            .expect("definition exists")
            .input_schema()
            .as_str(),
    )
    .expect("Git diff schema is JSON");

    assert_eq!(schema["type"], serde_json::json!("object"));
    assert!(schema["oneOf"].is_array());
}

#[test]
fn local_catalog_declares_no_remote_verb() {
    assert_eq!(LOCAL_GIT_TOOL_NAMES.len(), 7);
    assert_eq!(LOCAL_GIT_TOOL_NAMES[0], GIT_BRANCH_CREATE_NAME);
    assert_eq!(LOCAL_GIT_TOOL_NAMES[1], GIT_BRANCH_SWITCH_NAME);
    assert_eq!(LOCAL_GIT_TOOL_NAMES[2], GIT_CREATE_COMMIT_NAME);
    assert_eq!(LOCAL_GIT_TOOL_NAMES[3], GIT_DIFF_NAME);
    assert_eq!(LOCAL_GIT_TOOL_NAMES[4], GIT_LOG_NAME);
    assert_eq!(LOCAL_GIT_TOOL_NAMES[5], GIT_STAGE_NAME);
    assert_eq!(LOCAL_GIT_TOOL_NAMES[6], GIT_STATUS_NAME);
}

#[test]
fn read_verbs_are_effect_free() {
    assert_eq!(LocalToolKind::Status.effect(), ToolEffectClass::EffectFree);
    assert_eq!(LocalToolKind::Diff.effect(), ToolEffectClass::EffectFree);
    assert_eq!(LocalToolKind::Log.effect(), ToolEffectClass::EffectFree);
}

#[test]
fn local_write_verbs_are_effecting() {
    assert_eq!(
        LocalToolKind::Stage.effect(),
        ToolEffectClass::ExternalEffect
    );
    assert_eq!(
        LocalToolKind::Commit.effect(),
        ToolEffectClass::ExternalEffect
    );
    assert_eq!(
        LocalToolKind::BranchCreate.effect(),
        ToolEffectClass::ExternalEffect
    );
    assert_eq!(
        LocalToolKind::BranchSwitch.effect(),
        ToolEffectClass::ExternalEffect
    );
}
