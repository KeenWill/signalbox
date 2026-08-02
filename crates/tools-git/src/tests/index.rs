//! Index-lock acquisition and replacement properties.

use std::{fs, os::unix::fs::FileTypeExt};

use git2::{Index, IndexEntry, IndexTime, ObjectFormat};
use rustix::fs::{CWD, Mode, mkfifoat};
use sha1::{Digest, Sha1};

use crate::failure::LocalGitFailure;
use crate::index_lock::{IndexLock, copy_index_snapshot_with_test_hook, write_index_entries};
use crate::layout::validate_repository_layout;
use crate::limits::{MAX_INDEX_BYTES, MAX_INDEX_ENTRIES};
use crate::pinning::PinnedRepository;
use crate::tests::support::{Fixture, Sha256Fixture, workspace_root_identity};

#[test]
fn index_lock_acquisition_failure_removes_the_created_lock() {
    let fixture = Fixture::new();
    let index_path = fixture.root().join(".git/index");
    let lock_path = fixture.root().join(".git/index.lock");
    let index = fs::OpenOptions::new()
        .write(true)
        .open(&index_path)
        .expect("fixture index opens");
    index
        .set_len((MAX_INDEX_BYTES + 1) as u64)
        .expect("oversized sparse index sets length");

    let failure = IndexLock::acquire(&index_path, &lock_path)
        .err()
        .expect("oversized index rejects lock acquisition");

    assert_eq!(failure, LocalGitFailure::Repository);
    assert!(!lock_path.exists());
}

#[test]
fn repository_index_acquisition_rejects_a_head_transition_after_snapshot() {
    let fixture = Fixture::new();
    let head_path = fixture.root().join(".git/HEAD");
    let lock_path = fixture.root().join(".git/index.lock");
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");

    let failure = IndexLock::acquire_for_repository_with_test_hook(&authority, || {
        fs::write(&head_path, b"ref: refs/heads/racing\n").expect("racing HEAD writes");
    })
    .err()
    .expect("HEAD transition rejects repository index acquisition");

    assert_eq!(failure, LocalGitFailure::Repository);
    assert!(!lock_path.exists());
}

#[test]
fn index_lock_private_snapshot_failure_removes_the_created_lock() {
    let fixture = Fixture::new();
    let index_path = fixture.root().join(".git/index");
    let lock_path = fixture.root().join(".git/index.lock");

    let failure = IndexLock::acquire_with_private_directory(&index_path, &lock_path, || {
        Err(std::io::Error::other(
            "fixture private snapshot allocation fails",
        ))
    })
    .err()
    .expect("private snapshot allocation rejects lock acquisition");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert!(!lock_path.exists());
}

#[test]
fn index_lock_rejects_a_replaced_lock_path_without_touching_it() {
    let fixture = Fixture::new();
    let index_path = fixture.root().join(".git/index");
    let lock_path = fixture.root().join(".git/index.lock");
    let (mut index_lock, mut index) =
        IndexLock::acquire(&index_path, &lock_path).expect("fixture index lock acquires");
    fs::remove_file(&lock_path).expect("owned fixture lock unlinks");
    mkfifoat(CWD, &lock_path, Mode::RUSR | Mode::WUSR).expect("replacement lock FIFO constructs");

    index_lock
        .write(&mut index)
        .expect("descriptor-bound index write succeeds");
    let failure = index_lock
        .commit()
        .expect_err("replacement lock path rejects rename");
    let replacement = fs::symlink_metadata(&lock_path).expect("replacement FIFO remains");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert!(replacement.file_type().is_fifo());
}

