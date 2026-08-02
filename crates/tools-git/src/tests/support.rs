//! Shared fixtures for the local Git tool tests.

use std::{fs, io::Write, path::Path};

use flate2::{Compression, write::ZlibEncoder};
use git2::{
    IndexAddOption, ObjectFormat, ObjectType, Oid, Repository, RepositoryInitOptions, Signature,
};
use tempfile::TempDir;

pub(super) const AUTHOR_NAME: &str = "Signalbox Fixer";

pub(super) const AUTHOR_EMAIL: &str = "fixer@example.test";

pub(super) const INITIAL_MESSAGE: &str = "initial";

pub(super) const TRACKED_PATH: &str = "tracked.txt";

pub(super) const INITIAL_CONTENT: &str = "before\n";

pub(super) struct Fixture {
    directory: TempDir,
    pub(super) initial: Oid,
}

pub(super) struct Sha256Fixture {
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

impl Sha256Fixture {
    pub(super) fn new() -> Self {
        let directory = tempfile::tempdir().expect("temporary SHA-256 repository root constructs");
        let mut options = RepositoryInitOptions::new();
        options
            .external_template(false)
            .initial_head("main")
            .object_format(ObjectFormat::Sha256);
        let repository = Repository::init_opts(directory.path(), &options)
            .expect("SHA-256 repository initializes");
        fs::write(directory.path().join(TRACKED_PATH), INITIAL_CONTENT)
            .expect("SHA-256 fixture file writes");
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

pub(super) fn plant_loose_blob(root: &Path, content: &[u8]) -> std::path::PathBuf {
    let object_id = Oid::hash_object(ObjectType::Blob, content).expect("blob object hashes");
    let object_id = object_id.to_string();
    let object_directory = root.join(".git/objects").join(&object_id[..2]);
    let object_path = object_directory.join(&object_id[2..]);
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    write!(encoder, "blob {}\0", content.len()).expect("blob header compresses");
    encoder.write_all(content).expect("blob content compresses");
    let compressed = encoder.finish().expect("blob compression finishes");
    fs::create_dir_all(&object_directory).expect("loose object directory constructs");
    fs::write(&object_path, compressed).expect("loose object writes");
    object_path
}
