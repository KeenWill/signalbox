//! Shared fixtures for the local Git tool tests.

use std::{fs, io::Write, path::Path};

use flate2::{Compression, write::ZlibEncoder};
use git2::{
    IndexAddOption, ObjectFormat, ObjectType, Oid, Repository, RepositoryInitOptions, Signature,
};
use signalbox_tools_workspace::{LocalWorkspaceFileSystem, WorkspaceRoot, WorkspaceRootIdentity};
use tempfile::TempDir;

pub(super) const AUTHOR_NAME: &str = "Signalbox Fixer";

pub(super) const AUTHOR_EMAIL: &str = "fixer@example.test";

pub(super) const INITIAL_MESSAGE: &str = "initial";

pub(super) const TRACKED_PATH: &str = "tracked.txt";

pub(super) const INITIAL_CONTENT: &str = "before\n";

pub(super) fn workspace_root_identity(root: &Path) -> WorkspaceRootIdentity {
    WorkspaceRoot::try_new(&LocalWorkspaceFileSystem, root)
        .expect("fixture workspace root pins")
        .identity()
}

pub(super) fn real_git_packed_references() -> &'static [u8] {
    include_bytes!("fixtures/git-conformance/packed-refs.bin")
}

pub(super) fn real_git_packed_replacement_reference() -> &'static [u8] {
    include_bytes!("fixtures/git-conformance/packed-replacement-record")
}

pub(super) fn real_git_packed_topic_target() -> Oid {
    real_git_fixture_oid(include_bytes!(
        "fixtures/git-conformance/packed-topic-target"
    ))
}

pub(super) fn real_git_loose_topic() -> &'static [u8] {
    include_bytes!("fixtures/git-conformance/loose-topic")
}

pub(super) fn real_git_resolved_topic() -> Oid {
    real_git_fixture_oid(include_bytes!("fixtures/git-conformance/resolved-topic"))
}

pub(super) fn real_git_update_ref_before() -> &'static [u8] {
    include_bytes!("fixtures/git-conformance/update-ref-before")
}

pub(super) fn real_git_update_ref_after() -> &'static [u8] {
    include_bytes!("fixtures/git-conformance/update-ref-after")
}

pub(super) fn real_git_update_ref_target() -> Oid {
    real_git_fixture_oid(real_git_update_ref_after())
}

pub(super) fn real_git_update_ref_lock_exists() -> bool {
    include_bytes!("fixtures/git-conformance/update-ref-lock-state").as_slice() == b"present\n"
}

pub(super) fn real_git_contended_reference() -> &'static [u8] {
    include_bytes!("fixtures/git-conformance/contended-ref")
}

pub(super) fn real_git_contended_lock() -> &'static [u8] {
    include_bytes!("fixtures/git-conformance/contended-lock")
}

pub(super) fn real_git_contended_update_rejects() -> bool {
    include_bytes!("fixtures/git-conformance/contended-result").as_slice() == b"rejected\n"
}

fn real_git_fixture_oid(bytes: &[u8]) -> Oid {
    let record = bytes
        .strip_suffix(b"\n")
        .expect("real Git OID fixture ends in a newline");
    let text = std::str::from_utf8(record).expect("real Git OID fixture is UTF-8");
    crate::layout::parse_full_object_id(text, ObjectFormat::Sha1)
        .expect("real Git OID fixture is a full SHA-1 ID")
}

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
        let mut options = RepositoryInitOptions::new();
        options.external_template(false).initial_head("main");
        let repository =
            Repository::init_opts(directory.path(), &options).expect("repository initializes");
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
    let claimed_id = Oid::hash_object(ObjectType::Blob, content).expect("blob object hashes");
    plant_loose_blob_with_claimed_id(root, content, claimed_id)
}

pub(super) fn plant_loose_blob_with_claimed_id(
    root: &Path,
    content: &[u8],
    claimed_id: Oid,
) -> std::path::PathBuf {
    let object_id = claimed_id.to_string();
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

pub(super) fn plant_packed_blob(root: &Path, content: &[u8]) -> std::path::PathBuf {
    let repository = Repository::open(root).expect("fixture repository opens for packing");
    let object_id = repository.blob(content).expect("packed blob writes");
    let mut builder = repository.packbuilder().expect("pack builder constructs");
    builder
        .insert_object(object_id, None)
        .expect("packed blob enters builder");
    let pack_directory = root.join(".git/objects/pack");
    builder
        .write(&pack_directory, 0o600)
        .expect("fixture pack writes");
    let loose_id = object_id.to_string();
    fs::remove_file(
        root.join(".git/objects")
            .join(&loose_id[..2])
            .join(&loose_id[2..]),
    )
    .expect("oversized loose source removes after packing");
    fs::read_dir(pack_directory)
        .expect("fixture pack directory reads")
        .map(|entry| entry.expect("fixture pack entry reads").path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "pack")
        })
        .expect("fixture pack file exists")
}
