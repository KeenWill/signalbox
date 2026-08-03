//! Executor-level regression properties owned by the operations slice.

use std::{
    cell::RefCell,
    collections::BTreeSet,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

use git2::{Odb, Repository};
use rustix::fs::{Mode, OFlags, openat};

use crate::arguments::{GitCommitArguments, GitLogArguments, GitStageArguments, LocalOperation};
use crate::commit::commit;
use crate::failure::LocalGitFailure;
use crate::index_lock::remove_displaced_index_with_test_hook;
use crate::limits::{MAX_OBJECT_BYTES, MAX_OBJECT_DATABASE_BYTES};
use crate::log::log;
use crate::objects::{PackRoot, persist_objects};
use crate::pinning::{PinnedObjectDatabase, live_object_database_bytes};
use crate::reflog::ReferenceLogLock;
use crate::rollback::checkout_snapshot;
use crate::tests::support::{
    Fixture, INITIAL_CONTENT, MODEL_MESSAGE, TRACKED_PATH, commit_all, execute, identity,
    plant_packed_blob,
};

#[test]
fn object_publication_rejects_growth_beyond_the_live_database_budget() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let pinned_objects =
        PinnedObjectDatabase::capture(&executor.repository_authority).expect("fixture objects pin");
    let persistent_object_database = Odb::new_ext(executor.repository_authority.object_format)
        .expect("fixture persistent object database constructs");
    pinned_objects
        .add_to(&persistent_object_database)
        .expect("persistent fixture objects attach");
    let object_database = Odb::new_ext(executor.repository_authority.object_format)
        .expect("fixture object database constructs");
    pinned_objects
        .add_to(&object_database)
        .expect("fixture objects attach");
    let _mempack = object_database
        .add_new_mempack_backend(1000)
        .expect("fixture memory pack attaches");
    let repository = executor
        .repository_authority
        .repository()
        .expect("pinned fixture repository opens");
    repository
        .set_odb(&object_database)
        .expect("fixture object database installs");
    let new_object_content = b"new object outside the captured database\n";
    let object = repository
        .blob(new_object_content)
        .expect("fixture memory object writes");
    let live_bytes = live_object_database_bytes(&executor.repository_authority)
        .expect("fixture live object bytes measure");
    let growth_bytes = MAX_OBJECT_DATABASE_BYTES as u64 - live_bytes;
    fill_live_object_database_to_budget(fixture.root(), growth_bytes);
    let pack_entries_before = fs::read_dir(fixture.root().join(".git/objects/pack"))
        .expect("fixture pack directory reads")
        .count();

    let failure = persist_objects(
        &executor.repository_authority,
        &repository,
        &persistent_object_database,
        &object_database,
        &pinned_objects,
        &[PackRoot::Object(object)],
    )
    .expect_err("object database growth over budget rejects");
    let pack_entries_after = fs::read_dir(fixture.root().join(".git/objects/pack"))
        .expect("fixture pack directory rereads")
        .count();

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(pack_entries_after, pack_entries_before);
    assert_eq!(live_bytes + growth_bytes, MAX_OBJECT_DATABASE_BYTES as u64);
}

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
        .set_odb(&object_database)
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
fn pinned_object_database_admits_a_well_formed_pack_within_the_aggregate_budget() {
    let fixture = Fixture::new();
    let packed_content = b"well-formed packed fixture\n";
    let live_pack = plant_packed_blob(fixture.root(), packed_content);
    let live_bytes = fs::metadata(&live_pack)
        .expect("live pack metadata reads")
        .len();
    let executor = fixture.executor();

    let pinned = PinnedObjectDatabase::capture(&executor.repository_authority)
        .expect("well-formed in-budget pack snapshots");
    let pack_name = live_pack.file_name().expect("fixture pack name exists");
    let captured_bytes = fs::metadata(pinned.directory.path().join("pack").join(pack_name))
        .expect("captured pack metadata reads")
        .len();

    assert_eq!(captured_bytes, live_bytes);
    assert!(pinned.compressed_bytes() < MAX_OBJECT_DATABASE_BYTES as u64);
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
        .set_odb(&object_database)
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
    let tree = repository
        .find_commit(commit)
        .expect("fixture commit opens")
        .tree()
        .expect("fixture tree opens");
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

fn fill_live_object_database_to_budget(root: &Path, mut remaining: u64) {
    let mut sequence = 0_u64;
    while remaining > 0 {
        let directory = root
            .join(".git/objects")
            .join(format!("{:02x}", sequence % 256));
        fs::create_dir_all(&directory).expect("loose-object budget directory creates");
        let path = directory.join(format!("{sequence:038x}"));
        let bytes = remaining.min((MAX_OBJECT_BYTES * 2) as u64);
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .expect("loose-object budget file creates")
            .set_len(bytes)
            .expect("loose-object budget length sets");
        remaining -= bytes;
        sequence += 1;
    }
}
