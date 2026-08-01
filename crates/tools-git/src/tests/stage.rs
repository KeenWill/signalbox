//! Index staging properties.

use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use git2::Repository;
use rustix::fs::{CWD, Mode, mkfifoat};
use signalbox_tools_workspace::LocalWorkspaceFileSystem;

use crate::arguments::{GitStageArguments, LocalOperation};
use crate::catalog::LocalGitTools;
use crate::failure::LocalGitFailure;
use crate::limits::MAX_INDEX_ENTRIES;
use crate::tests::planting::{plant_maximum_index, plant_over_budget_index};
use crate::tests::support::{
    CHANGED_CONTENT, Fixture, INITIAL_CONTENT, INITIAL_MESSAGE, ObservingIndexLockFileSystem,
    RENAMED_TRACKED_PATH, TRACKED_PATH, UNTRACKED_CONTENT, UNTRACKED_PATH, commit_all, execute,
    identity, index_extension, install_deleted_conflict,
};

#[test]
fn stage_records_real_worktree_content() {
    let fixture = Fixture::new();
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT).expect("fixture change writes");
    let executor = fixture.executor();

    execute(
        &executor,
        LocalOperation::Stage(GitStageArguments {
            paths: vec![TRACKED_PATH.to_owned()],
        }),
    );
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let index = repository.index().expect("fixture index opens");
    let entry = index
        .get_path(Path::new(TRACKED_PATH), 0)
        .expect("staged path exists");
    let blob = repository.find_blob(entry.id).expect("staged blob exists");

    assert_eq!(blob.content(), CHANGED_CONTENT.as_bytes());
}

#[test]
fn stage_normalizes_lexical_path_spelling_before_indexing() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT).expect("fixture change writes");
    let lexical_path = format!("./{TRACKED_PATH}");

    executor
        .stage(
            &executor
                .repository_authority
                .repository()
                .expect("pinned fixture repository opens"),
            GitStageArguments {
                paths: vec![lexical_path],
            },
        )
        .expect("lexically redundant path stages");
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let index = repository.index().expect("fixture index opens");

    assert!(index.get_path(Path::new(TRACKED_PATH), 0).is_some());
    assert_eq!(index.len(), 1);
}

#[test]
fn stage_preserves_repository_index_permissions() {
    let fixture = Fixture::new();
    fs::set_permissions(
        fixture.root().join(".git/index"),
        fs::Permissions::from_mode(0o660),
    )
    .expect("fixture index permissions set");
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT).expect("fixture change writes");
    let executor = fixture.executor();

    execute(
        &executor,
        LocalOperation::Stage(GitStageArguments {
            paths: vec![TRACKED_PATH.to_owned()],
        }),
    );
    let mode = fs::metadata(fixture.root().join(".git/index"))
        .expect("updated index metadata reads")
        .permissions()
        .mode()
        & 0o777;

    assert_eq!(mode, 0o660);
}

#[test]
fn stage_creates_a_missing_index_with_repository_shared_permissions() {
    let fixture = Fixture::new();
    let git_directory_mode = 0o2770;
    let expected_index_mode = (git_directory_mode & 0o666) | 0o600;
    fs::set_permissions(
        fixture.root().join(".git"),
        fs::Permissions::from_mode(git_directory_mode),
    )
    .expect("fixture Git directory permissions set");
    fs::remove_file(fixture.root().join(".git/index")).expect("fixture index removes");
    let executor = fixture.executor();

    execute(
        &executor,
        LocalOperation::Stage(GitStageArguments {
            paths: vec![TRACKED_PATH.to_owned()],
        }),
    );
    let installed_mode = fs::metadata(fixture.root().join(".git/index"))
        .expect("created index metadata reads")
        .permissions()
        .mode()
        & 0o777;

    assert_eq!(installed_mode, expected_index_mode);
}

#[test]
fn stage_revalidates_the_injected_root_immediately_before_index_publication() {
    let parent = tempfile::tempdir().expect("workspace parent constructs");
    let root = parent.path().join("workspace");
    let retired = parent.path().join("retired");
    fs::create_dir(&root).expect("workspace root constructs");
    let original = Repository::init(&root).expect("original repository initializes");
    fs::write(root.join(TRACKED_PATH), INITIAL_CONTENT).expect("original fixture file writes");
    commit_all(&original, INITIAL_MESSAGE);
    fs::write(root.join(TRACKED_PATH), CHANGED_CONTENT).expect("original fixture change writes");
    let executor = LocalGitTools::try_new(LocalWorkspaceFileSystem, &root, identity())
        .expect("local Git suite constructs")
        .into_parts()
        .1;
    let original_index = fs::read(root.join(".git/index")).expect("original fixture index reads");
    let repository = executor
        .repository_authority
        .repository()
        .expect("pinned original repository opens");
    let mut replacement_index = Vec::new();

    let failure = executor
        .stage_with_pre_publish_hook(
            &repository,
            GitStageArguments {
                paths: vec![TRACKED_PATH.to_owned()],
            },
            || {
                fs::rename(&root, &retired).expect("original workspace retires");
                fs::create_dir(&root).expect("replacement workspace constructs");
                let replacement =
                    Repository::init(&root).expect("replacement repository initializes");
                fs::write(root.join(TRACKED_PATH), INITIAL_CONTENT)
                    .expect("replacement fixture file writes");
                commit_all(&replacement, INITIAL_MESSAGE);
                replacement_index =
                    fs::read(root.join(".git/index")).expect("replacement fixture index reads");
            },
        )
        .expect_err("root replacement rejects before index publication");

    assert_eq!(failure, LocalGitFailure::Repository);
    assert_eq!(
        fs::read(retired.join(".git/index")).expect("retired index reads"),
        original_index
    );
    assert_eq!(
        fs::read(root.join(".git/index")).expect("replacement index reads"),
        replacement_index
    );
}

