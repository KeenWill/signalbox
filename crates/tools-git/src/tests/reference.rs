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
use crate::tests::support::{Fixture, Sha256Fixture};

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
fn absent_reference_publication_preserves_a_post_publish_replacement() {
    let fixture = Fixture::new();
    let name = "refs/heads/topic";
    let reference_path = fixture.root().join(".git").join(name);
    let actor_target = git2::Oid::from_bytes(&[1_u8; 20]).expect("actor target constructs");
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
                fs::remove_file(&reference_path).expect("published reference removes");
                fs::write(&reference_path, format!("{actor_target}\n"))
                    .expect("actor replacement reference writes");
            },
        )
        .expect_err("post-publish replacement rejects publication");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read_to_string(reference_path).expect("actor replacement reference reads"),
        format!("{actor_target}\n")
    );
}

#[test]
fn exchanged_reference_publication_preserves_a_post_publish_replacement() {
    let fixture = Fixture::new();
    let name = "refs/heads/topic";
    let reference_path = fixture.root().join(".git").join(name);
    let actor_target = git2::Oid::from_bytes(&[1_u8; 20]).expect("actor target constructs");
    fs::write(&reference_path, format!("{}\n", fixture.initial)).expect("fixture reference writes");
    let expected_identity =
        validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected_identity).expect("fixture repository pins");
    let mut lock = ReferenceLock::acquire(&authority, name).expect("reference lock acquires");
    let expected = lock.read(&authority).expect("existing reference reads");
    lock.prepare(&authority, git2::Oid::ZERO_SHA1)
        .expect("replacement reference prepares");

    let failure = lock
        .publish_with_test_hooks(
            &authority,
            &expected,
            || {},
            || {
                fs::remove_file(&reference_path).expect("published reference removes");
                fs::write(&reference_path, format!("{actor_target}\n"))
                    .expect("actor replacement reference writes");
            },
        )
        .expect_err("post-publish replacement rejects publication");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read_to_string(reference_path).expect("actor replacement reference reads"),
        format!("{actor_target}\n")
    );
}

#[test]
fn reference_publication_rejects_an_in_place_rewrite_of_prepared_bytes() {
    let fixture = Fixture::new();
    let name = "refs/heads/topic";
    let reference_path = fixture.root().join(".git").join(name);
    let lock_path = fixture.root().join(".git/refs/heads/topic.lock");
    let actor_target = git2::Oid::from_bytes(&[1_u8; 20]).expect("actor target constructs");
    fs::write(&reference_path, format!("{}\n", fixture.initial)).expect("fixture reference writes");
    let expected_identity =
        validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected_identity).expect("fixture repository pins");
    let mut lock = ReferenceLock::acquire(&authority, name).expect("reference lock acquires");
    let expected = lock.read(&authority).expect("existing reference reads");
    lock.prepare(&authority, git2::Oid::ZERO_SHA1)
        .expect("replacement reference prepares");
    fs::write(&lock_path, format!("{actor_target}\n"))
        .expect("actor rewrites prepared reference in place");

    let failure = lock
        .publish(&authority, &expected)
        .expect_err("in-place prepared reference rewrite rejects publication");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read_to_string(reference_path).expect("original reference reads after rejection"),
        format!("{}\n", fixture.initial)
    );
}

#[test]
fn direct_reference_preparation_truncates_injected_trailing_bytes() {
    let fixture = Fixture::new();
    let name = "refs/heads/topic";
    let reference_path = fixture.root().join(".git").join(name);
    let lock_path = fixture.root().join(".git/refs/heads/topic.lock");
    let target = git2::Oid::ZERO_SHA1;
    fs::write(&reference_path, format!("{}\n", fixture.initial)).expect("fixture reference writes");
    let expected_identity =
        validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected_identity).expect("fixture repository pins");
    let mut lock = ReferenceLock::acquire(&authority, name).expect("reference lock acquires");
    let expected = lock.read(&authority).expect("existing reference reads");
    fs::write(&lock_path, b"actor payload with trailing bytes")
        .expect("actor payload enters reference lock");

    lock.prepare(&authority, target)
        .expect("direct replacement reference prepares");
    lock.publish(&authority, &expected)
        .expect("direct replacement reference publishes");

    assert_eq!(
        fs::read_to_string(reference_path).expect("direct reference reads"),
        format!("{target}\n")
    );
    assert!(!lock_path.exists());
}

