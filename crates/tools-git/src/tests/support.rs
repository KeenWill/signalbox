//! Shared fixtures for the local Git tool tests.

use std::{fs, path::Path};

use git2::{IndexAddOption, ObjectType, Oid, Repository, Signature};
use tempfile::TempDir;

pub(super) const AUTHOR_NAME: &str = "Signalbox Fixer";

pub(super) const AUTHOR_EMAIL: &str = "fixer@example.test";

pub(super) const INITIAL_MESSAGE: &str = "initial";

pub(super) const FIX_BRANCH: &str = "agent/fix";

pub(super) const TRACKED_PATH: &str = "tracked.txt";

pub(super) const INITIAL_CONTENT: &str = "before\n";

pub(super) struct Fixture {
    directory: TempDir,
    pub(super) initial: Oid,
}

impl Fixture {
    pub(super) fn new() -> Self {
        let directory = tempfile::tempdir().expect("temporary repository root constructs");
        let repository = Repository::init(directory.path()).expect("repository initializes");
        fs::write(directory.path().join(TRACKED_PATH), INITIAL_CONTENT)
            .expect("fixture file writes");
        let initial = commit_all(&repository, INITIAL_MESSAGE);
        Self { directory, initial }
    }

    pub(super) fn root(&self) -> &Path {
        self.directory.path()
    }
}

pub(super) fn commit_all(repository: &Repository, message: &str) -> Oid {
    let mut index = repository.index().expect("fixture index opens");
    index
        .add_all(["*"], IndexAddOption::DEFAULT, None)
        .expect("fixture stages");
    index.write().expect("fixture index writes");
    commit_index(repository, message)
}

pub(super) fn commit_index(repository: &Repository, message: &str) -> Oid {
    let mut index = repository.index().expect("fixture index opens");
    let tree_id = index.write_tree().expect("fixture tree writes");
    let tree = repository.find_tree(tree_id).expect("fixture tree opens");
    let signature =
        Signature::now(AUTHOR_NAME, AUTHOR_EMAIL).expect("fixture signature constructs");
    let parent = repository
        .head()
        .ok()
        .and_then(|head| head.peel_to_commit().ok());
    let parents = parent.iter().collect::<Vec<_>>();
    repository
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &parents,
        )
        .expect("fixture commit writes")
}

pub(super) fn raw_commit_with_tree(repository: &Repository, tree: Oid, parent: Oid) -> Oid {
    let raw = format!(
        "tree {tree}\nparent {parent}\nauthor Signalbox <fixer@example.test> 0 +0000\ncommitter Signalbox <fixer@example.test> 0 +0000\n\ndeep tree\n"
    );
    repository
        .odb()
        .expect("fixture object database opens")
        .write(ObjectType::Commit, raw.as_bytes())
        .expect("fixture commit object writes")
}
