//! Pinned status-reference snapshot properties.

use std::fs;

use git2::Repository;

use crate::failure::LocalGitFailure;
use crate::layout::validate_repository_layout;
use crate::pinning::PinnedRepository;
use crate::status_reference::{status_head, status_head_from_reference};
use crate::tests::support::{FIX_BRANCH, Fixture, raw_commit_with_tree, workspace_root_identity};

#[test]
fn status_head_rejects_a_symbolic_target_outside_git_reference_grammar() {
    let fixture = Fixture::new();
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");
    fs::write(
        fixture.root().join(".git/HEAD"),
        b"ref: refs/heads/topic..invalid\n",
    )
    .expect("invalid symbolic HEAD writes");

    let failure = status_head(&authority).expect_err("invalid symbolic target rejects status");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn status_head_rejects_a_non_utf8_symbolic_target() {
    let fixture = Fixture::new();
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");
    fs::write(
        fixture.root().join(".git/HEAD"),
        b"ref: refs/heads/non-utf8-\xff\n",
    )
    .expect("non-UTF-8 symbolic HEAD writes");

    let failure = status_head(&authority).expect_err("non-UTF-8 symbolic target rejects status");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn status_head_snapshot_does_not_mix_a_later_head_selection() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let captured_head = repository
        .find_reference("HEAD")
        .expect("fixture HEAD captures");
    let captured_branch = captured_head
        .symbolic_target()
        .expect("fixture HEAD is symbolic")
        .expect("fixture HEAD has a target")
        .strip_prefix("refs/heads/")
        .expect("fixture HEAD targets a local branch")
        .to_owned();
    let initial_tree = repository
        .find_commit(fixture.initial)
        .expect("fixture initial commit opens")
        .tree_id();
    let replacement = raw_commit_with_tree(&repository, initial_tree, fixture.initial);
    let replacement = repository
        .find_commit(replacement)
        .expect("replacement commit opens");
    repository
        .branch(FIX_BRANCH, &replacement, false)
        .expect("replacement branch creates");
    repository
        .set_head(&format!("refs/heads/{FIX_BRANCH}"))
        .expect("replacement HEAD selects");

    let (branch, truncated, head) =
        status_head_from_reference(&captured_head).expect("captured HEAD resolves");

    assert_eq!(branch.as_deref(), Some(captured_branch.as_str()));
    assert!(!truncated);
    assert_eq!(head, Some(fixture.initial));
}
