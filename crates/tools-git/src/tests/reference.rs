//! Reference-hierarchy creation and rollback properties.

use std::{ffi::OsStr, fs};

use rustix::fs::{AtFlags, CWD, Mode, OFlags, openat};

use crate::descriptor::{QuarantineDirectory, file_identity, remove_entry_if_identity};
use crate::failure::LocalGitFailure;
use crate::layout::validate_repository_layout;
use crate::limits::{MAX_BRANCH_BYTES, MAX_REFERENCE_BYTES, MAX_REVISION_BYTES};
use crate::pinning::PinnedRepository;
use crate::reference_lock::{
    ReferenceLock, ReferenceParentMode, open_or_create_ref_directory_with_mode_tracked_and_hook,
    open_reference_parent,
};
use crate::reference_read::{
    read_pinned_reference_with_test_hook, read_reference_leaf_with_test_hook,
};
use crate::tests::support::{Fixture, Sha256Fixture};

#[test]
fn created_reference_directory_replacement_is_never_treated_as_owned() {
    let parent = tempfile::tempdir().expect("reference parent constructs");
    let directory = openat(
        CWD,
        parent.path(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("reference parent opens");
    let created_name = OsStr::new("created");
    let retired_path = parent.path().join("retired");
    let actor_marker = b"actor-owned directory";

    let opened = open_or_create_ref_directory_with_mode_tracked_and_hook(
        &directory,
        created_name,
        Mode::RUSR | Mode::WUSR | Mode::XUSR,
        || {
            fs::rename(parent.path().join(created_name), &retired_path)
                .expect("created directory retires");
            fs::create_dir(parent.path().join(created_name))
                .expect("actor replacement directory constructs");
            fs::write(
                parent.path().join(created_name).join("marker"),
                actor_marker,
            )
            .expect("actor marker writes");
            Ok(())
        },
    )
    .expect("concurrent directory creation is adopted without delete authority");

    drop(opened);
    assert!(retired_path.exists());
    assert_eq!(
        fs::read(parent.path().join(created_name).join("marker")).expect("actor marker reads"),
        actor_marker
    );
}

#[test]
fn oversized_reference_name_rejects_before_creating_parent_directories() {
    let fixture = Fixture::new();
    let first_new_parent = fixture.root().join(".git/refs/heads/too-deep");
    let name = format!(
        "refs/heads/too-deep/{}leaf",
        "a/".repeat(MAX_REFERENCE_BYTES)
    );
    let expected = validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");

    let failure = ReferenceLock::acquire(&authority, &name)
        .err()
        .expect("oversized reference name rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert!(!first_new_parent.exists());
}

#[test]
fn reference_lock_accepts_a_branch_beyond_the_old_prefixed_name_bound() {
    let fixture = Fixture::new();
    let branch = "a".repeat(MAX_BRANCH_BYTES - "refs/heads/".len() + 1);
    let name = format!("refs/heads/{branch}");
    let expected = validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");

    ReferenceLock::acquire(&authority, &name).expect("bounded branch reference lock acquires");
}

#[test]
fn quarantine_rejects_a_replacement_after_its_created_identity_is_captured() {
    let parent = tempfile::tempdir().expect("quarantine parent constructs");
    let directory = openat(
        CWD,
        parent.path(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("quarantine parent opens");
    let retired_path = parent.path().join("retired");

    let failure = QuarantineDirectory::create_with_test_hook(&directory, || {
        let quarantine_path = fs::read_dir(parent.path())
            .expect("quarantine parent reads")
            .map(|entry| entry.expect("quarantine entry reads").path())
            .find(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(".signalbox-cleanup-"))
            })
            .expect("created quarantine exists");
        fs::rename(&quarantine_path, &retired_path).expect("created quarantine retires");
        fs::create_dir(&quarantine_path).expect("replacement quarantine constructs");
    })
    .err()
    .expect("replacement quarantine rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert!(retired_path.exists());
    assert_eq!(
        fs::read_dir(parent.path())
            .expect("quarantine parent reopens")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".signalbox-cleanup-")
            })
            .count(),
        1
    );
}

#[test]
fn quarantine_creation_failure_removes_its_persisted_directory() {
    let parent = tempfile::tempdir().expect("quarantine parent constructs");
    let parent_descriptor = openat(
        CWD,
        parent.path(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("quarantine parent opens");

    let failure = QuarantineDirectory::create_with_test_hook(&parent_descriptor, || {
        let quarantine_path = fs::read_dir(parent.path())
            .expect("quarantine parent reads")
            .next()
            .expect("created quarantine entry exists")
            .expect("created quarantine entry reads")
            .path();
        fs::remove_dir(quarantine_path).expect("created quarantine removes before reopen");
    })
    .err()
    .expect("removed quarantine rejects reopen");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert!(
        fs::read_dir(parent.path())
            .expect("quarantine parent reopens")
            .next()
            .is_none()
    );
}

#[test]
fn foreign_cleanup_entry_is_rejected_before_quarantine_creation() {
    let parent = tempfile::tempdir().expect("cleanup parent constructs");
    let directory = openat(
        CWD,
        parent.path(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("cleanup parent opens");
    let entry = parent.path().join("entry");
    let retired = parent.path().join("retired");
    let owned_bytes = b"owned entry";
    let actor_bytes = b"foreign entry";
    fs::write(&entry, owned_bytes).expect("owned entry writes");
    let owned = file_identity(&fs::metadata(&entry).expect("owned entry metadata reads"));
    fs::rename(&entry, &retired).expect("owned entry retires");
    fs::write(&entry, actor_bytes).expect("foreign entry writes");

    let failure =
        remove_entry_if_identity(&directory, OsStr::new("entry"), owned, AtFlags::empty())
            .expect_err("foreign entry rejects cleanup");
    let remaining_entries = fs::read_dir(parent.path())
        .expect("cleanup parent reads")
        .count();

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(fs::read(entry).expect("foreign entry reads"), actor_bytes);
    assert_eq!(fs::read(retired).expect("owned entry reads"), owned_bytes);
    assert_eq!(remaining_entries, 2);
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
fn reference_publication_restores_the_exact_entry_displaced_by_exchange() {
    let fixture = Fixture::new();
    let name = "refs/heads/topic";
    let reference_path = fixture.root().join(".git").join(name);
    let lock_path = fixture.root().join(".git/refs/heads/topic.lock");
    let actor_target = git2::Oid::from_bytes(&[1_u8; 20]).expect("actor target constructs");
    let actor_bytes = format!("{actor_target}\n");
    fs::write(&reference_path, format!("{}\n", fixture.initial)).expect("fixture reference writes");
    let expected_identity =
        validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected_identity).expect("fixture repository pins");
    let mut lock = ReferenceLock::acquire(&authority, name).expect("reference lock acquires");
    let expected = lock.read(&authority).expect("expected reference reads");
    lock.prepare(&authority, git2::Oid::ZERO_SHA1)
        .expect("replacement reference prepares");

    let failure = lock
        .publish_with_pre_exchange_test_hook(&authority, &expected, || {
            fs::write(&reference_path, &actor_bytes).expect("actor reference replaces expected")
        })
        .expect_err("exchange precondition race rejects publication");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read_to_string(reference_path).expect("actor reference reads"),
        actor_bytes
    );
    assert!(!lock_path.exists());
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
fn reference_publication_reports_success_when_displaced_reference_is_replaced_after_exchange() {
    let fixture = Fixture::new();
    let name = "refs/heads/topic";
    let reference_path = fixture.root().join(".git").join(name);
    let lock_path = fixture.root().join(".git/refs/heads/topic.lock");
    let actor_replacement = b"actor replacement\n".to_vec();
    fs::write(&reference_path, format!("{}\n", fixture.initial)).expect("fixture reference writes");
    let expected_identity =
        validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected_identity).expect("fixture repository pins");
    let mut lock = ReferenceLock::acquire(&authority, name).expect("reference lock acquires");
    let expected = lock.read(&authority).expect("existing reference reads");
    lock.prepare(&authority, git2::Oid::ZERO_SHA1)
        .expect("replacement reference prepares");

    lock.publish_with_test_hooks(
        &authority,
        &expected,
        || {},
        || {
            fs::write(&lock_path, &actor_replacement)
                .expect("actor replaces displaced reference in place");
        },
    )
    .expect("observable reference publication reports success");

    assert_eq!(
        fs::read_to_string(reference_path).expect("published reference reads"),
        format!("{}\n", git2::Oid::ZERO_SHA1)
    );
    assert_eq!(
        fs::read(lock_path).expect("actor replacement reads"),
        actor_replacement
    );
}

#[test]
fn reference_publication_reports_success_when_displaced_reference_is_removed_after_exchange() {
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

    lock.publish_with_test_hooks(
        &authority,
        &expected,
        || {},
        || {
            fs::remove_file(&lock_path).expect("actor removes displaced reference");
        },
    )
    .expect("observable reference publication reports success");

    assert_eq!(
        fs::read_to_string(reference_path).expect("published reference reads"),
        format!("{}\n", git2::Oid::ZERO_SHA1)
    );
    assert!(!lock_path.exists());
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
fn reference_preparation_rejects_bytes_rewritten_before_snapshot() {
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

    let failure = lock
        .prepare_with_test_hook(&authority, git2::Oid::ZERO_SHA1, || {
            fs::write(&lock_path, format!("{actor_target}\n"))
                .expect("actor rewrites reference before prepared snapshot");
        })
        .expect_err("pre-snapshot reference rewrite rejects preparation");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read_to_string(reference_path).expect("original reference reads after rejection"),
        format!("{}\n", fixture.initial)
    );
}

#[test]
fn nested_reference_publication_retains_created_parent_directories() {
    let fixture = Fixture::new();
    let name = "refs/heads/feature/topic";
    let reference_path = fixture.root().join(".git").join(name);
    let expected_identity =
        validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected_identity).expect("fixture repository pins");
    let mut lock =
        ReferenceLock::acquire(&authority, name).expect("nested reference lock acquires");
    let expected = lock
        .read(&authority)
        .expect("missing nested reference reads");
    lock.prepare(&authority, git2::Oid::ZERO_SHA1)
        .expect("nested reference prepares");

    lock.publish(&authority, &expected)
        .expect("nested reference publishes");

    assert_eq!(
        fs::read_to_string(reference_path).expect("nested reference reads"),
        format!("{}\n", git2::Oid::ZERO_SHA1)
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
fn symbolic_reference_preparation_rejects_an_oversized_target_without_mutation() {
    let fixture = Fixture::new();
    let name = "refs/heads/topic";
    let target = format!("refs/heads/{}", "a".repeat(MAX_REFERENCE_BYTES));
    let expected_identity =
        validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected_identity).expect("fixture repository pins");
    let mut lock = ReferenceLock::acquire(&authority, name).expect("reference lock acquires");

    let failure = lock
        .prepare_symbolic(&authority, &target)
        .expect_err("oversized symbolic target rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::metadata(fixture.root().join(".git/refs/heads/topic.lock"))
            .expect("unmodified reference lock metadata reads")
            .len(),
        0
    );
}

#[test]
fn oversized_reference_name_cannot_fall_back_to_packed_references() {
    let fixture = Fixture::new();
    let name = format!("refs/heads/{}", "a".repeat(MAX_REFERENCE_BYTES));
    fs::write(
        fixture.root().join(".git/packed-refs"),
        format!("# pack-refs with: sorted\n{} {name}\n", fixture.initial),
    )
    .expect("oversized packed reference writes");
    let expected_identity =
        validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected_identity).expect("fixture repository pins");

    let failure = crate::reference_read::read_pinned_reference(&authority, &name)
        .expect_err("oversized packed fallback rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn abbreviated_loose_reference_object_id_is_rejected() {
    let fixture = Fixture::new();
    let name = "refs/heads/abbreviated";
    fs::write(fixture.root().join(".git").join(name), "abc123\n")
        .expect("abbreviated loose reference writes");
    let expected_identity =
        validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected_identity).expect("fixture repository pins");

    let failure = crate::reference_read::read_pinned_reference(&authority, name)
        .expect_err("abbreviated loose reference rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
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
fn reference_publication_preserves_new_bytes_that_race_cleanup() {
    let fixture = Fixture::new();
    let name = "refs/heads/topic";
    let reference_path = fixture.root().join(".git").join(name);
    let lock_path = fixture.root().join(".git/refs/heads/topic.lock");
    let original = format!("{}\n", fixture.initial);
    let actor_target = format!(
        "{}\n",
        git2::Oid::hash_object(git2::ObjectType::Blob, b"actor target")
            .expect("actor target hashes")
    );
    fs::write(&reference_path, &original).expect("fixture reference writes");
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
            fs::write(&reference_path, &actor_target).expect("published reference rewrites")
        })
        .expect_err("racing publication rewrite rejects cleanup");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read_to_string(reference_path).expect("racing reference reads"),
        actor_target
    );
    assert_eq!(
        fs::read_to_string(lock_path).expect("displaced original reference reads"),
        original
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
    let bound = open_reference_parent(&authority, name, ReferenceParentMode::ExistingOnly)
        .expect("fixture reference parent opens");
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
    let bound = open_reference_parent(&authority, name, ReferenceParentMode::ExistingOnly)
        .expect("fixture reference parent opens");

    let failure =
        read_reference_leaf_with_test_hook(&bound.directory, &bound.leaf, &authority, name, || {
            fs::write(&reference_path, format!("{}\n", git2::Oid::ZERO_SHA1))
                .expect("fixture reference rewrites in place")
        })
        .expect_err("same-length rewrite rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn loose_reference_rejects_a_leaf_path_replacement_after_open() {
    let fixture = Fixture::new();
    let name = "refs/heads/replaced";
    let reference_path = fixture.root().join(".git").join(name);
    let retired_path = fixture.root().join(".git/refs/heads/replaced.retired");
    let actor_target = format!("{}\n", git2::Oid::ZERO_SHA1);
    fs::write(&reference_path, format!("{}\n", fixture.initial)).expect("fixture reference writes");
    let expected = validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");

    let failure = read_pinned_reference_with_test_hook(&authority, name, || {
        fs::rename(&reference_path, &retired_path).expect("opened reference retires");
        fs::write(&reference_path, &actor_target).expect("replacement reference writes");
    })
    .expect_err("replaced live leaf rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read_to_string(reference_path).expect("replacement reference reads"),
        actor_target
    );
}

#[test]
fn loose_reference_rejects_a_parent_path_replacement_after_open() {
    let fixture = Fixture::new();
    let name = "refs/heads/parent/topic";
    let parent_path = fixture.root().join(".git/refs/heads/parent");
    let retired_path = fixture.root().join(".git/refs/heads/parent.retired");
    let reference_path = parent_path.join("topic");
    let actor_target = format!("{}\n", git2::Oid::ZERO_SHA1);
    fs::create_dir(&parent_path).expect("fixture reference parent constructs");
    fs::write(&reference_path, format!("{}\n", fixture.initial)).expect("fixture reference writes");
    let expected = validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");

    let failure = read_pinned_reference_with_test_hook(&authority, name, || {
        fs::rename(&parent_path, &retired_path).expect("opened reference parent retires");
        fs::create_dir(&parent_path).expect("replacement reference parent constructs");
        fs::write(parent_path.join("topic"), &actor_target).expect("replacement reference writes");
    })
    .expect_err("replaced live hierarchy rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read_to_string(parent_path.join("topic")).expect("replacement reference reads"),
        actor_target
    );
}

#[test]
fn packed_reference_fallback_rejects_a_new_loose_leaf_after_snapshot() {
    let fixture = Fixture::new();
    let name = "refs/heads/packed-topic";
    let reference_path = fixture.root().join(".git").join(name);
    let packed_path = fixture.root().join(".git/packed-refs");
    let actor_target = format!("{}\n", git2::Oid::ZERO_SHA1);
    fs::write(&packed_path, format!("{} {name}\n", fixture.initial))
        .expect("packed reference writes");
    let expected = validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");

    let failure = read_pinned_reference_with_test_hook(&authority, name, || {
        fs::write(&reference_path, &actor_target).expect("racing loose reference writes");
        fs::write(&packed_path, format!("{} {name}\n", git2::Oid::ZERO_SHA1))
            .expect("replacement packed reference writes");
    })
    .expect_err("loose reference shadowing packed snapshot rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read_to_string(reference_path).expect("racing loose reference reads"),
        actor_target
    );
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

#[test]
fn absent_reference_final_verification_preserves_a_racing_replacement() {
    let fixture = Fixture::new();
    let name = "refs/heads/topic";
    let reference_path = fixture.root().join(".git").join(name);
    let actor_target = format!("{}\n", fixture.initial);
    let expected_identity =
        validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected_identity).expect("fixture repository pins");
    let mut lock = ReferenceLock::acquire(&authority, name).expect("reference lock acquires");
    let expected = lock.read(&authority).expect("missing reference reads");
    lock.prepare(&authority, git2::Oid::ZERO_SHA1)
        .expect("replacement reference prepares");

    let failure = lock
        .publish_with_cleanup_test_hook(&authority, &expected, || {
            fs::remove_file(&reference_path).expect("prepared publication removes");
            fs::write(&reference_path, &actor_target).expect("actor reference replacement writes");
        })
        .expect_err("late absent-reference replacement rejects publication");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read_to_string(reference_path).expect("actor reference replacement reads"),
        actor_target
    );
}
