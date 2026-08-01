//! Packed-reference ambiguity rejection properties.

use std::fs;

use git2::Repository;

use crate::failure::LocalGitFailure;
use crate::layout::validate_repository_layout;
use crate::packed_reference::packed_reference_target;
use crate::pinning::PinnedRepository;
use crate::tests::support::{Fixture, TRACKED_PATH, commit_all};

#[test]
fn duplicate_packed_reference_names_are_rejected() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    fs::write(fixture.root().join(TRACKED_PATH), "after\n").expect("second fixture content writes");
    let second = commit_all(&repository, "second");
    fs::write(
        fixture.root().join(".git/packed-refs"),
        format!(
            "# pack-refs with: sorted\n{} refs/heads/duplicate\n{} refs/heads/duplicate\n",
            fixture.initial, second
        ),
    )
    .expect("duplicate packed references write");
    let expected = validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");

    let failure = packed_reference_target(&authority, "refs/heads/duplicate")
        .expect_err("duplicate packed reference rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
}
