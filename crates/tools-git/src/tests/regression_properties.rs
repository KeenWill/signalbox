//! Executor-level regression properties owned by the operations slice.

use std::{cell::RefCell, collections::BTreeSet, ffi::OsStr, fs, path::PathBuf};

use git2::{Odb, Repository};
use rustix::fs::{Mode, OFlags, openat};

use crate::arguments::{GitCommitArguments, GitLogArguments, GitStageArguments, LocalOperation};
use crate::commit::commit;
use crate::failure::LocalGitFailure;
use crate::index_lock::remove_displaced_index_with_test_hook;
use crate::log::log;
use crate::pinning::PinnedObjectDatabase;
use crate::reflog::ReferenceLogLock;
use crate::rollback::checkout_snapshot;
use crate::tests::support::{
    Fixture, INITIAL_CONTENT, MODEL_MESSAGE, TRACKED_PATH, commit_all, execute, identity,
    plant_packed_blob,
};

#[test]
fn failed_unborn_commit_removes_its_new_reference_directories() {
    let root = tempfile::tempdir().expect("temporary repository root constructs");
    let repository = Repository::init(root.path()).expect("unborn repository initializes");
    let unborn_reference = "refs/heads/topic/v1";
    repository
        .set_head(unborn_reference)
        .expect("nested unborn branch selects");
    fs::write(root.path().join(TRACKED_PATH), INITIAL_CONTENT).expect("fixture file writes");
    let executor = crate::LocalGitExecutor::for_test(root.path(), identity());
    execute(
        &executor,
        LocalOperation::Stage(GitStageArguments {
            paths: vec![TRACKED_PATH.to_owned()],
        }),
    );
    let mut pinned_repository = executor
        .repository_authority
        .repository()
        .expect("pinned unborn repository opens");
    let pinned_objects =
        PinnedObjectDatabase::capture(&executor.repository_authority).expect("fixture objects pin");
    let persistent_object_database = Odb::new_ext(executor.repository_authority.object_format)
        .expect("fixture persistent object database constructs");
    pinned_objects
        .add_to(&persistent_object_database)
        .expect("fixture persistent objects attach");
    let object_database = Odb::new_ext(executor.repository_authority.object_format)
        .expect("fixture object database constructs");
    pinned_objects
        .add_to(&object_database)
        .expect("fixture writable objects attach");
    let _mempack = object_database
        .add_new_mempack_backend(1000)
        .expect("fixture memory pack attaches");
    pinned_repository
        .set_odb(&object_database, &pinned_objects)
        .expect("fixture writable object database installs");

    let failure = commit(
        &mut pinned_repository,
        &executor.identity,
        GitCommitArguments {
            message: MODEL_MESSAGE.to_owned(),
        },
        &executor.repository_authority,
        (
            &persistent_object_database,
            &object_database,
            &pinned_objects,
        ),
        || Err(LocalGitFailure::Repository),
    )
    .expect_err("final validation rejects unborn commit");

    assert_eq!(failure, LocalGitFailure::Repository);
    assert!(!root.path().join(".git/refs/heads/topic").exists());
    assert_eq!(
        repository
            .find_reference("HEAD")
            .expect("symbolic HEAD remains")
            .symbolic_target(),
        Ok(Some(unborn_reference))
    );
}

#[test]
fn reflog_rollback_removes_new_nested_hierarchy() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let signature = identity()
        .signature()
        .expect("fixture signature constructs");
    let nested_reference = "refs/heads/topic/v1";
    let nested_parent = fixture.root().join(".git/logs/refs/heads/topic");
    let mut log = ReferenceLogLock::acquire(&executor.repository_authority, nested_reference)
        .expect("nested fixture reflog locks");
    log.append(
        git2::Oid::ZERO_SHA1,
        fixture.initial,
        &signature,
        "fixture action",
    )
    .expect("nested fixture reflog appends");
    log.publish().expect("nested fixture reflog publishes");

    log.rollback().expect("nested fixture reflog rolls back");
    drop(log);

    assert!(!nested_parent.exists());
}

