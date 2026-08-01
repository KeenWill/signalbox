//! Branch creation properties.

use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
};

use git2::{BranchType, Odb, Repository};
use rustix::fs::{CWD, Mode, mkfifoat};
use signalbox_tools_workspace::LocalWorkspaceFileSystem;

use crate::arguments::{GitBranchCreateArguments, LocalOperation};
use crate::branch::{branch_create, create_loose_branch_reference_with_hook};
use crate::catalog::LocalGitTools;
use crate::failure::LocalGitFailure;
use crate::limits::MAX_WORKTREE_INSPECTIONS;
use crate::pinning::PinnedObjectDatabase;
use crate::reference_read::read_pinned_reference;
use crate::tests::support::{
    FIX_BRANCH, Fixture, INITIAL_CONTENT, INITIAL_MESSAGE, TRACKED_PATH, commit_all, execute,
    identity, plant_linear_history, raw_commit_with_tree,
};

#[test]
fn branch_create_writes_real_non_forced_reference() {
    let fixture = Fixture::new();
    let executor = fixture.executor();

    execute(
        &executor,
        LocalOperation::BranchCreate(GitBranchCreateArguments {
            name: FIX_BRANCH.to_owned(),
            start: fixture.initial.to_string(),
        }),
    );
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let created = repository
        .find_branch(FIX_BRANCH, BranchType::Local)
        .expect("created branch exists");
    let failure = executor
        .execute_operation(LocalOperation::BranchCreate(GitBranchCreateArguments {
            name: FIX_BRANCH.to_owned(),
            start: fixture.initial.to_string(),
        }))
        .expect_err("existing branch is not forced");

    assert_eq!(created.get().target(), Some(fixture.initial));
    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn branch_create_persists_pruned_ancestry_across_a_removed_shallow_boundary() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let tree = repository
        .find_commit(fixture.initial)
        .expect("fixture initial commit opens")
        .tree_id();
    let parent = raw_commit_with_tree(&repository, tree, fixture.initial);
    let target = raw_commit_with_tree(&repository, tree, parent);
    let executor = fixture.executor();
    let pinned_objects =
        PinnedObjectDatabase::capture(&executor.repository_authority).expect("fixture objects pin");
    let object_database = Odb::new().expect("fixture object database constructs");
    pinned_objects
        .add_to(&object_database)
        .expect("pinned objects attach");
    let pinned_repository = executor
        .repository_authority
        .repository()
        .expect("pinned fixture repository opens");
    pinned_repository
        .set_odb(&object_database)
        .expect("pinned object database installs");
    let target_text = target.to_string();
    let parent_text = parent.to_string();
    let loose_target = fixture
        .root()
        .join(".git/objects")
        .join(&target_text[..2])
        .join(&target_text[2..]);
    let loose_parent = fixture
        .root()
        .join(".git/objects")
        .join(&parent_text[..2])
        .join(&parent_text[2..]);
    fs::remove_file(loose_target).expect("live target object prunes");
    fs::remove_file(loose_parent).expect("live parent object prunes");
    let shallow = fixture.root().join(".git/shallow");
    fs::write(&shallow, format!("{target}\n")).expect("temporary shallow boundary writes");

    branch_create(
        &pinned_repository,
        &executor.repository_authority,
        &object_database,
        GitBranchCreateArguments {
            name: FIX_BRANCH.to_owned(),
            start: target_text,
        },
        || {
            fs::remove_file(&shallow).expect("temporary shallow boundary removes");
            Ok(())
        },
    )
    .expect("captured target persists before branch publication");
    drop(pinned_repository);
    let live_repository = Repository::open(fixture.root()).expect("live fixture repository opens");

    assert_eq!(
        live_repository
            .find_reference("refs/heads/agent/fix")
            .expect("created branch exists")
            .target(),
        Some(target)
    );
    assert_eq!(
        live_repository
            .find_commit(target)
            .expect("published target remains live")
            .tree_id(),
        tree
    );
    assert_eq!(
        live_repository
            .find_commit(parent)
            .expect("published target parent remains live")
            .tree_id(),
        tree
    );
}

