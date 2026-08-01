//! Commit creation and reflog properties.

use std::{
    fs,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::Path,
};

use git2::{Odb, Oid, Repository};
use rustix::fs::{CWD, Mode, mkfifoat};
use signalbox_tools_workspace::LocalWorkspaceFileSystem;

use crate::arguments::{GitCommitArguments, GitStageArguments, LocalOperation};
use crate::catalog::LocalGitTools;
use crate::commit::{COMMIT_REFLOG_ACTION, commit, publish_commit_reference_with_hook};
use crate::failure::LocalGitFailure;
use crate::pinning::PinnedObjectDatabase;
use crate::reference_lock::ReferenceLock;
use crate::reference_read::resolve_pinned_reference_chain;
use crate::tests::support::{
    AUTHOR_EMAIL, AUTHOR_NAME, CHANGED_CONTENT, Fixture, INITIAL_CONTENT, INITIAL_MESSAGE,
    MODEL_MESSAGE, TRACKED_PATH, commit_all, commit_rejects_reflog_without_wait, execute, identity,
    packed_object_counts, raw_commit_with_tree,
};

#[test]
fn commit_preserves_message_with_injected_identity() {
    let fixture = Fixture::new();
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT).expect("fixture change writes");
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let mut index = repository.index().expect("fixture index opens");
    index
        .add_path(Path::new(TRACKED_PATH))
        .expect("fixture change stages");
    index.write().expect("fixture index writes");
    let executor = fixture.executor();

    let result = execute(
        &executor,
        LocalOperation::Commit(GitCommitArguments {
            message: MODEL_MESSAGE.to_owned(),
        }),
    );
    let oid = Oid::from_str(result["commit"].as_str().expect("commit id is text"))
        .expect("commit id parses");
    let commit = repository.find_commit(oid).expect("created commit exists");

    assert_eq!(commit.message(), Ok(MODEL_MESSAGE));
    assert_eq!(commit.author().name(), Ok(AUTHOR_NAME));
    assert_eq!(commit.author().email(), Ok(AUTHOR_EMAIL));
    assert_eq!(commit.committer().name(), Ok(AUTHOR_NAME));
    assert_eq!(commit.committer().email(), Ok(AUTHOR_EMAIL));
}

#[test]
fn repeated_commits_pack_only_objects_created_by_each_operation() {
    let fixture = Fixture::new();
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT).expect("fixture change writes");
    let executor = fixture.executor();

    execute(
        &executor,
        LocalOperation::Stage(GitStageArguments {
            paths: vec![TRACKED_PATH.to_owned()],
        }),
    );
    execute(
        &executor,
        LocalOperation::Commit(GitCommitArguments {
            message: MODEL_MESSAGE.to_owned(),
        }),
    );
    execute(
        &executor,
        LocalOperation::Commit(GitCommitArguments {
            message: INITIAL_MESSAGE.to_owned(),
        }),
    );
    execute(
        &executor,
        LocalOperation::Commit(GitCommitArguments {
            message: CHANGED_CONTENT.to_owned(),
        }),
    );

    assert_eq!(packed_object_counts(fixture.root()), vec![1, 1, 1, 2]);
}