#[test]
fn stage_rejects_an_index_over_the_entry_budget_before_staging() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    plant_over_budget_index(&repository);
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Stage(GitStageArguments {
            paths: vec![TRACKED_PATH.to_owned()],
        }))
        .expect_err("oversized index rejects staging");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn stage_rejects_an_entry_that_would_exceed_the_index_budget() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    plant_maximum_index(&repository);
    fs::write(fixture.root().join(UNTRACKED_PATH), UNTRACKED_CONTENT)
        .expect("new fixture file writes");
    let executor = fixture.executor();

    let failure = executor
        .stage(
            &repository,
            GitStageArguments {
                paths: vec![UNTRACKED_PATH.to_owned()],
            },
        )
        .expect_err("entry beyond the index budget rejects");
    let observed = repository.index().expect("fixture index reopens");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(observed.len(), MAX_INDEX_ENTRIES);
    assert!(observed.get_path(Path::new(UNTRACKED_PATH), 0).is_none());
}

#[test]
fn stage_preserves_index_mode_when_core_filemode_is_false() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let original_mode = repository
        .index()
        .expect("fixture index opens")
        .get_path(Path::new(TRACKED_PATH), 0)
        .expect("original tracked path exists")
        .mode;
    repository
        .config()
        .expect("fixture config opens")
        .set_bool("core.filemode", false)
        .expect("fixture filemode disables");
    fs::set_permissions(
        fixture.root().join(TRACKED_PATH),
        fs::Permissions::from_mode(0o755),
    )
    .expect("fixture executable mode sets");
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
        .expect("fixture content change writes");
    let executor = fixture.executor();

    execute(
        &executor,
        LocalOperation::Stage(GitStageArguments {
            paths: vec![TRACKED_PATH.to_owned()],
        }),
    );
    let index = repository.index().expect("fixture index reopens");
    let entry = index
        .get_path(Path::new(TRACKED_PATH), 0)
        .expect("staged path exists");

    assert_eq!(entry.mode, original_mode);
}

#[test]
fn stage_records_exact_descriptor_bytes_without_attribute_filtering() {
    let fixture = Fixture::new();
    fs::write(fixture.root().join(".gitattributes"), "*.txt text eol=lf\n")
        .expect("fixture attributes write");
    fs::write(fixture.root().join(TRACKED_PATH), b"first\r\nsecond\r\n")
        .expect("CRLF fixture content writes");
    let executor = fixture.executor();

    execute(
        &executor,
        LocalOperation::Stage(GitStageArguments {
            paths: vec![TRACKED_PATH.to_owned()],
        }),
    );
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let index = repository.index().expect("fixture index opens");
    let entry = index
        .get_path(Path::new(TRACKED_PATH), 0)
        .expect("fixture path is indexed");
    let blob = repository.find_blob(entry.id).expect("fixture blob exists");

    assert_eq!(blob.content(), b"first\r\nsecond\r\n");
}

#[test]
fn stage_never_opens_a_worktree_attribute_fifo() {
    let fixture = Fixture::new();
    let attributes_path = fixture.root().join(".gitattributes");
    mkfifoat(CWD, &attributes_path, Mode::RUSR | Mode::WUSR)
        .expect("worktree attributes FIFO constructs");
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
        .expect("fixture content change writes");
    let executor = fixture.executor();

    execute(
        &executor,
        LocalOperation::Stage(GitStageArguments {
            paths: vec![TRACKED_PATH.to_owned()],
        }),
    );
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let index = repository.index().expect("fixture index opens");
    let entry = index
        .get_path(Path::new(TRACKED_PATH), 0)
        .expect("fixture path is indexed");
    let blob = repository.find_blob(entry.id).expect("fixture blob exists");

    assert_eq!(blob.content(), CHANGED_CONTENT.as_bytes());
}

