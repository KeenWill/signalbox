//! Argument admission properties.

use git2::ObjectFormat;
use signalbox_domain::NormalizedToolArguments;

use crate::contracts::LocalToolKind;
use crate::decode::decode_operation;

#[test]
fn stage_argument_rejects_parent_traversal_before_execution() {
    let arguments = NormalizedToolArguments::try_from_provider_text(
        serde_json::json!({"paths": ["../outside.txt"]}).to_string(),
    )
    .expect("JSON arguments normalize");

    assert!(decode_operation(LocalToolKind::Stage, &arguments, ObjectFormat::Sha1).is_err());
}

#[test]
fn stage_argument_rejects_the_repository_administration_directory() {
    let arguments = NormalizedToolArguments::try_from_provider_text(
        serde_json::json!({"paths": [".git/config"]}).to_string(),
    )
    .expect("JSON arguments normalize");

    assert!(decode_operation(LocalToolKind::Stage, &arguments, ObjectFormat::Sha1).is_err());
}

#[test]
fn revision_argument_rejects_unbounded_ancestry_expression_before_execution() {
    let arguments = NormalizedToolArguments::try_from_provider_text(
        serde_json::json!({"revision": "HEAD~1000000000", "max_entries": 1}).to_string(),
    )
    .expect("JSON arguments normalize");

    assert!(decode_operation(LocalToolKind::Log, &arguments, ObjectFormat::Sha1).is_err());
}

#[test]
fn branch_argument_rejects_the_reserved_head_shorthand_before_execution() {
    let arguments = NormalizedToolArguments::try_from_provider_text(
        serde_json::json!({"name": "HEAD", "start": "HEAD"}).to_string(),
    )
    .expect("JSON arguments normalize");

    assert!(decode_operation(LocalToolKind::BranchCreate, &arguments, ObjectFormat::Sha1).is_err());
}