#[test]
fn commit_revalidates_the_injected_root_before_reference_publication() {
    let parent = tempfile::tempdir().expect("workspace parent constructs");
    let root = parent.path().join("workspace");
    let retired = parent.path().join("retired");
    fs::create_dir(&root).expect("workspace root constructs");
    let original = Repository::init(&root).expect("original repository initializes");
    fs::write(root.join(TRACKED_PATH), INITIAL_CONTENT).expect("original fixture file writes");
    let original_head = commit_all(&original, INITIAL_MESSAGE);
    let executor = LocalGitTools::try_new(LocalWorkspaceFileSystem, &root, identity())
        .expect("local Git suite constructs")
        .into_parts()
        .1;
    fs::write(root.join(TRACKED_PATH), CHANGED_CONTENT).expect("original fixture change writes");
    execute(
        &executor,
        LocalOperation::Stage(GitStageArguments {
            paths: vec![TRACKED_PATH.to_owned()],
        }),
    );
    let mut repository = executor
        .repository_authority
        .repository()
        .expect("pinned original repository opens");
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
        .expect("fixture objects attach");
    let _mempack = object_database
        .add_new_mempack_backend(1000)
        .expect("fixture memory pack attaches");
    repository
        .set_odb(&object_database)
        .expect("fixture object database installs");
    let mut replacement_head = None;

    let failure = commit(
        &mut repository,
        &executor.identity,
        GitCommitArguments {
            message: MODEL_MESSAGE.to_owned(),
        },
        &executor.repository_authority,
        &persistent_object_database,
        &object_database,
        || {
            fs::rename(&root, &retired).expect("original workspace retires");
            fs::create_dir(&root).expect("replacement workspace constructs");
            let replacement = Repository::init(&root).expect("replacement repository initializes");
            fs::write(root.join(TRACKED_PATH), INITIAL_CONTENT)
                .expect("replacement fixture file writes");
            replacement_head.replace(commit_all(&replacement, INITIAL_MESSAGE));
            executor.validate_current_repository_identity()
        },
    )
    .expect_err("root replacement rejects before commit publication");
    let replacement = Repository::open(&root).expect("replacement repository opens");
    let retired_repository = Repository::open(&retired).expect("retired original repository opens");

    assert_eq!(failure, LocalGitFailure::Repository);
    assert_eq!(
        replacement
            .head()
            .expect("replacement HEAD exists")
            .target(),
        replacement_head
    );
    assert_eq!(
        retired_repository
            .head()
            .expect("retired original HEAD exists")
            .target(),
        Some(original_head)
    );
}

#[test]
fn commit_records_the_advanced_branch_in_the_head_reflog() {
    let fixture = Fixture::new();
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT).expect("fixture change writes");
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let original_head_target = repository
        .find_reference("HEAD")
        .expect("HEAD exists")
        .symbolic_target()
        .expect("HEAD is symbolic")
        .expect("HEAD has a symbolic target")
        .to_owned();
    let mut index = repository.index().expect("fixture index opens");
    index
        .add_path(Path::new(TRACKED_PATH))
        .expect("fixture change stages");
    index.write().expect("fixture index writes");
    let executor = fixture.executor();

    let result = execute(
        &executor,
        LocalOperation::Commit(GitCommitArguments {
            message: MODEL_MESSAGE.to_owned(),
        }),
    );
    let oid = Oid::from_str(result["commit"].as_str().expect("commit id is text"))
        .expect("commit id parses");
    let reflog = repository.reflog("HEAD").expect("HEAD reflog opens");
    let latest = reflog.get(0).expect("HEAD reflog has a latest entry");

    assert_eq!(latest.id_new(), oid);
    assert_eq!(latest.message(), Ok(Some(COMMIT_REFLOG_ACTION)));
    assert_eq!(
        repository
            .find_reference("HEAD")
            .expect("HEAD exists")
            .symbolic_target(),
        Ok(Some(original_head_target.as_str()))
    );
}

