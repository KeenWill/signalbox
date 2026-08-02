//! Review regressions for operation authority and Git-format parity.

use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
};

use git2::{
    BranchType, IndexEntry, IndexTime, ObjectFormat, Odb, Repository, RepositoryInitOptions,
    Signature,
};

use crate::LocalGitExecutor;
use crate::arguments::{
    GitBranchCreateArguments, GitBranchSwitchArguments, GitCommitArguments, GitDiffArguments,
    GitLogArguments, GitStageArguments, LocalOperation,
};
use crate::bounded::RevisionSnapshot;
use crate::branch::branch_create;
use crate::failure::LocalGitFailure;
use crate::limits::{
    GITLINK_MODE, INDEX_SKIP_WORKTREE, MAX_BRANCH_BYTES, MAX_COMMIT_MESSAGE_BYTES, MAX_LOG_ENTRIES,
    MAX_REVISION_BYTES, MAX_STAGE_PATHS,
};
use crate::pinning::{PinnedObjectDatabase, parse_pack_index};
use crate::reflog::ReferenceLogLock;
use crate::rollback::{
    atomic_restore_checkout_path, capture_rollback_identities, capture_worktree_rollback_state,
};
use crate::status_reference::StatusHeadSnapshot;
use crate::tests::support::{
    CHANGED_CONTENT, FIX_BRANCH, Fixture, INITIAL_CONTENT, MODEL_MESSAGE, Sha256Fixture,
    TRACKED_PATH, commit_all, execute, identity, install_deleted_conflict,
    real_git_sha256_pack_checksum, real_git_sha256_pack_index, real_git_sha256_pack_object_ids,
};

#[test]
fn commit_arguments_reject_an_empty_message_during_decode() {
    let result = serde_json::from_value::<GitCommitArguments>(serde_json::json!({"message": ""}));

    assert!(result.is_err());
}

#[test]
fn commit_arguments_reject_a_nul_message_during_decode() {
    let result = serde_json::from_value::<GitCommitArguments>(
        serde_json::json!({"message": "subject\u{0000}body"}),
    );

    assert!(result.is_err());
}

#[test]
fn commit_arguments_reject_an_over_byte_budget_message_during_decode() {
    let message = "x".repeat(MAX_COMMIT_MESSAGE_BYTES + 1);
    let result = serde_json::from_value::<GitCommitArguments>(serde_json::json!({
        "message": message,
    }));

    assert!(result.is_err());
}

#[test]
fn log_arguments_reject_zero_entries_during_decode() {
    let result = serde_json::from_value::<GitLogArguments>(serde_json::json!({
        "max_entries": 0,
    }));

    assert!(result.is_err());
}

#[test]
fn log_arguments_reject_entries_above_the_bound_during_decode() {
    let result = serde_json::from_value::<GitLogArguments>(serde_json::json!({
        "max_entries": MAX_LOG_ENTRIES + 1,
    }));

    assert!(result.is_err());
}

#[test]
fn stage_arguments_reject_an_empty_path_list_during_decode() {
    let result = serde_json::from_value::<GitStageArguments>(serde_json::json!({
        "paths": [],
    }));

    assert!(result.is_err());
}

#[test]
fn stage_arguments_reject_a_path_list_above_the_bound_during_decode() {
    let paths = vec![TRACKED_PATH; MAX_STAGE_PATHS + 1];
    let result = serde_json::from_value::<GitStageArguments>(serde_json::json!({
        "paths": paths,
    }));

    assert!(result.is_err());
}

#[test]
fn branch_arguments_reject_a_name_above_the_byte_bound_during_decode() {
    let name = "a".repeat(MAX_BRANCH_BYTES + 1);
    let result = serde_json::from_value::<GitBranchSwitchArguments>(serde_json::json!({
        "name": name,
    }));

    assert!(result.is_err());
}

#[test]
fn revision_arguments_reject_a_value_above_the_byte_bound_during_decode() {
    let revision = "a".repeat(MAX_REVISION_BYTES + 1);
    let result = serde_json::from_value::<GitLogArguments>(serde_json::json!({
        "revision": revision,
    }));

    assert!(result.is_err());
}