#[test]
fn selected_object_source_reads_a_well_formed_pack() {
    let fixture = Fixture::new();
    let content = b"well-formed packed fixture\n";
    plant_packed_blob(fixture.root(), content);
    let executor = fixture.executor();
    let shell = executor
        .repository_authority
        .open_repository_shell()
        .expect("fixture shell opens");
    shell
        .capture_objects_on_read(&executor.repository_authority)
        .expect("source binds");
    let oid = git2::Oid::hash_object(git2::ObjectType::Blob, content).expect("fixture ID hashes");
    assert_eq!(
        shell
            .object_content(oid)
            .expect("object captures")
            .prefix(content.len())
            .expect("object reads"),
        content
    );
    shell
        .validate_selected_objects(&executor.repository_authority)
        .expect("source still validates");
}

#[test]
fn log_rejects_a_nonempty_shallow_snapshot() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let repository = executor
        .repository_authority
        .repository()
        .expect("pinned fixture repository opens");
    let pinned_objects =
        PinnedObjectDatabase::capture(&executor.repository_authority).expect("fixture objects pin");
    let object_database = Odb::new_ext(executor.repository_authority.object_format)
        .expect("fixture object database constructs");
    pinned_objects
        .add_to(&object_database)
        .expect("fixture objects attach");
    repository
        .set_odb(&object_database, &pinned_objects)
        .expect("fixture object database installs");
    let shallow_content = format!("{}\n", fixture.initial);
    fs::write(fixture.root().join(".git/shallow"), &shallow_content)
        .expect("nonempty shallow snapshot writes");

    let failure = log(
        &repository,
        &executor.repository_authority,
        GitLogArguments {
            revision: "HEAD".to_owned(),
            max_entries: 50,
        },
    )
    .expect_err("nonempty shallow snapshot rejects log");

    assert_eq!(failure, LocalGitFailure::Repository);
}

#[test]
fn index_cleanup_retains_a_foreign_entry_when_restoration_is_blocked() {
    let fixture = Fixture::new();
    let git_directory = fixture.root().join(".git");
    let lock_name = OsStr::new("index.lock");
    let lock_path = git_directory.join(lock_name);
    let original_index = fs::read(git_directory.join("index")).expect("fixture index reads");
    let foreign_quarantined = b"foreign quarantined index";
    let blocking_writer = b"blocking writer index";
    fs::write(&lock_path, &original_index).expect("displaced index fixture writes");
    let git_descriptor = openat(
        rustix::fs::CWD,
        &git_directory,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("fixture Git directory pins");
    let retained_quarantine = RefCell::new(None::<PathBuf>);

    let failure = remove_displaced_index_with_test_hook(&git_descriptor, lock_name, |quarantine| {
        let quarantine_path = git_directory.join(quarantine.name());
        retained_quarantine.replace(Some(quarantine_path.clone()));
        fs::write(quarantine_path.join("displaced"), foreign_quarantined)
            .expect("foreign quarantined index writes");
        fs::write(&lock_path, blocking_writer).expect("blocking writer index writes");
    })
    .expect_err("blocked foreign restoration rejects cleanup");
    let quarantine_path = retained_quarantine
        .borrow()
        .clone()
        .expect("retained quarantine path records");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(lock_path).expect("blocking writer index reads"),
        blocking_writer
    );
    assert_eq!(
        fs::read(quarantine_path.join("displaced")).expect("foreign quarantined index reads"),
        foreign_quarantined
    );
}

#[test]
fn checkout_snapshot_treats_repository_paths_as_literal_filenames() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let literal_path = PathBuf::from("literal[1].txt");
    let literal_content = b"literal pathspec characters\n";
    fs::write(fixture.root().join(&literal_path), literal_content)
        .expect("literal fixture file writes");
    let commit = commit_all(&repository, MODEL_MESSAGE);
    let executor = fixture.executor();
    let repository = executor
        .repository_authority
        .open_repository_shell()
        .expect("snapshot shell opens");
    repository
        .capture_objects_on_read(&executor.repository_authority)
        .expect("source binds");
    crate::bounded::validate_object_header(&repository, commit).expect("commit captures");
    let tree = crate::bounded::tree_for_commit(&repository, commit).expect("fixture tree captures");
    let destination = tempfile::tempdir().expect("snapshot destination constructs");
    let checkout_paths = BTreeSet::from([literal_path.clone()]);

    checkout_snapshot(
        &repository,
        Some(&tree),
        &checkout_paths,
        destination.path(),
    )
    .expect("literal checkout snapshot writes");

    assert_eq!(
        fs::read(destination.path().join(literal_path)).expect("literal snapshot reads"),
        literal_content
    );
}