#[test]
fn index_lock_rolls_back_a_replacement_racing_publication() {
    let fixture = Fixture::new();
    let index_path = fixture.root().join(".git/index");
    let lock_path = fixture.root().join(".git/index.lock");
    let original_index = fs::read(&index_path).expect("fixture index reads");
    let (index_lock, _index) =
        IndexLock::acquire(&index_path, &lock_path).expect("fixture index lock acquires");

    let failure = index_lock
        .commit_with_test_hook(|| {
            fs::remove_file(&lock_path).expect("owned fixture lock unlinks during publication");
            mkfifoat(CWD, &lock_path, Mode::RUSR | Mode::WUSR)
                .expect("racing replacement lock FIFO constructs");
        })
        .expect_err("racing replacement rejects publication");
    let replacement = fs::symlink_metadata(&lock_path).expect("replacement FIFO remains");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(&index_path).expect("rolled-back index reads"),
        original_index
    );
    assert!(replacement.file_type().is_fifo());
}

#[test]
fn index_lock_rejects_an_index_replaced_during_publication() {
    let fixture = Fixture::new();
    let index_path = fixture.root().join(".git/index");
    let lock_path = fixture.root().join(".git/index.lock");
    let replacement_index = b"replacement index".to_vec();
    let (index_lock, _index) =
        IndexLock::acquire(&index_path, &lock_path).expect("fixture index lock acquires");

    let failure = index_lock
        .commit_with_test_hook(|| {
            fs::write(&index_path, &replacement_index)
                .expect("racing replacement index writes in place");
        })
        .expect_err("racing index replacement rejects publication");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(&index_path).expect("replacement index reads"),
        replacement_index
    );
    assert!(!lock_path.exists());
}

#[test]
fn absent_index_publication_rejects_a_replacement_during_layout_validation() {
    let fixture = Fixture::new();
    let index_path = fixture.root().join(".git/index");
    let lock_path = fixture.root().join(".git/index.lock");
    let actor_index = b"actor index remains live".to_vec();
    fs::remove_file(&index_path).expect("fixture index removes");
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");
    let (index_lock, _index) =
        IndexLock::acquire_for_repository(&authority).expect("repository index lock acquires");

    let failure = index_lock
        .commit_with_cleanup_test_hook(|| {
            fs::remove_file(&index_path).expect("published index removes during validation");
            fs::write(&index_path, &actor_index).expect("actor index replacement writes");
        })
        .expect_err("replacement during layout validation rejects publication");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(index_path).expect("actor index replacement reads"),
        actor_index
    );
    assert!(!lock_path.exists());
}

#[test]
fn index_lock_rejects_an_in_place_rewrite_of_prepared_bytes() {
    let fixture = Fixture::new();
    let index_path = fixture.root().join(".git/index");
    let lock_path = fixture.root().join(".git/index.lock");
    let original_index = fs::read(&index_path).expect("fixture index reads");
    let (mut index_lock, _index) =
        IndexLock::acquire(&index_path, &lock_path).expect("fixture index lock acquires");
    index_lock
        .write_raw(&original_index)
        .expect("prepared index writes");
    fs::write(&lock_path, b"actor index bytes").expect("actor rewrites prepared index in place");

    let failure = index_lock
        .commit()
        .expect_err("in-place prepared index rewrite rejects publication");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(index_path).expect("original index reads after rejection"),
        original_index
    );
}

#[test]
fn index_lock_rejects_bytes_rewritten_before_prepared_snapshot() {
    let fixture = Fixture::new();
    let index_path = fixture.root().join(".git/index");
    let lock_path = fixture.root().join(".git/index.lock");
    let original_index = fs::read(&index_path).expect("fixture index reads");
    let (mut index_lock, _index) =
        IndexLock::acquire(&index_path, &lock_path).expect("fixture index lock acquires");

    let failure = index_lock
        .write_raw_with_test_hook(&original_index, || {
            fs::write(&lock_path, b"actor index bytes")
                .expect("actor rewrites index before prepared snapshot");
        })
        .expect_err("pre-snapshot index rewrite rejects preparation");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(index_path).expect("original index reads after rejection"),
        original_index
    );
}

