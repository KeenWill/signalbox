use std::{collections::HashSet, fs, io::Write};

use git2::{Buf, Indexer, Odb, PackBuilder, Repository};

use crate::bounded::find_bounded_commit;
use crate::failure::LocalGitFailure;
use crate::limits::{GITLINK_MODE, MAX_OBJECT_DATABASE_BYTES, MAX_WORKTREE_INSPECTIONS};
use crate::pack_install::{
    ObjectPublicationLock, install_packed_object_pair, pack_installation_mode,
};
use crate::pinning::{PinnedObjectDatabase, PinnedRepository};

#[derive(Clone, Copy)]
pub(super) enum PackRoot {
    Object(git2::Oid),
    Commit(git2::Oid),
}

pub(super) fn persist_objects(
    authority: &PinnedRepository,
    repository: &Repository,
    persistent_objects: &Odb<'_>,
    object_database: &Odb<'_>,
    pinned_objects: &PinnedObjectDatabase,
    roots: &[PackRoot],
) -> Result<(), LocalGitFailure> {
    if roots.is_empty() {
        return Ok(());
    }
    let mut buffer = Buf::new();
    let mut builder = repository
        .packbuilder()
        .map_err(|_| LocalGitFailure::Operation)?;
    let mut inserted = HashSet::new();
    let mut budget = PackTraversalBudget::default();
    for root in roots {
        match root {
            PackRoot::Object(oid) => {
                insert_missing_pack_object(
                    repository,
                    persistent_objects,
                    &mut builder,
                    &mut inserted,
                    &mut budget,
                    *oid,
                )?;
            }
            PackRoot::Commit(oid) => {
                insert_missing_commit_graph(
                    repository,
                    persistent_objects,
                    &mut builder,
                    &mut inserted,
                    &mut budget,
                    *oid,
                )?;
            }
        }
    }
    if inserted.is_empty() {
        return Ok(());
    }
    builder
        .write_buf(&mut buffer)
        .map_err(|_| LocalGitFailure::Operation)?;
    let indexed = tempfile::tempdir().map_err(|_| LocalGitFailure::Operation)?;
    let mut indexer = Indexer::new_ext(
        Some(object_database),
        indexed.path(),
        0o600,
        true,
        authority.object_format,
    )
    .map_err(|_| LocalGitFailure::Operation)?;
    indexer
        .write_all(&buffer)
        .map_err(|_| LocalGitFailure::Operation)?;
    let checksum = indexer.commit().map_err(|_| LocalGitFailure::Operation)?;
    let stem = format!("pack-{checksum}");
    let generated_pack = indexed.path().join(format!("{stem}.pack"));
    let generated_index = indexed.path().join(format!("{stem}.idx"));
    let generated_pack_bytes = fs::metadata(&generated_pack)
        .map_err(|_| LocalGitFailure::Operation)?
        .len();
    let generated_index_bytes = fs::metadata(&generated_index)
        .map_err(|_| LocalGitFailure::Operation)?
        .len();
    let generated_bytes = generated_pack_bytes
        .checked_add(generated_index_bytes)
        .ok_or(LocalGitFailure::Operation)?;
    pinned_objects.validate_live(authority)?;
    let publication = ObjectPublicationLock::acquire(pinned_objects)?;
    pinned_objects
        .compressed_bytes()
        .checked_add(generated_bytes)
        .filter(|bytes| *bytes <= MAX_OBJECT_DATABASE_BYTES as u64)
        .ok_or(LocalGitFailure::Operation)?;
    let installation_mode = pack_installation_mode(&publication.directory)?;
    install_packed_object_pair(
        &publication.directory,
        &generated_pack,
        &generated_index,
        installation_mode,
    )
}

#[derive(Default)]
pub(super) struct PackTraversalBudget {
    inspections: usize,
    bytes: usize,
}

impl PackTraversalBudget {
    fn charge(&mut self, repository: &Repository, oid: git2::Oid) -> Result<(), LocalGitFailure> {
        let (bytes, _) = repository
            .odb()
            .and_then(|object_database| object_database.read_header(oid))
            .map_err(|_| LocalGitFailure::Operation)?;
        self.inspections = self
            .inspections
            .checked_add(1)
            .filter(|inspections| *inspections <= MAX_WORKTREE_INSPECTIONS)
            .ok_or(LocalGitFailure::Operation)?;
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .filter(|total| *total <= MAX_OBJECT_DATABASE_BYTES)
            .ok_or(LocalGitFailure::Operation)?;
        Ok(())
    }
}

pub(super) fn insert_missing_pack_object(
    repository: &Repository,
    persistent_objects: &Odb<'_>,
    builder: &mut PackBuilder<'_>,
    inserted: &mut HashSet<git2::Oid>,
    budget: &mut PackTraversalBudget,
    oid: git2::Oid,
) -> Result<bool, LocalGitFailure> {
    if !persistent_objects.exists(oid) && inserted.insert(oid) {
        budget.charge(repository, oid)?;
        builder
            .insert_object(oid, None)
            .map_err(|_| LocalGitFailure::Operation)?;
        return Ok(true);
    }
    Ok(false)
}

pub(super) fn insert_missing_commit_graph(
    repository: &Repository,
    persistent_objects: &Odb<'_>,
    builder: &mut PackBuilder<'_>,
    inserted: &mut HashSet<git2::Oid>,
    budget: &mut PackTraversalBudget,
    root: git2::Oid,
) -> Result<(), LocalGitFailure> {
    let mut pending = vec![root];
    while let Some(oid) = pending.pop() {
        if !insert_missing_pack_object(
            repository,
            persistent_objects,
            builder,
            inserted,
            budget,
            oid,
        )? {
            continue;
        }
        let commit = find_bounded_commit(repository, oid)?;
        insert_missing_tree_graph(
            repository,
            persistent_objects,
            builder,
            inserted,
            budget,
            commit.tree_id(),
        )?;
        let object_database = repository.odb().map_err(|_| LocalGitFailure::Operation)?;
        for parent in commit.parent_ids() {
            if persistent_objects.exists(parent) || object_database.exists(parent) {
                pending.push(parent);
            }
        }
    }
    Ok(())
}

pub(super) fn insert_missing_tree_graph(
    repository: &Repository,
    persistent_objects: &Odb<'_>,
    builder: &mut PackBuilder<'_>,
    inserted: &mut HashSet<git2::Oid>,
    budget: &mut PackTraversalBudget,
    root: git2::Oid,
) -> Result<(), LocalGitFailure> {
    let mut pending = vec![root];
    while let Some(oid) = pending.pop() {
        if !insert_missing_pack_object(
            repository,
            persistent_objects,
            builder,
            inserted,
            budget,
            oid,
        )? {
            continue;
        }
        let tree = repository
            .find_tree(oid)
            .map_err(|_| LocalGitFailure::Operation)?;
        for entry in &tree {
            match entry.kind() {
                Some(git2::ObjectType::Tree) => pending.push(entry.id()),
                Some(git2::ObjectType::Blob) => {
                    insert_missing_pack_object(
                        repository,
                        persistent_objects,
                        builder,
                        inserted,
                        budget,
                        entry.id(),
                    )?;
                }
                Some(git2::ObjectType::Commit) if entry.filemode() == GITLINK_MODE as i32 => {}
                _ => return Err(LocalGitFailure::Operation),
            }
        }
    }
    Ok(())
}
