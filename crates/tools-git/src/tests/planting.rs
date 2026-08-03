//! Fixtures that plant over-budget repository state.

use std::{fs, path::Path};

use git2::{IndexEntry, IndexTime, ObjectType, Oid, Repository, Signature};

use crate::limits::{
    MAX_INDEX_ENTRIES, MAX_OBJECT_BYTES, MAX_STAGE_FILE_BYTES, MAX_STAGE_TOTAL_BYTES,
    MAX_TREE_BLOB_BYTES, MAX_WORKTREE_INSPECTIONS,
};
use crate::tests::support::{
    AUTHOR_EMAIL, AUTHOR_NAME, Fixture, MODEL_MESSAGE, TRACKED_PATH, commit_all, index_extension,
    install_deleted_conflict, raw_commit_with_tree,
};

pub(super) fn plant_over_budget_worktree(root: &Path) {
    for sequence in 0..=MAX_WORKTREE_INSPECTIONS {
        fs::write(root.join(format!("untracked-{sequence:04}.txt")), [])
            .expect("worktree-budget fixture file writes");
    }
}

pub(super) fn plant_over_budget_directory(root: &Path, directory: &str) {
    let directory = root.join(directory);
    fs::create_dir(&directory).expect("worktree-budget fixture directory creates");
    plant_over_budget_entries(&directory);
}

pub(super) fn plant_over_budget_entries(directory: &Path) {
    for sequence in 0..=MAX_WORKTREE_INSPECTIONS {
        fs::write(directory.join(format!("entry-{sequence:04}.txt")), [])
            .expect("worktree-budget fixture file writes");
    }
}

pub(super) fn plant_aggregate_stage_files(root: &Path) -> Vec<String> {
    let bytes = vec![b'x'; MAX_STAGE_FILE_BYTES];
    let count = MAX_STAGE_TOTAL_BYTES / MAX_STAGE_FILE_BYTES + 1;
    let mut paths = Vec::with_capacity(count);
    for sequence in 0..count {
        let path = format!("aggregate-{sequence:02}.txt");
        fs::write(root.join(&path), &bytes).expect("aggregate fixture file writes");
        paths.push(path);
    }
    paths
}

pub(super) fn plant_sparse_pack(root: &Path, name: &str, bytes: u64) {
    let path = root.join(".git/objects/pack").join(name);
    fs::File::create(path)
        .expect("pack-budget fixture file creates")
        .set_len(bytes)
        .expect("pack-budget fixture length sets");
}

pub(super) fn fill_live_object_database_to_budget(root: &Path, mut remaining: u64) {
    let mut sequence = 0_u64;
    while remaining > 0 {
        let directory = root
            .join(".git/objects")
            .join(format!("{:02x}", sequence % 256));
        fs::create_dir_all(&directory).expect("loose-object budget directory creates");
        let path = directory.join(format!("{sequence:038x}"));
        let bytes = remaining.min((MAX_OBJECT_BYTES * 2) as u64);
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .expect("loose-object budget file creates")
            .set_len(bytes)
            .expect("loose-object budget length sets");
        remaining -= bytes;
        sequence += 1;
    }
}

pub(super) fn plant_shallow_entries(root: &Path, oid: Oid, count: usize) {
    fs::write(root.join(".git/shallow"), format!("{oid}\n").repeat(count))
        .expect("shallow-budget fixture writes");
}

pub(super) fn plant_status_over_byte_budget(fixture: &Fixture) {
    plant_aggregate_stage_files(fixture.root());
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    commit_all(&repository, MODEL_MESSAGE);
}

pub(super) fn plant_over_budget_index(repository: &Repository) {
    plant_index_entries(repository, MAX_INDEX_ENTRIES + 1, "");
}

pub(super) fn plant_maximum_index(repository: &Repository) {
    plant_index_entries(repository, MAX_INDEX_ENTRIES, "");
}

pub(super) fn plant_maximum_index_beneath_directory(repository: &Repository) {
    plant_index_entries(repository, MAX_INDEX_ENTRIES, "directory/");
}

pub(super) fn plant_index_entries(repository: &Repository, count: usize, prefix: &str) {
    let blob = repository.blob(b"bounded\n").expect("fixture blob writes");
    let mut index = repository.index().expect("fixture index opens");
    index.clear().expect("fixture index clears");
    for sequence in 0..count {
        let path = format!("{prefix}entry-{sequence:04}.txt");
        index
            .add(&IndexEntry {
                ctime: IndexTime::new(0, 0),
                mtime: IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                mode: 0o100644,
                uid: 0,
                gid: 0,
                file_size: 8,
                id: blob,
                flags: 0,
                flags_extended: 0,
                path: path.into_bytes(),
            })
            .expect("fixture index entry adds");
    }
    index.write().expect("fixture index writes");
}

pub(super) fn install_resolve_undo_extension(fixture: &Fixture, content: &str) -> Vec<u8> {
    install_deleted_conflict(fixture);
    fs::write(fixture.root().join(TRACKED_PATH), content).expect("resolved fixture file writes");
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let mut index = repository.index().expect("fixture index opens");
    index
        .add_path(Path::new(TRACKED_PATH))
        .expect("fixture conflict resolves");
    index.write().expect("fixture resolve-undo index writes");
    repository
        .cleanup_state()
        .expect("fixture merge state cleans");
    index_extension(
        &fs::read(fixture.root().join(".git/index")).expect("fixture index reads"),
        b"REUC",
    )
}