#[test]
fn branch_create_rejects_captured_ancestry_over_the_traversal_budget() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let newest = plant_linear_history(&repository, fixture.initial, MAX_WORKTREE_INSPECTIONS + 1);
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::BranchCreate(GitBranchCreateArguments {
            name: FIX_BRANCH.to_owned(),
            start: newest.to_string(),
        }))
        .expect_err("over-budget captured ancestry rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert!(!fixture.root().join(".git/refs/heads/agent/fix").exists());
}

#[test]
fn branch_create_rejects_an_alternates_fifo_planted_after_object_pinning() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let repository = executor
        .repository_authority
        .repository()
        .expect("pinned fixture repository opens");
    let pinned_objects =
        PinnedObjectDatabase::capture(&executor.repository_authority).expect("fixture objects pin");
    let object_database = Odb::new().expect("fixture object database constructs");
    pinned_objects
        .add_to(&object_database)
        .expect("pinned objects attach");
    repository
        .set_odb(&object_database)
        .expect("pinned object database installs");
    let alternates = fixture.root().join(".git/objects/info/alternates");
    mkfifoat(CWD, &alternates, Mode::RUSR | Mode::WUSR)
        .expect("replacement alternates FIFO constructs");

    let failure = branch_create(
        &repository,
        &executor.repository_authority,
        &object_database,
        GitBranchCreateArguments {
            name: FIX_BRANCH.to_owned(),
            start: fixture.initial.to_string(),
        },
        || executor.validate_current_repository(),
    )
    .expect_err("mutable alternates rejects before branch publication");
    fs::remove_file(&alternates).expect("replacement alternates FIFO removes");

    assert_eq!(failure, LocalGitFailure::Repository);
    assert!(repository.find_reference("refs/heads/agent/fix").is_err());
}

#[test]
fn branch_create_rejects_a_replaced_refs_hierarchy() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let retired_refs = fixture.root().join(".git/refs-retired");
    fs::rename(fixture.root().join(".git/refs"), &retired_refs).expect("fixture refs retire");
    let outside = tempfile::tempdir().expect("outside refs root constructs");
    fs::create_dir_all(outside.path().join("heads/agent"))
        .expect("outside refs hierarchy constructs");
    symlink(outside.path(), fixture.root().join(".git/refs"))
        .expect("replacement refs symlink constructs");
    let repository = executor
        .repository_authority
        .repository()
        .expect("pinned fixture repository opens");
    let object_database = repository
        .odb()
        .expect("fixture object database opens before replacement");

    let failure = branch_create(
        &repository,
        &executor.repository_authority,
        &object_database,
        GitBranchCreateArguments {
            name: FIX_BRANCH.to_owned(),
            start: fixture.initial.to_string(),
        },
        || Ok(()),
    )
    .expect_err("replaced refs hierarchy rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert!(!outside.path().join("heads/agent/fix").exists());
}

#[test]
fn branch_create_revalidates_the_injected_root_before_publication() {
    let parent = tempfile::tempdir().expect("workspace parent constructs");
    let root = parent.path().join("workspace");
    let retired = parent.path().join("retired");
    fs::create_dir(&root).expect("workspace root constructs");
    let original = Repository::init(&root).expect("original repository initializes");
    fs::write(root.join(TRACKED_PATH), INITIAL_CONTENT).expect("original fixture file writes");
    let initial = commit_all(&original, INITIAL_MESSAGE);
    let executor = LocalGitTools::try_new(LocalWorkspaceFileSystem, &root, identity())
        .expect("local Git suite constructs")
        .into_parts()
        .1;
    let repository = executor
        .repository_authority
        .repository()
        .expect("pinned original repository opens");
    let object_database = repository
        .odb()
        .expect("original object database opens before replacement");

    let failure = branch_create(
        &repository,
        &executor.repository_authority,
        &object_database,
        GitBranchCreateArguments {
            name: FIX_BRANCH.to_owned(),
            start: initial.to_string(),
        },
        || {
            fs::rename(&root, &retired).expect("original workspace retires");
            fs::create_dir(&root).expect("replacement workspace constructs");
            Repository::init(&root).expect("replacement repository initializes");
            executor.validate_current_repository_identity()
        },
    )
    .expect_err("replaced root rejects branch publication");

    assert_eq!(failure, LocalGitFailure::Repository);
    assert!(!retired.join(".git/refs/heads/agent/fix").exists());
    assert!(!root.join(".git/refs/heads/agent/fix").exists());
}

