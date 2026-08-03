//! Staged deletion and budget properties.

use std::{fs, path::Path};

use git2::Repository;
use signalbox_tools_workspace::LocalWorkspaceFileSystem;

use crate::arguments::{GitStageArguments, LocalOperation};
use crate::catalog::LocalGitTools;
use crate::failure::LocalGitFailure;
use crate::limits::{GITLINK_MODE, MAX_STAGE_FILE_BYTES};
use crate::tests::planting::plant_aggregate_stage_files;
use crate::tests::support::{
    CHANGED_CONTENT, Fixture, INITIAL_CONTENT, INITIAL_MESSAGE, NESTED_TRACKED_DIRECTORY,
    NESTED_TRACKED_PATH, SUBMODULE_PATH, TRACKED_PATH, UNTRACKED_PATH, commit_all,
    count_loose_objects, execute, identity, install_deleted_conflict, install_gitlink,
};

#[test]
fn stage_records_deletion_after_tracked_parent_directory_is_removed() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.root().join(NESTED_TRACKED_DIRECTORY))
        .expect("nested fixture directory constructs");
    fs::write(fixture.root().join(NESTED_TRACKED_PATH), INITIAL_CONTENT)
        .expect("nested tracked file writes");
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    commit_all(&repository, INITIAL_MESSAGE);
    fs::remove_dir_all(fixture.root().join(NESTED_TRACKED_DIRECTORY))
        .expect("tracked parent directory removes");
    let executor = fixture.executor();

    execute(
        &executor,
        LocalOperation::Stage(GitStageArguments {
            paths: vec![NESTED_TRACKED_PATH.to_owned()],
        }),
    );
    let updated_repository =
        Repository::open(fixture.root()).expect("updated fixture repository opens");
    let index = updated_repository
        .index()
        .expect("updated fixture index opens");

    assert!(index.get_path(Path::new(NESTED_TRACKED_PATH), 0).is_none());
}

#[test]
fn stage_records_child_deletion_when_its_parent_becomes_a_file() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.root().join(NESTED_TRACKED_DIRECTORY))
        .expect("nested fixture directory constructs");
    fs::write(fixture.root().join(NESTED_TRACKED_PATH), INITIAL_CONTENT)
        .expect("nested tracked file writes");
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    commit_all(&repository, INITIAL_MESSAGE);
    fs::remove_dir_all(fixture.root().join(NESTED_TRACKED_DIRECTORY))
        .expect("tracked parent directory removes");
    fs::write(
        fixture.root().join(NESTED_TRACKED_DIRECTORY),
        CHANGED_CONTENT,
    )
    .expect("replacement parent file writes");
    let executor = fixture.executor();

    execute(
        &executor,
        LocalOperation::Stage(GitStageArguments {
            paths: vec![NESTED_TRACKED_PATH.to_owned()],
        }),
    );
    let updated_repository =
        Repository::open(fixture.root()).expect("updated fixture repository opens");
    let index = updated_repository
        .index()
        .expect("updated fixture index opens");

    assert!(index.get_path(Path::new(NESTED_TRACKED_PATH), 0).is_none());
}

#[test]
fn stage_records_deletion_when_tracked_file_becomes_directory() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.root().join(TRACKED_PATH)).expect("tracked fixture file removes");
    fs::create_dir(fixture.root().join(TRACKED_PATH))
        .expect("replacement fixture directory constructs");
    let executor = fixture.executor();

    execute(
        &executor,
        LocalOperation::Stage(GitStageArguments {
            paths: vec![TRACKED_PATH.to_owned()],
        }),
    );
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let index = repository.index().expect("fixture index opens");

    assert!(index.get_path(Path::new(TRACKED_PATH), 0).is_none());
}

#[test]
fn stage_rejects_live_gitlink_without_staging_deletion() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    install_gitlink(&repository, SUBMODULE_PATH, fixture.initial);
    fs::create_dir(fixture.root().join(SUBMODULE_PATH)).expect("live gitlink directory constructs");
    let executor = fixture.executor();

    let failure = executor
        .stage(
            &executor
                .repository_authority
                .repository()
                .expect("pinned fixture repository opens"),
            GitStageArguments {
                paths: vec![SUBMODULE_PATH.to_owned()],
            },
        )
        .expect_err("live gitlink staging rejects");
    let index = repository.index().expect("fixture index reopens");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        index
            .get_path(Path::new(SUBMODULE_PATH), 0)
            .expect("gitlink remains indexed")
            .mode,
        GITLINK_MODE
    );
}

