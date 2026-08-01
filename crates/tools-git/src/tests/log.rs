//! Bounded history properties.

use std::{collections::BTreeSet, fs};

use git2::{ObjectType, Repository, Signature};
use rustix::fs::{CWD, Mode, mkfifoat};
use signalbox_tools_workspace::LocalWorkspaceFileSystem;

use crate::arguments::{GitDiffArguments, GitLogArguments, LocalOperation};
use crate::catalog::LocalGitTools;
use crate::construction::LocalGitToolsConstructionError;
use crate::diff::diff;
use crate::failure::LocalGitFailure;
use crate::limits::{MAX_SHALLOW_ENTRIES, MAX_WORKTREE_INSPECTIONS};
use crate::log::log;
use crate::status::status;
use crate::tests::planting::{oversized_commit_object, plant_shallow_entries};
use crate::tests::support::{
    AUTHOR_EMAIL, AUTHOR_NAME, CHANGED_CONTENT, Fixture, MODEL_MESSAGE, TRACKED_PATH, commit_all,
    commit_with_parents, execute, identity, invalid_utf8_commit, long_author_email,
    long_author_name, plant_linear_history, raw_message_commit,
};

#[test]
fn log_uses_real_commits() {
    let fixture = Fixture::new();
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT).expect("fixture change writes");
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let changed = commit_all(&repository, MODEL_MESSAGE);
    let executor = fixture.executor();

    let log = execute(
        &executor,
        LocalOperation::Log(GitLogArguments {
            revision: changed.to_string(),
            max_entries: 1,
        }),
    );

    assert_eq!(log["commits"][0]["commit"], changed.to_string());
    assert_eq!(log["commits"][0]["message"], MODEL_MESSAGE);
}

#[test]
fn log_rejects_a_fifo_head_without_blocking() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let head_path = fixture.root().join(".git/HEAD");
    fs::remove_file(&head_path).expect("fixture HEAD removes");
    mkfifoat(CWD, &head_path, Mode::RUSR | Mode::WUSR).expect("fixture HEAD FIFO constructs");
    let repository = executor
        .repository_authority
        .repository()
        .expect("pinned fixture repository opens");

    let failure = log(
        &repository,
        &executor.repository_authority,
        GitLogArguments {
            revision: "HEAD".to_owned(),
            max_entries: 1,
        },
    )
    .expect_err("FIFO HEAD rejects without libgit2 reopen");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn status_and_worktree_diff_reject_a_fifo_head_without_blocking() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let head_path = fixture.root().join(".git/HEAD");
    fs::remove_file(&head_path).expect("fixture HEAD removes");
    mkfifoat(CWD, &head_path, Mode::RUSR | Mode::WUSR).expect("fixture HEAD FIFO constructs");
    let repository = executor
        .repository_authority
        .repository()
        .expect("pinned fixture repository opens");

    let status_failure = status(
        &repository,
        &executor.repository_authority,
        &executor.filesystem,
        &executor.root,
        Vec::new(),
    )
    .expect_err("status rejects a FIFO HEAD");
    let diff_failure = diff(
        &repository,
        &executor.repository_authority,
        GitDiffArguments::Worktree,
        &executor.filesystem,
        &executor.root,
        Vec::new(),
    )
    .expect_err("worktree diff rejects a FIFO HEAD");

    assert_eq!(status_failure, LocalGitFailure::Operation);
    assert_eq!(diff_failure, LocalGitFailure::Operation);
}

#[test]
fn log_stops_after_the_requested_page_in_a_long_history() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let newest = plant_linear_history(&repository, fixture.initial, MAX_WORKTREE_INSPECTIONS + 1);
    let executor = fixture.executor();

    let log = execute(
        &executor,
        LocalOperation::Log(GitLogArguments {
            revision: newest.to_string(),
            max_entries: 1,
        }),
    );

    assert_eq!(log["commits"][0]["commit"], newest.to_string());
    assert_eq!(log["truncated"], true);
}

#[test]
fn one_entry_log_does_not_order_an_unreturned_long_merge_parent() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let short_parent =
        commit_with_parents(&repository, &[fixture.initial], "short independent parent");
    let long_parent =
        plant_linear_history(&repository, fixture.initial, MAX_WORKTREE_INSPECTIONS + 1);
    let merge = commit_with_parents(
        &repository,
        &[short_parent, long_parent],
        "bounded merge page",
    );
    let executor = fixture.executor();

    let log = execute(
        &executor,
        LocalOperation::Log(GitLogArguments {
            revision: merge.to_string(),
            max_entries: 1,
        }),
    );

    assert_eq!(log["commits"][0]["commit"], merge.to_string());
    assert_eq!(log["truncated"], true);
}

#[test]
fn log_honors_a_repository_shallow_boundary() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let boundary = plant_linear_history(&repository, fixture.initial, 1);
    let newest = plant_linear_history(&repository, boundary, 1);
    fs::write(fixture.root().join(".git/shallow"), format!("{boundary}\n"))
        .expect("fixture shallow boundary writes");
    let executor = fixture.executor();

    let log = execute(
        &executor,
        LocalOperation::Log(GitLogArguments {
            revision: newest.to_string(),
            max_entries: 10,
        }),
    );

    assert_eq!(
        log["commits"]
            .as_array()
            .expect("commits are an array")
            .len(),
        2
    );
    assert_eq!(log["commits"][0]["commit"], newest.to_string());
    assert_eq!(log["commits"][1]["commit"], boundary.to_string());
}

