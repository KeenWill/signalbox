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