#[test]
fn repository_index_commit_rejects_config_changed_after_acquisition() {
    let fixture = Fixture::new();
    let index_path = fixture.root().join(".git/index");
    let lock_path = fixture.root().join(".git/index.lock");
    let config_path = fixture.root().join(".git/config");
    let original_index = fs::read(&index_path).expect("fixture index reads");
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");
    let (index_lock, _index) =
        IndexLock::acquire_for_repository(&authority).expect("repository index lock acquires");

    let failure = index_lock
        .commit_with_test_hook(|| {
            fs::write(
                &config_path,
                "[core]\nrepositoryformatversion = 1\nbare = false\n[extensions]\nobjectformat = sha256\n",
            )
            .expect("live config object format changes");
        })
        .expect_err("changed config rejects index publication");

    assert_eq!(failure, LocalGitFailure::Repository);
    assert_eq!(
        fs::read(index_path).expect("original index reads after rejection"),
        original_index
    );
    assert!(!lock_path.exists());
}

#[test]
fn repository_index_commit_rolls_back_config_changed_after_exchange() {
    let fixture = Fixture::new();
    let index_path = fixture.root().join(".git/index");
    let lock_path = fixture.root().join(".git/index.lock");
    let config_path = fixture.root().join(".git/config");
    let original_index = fs::read(&index_path).expect("fixture index reads");
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");
    let (mut index_lock, _index) =
        IndexLock::acquire_for_repository(&authority).expect("repository index lock acquires");
    index_lock
        .write_raw(&original_index)
        .expect("replacement index prepares");

    let failure = index_lock
        .commit_with_exchange_test_hook(|| {
            fs::write(
                &config_path,
                "[core]\nrepositoryformatversion = 1\nbare = false\n[extensions]\nobjectformat = sha256\n",
            )
            .expect("live config object format changes after exchange");
        })
        .expect_err("post-exchange config change rejects index publication");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(index_path).expect("rolled-back index reads"),
        original_index
    );
    assert!(!lock_path.exists());
}

#[test]
fn repository_index_commit_rejects_head_changed_before_publication() {
    let fixture = Fixture::new();
    let index_path = fixture.root().join(".git/index");
    let head_path = fixture.root().join(".git/HEAD");
    let original_index = fs::read(&index_path).expect("fixture index reads");
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");
    let (mut lock, mut index) =
        IndexLock::acquire_for_repository(&authority).expect("repository index lock acquires");
    lock.write(&mut index).expect("repository index prepares");

    let failure = lock
        .commit_with_test_hook(|| {
            fs::write(&head_path, b"not a head\n").expect("malformed live HEAD writes")
        })
        .expect_err("changed HEAD rejects index publication");

    assert_eq!(failure, LocalGitFailure::Repository);
    assert_eq!(
        fs::read(index_path).expect("original index reads"),
        original_index
    );
}

#[test]
fn repository_index_commit_rejects_alternates_created_after_acquisition() {
    let fixture = Fixture::new();
    let index_path = fixture.root().join(".git/index");
    let lock_path = fixture.root().join(".git/index.lock");
    let alternates_path = fixture.root().join(".git/objects/info/alternates");
    let original_index = fs::read(&index_path).expect("fixture index reads");
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");
    let (index_lock, _index) =
        IndexLock::acquire_for_repository(&authority).expect("repository index lock acquires");
    fs::create_dir_all(alternates_path.parent().expect("alternates parent exists"))
        .expect("object info directory constructs");
    fs::write(&alternates_path, "/outside/objects\n").expect("late alternates writes");

    let failure = index_lock
        .commit()
        .expect_err("late alternates reject index publication");

    assert_eq!(failure, LocalGitFailure::Repository);
    assert_eq!(
        fs::read(index_path).expect("original index reads after rejection"),
        original_index
    );
    assert!(!lock_path.exists());
}