#[test]
fn stage_rejects_a_repository_attribute_fifo_without_opening_it() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let attributes_path = fixture.root().join(".git/info/attributes");
    mkfifoat(CWD, &attributes_path, Mode::RUSR | Mode::WUSR)
        .expect("repository attributes FIFO constructs");
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
        .expect("fixture content change writes");

    let failure = executor
        .execute_operation(LocalOperation::Stage(GitStageArguments {
            paths: vec![TRACKED_PATH.to_owned()],
        }))
        .expect_err("repository attributes FIFO rejects without blocking");

    assert_eq!(failure, LocalGitFailure::Repository);
}

#[test]
fn stage_holds_index_lock_while_reading_worktree_bytes() {
    let fixture = Fixture::new();
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
        .expect("requested fixture change writes");
    let lock_observed = Arc::new(AtomicBool::new(false));
    let filesystem = ObservingIndexLockFileSystem {
        root_path: fixture.root().to_owned(),
        lock_observed: Arc::clone(&lock_observed),
    };
    let executor = LocalGitTools::try_new(filesystem, fixture.root(), identity())
        .expect("observing-index suite constructs")
        .into_parts()
        .1;

    let result = executor
        .stage(
            &executor
                .repository_authority
                .repository()
                .expect("pinned fixture repository opens"),
            GitStageArguments {
                paths: vec![TRACKED_PATH.to_owned()],
            },
        )
        .expect("locked staging succeeds");
    let repository = Repository::open(fixture.root()).expect("fixture repository reopens");
    let index = repository.index().expect("fixture index reopens");

    assert_eq!(result.staged_paths, 1);
    assert!(lock_observed.load(Ordering::SeqCst));
    assert!(index.get_path(Path::new(TRACKED_PATH), 0).is_some());
}

#[test]
fn stage_preserves_a_monolithic_index_resolve_undo_extension() {
    let fixture = Fixture::new();
    install_deleted_conflict(&fixture);
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
        .expect("resolved fixture file writes");
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let mut index = repository.index().expect("fixture index opens");
    index
        .add_path(Path::new(TRACKED_PATH))
        .expect("fixture conflict resolves");
    index.write().expect("fixture resolve-undo index writes");
    let expected_extension = index_extension(
        &fs::read(fixture.root().join(".git/index")).expect("fixture index reads"),
        b"REUC",
    );
    fs::write(fixture.root().join(UNTRACKED_PATH), UNTRACKED_CONTENT)
        .expect("unrelated fixture file writes");
    let executor = fixture.executor();

    execute(
        &executor,
        LocalOperation::Stage(GitStageArguments {
            paths: vec![UNTRACKED_PATH.to_owned()],
        }),
    );
    let observed_extension = index_extension(
        &fs::read(fixture.root().join(".git/index")).expect("staged index reads"),
        b"REUC",
    );

    assert_eq!(observed_extension, expected_extension);
}

#[test]
fn stage_rejects_an_existing_index_lock_before_reading_files() {
    let fixture = Fixture::new();
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
        .expect("requested fixture change writes");
    fs::write(fixture.root().join(".git/index.lock"), []).expect("competing index lock constructs");
    let executor = fixture.executor();

    let failure = executor
        .stage(
            &executor
                .repository_authority
                .repository()
                .expect("pinned fixture repository opens"),
            GitStageArguments {
                paths: vec![TRACKED_PATH.to_owned()],
            },
        )
        .expect_err("competing index lock rejects staging");
    let repository = Repository::open(fixture.root()).expect("fixture repository reopens");
    let index = repository.index().expect("fixture index reopens");
    let tracked = index
        .get_path(Path::new(TRACKED_PATH), 0)
        .expect("original tracked path remains indexed");
    let blob = repository
        .find_blob(tracked.id)
        .expect("original tracked blob remains available");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(blob.content(), INITIAL_CONTENT.as_bytes());
}

#[test]
fn stage_constructs_and_commits_a_missing_empty_index_under_lock() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.root().join(".git/index")).expect("fixture index removes");
    fs::write(fixture.root().join(RENAMED_TRACKED_PATH), CHANGED_CONTENT)
        .expect("new fixture path writes");
    let executor = fixture.executor();

    execute(
        &executor,
        LocalOperation::Stage(GitStageArguments {
            paths: vec![RENAMED_TRACKED_PATH.to_owned()],
        }),
    );
    let repository = Repository::open(fixture.root()).expect("fixture repository reopens");
    let index = repository.index().expect("fixture index recreates");

    assert!(index.get_path(Path::new(RENAMED_TRACKED_PATH), 0).is_some());
}

#[test]
fn stage_rejects_intermediate_symlink_escape() {
    let fixture = Fixture::new();
    let outside = tempfile::tempdir().expect("outside directory constructs");
    fs::write(outside.path().join("outside.txt"), CHANGED_CONTENT).expect("outside file writes");
    symlink(outside.path(), fixture.root().join("escape")).expect("escaping symlink constructs");
    let executor = fixture.executor();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");

    let failure = executor
        .stage(
            &repository,
            GitStageArguments {
                paths: vec!["escape/outside.txt".to_owned()],
            },
        )
        .expect_err("escaping path rejects");

    assert_eq!(failure, LocalGitFailure::Path);
}