#[test]
fn construction_rejects_a_shallow_file_over_the_entry_budget() {
    let fixture = Fixture::new();
    plant_shallow_entries(fixture.root(), fixture.initial, MAX_SHALLOW_ENTRIES + 1);

    let error = LocalGitTools::try_new(LocalWorkspaceFileSystem, fixture.root(), identity())
        .expect_err("over-budget shallow boundary rejects");

    assert!(matches!(error, LocalGitToolsConstructionError::Repository));
}

#[test]
fn log_never_emits_an_ancestor_before_a_direct_merge_parent() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let first = commit_with_parents(&repository, &[fixture.initial], "first parent");
    let second = commit_with_parents(&repository, &[fixture.initial], "second parent");
    let merge = commit_with_parents(&repository, &[first, second], "merge");
    let executor = fixture.executor();

    let log = execute(
        &executor,
        LocalOperation::Log(GitLogArguments {
            revision: merge.to_string(),
            max_entries: 3,
        }),
    );
    let returned_parents = BTreeSet::from([
        log["commits"][1]["commit"]
            .as_str()
            .expect("first returned parent is text"),
        log["commits"][2]["commit"]
            .as_str()
            .expect("second returned parent is text"),
    ])
    .into_iter()
    .map(str::to_owned)
    .collect::<BTreeSet<_>>();

    assert_eq!(log["commits"][0]["commit"], merge.to_string());
    assert_eq!(
        returned_parents,
        BTreeSet::from([first.to_string(), second.to_string()])
    );
}

#[test]
fn log_rejects_oversized_commit_object() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let oversized = oversized_commit_object(&repository, fixture.initial);
    repository
        .reference(
            "refs/heads/oversized",
            oversized,
            false,
            "fixture reference",
        )
        .expect("oversized fixture reference writes");
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Log(GitLogArguments {
            revision: "refs/heads/oversized".to_owned(),
            max_entries: 1,
        }))
        .expect_err("oversized commit object rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn log_rejects_an_exact_oid_before_loading_an_oversized_commit() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let oversized = oversized_commit_object(&repository, fixture.initial);
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Log(GitLogArguments {
            revision: oversized.to_string(),
            max_entries: 1,
        }))
        .expect_err("exact oversized commit object rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn log_peels_annotated_tag_to_commit() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial = repository
        .find_commit(fixture.initial)
        .expect("fixture commit exists");
    let signature =
        Signature::now(AUTHOR_NAME, AUTHOR_EMAIL).expect("fixture signature constructs");
    repository
        .tag("release", initial.as_object(), &signature, "release", false)
        .expect("annotated tag creates");
    let executor = fixture.executor();

    let log = execute(
        &executor,
        LocalOperation::Log(GitLogArguments {
            revision: "refs/tags/release".to_owned(),
            max_entries: 1,
        }),
    );

    assert_eq!(log["commits"][0]["commit"], fixture.initial.to_string());
}

#[test]
fn log_marks_truncated_author_identity() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let mut index = repository.index().expect("fixture index opens");
    let tree_id = index.write_tree().expect("fixture tree writes");
    let tree = repository.find_tree(tree_id).expect("fixture tree opens");
    let parent = repository
        .find_commit(fixture.initial)
        .expect("fixture parent commit exists");
    let author = Signature::now(&long_author_name(), &long_author_email())
        .expect("long fixture signature constructs");
    let committer =
        Signature::now(AUTHOR_NAME, AUTHOR_EMAIL).expect("fixture committer constructs");
    let commit = repository
        .commit(
            Some("HEAD"),
            &author,
            &committer,
            MODEL_MESSAGE,
            &tree,
            &[&parent],
        )
        .expect("long-author fixture commit writes");
    let executor = fixture.executor();

    let log = execute(
        &executor,
        LocalOperation::Log(GitLogArguments {
            revision: commit.to_string(),
            max_entries: 1,
        }),
    );

    assert_eq!(log["commits"][0]["author_name_truncated"], true);
    assert_eq!(log["commits"][0]["author_email_truncated"], true);
}

#[test]
fn log_marks_invalid_utf8_fields_incomplete() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let commit = invalid_utf8_commit(&repository, fixture.initial);
    let executor = fixture.executor();

    let log = execute(
        &executor,
        LocalOperation::Log(GitLogArguments {
            revision: commit.to_string(),
            max_entries: 1,
        }),
    );

    assert_eq!(log["commits"][0]["author_name_truncated"], true);
    assert_eq!(log["commits"][0]["author_email_truncated"], true);
    assert_eq!(log["commits"][0]["message_truncated"], true);
}

#[test]
fn log_preserves_raw_leading_message_newlines() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let commit = raw_message_commit(&repository, fixture.initial);
    let executor = fixture.executor();

    let log = execute(
        &executor,
        LocalOperation::Log(GitLogArguments {
            revision: commit.to_string(),
            max_entries: 1,
        }),
    );

    assert_eq!(log["commits"][0]["message"], "\n\nmessage\n");
    assert_eq!(log["commits"][0]["message_truncated"], false);
}

#[test]
fn log_bounds_the_extra_commit_used_only_for_truncation() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let oversized = oversized_commit_object(&repository, fixture.initial);
    let tree = repository
        .find_commit(fixture.initial)
        .expect("fixture initial commit opens")
        .tree_id();
    let raw_commit = format!(
        "tree {tree}\nparent {oversized}\nauthor Signalbox <fixer@example.test> 0 +0000\ncommitter Signalbox <fixer@example.test> 0 +0000\n\nsmall child\n"
    );
    let child = repository
        .odb()
        .expect("fixture object database opens")
        .write(ObjectType::Commit, raw_commit.as_bytes())
        .expect("small child commit writes");
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Log(GitLogArguments {
            revision: child.to_string(),
            max_entries: 1,
        }))
        .expect_err("oversized truncation candidate rejects before parsing");

    assert_eq!(failure, LocalGitFailure::Operation);
}