#[test]
fn stage_rejects_a_nested_repository_administration_path() {
    let fixture = Fixture::new();
    let nested_administration_path = "vendor/repository/.git/config";
    fs::create_dir_all(fixture.root().join("vendor/repository/.git"))
        .expect("nested administration directory creates");
    fs::write(
        fixture.root().join(nested_administration_path),
        INITIAL_CONTENT,
    )
    .expect("nested administration fixture writes");
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Stage(GitStageArguments {
            paths: vec![nested_administration_path.to_owned()],
        }))
        .expect_err("nested administration path rejects");

    assert_eq!(failure, LocalGitFailure::Path);
}

#[test]
fn branch_create_rejects_a_name_outside_git_reference_grammar() {
    let fixture = Fixture::new();
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::BranchCreate(GitBranchCreateArguments {
            name: "topic..escape".to_owned(),
            start: fixture.initial.to_string(),
        }))
        .expect_err("invalid branch name rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert!(
        !fixture
            .root()
            .join(".git/refs/heads/topic..escape")
            .exists()
    );
}

#[test]
fn branch_create_rejects_a_start_reference_changed_before_publication() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT).expect("fixture change writes");
    commit_all(&repository, MODEL_MESSAGE);
    let executor = fixture.executor();
    let pinned_objects =
        PinnedObjectDatabase::capture(&executor.repository_authority).expect("objects capture");
    let object_database = Odb::new_ext(executor.repository_authority.object_format)
        .expect("private object database constructs");
    pinned_objects
        .add_to(&object_database)
        .expect("captured objects install");
    repository
        .set_odb(&object_database)
        .expect("captured objects bind");

    let failure = branch_create(
        &repository,
        &executor.repository_authority,
        &object_database,
        GitBranchCreateArguments {
            name: FIX_BRANCH.to_owned(),
            start: "refs/heads/main".to_owned(),
        },
        || {
            fs::write(
                fixture.root().join(".git/refs/heads/main"),
                format!("{}\n", fixture.initial),
            )
            .expect("racing branch tip writes");
            Ok(())
        },
    )
    .expect_err("changed start reference rejects branch publication");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert!(!fixture.root().join(".git/refs/heads/agent/fix").exists());
}

#[test]
fn branch_switch_checks_out_root_level_changes_inside_the_injected_root() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial = repository
        .find_commit(fixture.initial)
        .expect("fixture initial commit exists");
    repository
        .branch(FIX_BRANCH, &initial, false)
        .expect("fixture branch creates");
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
        .expect("root-level fixture change writes");
    commit_all(&repository, MODEL_MESSAGE);
    let executor = fixture.executor();

    execute(
        &executor,
        LocalOperation::BranchSwitch(GitBranchSwitchArguments {
            name: FIX_BRANCH.to_owned(),
        }),
    );
    let status = execute(&executor, LocalOperation::Status);

    assert_eq!(
        fs::read(fixture.root().join(TRACKED_PATH)).expect("switched content reads"),
        INITIAL_CONTENT.as_bytes()
    );
    assert_eq!(
        repository.head().expect("switched HEAD exists").shorthand(),
        Ok(FIX_BRANCH)
    );
    assert_eq!(status["branch"], FIX_BRANCH);
}

#[test]
fn branch_switch_rejects_merge_state_from_the_injected_git_directory() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial = repository
        .find_commit(fixture.initial)
        .expect("fixture initial commit exists");
    repository
        .branch(FIX_BRANCH, &initial, false)
        .expect("fixture branch creates");
    fs::write(
        fixture.root().join(".git/MERGE_HEAD"),
        format!("{}\n", fixture.initial),
    )
    .expect("merge state writes");
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::BranchSwitch(GitBranchSwitchArguments {
            name: FIX_BRANCH.to_owned(),
        }))
        .expect_err("merge state blocks branch switching");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        repository
            .head()
            .expect("original HEAD remains")
            .shorthand(),
        Ok("main")
    );
}

#[test]
fn branch_switch_rejects_operation_state_created_before_head_publication() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial = repository
        .find_commit(fixture.initial)
        .expect("fixture initial commit exists");
    repository
        .branch(FIX_BRANCH, &initial, false)
        .expect("fixture branch creates");
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT).expect("fixture change writes");
    commit_all(&repository, MODEL_MESSAGE);
    let executor = fixture.executor();

    let failure = executor
        .branch_switch_with_head_publish_hook(
            &repository,
            GitBranchSwitchArguments {
                name: FIX_BRANCH.to_owned(),
            },
            || {
                fs::write(
                    fixture.root().join(".git/MERGE_HEAD"),
                    format!("{}\n", fixture.initial),
                )
                .expect("racing merge state writes");
            },
        )
        .expect_err("changed operation state rejects HEAD publication");
    let observed = Repository::open(fixture.root()).expect("fixture repository reopens");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        observed.head().expect("original HEAD remains").shorthand(),
        Ok("main")
    );
}

