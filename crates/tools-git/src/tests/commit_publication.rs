//! Commit publication and rollback properties.

use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
};

use git2::{Odb, Oid, Repository, RepositoryState};
use signalbox_tools_workspace::LocalWorkspaceFileSystem;

use crate::arguments::{GitCommitArguments, GitStageArguments, LocalOperation};
use crate::catalog::LocalGitTools;
use crate::commit::commit;
use crate::failure::LocalGitFailure;
use crate::limits::MAX_MERGE_PARENTS;
use crate::pinning::PinnedObjectDatabase;
use crate::reference_lock::ReferenceLock;
use crate::reference_read::resolve_pinned_reference_chain;
use crate::tests::planting::{
    oversized_commit_object, plant_index_over_blob_budget, plant_maximum_index_beneath_directory,
    plant_over_budget_index,
};
use crate::tests::support::{
    CHANGED_CONTENT, Fixture, INITIAL_CONTENT, MODEL_MESSAGE, TRACKED_PATH, execute, identity,
    install_deleted_conflict,
};

#[test]
fn commit_transaction_advances_an_unborn_symbolic_branch() {
    let directory = tempfile::tempdir().expect("temporary repository root constructs");
    Repository::init(directory.path()).expect("unborn repository initializes");
    fs::write(directory.path().join(TRACKED_PATH), INITIAL_CONTENT).expect("fixture file writes");
    let executor = LocalGitTools::try_new(LocalWorkspaceFileSystem, directory.path(), identity())
        .expect("local Git suite constructs")
        .into_parts()
        .1;
    execute(
        &executor,
        LocalOperation::Stage(GitStageArguments {
            paths: vec![TRACKED_PATH.to_owned()],
        }),
    );

    let result = execute(
        &executor,
        LocalOperation::Commit(GitCommitArguments {
            message: MODEL_MESSAGE.to_owned(),
        }),
    );
    let oid = Oid::from_str(result["commit"].as_str().expect("commit id is text"))
        .expect("commit id parses");
    let repository = Repository::open(directory.path()).expect("fixture repository reopens");
    let commit = repository.find_commit(oid).expect("created commit exists");

    assert_eq!(commit.parent_count(), 0);
    assert_eq!(repository.head().expect("HEAD exists").target(), Some(oid));
}

#[test]
fn commit_rejects_an_unborn_branch_beneath_a_packed_reference() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let packed_reference = "refs/heads/release";
    let unborn_reference = "refs/heads/release/v1";
    repository
        .set_head(unborn_reference)
        .expect("unborn fixture branch selects");
    fs::write(
        fixture.root().join(".git/packed-refs"),
        format!("{} {}\n", fixture.initial, packed_reference),
    )
    .expect("packed ancestor fixture writes");
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Commit(GitCommitArguments {
            message: MODEL_MESSAGE.to_owned(),
        }))
        .expect_err("packed ancestor rejects unborn commit");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert!(!fixture.root().join(".git/refs/heads/release").exists());
    assert_eq!(
        repository
            .find_reference("HEAD")
            .expect("symbolic HEAD remains")
            .symbolic_target(),
        Ok(Some(unborn_reference))
    );
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
    let executor = LocalGitTools::try_new(LocalWorkspaceFileSystem, root.path(), identity())
        .expect("local Git suite constructs")
        .into_parts()
        .1;
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
    let persistent_object_database =
        Odb::new().expect("fixture persistent object database constructs");
    pinned_objects
        .add_to(&persistent_object_database)
        .expect("fixture persistent objects attach");
    let object_database = Odb::new().expect("fixture object database constructs");
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
        &persistent_object_database,
        &object_database,
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
fn commit_rejects_an_existing_index_lock_before_advancing_head() {
    let fixture = Fixture::new();
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT).expect("fixture change writes");
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let mut index = repository.index().expect("fixture index opens");
    index
        .add_path(Path::new(TRACKED_PATH))
        .expect("fixture change stages");
    index.write().expect("fixture index writes");
    fs::write(fixture.root().join(".git/index.lock"), []).expect("competing index lock constructs");
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Commit(GitCommitArguments {
            message: MODEL_MESSAGE.to_owned(),
        }))
        .expect_err("competing index lock rejects commit");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        repository.head().expect("HEAD exists").target(),
        Some(fixture.initial)
    );
}

