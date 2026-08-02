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

#[test]
fn orphaned_packed_reference_peel_is_rejected() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root().join(".git/packed-refs"),
        format!("^{}\n", fixture.initial),
    )
    .expect("orphaned packed-reference peel writes");
    let expected = validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");

    let failure = packed_reference_target(&authority, "refs/heads/topic")
        .expect_err("orphaned packed-reference peel rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn malformed_packed_reference_peel_is_rejected() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root().join(".git/packed-refs"),
        format!("{} refs/heads/topic\n^not-an-object-id\n", fixture.initial),
    )
    .expect("malformed packed-reference peel writes");
    let expected = validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");

    let failure = packed_reference_target(&authority, "refs/heads/topic")
        .expect_err("malformed packed-reference peel rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn immediate_valid_packed_reference_peel_is_accepted() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root().join(".git/packed-refs"),
        format!(
            "{} refs/heads/topic\n^{}\n",
            fixture.initial, fixture.initial
        ),
    )
    .expect("valid packed-reference peel writes");
    let expected = validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");

    let target = packed_reference_target(&authority, "refs/heads/topic")
        .expect("valid packed-reference peel reads");

    assert_eq!(target, Some(fixture.initial));
}