#[test]
fn checkout_rollback_preserves_a_same_content_foreign_replacement() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let tracked_path = Path::new(TRACKED_PATH);
    fs::write(fixture.root().join(tracked_path), CHANGED_CONTENT)
        .expect("checked-out fixture content writes");
    let checkout_paths = BTreeSet::from([tracked_path.to_path_buf()]);
    let expected =
        capture_worktree_rollback_state(&executor.filesystem, &executor.root, &checkout_paths)
            .expect("checked-out state captures");
    let expected_identities = capture_rollback_identities(
        &executor.repository_authority.root,
        Path::new(""),
        &expected,
    )
    .expect("checked-out identities capture");
    let original_prefix = Path::new(".rollback-original");
    fs::create_dir(fixture.root().join(original_prefix))
        .expect("rollback original directory creates");
    fs::write(
        fixture.root().join(original_prefix).join(tracked_path),
        INITIAL_CONTENT,
    )
    .expect("rollback original content writes");
    let replacement_path = fixture.root().join("foreign-replacement");
    fs::write(&replacement_path, CHANGED_CONTENT).expect("foreign replacement writes");
    fs::rename(&replacement_path, fixture.root().join(tracked_path))
        .expect("foreign replacement publishes");

    atomic_restore_checkout_path(
        &executor.filesystem,
        &executor.root,
        &executor.repository_authority,
        original_prefix,
        tracked_path,
        &expected,
        Some(&expected_identities),
    )
    .expect("foreign replacement is preserved");

    assert_eq!(
        fs::read(fixture.root().join(tracked_path)).expect("foreign replacement reads"),
        CHANGED_CONTENT.as_bytes()
    );
    assert_eq!(
        fs::read(fixture.root().join(original_prefix).join(tracked_path))
            .expect("rollback original reads"),
        INITIAL_CONTENT.as_bytes()
    );
}

#[test]
fn reflog_publication_rejects_a_live_leaf_changed_after_snapshot() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let reflog_path = fixture.root().join(".git/logs/HEAD");
    let foreign_record = b"foreign reflog record\n";
    let mut expected_live = fs::read(&reflog_path).expect("original reflog reads");
    expected_live.extend_from_slice(foreign_record);
    let mut lock = ReferenceLogLock::acquire(&executor.repository_authority, "HEAD")
        .expect("reflog lock acquires");
    fs::OpenOptions::new()
        .append(true)
        .open(&reflog_path)
        .expect("live reflog opens for append")
        .write_all(foreign_record)
        .expect("foreign reflog record appends");

    let failure = lock
        .publish()
        .expect_err("changed live reflog rejects publication");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(&reflog_path).expect("foreign live reflog reads"),
        expected_live
    );
}

#[test]
fn branch_switch_rejects_a_skip_worktree_index_entry() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial = repository
        .find_commit(fixture.initial)
        .expect("fixture initial commit exists");
    repository
        .branch(FIX_BRANCH, &initial, false)
        .expect("fixture branch creates");
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT).expect("fixture change writes");
    commit_all(&repository, MODEL_MESSAGE);
    let mut index = repository.index().expect("fixture index opens");
    let mut entry = index
        .get_path(Path::new(TRACKED_PATH), 0)
        .expect("tracked entry exists");
    entry.flags_extended |= INDEX_SKIP_WORKTREE;
    index.add(&entry).expect("skip-worktree entry replaces");
    index.write().expect("skip-worktree index publishes");
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::BranchSwitch(GitBranchSwitchArguments {
            name: FIX_BRANCH.to_owned(),
        }))
        .expect_err("skip-worktree index rejects branch switching");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(fixture.root().join(TRACKED_PATH)).expect("worktree content remains"),
        CHANGED_CONTENT.as_bytes()
    );
}