#[test]
fn commit_rejects_an_index_over_the_entry_budget() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    plant_over_budget_index(&repository);
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Commit(GitCommitArguments {
            message: MODEL_MESSAGE.to_owned(),
        }))
        .expect_err("over-budget index rejects before object traversal");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        repository.head().expect("HEAD exists").target(),
        Some(fixture.initial)
    );
}

#[test]
fn commit_rejects_a_generated_tree_over_the_recursive_budget() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    plant_maximum_index_beneath_directory(&repository);
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Commit(GitCommitArguments {
            message: MODEL_MESSAGE.to_owned(),
        }))
        .expect_err("recursive tree beyond discovery budget rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        repository.head().expect("HEAD exists").target(),
        Some(fixture.initial)
    );
}

#[test]
fn commit_rejects_indexed_blob_bytes_over_the_tree_budget() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    plant_index_over_blob_budget(&repository);
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Commit(GitCommitArguments {
            message: MODEL_MESSAGE.to_owned(),
        }))
        .expect_err("aggregate indexed blobs reject before packing");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        repository.head().expect("HEAD exists").target(),
        Some(fixture.initial)
    );
}

#[test]
fn commit_rejects_an_existing_head_target_lock_before_selecting_parent() {
    let fixture = Fixture::new();
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT).expect("fixture change writes");
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let mut index = repository.index().expect("fixture index opens");
    index
        .add_path(Path::new(TRACKED_PATH))
        .expect("fixture change stages");
    index.write().expect("fixture index writes");
    let head_name = repository
        .head()
        .expect("HEAD exists")
        .name()
        .expect("HEAD target is UTF-8")
        .to_owned();
    fs::write(
        fixture
            .root()
            .join(".git")
            .join(format!("{head_name}.lock")),
        [],
    )
    .expect("competing HEAD target lock constructs");
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Commit(GitCommitArguments {
            message: MODEL_MESSAGE.to_owned(),
        }))
        .expect_err("competing HEAD target lock rejects commit");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        repository.head().expect("HEAD exists").target(),
        Some(fixture.initial)
    );
}

#[test]
fn commit_reference_publication_rejects_a_replaced_refs_hierarchy() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let (chain, _) = resolve_pinned_reference_chain(&executor.repository_authority, None)
        .expect("fixture reference chain resolves");
    let mut locks = chain
        .iter()
        .map(|name| ReferenceLock::acquire(&executor.repository_authority, name))
        .collect::<Result<Vec<_>, _>>()
        .expect("fixture reference locks acquire");
    let target_name = chain
        .last()
        .expect("fixture branch target exists")
        .to_owned();
    let target_position = locks
        .iter()
        .position(|lock| lock.name == target_name)
        .expect("fixture branch lock exists");
    let target_lock = locks.swap_remove(target_position);
    let retired_refs = fixture.root().join(".git/refs-retired");
    fs::rename(fixture.root().join(".git/refs"), &retired_refs).expect("fixture refs retire");
    let outside = tempfile::tempdir().expect("outside refs root constructs");
    fs::create_dir(outside.path().join("heads")).expect("outside heads directory constructs");
    symlink(outside.path(), fixture.root().join(".git/refs"))
        .expect("replacement refs symlink constructs");

    let failure = target_lock
        .commit(&executor.repository_authority, fixture.initial)
        .expect_err("replacement refs hierarchy rejects publication");
    let relative_target = target_name
        .strip_prefix("refs/")
        .expect("fixture target is under refs");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read_to_string(retired_refs.join(relative_target))
            .expect("retired fixture branch reads"),
        format!("{}\n", fixture.initial)
    );
    assert!(!outside.path().join(relative_target).exists());
    drop(locks);
    fs::remove_file(fixture.root().join(".git/refs")).expect("replacement refs symlink removes");
    fs::rename(retired_refs, fixture.root().join(".git/refs")).expect("fixture refs restore");
}

