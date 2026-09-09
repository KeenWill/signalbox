use crate::{
    failure::LocalGitFailure,
    limits::{GITLINK_MODE, MAX_WORKTREE_INSPECTIONS},
    pack_install::{ObjectPublicationLock, install_packed_object_pair, pack_installation_mode},
    pinning::{PinnedObjectDatabase, PinnedRepository, RepositoryShell},
};
use git2::Odb;
use std::collections::BTreeSet;

#[derive(Clone, Copy)]
pub(super) enum PackRoot {
    Object(git2::Oid),
    Commit(git2::Oid),
}

pub(super) fn persist_objects(
    authority: &PinnedRepository,
    repository: &RepositoryShell,
    _persistent_objects: &Odb<'_>,
    _object_database: &Odb<'_>,
    pinned_objects: &PinnedObjectDatabase,
    roots: &[PackRoot],
) -> Result<(), LocalGitFailure> {
    let directory = tempfile::tempdir().map_err(|_| LocalGitFailure::Operation)?;
    let mut pending = roots
        .iter()
        .map(|root| match root {
            PackRoot::Object(oid) | PackRoot::Commit(oid) => *oid,
        })
        .collect::<Vec<_>>();
    let mut inserted = BTreeSet::new();
    let mut objects = Vec::new();
    while let Some(oid) = pending.pop() {
        if !inserted.insert(oid) || pinned_objects.contains(oid)? {
            continue;
        }
        if inserted.len() > MAX_WORKTREE_INSPECTIONS {
            return Err(LocalGitFailure::Operation);
        }
        let (size, kind) = repository
            .read_object_header(oid)
            .map_err(|_| LocalGitFailure::Operation)?;
        if size > repository.object_byte_limit() {
            return Err(LocalGitFailure::Operation);
        }
        match kind {
            git2::ObjectType::Commit => {
                let commit = repository
                    .find_commit(oid)
                    .map_err(|_| LocalGitFailure::Operation)?;
                pending.push(commit.tree_id());
                pending.extend(commit.parent_ids());
            }
            git2::ObjectType::Tree => {
                let tree = repository
                    .find_tree(oid)
                    .map_err(|_| LocalGitFailure::Operation)?;
                for entry in &tree {
                    if entry.filemode() != GITLINK_MODE as i32 {
                        pending.push(entry.id());
                    }
                }
            }
            git2::ObjectType::Blob => {}
            _ => return Err(LocalGitFailure::Operation),
        }
        objects.push(oid);
    }
    if objects.is_empty() {
        pinned_objects.validate_live(authority)?;
        return Ok(());
    }
    let (pack, index) = crate::streamed_object::write_pack(
        &objects,
        |oid| repository.object_content(oid),
        authority.object_format,
        directory.path(),
    )?;
    pinned_objects.validate_live(authority)?;
    let publication = ObjectPublicationLock::acquire(pinned_objects)?;
    let mode = pack_installation_mode(&publication.directory)?;
    install_packed_object_pair(&publication.directory, &pack, &index, mode)?;
    Ok(())
}