#[test]
fn symbolic_reference_preparation_truncates_injected_trailing_bytes() {
    let fixture = Fixture::new();
    let name = "refs/heads/topic";
    let reference_path = fixture.root().join(".git").join(name);
    let lock_path = fixture.root().join(".git/refs/heads/topic.lock");
    let target = "refs/heads/main";
    fs::write(&reference_path, format!("{}\n", fixture.initial)).expect("fixture reference writes");
    let expected_identity =
        validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected_identity).expect("fixture repository pins");
    let mut lock = ReferenceLock::acquire(&authority, name).expect("reference lock acquires");
    let expected = lock.read(&authority).expect("existing reference reads");
    fs::write(&lock_path, b"actor payload with trailing bytes")
        .expect("actor payload enters reference lock");

    lock.prepare_symbolic(&authority, target)
        .expect("symbolic replacement reference prepares");
    lock.publish(&authority, &expected)
        .expect("symbolic replacement reference publishes");

    assert_eq!(
        fs::read_to_string(reference_path).expect("symbolic reference reads"),
        format!("ref: {target}\n")
    );
    assert!(!lock_path.exists());
}

#[test]
fn symbolic_reference_preparation_rejects_a_non_reference_target_without_mutation() {
    let fixture = Fixture::new();
    let name = "refs/heads/topic";
    let lock_path = fixture.root().join(".git/refs/heads/topic.lock");
    let actor_payload = b"actor payload remains unchanged".to_vec();
    let expected_identity =
        validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected_identity).expect("fixture repository pins");
    let mut lock = ReferenceLock::acquire(&authority, name).expect("reference lock acquires");
    fs::write(&lock_path, &actor_payload).expect("actor payload enters reference lock");

    let failure = lock
        .prepare_symbolic(&authority, "HEAD")
        .expect_err("non-reference symbolic target rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(lock_path).expect("unmodified reference lock reads"),
        actor_payload
    );
}

#[test]
fn symbolic_reference_preparation_rejects_a_newline_target_without_mutation() {
    let fixture = Fixture::new();
    let name = "refs/heads/topic";
    let lock_path = fixture.root().join(".git/refs/heads/topic.lock");
    let actor_payload = b"actor payload remains unchanged".to_vec();
    let expected_identity =
        validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected_identity).expect("fixture repository pins");
    let mut lock = ReferenceLock::acquire(&authority, name).expect("reference lock acquires");
    fs::write(&lock_path, &actor_payload).expect("actor payload enters reference lock");

    let failure = lock
        .prepare_symbolic(&authority, "refs/heads/main\nrefs/heads/other")
        .expect_err("newline symbolic target rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(lock_path).expect("unmodified reference lock reads"),
        actor_payload
    );
}

#[test]
fn direct_reference_preparation_rejects_an_oid_from_another_object_format() {
    let fixture = Sha256Fixture::new();
    let name = "refs/heads/topic";
    let lock_path = fixture.root().join(".git/refs/heads/topic.lock");
    let actor_payload = b"actor payload remains unchanged".to_vec();
    let expected_identity =
        validate_repository_layout(fixture.root()).expect("SHA-256 layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected_identity).expect("SHA-256 repository pins");
    let mut lock = ReferenceLock::acquire(&authority, name).expect("reference lock acquires");
    fs::write(&lock_path, &actor_payload).expect("actor payload enters reference lock");

    let failure = lock
        .prepare(&authority, git2::Oid::ZERO_SHA1)
        .expect_err("SHA-1 target rejects under SHA-256 authority");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(lock_path).expect("unmodified reference lock reads"),
        actor_payload
    );
}

#[test]
fn reference_publication_preserves_a_directory_replacing_the_cleanup_path() {
    let fixture = Fixture::new();
    let name = "refs/heads/topic";
    let reference_path = fixture.root().join(".git").join(name);
    let lock_path = fixture.root().join(".git/refs/heads/topic.lock");
    fs::write(&reference_path, format!("{}\n", fixture.initial)).expect("fixture reference writes");
    let expected_identity =
        validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected_identity).expect("fixture repository pins");
    let mut lock = ReferenceLock::acquire(&authority, name).expect("reference lock acquires");
    let expected = lock.read(&authority).expect("existing reference reads");
    lock.prepare(&authority, git2::Oid::ZERO_SHA1)
        .expect("replacement reference prepares");

    let failure = lock
        .publish_with_cleanup_test_hook(&authority, &expected, || {
            fs::remove_file(&lock_path).expect("displaced reference removes before cleanup");
            fs::create_dir(&lock_path).expect("actor cleanup directory constructs");
        })
        .expect_err("cleanup-path directory rejects reference cleanup");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read_to_string(reference_path).expect("prepared live reference reads"),
        format!("{}\n", git2::Oid::ZERO_SHA1)
    );
    assert!(lock_path.is_dir());
}