#[test]
fn index_lock_rejects_prepared_bytes_rewritten_before_private_clone() {
    let fixture = Fixture::new();
    let actor_fixture = Fixture::new();
    let index_path = fixture.root().join(".git/index");
    let lock_path = fixture.root().join(".git/index.lock");
    let original_index = fs::read(&index_path).expect("fixture index reads");
    fs::write(actor_fixture.root().join("tracked.txt"), "actor content\n")
        .expect("actor fixture file rewrites");
    let actor_repository =
        git2::Repository::open(actor_fixture.root()).expect("actor fixture repository opens");
    let mut actor_index = actor_repository.index().expect("actor fixture index opens");
    actor_index
        .add_path(std::path::Path::new("tracked.txt"))
        .expect("actor fixture path stages");
    actor_index.write().expect("actor fixture index writes");
    let actor_bytes =
        fs::read(actor_fixture.root().join(".git/index")).expect("actor fixture index reads");

    let failure = IndexLock::acquire_with_preclone_hook(&index_path, &lock_path, || {
        fs::write(&lock_path, &actor_bytes).expect("actor rewrites lock before private clone");
    })
    .err()
    .expect("pre-clone prepared rewrite rejects acquisition");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(index_path).expect("original index reads after rejection"),
        original_index
    );
    assert_ne!(actor_bytes, original_index);
}

#[test]
fn index_snapshot_rejects_same_length_rewrite_after_metadata_capture() {
    let fixture = Fixture::new();
    let actor_fixture = Fixture::new();
    let index_path = fixture.root().join(".git/index");
    let original_bytes = fs::read(&index_path).expect("fixture index reads");
    fs::write(actor_fixture.root().join("tracked.txt"), "actor content\n")
        .expect("actor fixture file rewrites");
    let actor_repository =
        git2::Repository::open(actor_fixture.root()).expect("actor fixture repository opens");
    let mut actor_index = actor_repository.index().expect("actor fixture index opens");
    actor_index
        .add_path(std::path::Path::new("tracked.txt"))
        .expect("actor fixture path stages");
    actor_index.write().expect("actor fixture index writes");
    let actor_bytes =
        fs::read(actor_fixture.root().join(".git/index")).expect("actor fixture index reads");
    let mut destination = tempfile::tempfile().expect("private index snapshot constructs");

    let failure = copy_index_snapshot_with_test_hook(
        &index_path,
        &mut destination,
        ObjectFormat::Sha1,
        || fs::write(&index_path, &actor_bytes).expect("live index rewrites in place"),
    )
    .expect_err("same-length index rewrite rejects snapshot");

    assert_eq!(original_bytes.len(), actor_bytes.len());
    assert_ne!(original_bytes, actor_bytes);
    assert_eq!(failure, LocalGitFailure::Repository);
    assert_eq!(
        destination
            .metadata()
            .expect("private snapshot metadata reads")
            .len(),
        0
    );
}

#[test]
fn index_snapshot_rejects_a_source_path_replaced_after_open() {
    let fixture = Fixture::new();
    let actor_fixture = Fixture::new();
    let index_path = fixture.root().join(".git/index");
    let retired_index = fixture.root().join(".git/index.retired");
    let original_bytes = fs::read(&index_path).expect("fixture index reads");
    let actor_bytes =
        fs::read(actor_fixture.root().join(".git/index")).expect("actor fixture index reads");
    let mut destination = tempfile::tempfile().expect("private index snapshot constructs");

    let failure = copy_index_snapshot_with_test_hook(
        &index_path,
        &mut destination,
        ObjectFormat::Sha1,
        || {
            fs::rename(&index_path, &retired_index).expect("source index retires");
            fs::write(&index_path, &actor_bytes).expect("replacement index writes");
        },
    )
    .expect_err("replaced source path rejects index snapshot");

    assert_eq!(failure, LocalGitFailure::Repository);
    assert_eq!(
        fs::read(&index_path).expect("replacement index reads"),
        actor_bytes
    );
    assert_eq!(
        fs::read(retired_index).expect("retired source index reads"),
        original_bytes
    );
}

#[test]
fn index_commit_rejects_a_replacement_before_exchange() {
    let fixture = Fixture::new();
    let index_path = fixture.root().join(".git/index");
    let lock_path = fixture.root().join(".git/index.lock");
    let actor_index = b"actor index replacement".to_vec();
    let expected_identity =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected_identity).expect("fixture repository pins");
    let (mut index_lock, mut index) =
        IndexLock::acquire_for_repository(&authority).expect("repository index lock acquires");
    index_lock
        .write(&mut index)
        .expect("prepared repository index writes");

    let failure = index_lock
        .commit_with_pre_exchange_test_hook(|| {
            fs::write(&index_path, &actor_index).expect("actor index replaces expected")
        })
        .expect_err("exchange precondition race rejects index publication");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(index_path).expect("actor index reads after rollback"),
        actor_index
    );
    assert!(!lock_path.exists());
}

