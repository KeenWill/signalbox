//! Pinned status-reference snapshot properties.

use git2::Repository;

use crate::status_reference::status_head_from_reference;
use crate::tests::support::{FIX_BRANCH, Fixture, raw_commit_with_tree};

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