#[test]
fn status_rejects_an_unsupported_gitlink_worktree() {
    let fixture = Fixture::new();
    install_gitlink_index_entry(&fixture);
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Status)
        .expect_err("gitlink worktree status rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn worktree_diff_rejects_an_unsupported_gitlink_worktree() {
    let fixture = Fixture::new();
    install_gitlink_index_entry(&fixture);
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Diff(GitDiffArguments::Worktree))
        .expect_err("gitlink worktree diff rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn status_head_snapshot_rejects_a_branch_tip_changed_during_the_operation() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let snapshot = StatusHeadSnapshot::capture(&executor.repository_authority)
        .expect("status HEAD snapshot captures");
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT).expect("fixture change writes");
    commit_all(&repository, MODEL_MESSAGE);

    let failure = snapshot
        .validate(&executor.repository_authority)
        .expect_err("changed branch tip rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn revision_snapshot_rejects_a_branch_tip_changed_during_the_operation() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let snapshot = RevisionSnapshot::capture(&executor.repository_authority, "refs/heads/main")
        .expect("revision snapshot captures");
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT).expect("fixture change writes");
    commit_all(&repository, MODEL_MESSAGE);

    let failure = snapshot
        .validate(&executor.repository_authority)
        .expect_err("changed revision rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn commit_consumes_real_git_merge_state_and_preserves_both_parents() {
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
    let commit = repository
        .find_commit(
            git2::Oid::from_str(result["commit"].as_str().expect("commit id is text"))
                .expect("commit id parses"),
        )
        .expect("merge commit exists");

    assert_eq!(commit.parent_count(), 2);
    assert_eq!(repository.state(), git2::RepositoryState::Clean);
    assert_eq!(result["state_cleaned"], true);
}

#[test]
fn sha256_status_recognizes_an_unchanged_worktree_blob() {
    let fixture = Sha256Fixture::new();
    let executor = fixture.executor();

    let status = execute(&executor, LocalOperation::Status);

    assert_eq!(status["entries"], serde_json::json!([]));
}

#[test]
fn real_git_sha256_pack_index_matches_the_bounded_parser() {
    let index = real_git_sha256_pack_index();
    let checksum = real_git_sha256_pack_checksum();
    let expected = real_git_sha256_pack_object_ids();

    let parsed = parse_pack_index(&index, checksum, ObjectFormat::Sha256)
        .expect("real Git SHA-256 pack index parses");

    assert_eq!(parsed, expected);
}

#[test]
fn sha256_branch_create_uses_a_matching_temporary_object_database() {
    let fixture = Sha256Fixture::new();
    let executor = fixture.executor();

    execute(
        &executor,
        LocalOperation::BranchCreate(GitBranchCreateArguments {
            name: FIX_BRANCH.to_owned(),
            start: fixture.initial.to_string(),
        }),
    );
    let repository = Repository::open(fixture.root()).expect("SHA-256 repository reopens");

    assert_eq!(
        repository
            .find_branch(FIX_BRANCH, BranchType::Local)
            .expect("created SHA-256 branch exists")
            .get()
            .target(),
        Some(fixture.initial)
    );
}

#[test]
fn sha256_initial_commit_records_the_matching_zero_object_id() {
    let directory = tempfile::tempdir().expect("temporary SHA-256 repository root constructs");
    let mut options = RepositoryInitOptions::new();
    options
        .external_template(false)
        .initial_head("main")
        .object_format(ObjectFormat::Sha256);
    Repository::init_opts(directory.path(), &options).expect("SHA-256 repository initializes");
    fs::write(directory.path().join(TRACKED_PATH), INITIAL_CONTENT)
        .expect("SHA-256 worktree file writes");
    let executor = LocalGitExecutor::for_test(directory.path(), identity());
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
    let reflog = fs::read_to_string(directory.path().join(".git/logs/refs/heads/main"))
        .expect("SHA-256 branch reflog reads");
    let old = reflog
        .split_ascii_whitespace()
        .next()
        .expect("SHA-256 reflog old id exists");

    assert_eq!(old, git2::Oid::ZERO_SHA256.to_string());
    assert_eq!(
        result["commit"]
            .as_str()
            .expect("SHA-256 commit id is text")
            .len(),
        git2::Oid::ZERO_SHA256.to_string().len()
    );
}