#[test]
fn index_lock_reports_success_when_the_displaced_index_is_replaced_after_exchange() {
    let fixture = Fixture::new();
    let index_path = fixture.root().join(".git/index");
    let lock_path = fixture.root().join(".git/index.lock");
    let prepared_index = fs::read(&index_path).expect("valid prepared index reads");
    let actor_replacement = b"actor replacement bytes".to_vec();
    let (mut index_lock, _index) =
        IndexLock::acquire(&index_path, &lock_path).expect("fixture index lock acquires");
    index_lock
        .write_raw(&prepared_index)
        .expect("prepared index writes");

    index_lock
        .commit_with_exchange_test_hook(|| {
            fs::write(&lock_path, &actor_replacement)
                .expect("actor replaces displaced index in place");
        })
        .expect("observable index publication reports success");

    assert_eq!(
        fs::read(index_path).expect("prepared live index reads"),
        prepared_index
    );
    assert_eq!(
        fs::read(lock_path).expect("actor displaced replacement reads"),
        actor_replacement
    );
}

#[test]
fn index_lock_reports_success_when_the_displaced_index_is_removed_after_exchange() {
    let fixture = Fixture::new();
    let index_path = fixture.root().join(".git/index");
    let lock_path = fixture.root().join(".git/index.lock");
    let prepared_index = fs::read(&index_path).expect("valid prepared index reads");
    let (mut index_lock, _index) =
        IndexLock::acquire(&index_path, &lock_path).expect("fixture index lock acquires");
    index_lock
        .write_raw(&prepared_index)
        .expect("prepared index writes");

    index_lock
        .commit_with_exchange_test_hook(|| {
            fs::remove_file(&lock_path).expect("actor removes displaced index");
        })
        .expect("observable index publication reports success");

    assert_eq!(
        fs::read(index_path).expect("prepared live index reads"),
        prepared_index
    );
    assert!(!lock_path.exists());
}

#[test]
fn index_lock_accepts_a_new_writer_after_displaced_index_removal() {
    let fixture = Fixture::new();
    let index_path = fixture.root().join(".git/index");
    let lock_path = fixture.root().join(".git/index.lock");
    let prepared_index = fs::read(&index_path).expect("valid prepared index reads");
    let next_writer = b"next writer lock".to_vec();
    let (mut index_lock, _index) =
        IndexLock::acquire(&index_path, &lock_path).expect("fixture index lock acquires");
    index_lock
        .write_raw(&prepared_index)
        .expect("prepared index writes");

    index_lock
        .commit_with_post_cleanup_test_hook(|| {
            fs::write(&lock_path, &next_writer).expect("next writer lock acquires")
        })
        .expect("completed publication accepts the next writer");

    assert_eq!(
        fs::read(index_path).expect("prepared live index reads"),
        prepared_index
    );
    assert_eq!(
        fs::read(lock_path).expect("next writer lock reads"),
        next_writer
    );
}

#[test]
fn index_lock_preserves_a_directory_replacing_the_displaced_cleanup_path() {
    let fixture = Fixture::new();
    let index_path = fixture.root().join(".git/index");
    let lock_path = fixture.root().join(".git/index.lock");
    let prepared_index = fs::read(&index_path).expect("valid prepared index reads");
    let (mut index_lock, _index) =
        IndexLock::acquire(&index_path, &lock_path).expect("fixture index lock acquires");
    index_lock
        .write_raw(&prepared_index)
        .expect("prepared index writes");

    let failure = index_lock
        .commit_with_cleanup_test_hook(|| {
            fs::remove_file(&lock_path).expect("displaced index removes before cleanup");
            fs::create_dir(&lock_path).expect("actor cleanup directory constructs");
        })
        .expect_err("cleanup-path directory rejects index cleanup");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(index_path).expect("prepared live index reads"),
        prepared_index
    );
    assert!(lock_path.is_dir());
}

