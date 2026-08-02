//! Index-lock acquisition and replacement properties.

use std::{fs, os::unix::fs::FileTypeExt};

use git2::ObjectFormat;
use rustix::fs::{CWD, Mode, mkfifoat};
use sha1::{Digest, Sha1};

use crate::failure::LocalGitFailure;
use crate::index_lock::{IndexLock, write_index_entries};
use crate::limits::MAX_INDEX_BYTES;
use crate::tests::support::{Fixture, Sha256Fixture};

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
fn index_lock_rejects_an_in_place_rewrite_of_prepared_bytes() {
    let fixture = Fixture::new();
    let index_path = fixture.root().join(".git/index");
    let lock_path = fixture.root().join(".git/index.lock");
    let original_index = fs::read(&index_path).expect("fixture index reads");
    let (mut index_lock, _index) =
        IndexLock::acquire(&index_path, &lock_path).expect("fixture index lock acquires");
    index_lock
        .write_raw(b"prepared index bytes")
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
fn index_lock_preserves_a_displaced_replacement_after_exchange() {
    let fixture = Fixture::new();
    let index_path = fixture.root().join(".git/index");
    let lock_path = fixture.root().join(".git/index.lock");
    let prepared_index = b"prepared index bytes".to_vec();
    let actor_replacement = b"actor replacement bytes".to_vec();
    let (mut index_lock, _index) =
        IndexLock::acquire(&index_path, &lock_path).expect("fixture index lock acquires");
    index_lock
        .write_raw(&prepared_index)
        .expect("prepared index writes");

    let failure = index_lock
        .commit_with_exchange_test_hook(|| {
            fs::write(&lock_path, &actor_replacement)
                .expect("actor replaces displaced index in place");
        })
        .expect_err("displaced index replacement rejects cleanup");

    assert_eq!(failure, LocalGitFailure::Operation);
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
fn index_lock_preserves_a_directory_replacing_the_displaced_cleanup_path() {
    let fixture = Fixture::new();
    let index_path = fixture.root().join(".git/index");
    let lock_path = fixture.root().join(".git/index.lock");
    let prepared_index = b"prepared index bytes".to_vec();
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
    let prepared_index = b"prepared index bytes".to_vec();
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