#[test]
fn sha256_unborn_branch_switch_records_the_matching_zero_object_id() {
    let directory = tempfile::tempdir().expect("temporary SHA-256 repository root constructs");
    let mut options = RepositoryInitOptions::new();
    options
        .external_template(false)
        .initial_head("main")
        .object_format(ObjectFormat::Sha256);
    let repository =
        Repository::init_opts(directory.path(), &options).expect("SHA-256 repository initializes");
    repository
        .index()
        .expect("empty SHA-256 index opens")
        .write()
        .expect("empty SHA-256 index writes");
    let blob = repository
        .blob(INITIAL_CONTENT.as_bytes())
        .expect("SHA-256 target blob writes");
    let mut builder = repository
        .treebuilder(None)
        .expect("SHA-256 target tree builder opens");
    builder
        .insert(TRACKED_PATH, blob, 0o100644)
        .expect("SHA-256 target path inserts");
    let tree = repository
        .find_tree(builder.write().expect("SHA-256 target tree writes"))
        .expect("SHA-256 target tree opens");
    let signature = Signature::now("Fixture", "fixture@example.test")
        .expect("SHA-256 fixture signature constructs");
    let target = repository
        .commit(None, &signature, &signature, "target", &tree, &[])
        .expect("SHA-256 target commit writes");
    repository
        .reference("refs/heads/agent/fix", target, false, "fixture target")
        .expect("SHA-256 target branch writes");
    drop(tree);
    drop(builder);
    drop(repository);
    let executor = LocalGitExecutor::for_test(directory.path(), identity());

    execute(
        &executor,
        LocalOperation::BranchSwitch(GitBranchSwitchArguments {
            name: FIX_BRANCH.to_owned(),
        }),
    );
    let reflog = fs::read_to_string(directory.path().join(".git/logs/HEAD"))
        .expect("SHA-256 HEAD reflog reads");
    let old = reflog
        .split_ascii_whitespace()
        .next()
        .expect("SHA-256 reflog old id exists");

    assert_eq!(old, git2::Oid::ZERO_SHA256.to_string());
    assert_eq!(
        fs::read(directory.path().join(TRACKED_PATH)).expect("switched SHA-256 content reads"),
        INITIAL_CONTENT.as_bytes()
    );
}

#[test]
fn commit_rejects_an_ordinary_no_op_before_writing_a_commit() {
    let fixture = Fixture::new();
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Commit(GitCommitArguments {
            message: MODEL_MESSAGE.to_owned(),
        }))
        .expect_err("ordinary no-op commit rejects");
    let repository = Repository::open(fixture.root()).expect("fixture repository reopens");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        repository.head().expect("HEAD remains").target(),
        Some(fixture.initial)
    );
}

#[test]
fn worktree_diff_does_not_preserve_a_symlink_mode_for_a_regular_file() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    fs::remove_file(fixture.root().join(TRACKED_PATH)).expect("regular fixture file removes");
    symlink(INITIAL_CONTENT, fixture.root().join(TRACKED_PATH)).expect("fixture symlink creates");
    commit_all(&repository, "symlink");
    repository
        .config()
        .expect("fixture config opens")
        .set_bool("core.filemode", false)
        .expect("fixture filemode disables");
    fs::remove_file(fixture.root().join(TRACKED_PATH)).expect("fixture symlink removes");
    fs::write(fixture.root().join(TRACKED_PATH), INITIAL_CONTENT)
        .expect("replacement regular file writes");
    fs::set_permissions(
        fixture.root().join(TRACKED_PATH),
        fs::Permissions::from_mode(0o644),
    )
    .expect("replacement regular mode sets");
    let executor = fixture.executor();

    let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));

    assert!(
        diff["patch"]
            .as_str()
            .expect("worktree patch is text")
            .contains("old mode 120000\nnew mode 100644\n")
    );
}

