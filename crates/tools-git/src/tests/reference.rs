//! Reference-hierarchy creation and rollback properties.

use std::{ffi::OsStr, fs};

use rustix::fs::{CWD, Mode, OFlags, openat};

use crate::failure::LocalGitFailure;
use crate::layout::validate_repository_layout;
use crate::limits::MAX_REVISION_BYTES;
use crate::pinning::PinnedRepository;
use crate::reference_lock::{
    ReferenceLock, open_or_create_ref_directory_with_mode_tracked_and_hook, open_reference_parent,
};
use crate::reference_read::read_reference_leaf_with_test_hook;
use crate::tests::support::Fixture;

#[test]
fn created_reference_directory_preserves_post_create_failure_and_removes_owned_path() {
    let parent = tempfile::tempdir().expect("reference parent constructs");
    let directory = openat(
        CWD,
        parent.path(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("reference parent opens");
    let created_name = OsStr::new("created");

    let failure = open_or_create_ref_directory_with_mode_tracked_and_hook(
        &directory,
        created_name,
        Mode::RUSR | Mode::WUSR | Mode::XUSR,
        || Err(LocalGitFailure::Repository),
    )
    .expect_err("post-create capture failure rejects");

    assert_eq!(failure, LocalGitFailure::Repository);
    assert!(!parent.path().join(created_name).exists());
}

#[test]
fn reference_publication_rolls_back_when_its_hierarchy_is_replaced() {
    let fixture = Fixture::new();
    let heads = fixture.root().join(".git/refs/heads");
    let retired_heads = fixture.root().join(".git/refs/heads.retired");
    let reference_path = heads.join("topic");
    fs::write(&reference_path, format!("{}\n", fixture.initial)).expect("fixture reference writes");
    let expected_identity =
        validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected_identity).expect("fixture repository pins");
    let mut lock =
        ReferenceLock::acquire(&authority, "refs/heads/topic").expect("reference lock acquires");
    let expected = lock.read(&authority).expect("expected reference reads");
    lock.prepare(&authority, git2::Oid::ZERO_SHA1)
        .expect("replacement reference prepares");

    let failure = lock
        .publish_with_test_hooks(
            &authority,
            &expected,
            || {},
            || {
                fs::rename(&heads, &retired_heads).expect("reference hierarchy retires");
                fs::create_dir(&heads).expect("replacement reference hierarchy constructs");
            },
        )
        .expect_err("replaced hierarchy rejects publication");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read_to_string(retired_heads.join("topic"))
            .expect("rolled-back original reference reads"),
        format!("{}\n", fixture.initial)
    );
    assert!(!heads.join("topic").exists());
}

#[test]
fn loose_reference_rejects_growth_after_metadata_capture() {
    let fixture = Fixture::new();
    let name = "refs/heads/growing";
    let reference_path = fixture.root().join(".git").join(name);
    fs::write(&reference_path, format!("{}\n", fixture.initial)).expect("fixture reference writes");
    let expected = validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");
    let bound =
        open_reference_parent(&authority, name, false).expect("fixture reference parent opens");
    let mut oversized = b"ref: refs/heads/".to_vec();
    oversized.extend(vec![b'a'; MAX_REVISION_BYTES]);

    let failure =
        read_reference_leaf_with_test_hook(&bound.directory, &bound.leaf, &authority, name, || {
            fs::write(&reference_path, &oversized).expect("fixture reference grows in place")
        })
        .expect_err("grown reference rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn loose_reference_rejects_same_length_rewrite_after_metadata_capture() {
    let fixture = Fixture::new();
    let name = "refs/heads/rewritten";
    let reference_path = fixture.root().join(".git").join(name);
    fs::write(&reference_path, format!("{}\n", fixture.initial)).expect("fixture reference writes");
    let expected = validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");
    let bound =
        open_reference_parent(&authority, name, false).expect("fixture reference parent opens");

    let failure =
        read_reference_leaf_with_test_hook(&bound.directory, &bound.leaf, &authority, name, || {
            fs::write(&reference_path, format!("{}\n", git2::Oid::ZERO_SHA1))
                .expect("fixture reference rewrites in place")
        })
        .expect_err("same-length rewrite rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn reference_publication_rejects_an_existing_packed_namespace_conflict() {
    let fixture = Fixture::new();
    let name = "refs/heads/topic";
    let reference_path = fixture.root().join(".git").join(name);
    fs::write(
        fixture.root().join(".git/packed-refs"),
        format!("{} refs/heads/topic/child\n", fixture.initial),
    )
    .expect("packed namespace conflict writes");
    let expected_identity =
        validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected_identity).expect("fixture repository pins");
    let mut lock = ReferenceLock::acquire(&authority, name).expect("reference lock acquires");
    let expected = lock.read(&authority).expect("missing reference reads");
    lock.prepare(&authority, git2::Oid::ZERO_SHA1)
        .expect("replacement reference prepares");

    let failure = lock
        .publish(&authority, &expected)
        .expect_err("packed namespace conflict rejects publication");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert!(!reference_path.exists());
}

#[test]
fn reference_publication_rolls_back_a_racing_packed_namespace_conflict() {
    let fixture = Fixture::new();
    let name = "refs/heads/topic";
    let reference_path = fixture.root().join(".git").join(name);
    let packed_path = fixture.root().join(".git/packed-refs");
    let expected_identity =
        validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected_identity).expect("fixture repository pins");
    let mut lock = ReferenceLock::acquire(&authority, name).expect("reference lock acquires");
    let expected = lock.read(&authority).expect("missing reference reads");
    lock.prepare(&authority, git2::Oid::ZERO_SHA1)
        .expect("replacement reference prepares");

    let failure = lock
        .publish_with_test_hooks(
            &authority,
            &expected,
            || {},
            || {
                fs::write(
                    &packed_path,
                    format!("{} refs/heads/topic/child\n", fixture.initial),
                )
                .expect("racing packed namespace conflict writes");
            },
        )
        .expect_err("racing packed namespace conflict rejects publication");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert!(!reference_path.exists());
}