#[test]
fn commit_preserves_every_merge_parent() {
    let fixture = Fixture::new();
    install_deleted_conflict(&fixture);
    let executor = fixture.executor();
    execute(
        &executor,
        LocalOperation::Stage(GitStageArguments {
            paths: vec![TRACKED_PATH.to_owned()],
        }),
    );

    let result = execute(
        &executor,
        LocalOperation::Commit(GitCommitArguments {
            message: MODEL_MESSAGE.to_owned(),
        }),
    );
    let repository = Repository::open(fixture.root()).expect("fixture repository reopens");
    let oid = Oid::from_str(result["commit"].as_str().expect("commit id is text"))
        .expect("commit id parses");
    let commit = repository.find_commit(oid).expect("merge commit exists");

    assert_eq!(commit.parent_count(), 2);
    assert_eq!(repository.state(), RepositoryState::Clean);
    assert_eq!(result["state_cleaned"], true);
}

#[test]
fn commit_rejects_merge_head_over_the_parent_budget() {
    let fixture = Fixture::new();
    install_deleted_conflict(&fixture);
    let oversized_merge_head = format!("{}\n", fixture.initial).repeat(MAX_MERGE_PARENTS + 1);
    fs::write(fixture.root().join(".git/MERGE_HEAD"), oversized_merge_head)
        .expect("oversized merge parent fixture writes");
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Commit(GitCommitArguments {
            message: MODEL_MESSAGE.to_owned(),
        }))
        .expect_err("oversized merge parent set rejects commit");

    assert_eq!(failure, LocalGitFailure::Repository);
}

#[test]
fn commit_rejects_an_oversized_merge_parent_before_parsing() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let oversized = oversized_commit_object(&repository, fixture.initial);
    fs::write(
        fixture.root().join(".git/MERGE_HEAD"),
        format!("{oversized}\n"),
    )
    .expect("oversized merge parent fixture writes");
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Commit(GitCommitArguments {
            message: MODEL_MESSAGE.to_owned(),
        }))
        .expect_err("oversized merge parent rejects before parsing");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        repository.head().expect("fixture HEAD remains").target(),
        Some(fixture.initial)
    );
}

#[test]
fn commit_reports_success_after_merge_state_cleanup_failure() {
    let fixture = Fixture::new();
    install_deleted_conflict(&fixture);
    let executor = fixture.executor();
    execute(
        &executor,
        LocalOperation::Stage(GitStageArguments {
            paths: vec![TRACKED_PATH.to_owned()],
        }),
    );
    let mut repository = executor
        .repository_authority
        .repository()
        .expect("pinned fixture repository opens");
    let pinned_objects =
        PinnedObjectDatabase::capture(&executor.repository_authority).expect("fixture objects pin");
    let persistent_object_database =
        Odb::new().expect("fixture persistent object database constructs");
    pinned_objects
        .add_to(&persistent_object_database)
        .expect("fixture persistent objects attach");
    let object_database = Odb::new().expect("fixture object database constructs");
    pinned_objects
        .add_to(&object_database)
        .expect("fixture pinned objects attach");
    let _mempack = object_database
        .add_new_mempack_backend(1000)
        .expect("fixture mempack attaches");
    repository
        .set_odb(&object_database)
        .expect("fixture repository uses pinned objects");
    let merge_mode = fixture.root().join(".git/MERGE_MODE");
    fs::remove_file(&merge_mode).expect("fixture merge mode file removes");
    fs::create_dir(&merge_mode).expect("blocked merge mode directory constructs");
    fs::write(merge_mode.join("blocker"), []).expect("merge cleanup blocker writes");
    fs::set_permissions(&merge_mode, fs::Permissions::from_mode(0o0))
        .expect("merge cleanup blocker permissions set");

    let result = commit(
        &mut repository,
        &executor.identity,
        GitCommitArguments {
            message: MODEL_MESSAGE.to_owned(),
        },
        &executor.repository_authority,
        &persistent_object_database,
        &object_database,
        || executor.validate_current_repository_identity(),
    )
    .expect("commit succeeds after advancing HEAD");
    fs::set_permissions(&merge_mode, fs::Permissions::from_mode(0o700))
        .expect("merge cleanup blocker permissions restore");

    assert!(!result.state_cleaned);
    assert_eq!(
        repository.head().expect("advanced HEAD exists").target(),
        Some(Oid::from_str(&result.commit).expect("commit id parses"))
    );
}