#[test]
fn branch_create_rejects_a_packed_descendant_reference() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root().join(".git/packed-refs"),
        format!(
            "# pack-refs with: peeled fully-peeled sorted\n{} refs/heads/release/v1\n",
            fixture.initial
        ),
    )
    .expect("fixture packed reference writes");
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::BranchCreate(GitBranchCreateArguments {
            name: "release".to_owned(),
            start: fixture.initial.to_string(),
        }))
        .expect_err("packed descendant reference rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert!(!fixture.root().join(".git/refs/heads/release").exists());
}

#[test]
fn branch_create_rechecks_packed_references_after_pinning_loose_hierarchy() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let packed_references = fixture.root().join(".git/packed-refs");

    let failure = create_loose_branch_reference_with_hook(
        &executor.repository_authority,
        "release/v1",
        fixture.initial,
        || {
            fs::write(
                &packed_references,
                format!(
                    "# pack-refs with: peeled fully-peeled sorted\n{} refs/heads/release\n",
                    fixture.initial
                ),
            )
            .expect("racing packed reference writes");
        },
    )
    .expect_err("packed ancestor appearing before publication rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert!(!fixture.root().join(".git/refs/heads/release/v1").exists());
    assert!(!fixture.root().join(".git/refs/heads/release").exists());
}

#[test]
fn packed_reference_fallback_rejects_an_inaccessible_loose_parent() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let packed_name = "refs/tags/version";
    fs::write(
        fixture.root().join(".git/packed-refs"),
        format!("{} {packed_name}\n", fixture.initial),
    )
    .expect("packed fixture reference writes");
    let executor = fixture.executor();
    let tags = fixture.root().join(".git/refs/tags");
    let retired_tags = fixture.root().join(".git/refs/tags.retired");
    let outside = tempfile::tempdir().expect("outside directory constructs");
    fs::rename(&tags, &retired_tags).expect("loose tag parent retires");
    symlink(outside.path(), &tags).expect("loose tag parent symlink installs");

    let failure = read_pinned_reference(&executor.repository_authority, packed_name)
        .expect_err("inaccessible loose hierarchy rejects packed fallback");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        repository.head().expect("fixture HEAD remains").target(),
        Some(fixture.initial)
    );
}

#[test]
fn branch_create_uses_shared_reference_hierarchy_modes() {
    let fixture = Fixture::new();
    let refs = fixture.root().join(".git/refs");
    let shared_refs_mode = 0o2770;
    let expected_directory_mode = (shared_refs_mode & 0o2777) | 0o700;
    let expected_file_mode = (shared_refs_mode & 0o666) | 0o600;
    fs::set_permissions(&refs, fs::Permissions::from_mode(shared_refs_mode))
        .expect("fixture shared refs permissions set");
    let executor = fixture.executor();

    execute(
        &executor,
        LocalOperation::BranchCreate(GitBranchCreateArguments {
            name: "shared/topic/fix".to_owned(),
            start: fixture.initial.to_string(),
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
fn branch_create_rejects_a_replaced_lock_without_touching_it() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let lock_path = fixture.root().join(".git/refs/heads/race.lock");
    let retired_lock = fixture.root().join(".git/refs/heads/race.lock.pinned");

    let failure = create_loose_branch_reference_with_hook(
        &executor.repository_authority,
        "race",
        fixture.initial,
        || {
            fs::rename(&lock_path, &retired_lock).expect("fixture branch lock retires");
            fs::write(&lock_path, b"replacement\n").expect("replacement branch lock writes");
        },
    )
    .expect_err("replaced branch lock rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(&lock_path).expect("replacement branch lock reads"),
        b"replacement\n"
    );
    assert!(!fixture.root().join(".git/refs/heads/race").exists());
    fs::remove_file(lock_path).expect("replacement branch lock removes");
    fs::remove_file(retired_lock).expect("retired branch lock removes");
}