#[test]
fn index_lock_preserves_a_file_replacing_the_displaced_cleanup_path() {
    let fixture = Fixture::new();
    let index_path = fixture.root().join(".git/index");
    let lock_path = fixture.root().join(".git/index.lock");
    let prepared_index = fs::read(&index_path).expect("valid prepared index reads");
    let actor_replacement = b"actor cleanup replacement".to_vec();
    let (mut index_lock, _index) =
        IndexLock::acquire(&index_path, &lock_path).expect("fixture index lock acquires");
    index_lock
        .write_raw(&prepared_index)
        .expect("prepared index writes");

    let failure = index_lock
        .commit_with_cleanup_test_hook(|| {
            fs::remove_file(&lock_path).expect("displaced index removes before cleanup");
            fs::write(&lock_path, &actor_replacement).expect("actor cleanup replacement writes");
        })
        .expect_err("cleanup-path file replacement rejects index cleanup");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(index_path).expect("prepared live index reads"),
        prepared_index
    );
    assert_eq!(
        fs::read(lock_path).expect("actor cleanup replacement reads"),
        actor_replacement
    );
}

#[test]
fn index_final_verification_preserves_the_original_when_publication_changes() {
    let fixture = Fixture::new();
    let index_path = fixture.root().join(".git/index");
    let lock_path = fixture.root().join(".git/index.lock");
    let original_index = fs::read(&index_path).expect("fixture index reads");
    let prepared_index = original_index.clone();
    let actor_index = b"actor index remains live".to_vec();
    let (mut index_lock, _index) =
        IndexLock::acquire(&index_path, &lock_path).expect("fixture index lock acquires");
    index_lock
        .write_raw(&prepared_index)
        .expect("prepared index writes");

    let failure = index_lock
        .commit_with_cleanup_test_hook(|| {
            fs::write(&index_path, &actor_index).expect("actor publication rewrite writes");
        })
        .expect_err("publication rewrite rejects final verification");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(index_path).expect("actor live index reads"),
        actor_index
    );
    assert_eq!(
        fs::read(lock_path).expect("displaced original index reads"),
        original_index
    );
}

#[test]
fn index_lock_rejects_split_index_without_opening_shared_backing() {
    let fixture = Fixture::new();
    let index_path = fixture.root().join(".git/index");
    let lock_path = fixture.root().join(".git/index.lock");
    let mut split_index = fs::read(&index_path).expect("fixture index reads");
    split_index.truncate(split_index.len() - 20);
    split_index.extend_from_slice(b"link");
    split_index.extend_from_slice(&0_u32.to_be_bytes());
    let checksum = Sha1::digest(&split_index);
    split_index.extend_from_slice(&checksum);
    fs::write(&index_path, split_index).expect("split-index fixture writes");

    let failure = IndexLock::acquire(&index_path, &lock_path)
        .err()
        .expect("split index rejects lock acquisition");

    assert_eq!(failure, LocalGitFailure::Repository);
    assert!(!lock_path.exists());
}

#[test]
fn manual_index_serialization_uses_the_sha256_checksum_width() {
    let fixture = Sha256Fixture::new();
    let repository = git2::Repository::open(fixture.root()).expect("SHA-256 fixture opens");
    let index = repository.index().expect("SHA-256 fixture index opens");
    let expected_entry = index.get(0).map(|entry| entry.id);
    let directory = tempfile::tempdir().expect("private index directory constructs");
    let index_path = directory.path().join("index");
    let mut serialized = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&index_path)
        .expect("private index constructs");

    write_index_entries(&mut serialized, &index, ObjectFormat::Sha256)
        .expect("SHA-256 index serializes");
    let decoded = git2::Index::open_ext(&index_path, ObjectFormat::Sha256)
        .expect("SHA-256 checksum validates");

    assert_eq!(decoded.len(), index.len());
    assert_eq!(decoded.get(0).map(|entry| entry.id), expected_entry);
}