#[test]
fn reference_publication_preserves_a_file_replacing_the_cleanup_path() {
    let fixture = Fixture::new();
    let name = "refs/heads/topic";
    let reference_path = fixture.root().join(".git").join(name);
    let lock_path = fixture.root().join(".git/refs/heads/topic.lock");
    let actor_replacement = b"actor cleanup replacement\n".to_vec();
    fs::write(&reference_path, format!("{}\n", fixture.initial)).expect("fixture reference writes");
    let expected_identity =
        validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected_identity).expect("fixture repository pins");
    let mut lock = ReferenceLock::acquire(&authority, name).expect("reference lock acquires");
    let expected = lock.read(&authority).expect("existing reference reads");
    lock.prepare(&authority, git2::Oid::ZERO_SHA1)
        .expect("replacement reference prepares");

    let failure = lock
        .publish_with_cleanup_test_hook(&authority, &expected, || {
            fs::remove_file(&lock_path).expect("displaced reference removes before cleanup");
            fs::write(&lock_path, &actor_replacement).expect("actor cleanup replacement writes");
        })
        .expect_err("cleanup-path file replacement rejects reference cleanup");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read_to_string(reference_path).expect("prepared live reference reads"),
        format!("{}\n", git2::Oid::ZERO_SHA1)
    );
    assert_eq!(
        fs::read(lock_path).expect("actor cleanup replacement reads"),
        actor_replacement
    );
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

#[test]
fn absent_reference_rollback_preserves_a_replacement_after_validation() {
    let fixture = Fixture::new();
    let name = "refs/heads/topic";
    let reference_path = fixture.root().join(".git").join(name);
    let packed_path = fixture.root().join(".git/packed-refs");
    let actor_replacement = b"actor replacement remains\n".to_vec();
    let expected_identity =
        validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected_identity).expect("fixture repository pins");
    let mut lock = ReferenceLock::acquire(&authority, name).expect("reference lock acquires");
    let expected = lock.read(&authority).expect("missing reference reads");
    lock.prepare(&authority, git2::Oid::ZERO_SHA1)
        .expect("replacement reference prepares");

    let failure = lock
        .publish_with_rollback_test_hook(
            &authority,
            &expected,
            || {
                fs::write(
                    &packed_path,
                    format!("{} refs/heads/topic/child\n", fixture.initial),
                )
                .expect("racing packed namespace conflict writes");
            },
            || {
                fs::remove_file(&reference_path).expect("prepared publication removes");
                fs::write(&reference_path, &actor_replacement)
                    .expect("actor publication replacement writes");
            },
        )
        .expect_err("post-validation absent replacement rejects rollback");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(reference_path).expect("actor publication replacement reads"),
        actor_replacement
    );
}

#[test]
fn exchange_rollback_preserves_a_displaced_replacement_after_validation() {
    let fixture = Fixture::new();
    let name = "refs/heads/topic";
    let reference_path = fixture.root().join(".git").join(name);
    let lock_path = fixture.root().join(".git/refs/heads/topic.lock");
    let packed_path = fixture.root().join(".git/packed-refs");
    let actor_replacement = b"actor displaced replacement\n".to_vec();
    fs::write(&reference_path, format!("{}\n", fixture.initial)).expect("fixture reference writes");
    let expected_identity =
        validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected_identity).expect("fixture repository pins");
    let mut lock = ReferenceLock::acquire(&authority, name).expect("reference lock acquires");
    let expected = lock.read(&authority).expect("existing reference reads");
    lock.prepare(&authority, git2::Oid::ZERO_SHA1)
        .expect("replacement reference prepares");

    let failure = lock
        .publish_with_rollback_test_hook(
            &authority,
            &expected,
            || {
                fs::write(
                    &packed_path,
                    format!("{} refs/heads/topic/child\n", fixture.initial),
                )
                .expect("racing packed namespace conflict writes");
            },
            || {
                fs::remove_file(&lock_path).expect("displaced reference removes");
                fs::write(&lock_path, &actor_replacement)
                    .expect("actor displaced replacement writes");
            },
        )
        .expect_err("post-validation displaced replacement rejects rollback");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read_to_string(reference_path).expect("prepared live reference reads"),
        format!("{}\n", git2::Oid::ZERO_SHA1)
    );
    assert_eq!(
        fs::read(lock_path).expect("actor displaced replacement reads"),
        actor_replacement
    );
}

#[test]
fn absent_reference_publication_rolls_back_a_racing_commondir() {
    let fixture = Fixture::new();
    let name = "refs/heads/topic";
    let actor_commondir = "../outside\n";
    let reference_path = fixture.root().join(".git").join(name);
    let commondir_path = fixture.root().join(".git/commondir");
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
            || fs::write(&commondir_path, actor_commondir).expect("racing commondir writes"),
        )
        .expect_err("racing commondir rejects publication");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert!(!reference_path.exists());
    assert_eq!(
        fs::read_to_string(commondir_path).expect("racing commondir reads"),
        actor_commondir
    );
}