#[test]
fn commit_creates_reflogs_with_shared_reference_modes() {
    let fixture = Fixture::new();
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT).expect("fixture change writes");
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let mut index = repository.index().expect("fixture index opens");
    index
        .add_path(Path::new(TRACKED_PATH))
        .expect("fixture change stages");
    index.write().expect("fixture index writes");
    let branch = repository
        .head()
        .expect("fixture HEAD exists")
        .name()
        .expect("fixture branch is UTF-8")
        .to_owned();
    let shared_refs_mode = 0o2770;
    let expected_directory_mode = (shared_refs_mode & 0o2777) | 0o700;
    let expected_file_mode = (shared_refs_mode & 0o666) | 0o600;
    fs::set_permissions(
        fixture.root().join(".git/refs"),
        fs::Permissions::from_mode(shared_refs_mode),
    )
    .expect("fixture shared refs permissions set");
    fs::remove_dir_all(fixture.root().join(".git/logs")).expect("fixture reflog hierarchy removes");
    let executor = fixture.executor();

    execute(
        &executor,
        LocalOperation::Commit(GitCommitArguments {
            message: MODEL_MESSAGE.to_owned(),
        }),
    );
    let logs = fixture.root().join(".git/logs");
    let branch_log = logs.join(branch);
    let logs_mode = fs::metadata(&logs)
        .expect("created logs metadata reads")
        .permissions()
        .mode()
        & 0o2777;
    let branch_parent_mode = fs::metadata(
        branch_log
            .parent()
            .expect("created branch reflog has a parent"),
    )
    .expect("created branch reflog parent metadata reads")
    .permissions()
    .mode()
        & 0o2777;
    let head_log_mode = fs::metadata(logs.join("HEAD"))
        .expect("created HEAD reflog metadata reads")
        .permissions()
        .mode()
        & 0o777;
    let branch_log_mode = fs::metadata(branch_log)
        .expect("created branch reflog metadata reads")
        .permissions()
        .mode()
        & 0o777;

    assert_eq!(logs_mode, expected_directory_mode);
    assert_eq!(branch_parent_mode, expected_directory_mode);
    assert_eq!(head_log_mode, expected_file_mode);
    assert_eq!(branch_log_mode, expected_file_mode);
}

#[test]
fn commit_creates_a_missing_reference_with_shared_modes() {
    let fixture = Fixture::new();
    let refs = fixture.root().join(".git/refs");
    let shared_refs_mode = 0o2770;
    let expected_directory_mode = (shared_refs_mode & 0o2777) | 0o700;
    let expected_file_mode = (shared_refs_mode & 0o666) | 0o600;
    fs::set_permissions(&refs, fs::Permissions::from_mode(shared_refs_mode))
        .expect("fixture shared refs permissions set");
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    repository
        .set_head("refs/heads/shared/topic/fix")
        .expect("missing fixture branch selects");
    let executor = fixture.executor();

    execute(
        &executor,
        LocalOperation::Commit(GitCommitArguments {
            message: MODEL_MESSAGE.to_owned(),
        }),
    );
    let created_directory_mode = fs::metadata(refs.join("heads/shared/topic"))
        .expect("created reference directory metadata reads")
        .permissions()
        .mode()
        & 0o7777;
    let created_file_mode = fs::metadata(refs.join("heads/shared/topic/fix"))
        .expect("created reference metadata reads")
        .permissions()
        .mode()
        & 0o777;

    assert_eq!(created_directory_mode, expected_directory_mode);
    assert_eq!(created_file_mode, expected_file_mode);
}

#[test]
fn commit_rejects_a_reflog_fifo_without_blocking_or_advancing() {
    let fixture = Fixture::new();
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT).expect("fixture change writes");
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let mut index = repository.index().expect("fixture index opens");
    index
        .add_path(Path::new(TRACKED_PATH))
        .expect("fixture change stages");
    index.write().expect("fixture index writes");
    let branch = repository
        .head()
        .expect("fixture HEAD exists")
        .name()
        .expect("fixture branch is UTF-8")
        .to_owned();
    let branch_log = fixture.root().join(".git/logs").join(branch);
    let executor = fixture.executor();
    fs::remove_file(&branch_log).expect("fixture branch reflog removes");
    mkfifoat(CWD, &branch_log, Mode::RUSR | Mode::WUSR)
        .expect("fixture branch reflog FIFO constructs");

    let rejected_without_wait = commit_rejects_reflog_without_wait(executor, branch_log.clone());

    assert!(rejected_without_wait);
    assert!(
        fs::symlink_metadata(branch_log)
            .expect("fixture reflog metadata reads")
            .file_type()
            .is_fifo()
    );
    assert_eq!(
        repository.head().expect("HEAD exists").target(),
        Some(fixture.initial)
    );
}

