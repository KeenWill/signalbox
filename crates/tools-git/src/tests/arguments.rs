//! Argument admission properties.

use git2::ObjectFormat;
use signalbox_domain::NormalizedToolArguments;

use crate::arguments::{GitDiffArguments, LocalOperation};
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

/// Rooting `git_diff`'s advertised schema in an object widened what the
/// *schema* permits, not what serde decodes: the argument type is unchanged,
/// so both scopes still decode to exactly the same operation.
#[test]
fn diff_arguments_decode_unchanged_under_the_object_rooted_schema() {
    let requested_base = "refs/heads/main";
    let requested_head = "HEAD";
    let worktree = NormalizedToolArguments::try_from_provider_text(
        serde_json::json!({"scope": "worktree"}).to_string(),
    )
    .expect("JSON arguments normalize");
    let revisions = NormalizedToolArguments::try_from_provider_text(
        serde_json::json!({"scope": "revisions", "base": requested_base, "head": requested_head})
            .to_string(),
    )
    .expect("JSON arguments normalize");

    let decoded_worktree = decode_operation(LocalToolKind::Diff, &worktree, ObjectFormat::Sha1)
        .expect("worktree scope is admitted");
    let decoded_revisions = decode_operation(LocalToolKind::Diff, &revisions, ObjectFormat::Sha1)
        .expect("revision scope is admitted");

    assert!(matches!(
        decoded_worktree,
        LocalOperation::Diff(GitDiffArguments::Worktree)
    ));
    let LocalOperation::Diff(GitDiffArguments::Revisions { base, head }) = decoded_revisions else {
        panic!("revision arguments decode as the revision scope");
    };
    assert_eq!(base, requested_base);
    assert_eq!(head, requested_head);
}

/// The flat advertised object cannot state that `base` and `head` belong only
/// to the revision scope, so the argument type remains the authority: an
/// absent tag and an incomplete revision payload are still refused, and a
/// revision object still admits no unknown property.
#[test]
fn diff_arguments_still_reject_an_untagged_or_incomplete_object() {
    let untagged = NormalizedToolArguments::try_from_provider_text(
        serde_json::json!({"base": "refs/heads/main", "head": "HEAD"}).to_string(),
    )
    .expect("JSON arguments normalize");
    let incomplete = NormalizedToolArguments::try_from_provider_text(
        serde_json::json!({"scope": "revisions", "base": "refs/heads/main"}).to_string(),
    )
    .expect("JSON arguments normalize");
    let unknown_property = NormalizedToolArguments::try_from_provider_text(
        serde_json::json!({
            "scope": "revisions",
            "base": "refs/heads/main",
            "head": "HEAD",
            "paths": ["a"]
        })
        .to_string(),
    )
    .expect("JSON arguments normalize");

    assert!(decode_operation(LocalToolKind::Diff, &untagged, ObjectFormat::Sha1).is_err());
    assert!(decode_operation(LocalToolKind::Diff, &incomplete, ObjectFormat::Sha1).is_err());
    assert!(decode_operation(LocalToolKind::Diff, &unknown_property, ObjectFormat::Sha1).is_err());
}

/// Serde has always admitted a worktree object carrying revision properties,
/// because an internally tagged unit variant ignores the rest of the map. The
/// root `oneOf` claimed otherwise; the object-rooted schema does not, so the
/// advertised shape and the decoded shape now agree on this object.
#[test]
fn diff_worktree_scope_admits_revision_properties_exactly_as_before() {
    let worktree_with_revisions = NormalizedToolArguments::try_from_provider_text(
        serde_json::json!({"scope": "worktree", "base": "refs/heads/main", "head": "HEAD"})
            .to_string(),
    )
    .expect("JSON arguments normalize");

    let decoded = decode_operation(
        LocalToolKind::Diff,
        &worktree_with_revisions,
        ObjectFormat::Sha1,
    )
    .expect("the worktree scope ignores the remaining properties");

    assert!(matches!(
        decoded,
        LocalOperation::Diff(GitDiffArguments::Worktree)
    ));
}