#[test]
fn stage_does_not_preserve_a_symlink_mode_for_a_regular_file() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    fs::remove_file(fixture.root().join(TRACKED_PATH)).expect("regular fixture file removes");
    symlink(INITIAL_CONTENT, fixture.root().join(TRACKED_PATH)).expect("fixture symlink creates");
    commit_all(&repository, "symlink");
    repository
        .config()
        .expect("fixture config opens")
        .set_bool("core.filemode", false)
        .expect("fixture filemode disables");
    fs::remove_file(fixture.root().join(TRACKED_PATH)).expect("fixture symlink removes");
    fs::write(fixture.root().join(TRACKED_PATH), INITIAL_CONTENT)
        .expect("replacement regular file writes");
    fs::set_permissions(
        fixture.root().join(TRACKED_PATH),
        fs::Permissions::from_mode(0o644),
    )
    .expect("replacement regular mode sets");
    let executor = fixture.executor();

    execute(
        &executor,
        LocalOperation::Stage(GitStageArguments {
            paths: vec![TRACKED_PATH.to_owned()],
        }),
    );
    let staged_repository = Repository::open(fixture.root()).expect("staged repository reopens");
    let index = staged_repository.index().expect("staged index reopens");
    let entry = index
        .get_path(Path::new(TRACKED_PATH), 0)
        .expect("staged path exists");

    assert_eq!(entry.mode, 0o100644);
}

#[test]
fn stage_rejects_when_a_captured_live_object_disappears_before_publication() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial_blob = repository
        .find_commit(fixture.initial)
        .expect("initial commit opens")
        .tree()
        .expect("initial tree opens")
        .get_name(TRACKED_PATH)
        .expect("initial tree entry exists")
        .id();
    let initial_blob_text = initial_blob.to_string();
    let initial_blob_path = fixture
        .root()
        .join(".git/objects")
        .join(&initial_blob_text[..2])
        .join(&initial_blob_text[2..]);
    let original_index_id = repository
        .index()
        .expect("original index opens")
        .get_path(Path::new(TRACKED_PATH), 0)
        .expect("original index entry exists")
        .id;
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT).expect("fixture change writes");
    let executor = fixture.executor();

    let failure = executor
        .stage_with_pre_publish_hook(
            &repository,
            GitStageArguments {
                paths: vec![TRACKED_PATH.to_owned()],
            },
            || fs::remove_file(&initial_blob_path).expect("captured live blob removes"),
        )
        .expect_err("missing captured object rejects index publication");
    let observed_index = Repository::open(fixture.root())
        .expect("fixture repository reopens")
        .index()
        .expect("observed index opens");

    assert_eq!(failure, LocalGitFailure::Repository);
    assert_eq!(
        observed_index
            .get_path(Path::new(TRACKED_PATH), 0)
            .expect("observed index entry exists")
            .id,
        original_index_id
    );
}

#[test]
fn stage_rejects_when_its_new_object_pack_disappears_before_publication() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let original_index_id = repository
        .index()
        .expect("original index opens")
        .get_path(Path::new(TRACKED_PATH), 0)
        .expect("original index entry exists")
        .id;
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT).expect("fixture change writes");
    let executor = fixture.executor();

    let failure = executor
        .stage_with_pre_publish_hook(
            &repository,
            GitStageArguments {
                paths: vec![TRACKED_PATH.to_owned()],
            },
            || remove_first_installed_pack(fixture.root()),
        )
        .expect_err("missing newly installed objects reject index publication");
    let observed_index = Repository::open(fixture.root())
        .expect("fixture repository reopens")
        .index()
        .expect("observed index opens");

    assert_eq!(failure, LocalGitFailure::Repository);
    assert_eq!(
        observed_index
            .get_path(Path::new(TRACKED_PATH), 0)
            .expect("observed index entry exists")
            .id,
        original_index_id
    );
}

fn remove_first_installed_pack(root: &Path) {
    let pack = fs::read_dir(root.join(".git/objects/pack"))
        .expect("pack directory reads")
        .map(|entry| entry.expect("pack entry reads").path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "pack")
        })
        .expect("installed object pack exists");
    fs::remove_file(pack).expect("installed object pack removes");
}

fn install_gitlink_index_entry(fixture: &Fixture) {
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let mut index = repository.index().expect("fixture index opens");
    let entry = IndexEntry {
        ctime: IndexTime::new(0, 0),
        mtime: IndexTime::new(0, 0),
        dev: 0,
        ino: 0,
        mode: GITLINK_MODE,
        uid: 0,
        gid: 0,
        file_size: 0,
        id: fixture.initial,
        flags: 0,
        flags_extended: 0,
        path: b"submodule".to_vec(),
    };
    index.add(&entry).expect("gitlink index entry adds");
    index.write().expect("gitlink index publishes");
    fs::create_dir(fixture.root().join("submodule")).expect("gitlink worktree directory creates");
}