#[test]
fn stage_rejects_an_absent_gitlink_without_staging_deletion() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    install_gitlink(&repository, SUBMODULE_PATH, fixture.initial);
    let executor = fixture.executor();

    let failure = executor
        .stage(
            &executor
                .repository_authority
                .repository()
                .expect("pinned fixture repository opens"),
            GitStageArguments {
                paths: vec![SUBMODULE_PATH.to_owned()],
            },
        )
        .expect_err("absent gitlink staging rejects");
    let index = repository.index().expect("fixture index reopens");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        index
            .get_path(Path::new(SUBMODULE_PATH), 0)
            .expect("gitlink remains indexed")
            .mode,
        GITLINK_MODE
    );
}

#[test]
fn stage_deleted_path_removes_every_conflict_stage() {
    let fixture = Fixture::new();
    install_deleted_conflict(&fixture);
    let executor = fixture.executor();

    execute(
        &executor,
        LocalOperation::Stage(GitStageArguments {
            paths: vec![TRACKED_PATH.to_owned()],
        }),
    );
    let repository = Repository::open(fixture.root()).expect("fixture repository reopens");
    let index = repository.index().expect("fixture index reopens");

    assert!(index.conflict_get(Path::new(TRACKED_PATH)).is_err());
    assert!(index.get_path(Path::new(TRACKED_PATH), 0).is_none());
}

#[test]
fn stage_rejects_aggregate_limit_before_writing_objects() {
    let fixture = Fixture::new();
    let paths = plant_aggregate_stage_files(fixture.root());
    let objects_before = count_loose_objects(fixture.root());
    let executor = fixture.executor();

    let failure = executor
        .stage(
            &executor
                .repository_authority
                .repository()
                .expect("pinned fixture repository opens"),
            GitStageArguments {
                paths: paths.clone(),
            },
        )
        .expect_err("aggregate staging limit rejects");
    let repository = Repository::open(fixture.root()).expect("fixture repository reopens");
    let index = repository.index().expect("fixture index reopens");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert!(index.get_path(Path::new(&paths[0]), 0).is_none());
    assert!(
        index
            .get_path(Path::new(&paths[paths.len() - 1]), 0)
            .is_none()
    );
    assert_eq!(count_loose_objects(fixture.root()), objects_before);
}

#[test]
fn stage_rejects_a_file_larger_than_the_object_read_limit() {
    let fixture = Fixture::new();
    let oversized = fixture.root().join(UNTRACKED_PATH);
    let file = fs::File::create(&oversized).expect("oversized fixture creates");
    file.set_len((MAX_STAGE_FILE_BYTES + 1) as u64)
        .expect("oversized fixture length sets");
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Stage(GitStageArguments {
            paths: vec![UNTRACKED_PATH.to_owned()],
        }))
        .expect_err("oversized staging input rejects");
    let repository = Repository::open(fixture.root()).expect("fixture repository reopens");
    let index = repository.index().expect("fixture index reopens");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert!(index.get_path(Path::new(UNTRACKED_PATH), 0).is_none());
}

#[test]
fn pinned_index_never_writes_replacement_repository() {
    let parent = tempfile::tempdir().expect("workspace parent constructs");
    let root = parent.path().join("workspace");
    fs::create_dir(&root).expect("workspace root constructs");
    let original = Repository::init(&root).expect("original repository initializes");
    fs::write(root.join(TRACKED_PATH), INITIAL_CONTENT).expect("original file writes");
    commit_all(&original, INITIAL_MESSAGE);
    let executor = LocalGitTools::try_new(LocalWorkspaceFileSystem, &root, identity())
        .expect("suite constructs")
        .into_parts()
        .1;
    let pinned_repository = executor
        .repository_authority
        .repository()
        .expect("pinned original repository opens");
    fs::write(root.join(TRACKED_PATH), CHANGED_CONTENT).expect("original change writes");
    let retired = parent.path().join("retired");
    fs::rename(&root, &retired).expect("original workspace retires");
    fs::create_dir(&root).expect("replacement root constructs");
    let replacement = Repository::init(&root).expect("replacement repository initializes");
    fs::write(root.join(TRACKED_PATH), INITIAL_CONTENT).expect("replacement file writes");
    commit_all(&replacement, INITIAL_MESSAGE);

    let failure = executor
        .stage(
            &pinned_repository,
            GitStageArguments {
                paths: vec![TRACKED_PATH.to_owned()],
            },
        )
        .expect_err("replacement during staging rejects");
    let replacement_index = replacement.index().expect("replacement index opens");
    let replacement_entry = replacement_index
        .get_path(Path::new(TRACKED_PATH), 0)
        .expect("replacement path remains indexed");
    let replacement_blob = replacement
        .find_blob(replacement_entry.id)
        .expect("replacement blob remains available");

    assert_eq!(failure, LocalGitFailure::Repository);
    assert_eq!(replacement_blob.content(), INITIAL_CONTENT.as_bytes());
}