#[test]
fn manual_index_serialization_rejects_entries_from_another_object_format() {
    let fixture = Fixture::new();
    let repository = git2::Repository::open(fixture.root()).expect("SHA-1 fixture opens");
    let index = repository.index().expect("SHA-1 fixture index opens");
    let mut serialized = tempfile::tempfile().expect("private index constructs");

    let failure = write_index_entries(&mut serialized, &index, ObjectFormat::Sha256)
        .expect_err("SHA-1 entries reject under SHA-256 serialization");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        serialized
            .metadata()
            .expect("private index metadata reads")
            .len(),
        0
    );
}

#[test]
fn manual_index_serialization_rejects_too_many_entries_before_writing() {
    let index = index_with_entries(MAX_INDEX_ENTRIES + 1);
    let mut serialized = tempfile::tempfile().expect("private index constructs");

    let failure = write_index_entries(&mut serialized, &index, ObjectFormat::Sha1)
        .expect_err("over-limit index rejects serialization");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        serialized
            .metadata()
            .expect("private index metadata reads")
            .len(),
        0
    );
}

#[test]
fn repository_index_write_rejects_too_many_path_backed_entries() {
    let fixture = Fixture::new();
    let lock_path = fixture.root().join(".git/index.lock");
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");
    let (mut lock, mut index) =
        IndexLock::acquire_for_repository(&authority).expect("repository index lock acquires");
    add_index_entries_until(&mut index, MAX_INDEX_ENTRIES + 1);
    let prepared_before = fs::read(&lock_path).expect("prepared lock reads");

    let failure = lock
        .write(&mut index)
        .expect_err("over-limit path-backed index rejects write");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(lock_path).expect("unchanged prepared lock reads"),
        prepared_before
    );
}

#[test]
fn raw_index_write_rejects_malformed_bytes_without_mutating_the_lock() {
    let fixture = Fixture::new();
    let index_path = fixture.root().join(".git/index");
    let lock_path = fixture.root().join(".git/index.lock");
    let (mut lock, _index) =
        IndexLock::acquire(&index_path, &lock_path).expect("fixture index lock acquires");
    let prepared_before = fs::read(&lock_path).expect("prepared lock reads");

    let failure = lock
        .write_raw(b"not a Git index")
        .expect_err("malformed raw index rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(lock_path).expect("unchanged prepared lock reads"),
        prepared_before
    );
}

#[test]
fn raw_index_write_rejects_another_object_formats_checksum() {
    let fixture = Sha256Fixture::new();
    let sha1_fixture = Fixture::new();
    let lock_path = fixture.root().join(".git/index.lock");
    let sha1_index =
        fs::read(sha1_fixture.root().join(".git/index")).expect("SHA-1 index fixture reads");
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("SHA-256 layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("SHA-256 repository pins");
    let (mut lock, _index) =
        IndexLock::acquire_for_repository(&authority).expect("SHA-256 index lock acquires");
    let prepared_before = fs::read(&lock_path).expect("SHA-256 prepared lock reads");

    let failure = lock
        .write_raw(&sha1_index)
        .expect_err("SHA-1 raw index rejects under SHA-256 authority");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(lock_path).expect("unchanged SHA-256 prepared lock reads"),
        prepared_before
    );
}

fn index_with_entries(entries: usize) -> Index {
    let mut index = Index::new().expect("in-memory index constructs");
    add_index_entries_until(&mut index, entries);
    index
}

fn add_index_entries_until(index: &mut Index, entries: usize) {
    (index.len()..entries).for_each(|entry_number| {
        index
            .add(&IndexEntry {
                ctime: IndexTime::new(0, 0),
                mtime: IndexTime::new(0, 0),
                dev: 0,
                ino: entry_number as u32,
                mode: 0o100644,
                uid: 0,
                gid: 0,
                file_size: 0,
                id: git2::Oid::ZERO_SHA1,
                flags: 0,
                flags_extended: 0,
                path: format!("entry-{entry_number:04}").into_bytes(),
            })
            .expect("fixture index entry adds");
    });
}
