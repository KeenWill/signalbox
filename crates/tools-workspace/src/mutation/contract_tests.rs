//! Contract tests for the `write_file`, `edit_file`, and `apply_patch` tool
//! definitions: schema-rendered path and content bounds, `replace_all`
//! defaults, and catalog registration.

use serde_json::json;
use signalbox_application::{ToolCatalog, ToolCatalogValidationFailure};
use signalbox_domain::{NormalizedToolArguments, ToolName};
use signalbox_tool_contract::rendered_contract_schema;

use super::*;
use crate::LocalWorkspaceFileSystem;

fn arguments(value: &str) -> NormalizedToolArguments {
    NormalizedToolArguments::try_from_provider_text(value.to_owned())
        .expect("fixture arguments are admitted")
}

fn tool_name(value: &str) -> ToolName {
    ToolName::try_new(value.to_owned()).expect("fixture tool name is valid")
}

fn fixture_catalog(workspace: &tempfile::TempDir) -> CompiledToolCatalog {
    WorkspaceMutationTools::try_new(LocalWorkspaceFileSystem, workspace.path())
        .expect("fixture tools construct")
        .into_parts()
        .0
}

#[test]
fn write_file_schema_carries_path_and_content_bounds() {
    let schema = rendered_contract_schema::<WriteFileContract>();

    assert_eq!(schema["properties"]["path"]["minLength"], json!(1));
    assert_eq!(
        schema["properties"]["path"]["maxLength"],
        json!(crate::path::MAX_WORKSPACE_PATH_CHARACTERS)
    );
    assert_eq!(
        schema["properties"]["content"]["maxLength"],
        json!(MAX_WORKSPACE_MUTATION_FILE_BYTES)
    );
}

#[test]
fn edit_file_schema_carries_match_bounds_and_replace_all_default() {
    let schema = rendered_contract_schema::<EditFileContract>();

    assert_eq!(
        schema["properties"]["path"]["maxLength"],
        json!(crate::path::MAX_WORKSPACE_PATH_CHARACTERS)
    );
    assert_eq!(schema["properties"]["old_string"]["minLength"], json!(1));
    assert_eq!(
        schema["properties"]["old_string"]["maxLength"],
        json!(MAX_WORKSPACE_MUTATION_FILE_BYTES)
    );
    assert_eq!(
        schema["properties"]["new_string"]["maxLength"],
        json!(MAX_WORKSPACE_MUTATION_FILE_BYTES)
    );
    assert_eq!(schema["properties"]["replace_all"]["default"], json!(false));
}

#[test]
fn apply_patch_schema_carries_bound_and_model_facing_grammar() {
    let schema = rendered_contract_schema::<ApplyPatchContract>();
    let description = schema["properties"]["patch"]["description"]
        .as_str()
        .expect("patch description is rendered");

    assert_eq!(schema["properties"]["patch"]["minLength"], json!(1));
    assert_eq!(
        schema["properties"]["patch"]["maxLength"],
        json!(crate::MAX_PATCH_BYTES)
    );
    assert!(description.contains("*** Begin Patch"));
    assert!(description.contains("*** End Patch"));
    assert!(description.contains("*** Add File: path"));
    assert!(description.contains("*** Update File: path"));
    assert!(description.contains("*** Delete File: path"));
}

#[test]
fn catalog_preserves_parent_traversal_rejection_detail() {
    let workspace = tempfile::tempdir().expect("workspace fixture constructs");
    let catalog = fixture_catalog(&workspace);

    let result = catalog.validate_arguments(
        &tool_name(WRITE_FILE_NAME),
        &arguments(r#"{"content":"outside","path":"../outside.txt"}"#),
    );
    let Err(ToolCatalogValidationFailure::InvalidArguments {
        detail: Some(detail),
    }) = result
    else {
        panic!("parent traversal is rejected with typed detail")
    };

    assert_eq!(
        detail.as_str(),
        "workspace mutation path rejected: parent traversal in workspace path rejected"
    );
}

#[test]
fn catalog_preserves_truncated_patch_location_and_reason() {
    let workspace = tempfile::tempdir().expect("workspace fixture constructs");
    let catalog = fixture_catalog(&workspace);

    let result = catalog.validate_arguments(
        &tool_name(APPLY_PATCH_NAME),
        &arguments(r#"{"patch":"*** Begin Patch\n*** Add File: new.txt\n+new"}"#),
    );
    let Err(ToolCatalogValidationFailure::InvalidArguments {
        detail: Some(detail),
    }) = result
    else {
        panic!("truncated patch is rejected with parse detail")
    };

    assert_eq!(
        detail.as_str(),
        "patch parse failed at line 4: Truncated { expected: EndPatch }"
    );
}

#[test]
fn oversized_mutation_argument_detail_names_field_size_and_bound() {
    let old_string = validate_argument_byte_length(
        "old_string",
        MAX_WORKSPACE_MUTATION_FILE_BYTES + 17,
        MAX_WORKSPACE_MUTATION_FILE_BYTES,
        "MAX_WORKSPACE_MUTATION_FILE_BYTES",
    )
    .expect_err("oversized edit source is rejected")
    .tool_detail()
    .expect("oversized edit source has typed detail");
    let patch = validate_argument_byte_length(
        "patch",
        crate::MAX_PATCH_BYTES + 31,
        crate::MAX_PATCH_BYTES,
        "MAX_PATCH_BYTES",
    )
    .expect_err("oversized patch is rejected")
    .tool_detail()
    .expect("oversized patch has typed detail");

    assert_eq!(
        old_string.as_str(),
        format!(
            "workspace mutation argument \"old_string\" has {} UTF-8 bytes; \
             MAX_WORKSPACE_MUTATION_FILE_BYTES is {} bytes",
            MAX_WORKSPACE_MUTATION_FILE_BYTES + 17,
            MAX_WORKSPACE_MUTATION_FILE_BYTES
        )
    );
    assert_eq!(
        patch.as_str(),
        format!(
            "workspace mutation argument \"patch\" has {} UTF-8 bytes; \
             MAX_PATCH_BYTES is {} bytes",
            crate::MAX_PATCH_BYTES + 31,
            crate::MAX_PATCH_BYTES
        )
    );
}

#[test]
fn unicode_mutation_path_bound_matches_schema_and_runtime_validation() {
    const CHARACTER: &str = "é";
    const CHARACTER_COUNT: usize = 3_000;

    let path = CHARACTER.repeat(CHARACTER_COUNT);
    let workspace = tempfile::tempdir().expect("workspace fixture constructs");
    let catalog = fixture_catalog(&workspace);

    assert!(path.len() > crate::path::MAX_WORKSPACE_PATH_CHARACTERS);

    catalog
        .validate_arguments(
            &tool_name(WRITE_FILE_NAME),
            &arguments(
                &json!({
                    "content": "",
                    "path": path,
                })
                .to_string(),
            ),
        )
        .expect("schema-admitted Unicode path validates at runtime");
}