#[test]
fn commit_rejects_a_multiply_linked_reflog_without_mutating_outside() {
    let fixture = Fixture::new();
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT).expect("fixture change writes");
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let mut index = repository.index().expect("fixture index opens");
    index
        .add_path(Path::new(TRACKED_PATH))
        .expect("fixture change stages");
    index.write().expect("fixture index writes");
    let branch = repository
        .head()
        .expect("fixture HEAD exists")
        .name()
        .expect("fixture branch is UTF-8")
        .to_owned();
    let branch_log = fixture.root().join(".git/logs").join(branch);
    fs::remove_file(&branch_log).expect("fixture branch reflog removes");
    let outside = tempfile::tempdir().expect("outside directory constructs");
    let outside_log = outside.path().join("outside.log");
    let outside_content = b"outside reflog remains exact\n";
    fs::write(&outside_log, outside_content).expect("outside reflog writes");
    fs::hard_link(&outside_log, &branch_log).expect("outside reflog hard-links into repository");
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Commit(GitCommitArguments {
            message: MODEL_MESSAGE.to_owned(),
        }))
        .expect_err("multiply linked reflog rejects commit");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(outside_log).expect("outside reflog reads"),
        outside_content
    );
    assert_eq!(
        repository.head().expect("HEAD exists").target(),
        Some(fixture.initial)
    );
}

#[test]
fn commit_preserves_the_existing_loose_reference_permissions() {
    let fixture = Fixture::new();
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT).expect("fixture change writes");
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let mut index = repository.index().expect("fixture index opens");
    index
        .add_path(Path::new(TRACKED_PATH))
        .expect("fixture change stages");
    index.write().expect("fixture index writes");
    let branch = repository
        .head()
        .expect("fixture HEAD exists")
        .name()
        .expect("fixture branch is UTF-8")
        .to_owned();
    let branch_path = fixture.root().join(".git").join(branch);
    let expected_mode = 0o640;
    fs::set_permissions(&branch_path, fs::Permissions::from_mode(expected_mode))
        .expect("fixture reference permissions set");
    let executor = fixture.executor();

    execute(
        &executor,
        LocalOperation::Commit(GitCommitArguments {
            message: MODEL_MESSAGE.to_owned(),
        }),
    );
    let updated_mode = fs::metadata(branch_path)
        .expect("updated reference metadata reads")
        .permissions()
        .mode()
        & 0o777;

    assert_eq!(updated_mode, expected_mode);
}

#[test]
fn commit_publishes_reflogs_while_the_target_reference_lock_is_held() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let tree = repository
        .find_commit(fixture.initial)
        .expect("fixture commit opens")
        .tree_id();
    let new = raw_commit_with_tree(&repository, tree, fixture.initial);
    let executor = fixture.executor();
    let (chain, old) = resolve_pinned_reference_chain(&executor.repository_authority, None)
        .expect("fixture reference chain resolves");
    let update_reference = chain.last().expect("fixture branch target exists");
    let update_lock = ReferenceLock::acquire(&executor.repository_authority, update_reference)
        .expect("fixture target reference locks");
    let reference_path = fixture.root().join(".git").join(update_reference);
    let lock_path = reference_path.with_extension("lock");
    let head_log = fixture.root().join(".git/logs/HEAD");
    let branch_log = fixture.root().join(".git/logs").join(update_reference);
    let signature = identity()
        .signature()
        .expect("fixture signature constructs");

    publish_commit_reference_with_hook(
        &executor.repository_authority,
        update_lock,
        update_reference,
        old.expect("fixture parent exists"),
        new,
        &signature,
        || {
            assert!(lock_path.exists());
            assert_eq!(
                fs::read_to_string(&reference_path).expect("locked reference reads"),
                format!("{}\n", fixture.initial)
            );
            assert!(
                fs::read_to_string(&head_log)
                    .expect("published HEAD reflog reads")
                    .contains(&new.to_string())
            );
            assert!(
                fs::read_to_string(&branch_log)
                    .expect("published branch reflog reads")
                    .contains(&new.to_string())
            );
        },
    )
    .expect("fixture reference and reflogs publish");

    assert_eq!(
        fs::read_to_string(reference_path).expect("published reference reads"),
        format!("{new}\n")
    );
}
