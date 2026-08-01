//! Index-lock acquisition and replacement properties.

use std::{fs, os::unix::fs::FileTypeExt};

use rustix::fs::{CWD, Mode, mkfifoat};

use crate::failure::LocalGitFailure;
use crate::index_lock::IndexLock;
use crate::limits::MAX_INDEX_BYTES;
use crate::tests::support::Fixture;

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
