//! Packed-reference ambiguity rejection properties.

use std::fs;

use git2::Repository;

use crate::failure::LocalGitFailure;
use crate::layout::validate_repository_layout;
use crate::limits::MAX_REFERENCE_BYTES;
use crate::packed_reference::{
    PackedReferenceNamespace, PackedReferenceState, packed_reference_state,
    packed_reference_target, read_packed_references_with_test_hook,
};
use crate::pinning::PinnedRepository;
use crate::tests::support::{Fixture, TRACKED_PATH, commit_all, workspace_root_identity};

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
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");

    let failure = packed_reference_target(&authority, "refs/heads/duplicate")
        .expect_err("duplicate packed reference rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn abbreviated_packed_reference_object_id_is_rejected() {
    let fixture = Fixture::new();
    let name = "refs/heads/abbreviated";
    fs::write(
        fixture.root().join(".git/packed-refs"),
        format!("# pack-refs with: sorted\nabc123 {name}\n"),
    )
    .expect("abbreviated packed reference writes");
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");

    let failure = packed_reference_target(&authority, name)
        .expect_err("abbreviated packed reference rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn repository_layout_rejects_a_non_utf8_packed_reference_name() {
    let fixture = Fixture::new();
    let mut packed = format!("{} refs/heads/", fixture.initial).into_bytes();
    packed.extend_from_slice(b"nonutf-\xff\n");
    fs::write(fixture.root().join(".git/packed-refs"), packed)
        .expect("non-UTF-8 packed reference writes");

    let failure =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("non-UTF-8 packed reference rejects admission");

    assert!(matches!(
        failure,
        crate::construction::LocalGitToolsConstructionError::Repository
    ));
}

#[test]
fn repository_layout_rejects_an_oversized_packed_reference_name() {
    let fixture = Fixture::new();
    let name = format!("refs/heads/{}", "x".repeat(MAX_REFERENCE_BYTES));
    fs::write(
        fixture.root().join(".git/packed-refs"),
        format!("{} {name}\n", fixture.initial),
    )
    .expect("oversized packed reference writes");

    let failure =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("oversized packed reference rejects admission");

    assert!(matches!(
        failure,
        crate::construction::LocalGitToolsConstructionError::Repository
    ));
}

#[test]
fn abbreviated_peeled_packed_reference_object_id_is_rejected() {
    let fixture = Fixture::new();
    let name = "refs/heads/peeled";
    fs::write(
        fixture.root().join(".git/packed-refs"),
        format!(
            "# pack-refs with: peeled sorted\n{} {name}\n^abc123\n",
            fixture.initial
        ),
    )
    .expect("abbreviated peeled reference writes");
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");

    let failure = packed_reference_target(&authority, name)
        .expect_err("abbreviated peeled reference rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn sorted_packed_reference_header_rejects_unsorted_records() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root().join(".git/packed-refs"),
        format!(
            "# pack-refs with: sorted\n{} refs/heads/z-last\n{} refs/heads/a-first\n",
            fixture.initial, fixture.initial
        ),
    )
    .expect("unsorted packed references write");
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");

    let failure = packed_reference_target(&authority, "refs/heads/a-first")
        .expect_err("advertised sorted order rejects unsorted records");

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
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
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
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
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
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");

    let target = packed_reference_target(&authority, "refs/heads/topic")
        .expect("valid packed-reference peel reads");

    assert_eq!(target, Some(fixture.initial));
}

#[test]
fn packed_reference_state_combines_target_and_namespace_from_one_snapshot() {
    let fixture = Fixture::new();
    let name = "refs/heads/topic";
    fs::write(
        fixture.root().join(".git/packed-refs"),
        format!(
            "{} {name}\n{} {name}/child\n",
            fixture.initial, fixture.initial
        ),
    )
    .expect("target and namespace-conflict snapshot writes");
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");

    let state =
        packed_reference_state(&authority, name).expect("packed reference state reads once");

    assert_eq!(
        state,
        PackedReferenceState {
            target: Some(fixture.initial),
            namespace: PackedReferenceNamespace::Conflicts,
        }
    );
}

#[test]
fn interior_blank_packed_reference_record_is_rejected() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root().join(".git/packed-refs"),
        format!(
            "{} refs/heads/topic\n\n{} refs/heads/other\n",
            fixture.initial, fixture.initial
        ),
    )
    .expect("interior blank packed-reference record writes");
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");

    let failure = packed_reference_target(&authority, "refs/heads/topic")
        .expect_err("interior blank packed-reference record rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn doubled_final_packed_reference_newline_is_rejected() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root().join(".git/packed-refs"),
        format!("{} refs/heads/topic\n\n", fixture.initial),
    )
    .expect("doubled final packed-reference newline writes");
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");

    let failure = packed_reference_target(&authority, "refs/heads/topic")
        .expect_err("doubled final packed-reference newline rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn unrecognized_packed_reference_comment_is_rejected() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root().join(".git/packed-refs"),
        format!(
            "# unrelated comment\n{} refs/heads/topic\n",
            fixture.initial
        ),
    )
    .expect("unrecognized packed-reference comment writes");
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");

    let failure = packed_reference_target(&authority, "refs/heads/topic")
        .expect_err("unrecognized packed-reference comment rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn packed_reference_read_rejects_a_path_replaced_after_snapshot() {
    let fixture = Fixture::new();
    let packed_path = fixture.root().join(".git/packed-refs");
    let retired_packed_path = fixture.root().join(".git/packed-refs.retired");
    let replacement = format!("{} refs/heads/replacement\n", fixture.initial);
    fs::write(
        &packed_path,
        format!("{} refs/heads/topic\n", fixture.initial),
    )
    .expect("validated packed references write");
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");

    let failure = read_packed_references_with_test_hook(&authority, || {
        fs::rename(&packed_path, &retired_packed_path).expect("validated packed references retire");
        fs::write(&packed_path, &replacement).expect("replacement packed references write");
    })
    .expect_err("replaced packed-reference pathname rejects read");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read_to_string(packed_path).expect("replacement packed references read"),
        replacement
    );
}