pub(super) fn plant_index_over_blob_budget(repository: &Repository) {
    let mut index = repository.index().expect("fixture index opens");
    index.clear().expect("fixture index clears");
    let count = MAX_TREE_BLOB_BYTES / MAX_OBJECT_BYTES + 1;
    for sequence in 0..count {
        let mut bytes = vec![b'x'; MAX_OBJECT_BYTES];
        bytes[..std::mem::size_of::<usize>()].copy_from_slice(&sequence.to_le_bytes());
        let blob = repository.blob(&bytes).expect("fixture blob writes");
        let path = format!("aggregate-blob-{sequence:02}.txt");
        index
            .add(&IndexEntry {
                ctime: IndexTime::new(0, 0),
                mtime: IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                mode: 0o100644,
                uid: 0,
                gid: 0,
                file_size: MAX_OBJECT_BYTES as u32,
                id: blob,
                flags: 0,
                flags_extended: 0,
                path: path.into_bytes(),
            })
            .expect("fixture index entry adds");
    }
    index.write().expect("fixture index writes");
}

pub(super) fn over_budget_tree_commit(repository: &Repository, parent: Oid) -> Oid {
    let blob = repository.blob(b"bounded\n").expect("fixture blob writes");
    let mut builder = repository.treebuilder(None).expect("tree builder opens");
    for sequence in 0..=MAX_WORKTREE_INSPECTIONS {
        builder
            .insert(format!("entry-{sequence:04}.txt"), blob, 0o100644)
            .expect("over-budget tree entry inserts");
    }
    let tree_id = builder.write().expect("over-budget tree writes");
    let tree = repository
        .find_tree(tree_id)
        .expect("over-budget tree opens");
    let parent = repository
        .find_commit(parent)
        .expect("fixture parent commit opens");
    let signature = Signature::now(AUTHOR_NAME, AUTHOR_EMAIL).expect("signature constructs");
    repository
        .commit(
            None,
            &signature,
            &signature,
            MODEL_MESSAGE,
            &tree,
            &[&parent],
        )
        .expect("over-budget tree commit writes")
}

pub(super) fn aggregate_blob_tree_commit(repository: &Repository, parent: Oid) -> Oid {
    let bytes = vec![b'x'; MAX_OBJECT_BYTES];
    let blob = repository
        .blob(&bytes)
        .expect("aggregate-tree fixture blob writes");
    let mut builder = repository
        .treebuilder(None)
        .expect("aggregate-tree builder opens");
    let count = MAX_TREE_BLOB_BYTES / MAX_OBJECT_BYTES + 1;
    for sequence in 0..count {
        builder
            .insert(format!("large-{sequence:02}.bin"), blob, 0o100644)
            .expect("aggregate-tree entry inserts");
    }
    let tree = builder.write().expect("aggregate-tree writes");
    raw_commit_with_tree(repository, tree, parent)
}

pub(super) fn oversized_root_tree_commit(repository: &Repository, parent: Oid) -> Oid {
    let blob = repository.blob(b"bounded\n").expect("fixture blob writes");
    let mut raw_tree = Vec::new();
    for sequence in 0..=MAX_WORKTREE_INSPECTIONS {
        let name = format!("entry-{sequence:04}-{}", "x".repeat(220));
        raw_tree.extend_from_slice(b"100644 ");
        raw_tree.extend_from_slice(name.as_bytes());
        raw_tree.push(0);
        raw_tree.extend_from_slice(blob.as_bytes());
    }
    assert!(raw_tree.len() > MAX_OBJECT_BYTES);
    let tree = repository
        .odb()
        .expect("fixture object database opens")
        .write(ObjectType::Tree, &raw_tree)
        .expect("oversized root tree writes");
    let raw_commit = format!(
        "tree {tree}\nparent {parent}\nauthor Signalbox <fixer@example.test> 0 +0000\ncommitter Signalbox <fixer@example.test> 0 +0000\n\noversized tree\n"
    );
    repository
        .odb()
        .expect("fixture object database reopens")
        .write(ObjectType::Commit, raw_commit.as_bytes())
        .expect("oversized-root-tree fixture commit writes")
}

pub(super) fn oversized_commit_object(repository: &Repository, parent: Oid) -> Oid {
    let tree = repository
        .find_commit(parent)
        .expect("fixture parent commit exists")
        .tree_id();
    let mut raw = format!(
        "tree {tree}\nparent {parent}\nauthor Signalbox <fixer@example.test> 0 +0000\ncommitter Signalbox <fixer@example.test> 0 +0000\n\n"
    )
    .into_bytes();
    raw.resize(MAX_OBJECT_BYTES + 1, b'x');
    repository
        .odb()
        .expect("fixture object database opens")
        .write(ObjectType::Commit, &raw)
        .expect("oversized fixture commit writes")
}
