use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::ffi::OsStrExt,
    path::{Component, Path, PathBuf},
};

use git2::{DiffOptions, Patch};
use rustix::{
    fs::{Mode, OFlags, openat, readlinkat_raw},
    io::dup,
};
use signalbox_tools_workspace::{
    WorkspaceEntryKind, WorkspaceFileSystem, WorkspacePathRejection, WorkspaceResolveError,
    WorkspaceRoot,
};

use crate::arguments::GitDiffArguments;
use crate::bounded::{
    bounded_text, resolve_bounded_tree, tree_files, tree_for_commit, validate_index_objects,
    validate_tree_discovery,
};
use crate::executor::regular_file_mode;
use crate::failure::LocalGitFailure;
use crate::limits::{GITLINK_MODE, MAX_DIFF_BYTES};
use crate::pinning::{PinnedRepository, RepositoryShell, repository_filemode};
use crate::result::DiffResult;
use crate::status::{conflicted_index_paths, index_backed_worktree_files, index_files};
use crate::status_reference::StatusHeadSnapshot;

pub(super) fn diff<FileSystem: WorkspaceFileSystem>(
    repository: &RepositoryShell,
    authority: &PinnedRepository,
    arguments: GitDiffArguments,
    filesystem: &FileSystem,
    root: &WorkspaceRoot,
    untracked: Vec<PathBuf>,
) -> Result<DiffResult, LocalGitFailure> {
    let GitDiffArguments::Revisions { base, head } = arguments else {
        return worktree_diff(repository, authority, filesystem, root, untracked);
    };
    let mut options = DiffOptions::new();
    options.ignore_submodules(false);
    let (base_tree, base_snapshot) = resolve_bounded_tree(repository, authority, &base)?;
    let (head_tree, head_snapshot) = resolve_bounded_tree(repository, authority, &head)?;
    validate_tree_discovery(repository, &base_tree)?;
    validate_tree_discovery(repository, &head_tree)?;
    let diff = repository
        .diff_tree_to_tree(Some(&base_tree), Some(&head_tree), Some(&mut options))
        .map_err(|_| LocalGitFailure::Operation)?;
    let mut bytes = Vec::new();
    let mut truncated = false;
    for delta in diff.deltas() {
        let path = delta
            .new_file()
            .path()
            .or(delta.old_file().path())
            .ok_or(LocalGitFailure::Operation)?;
        let mut buffers = Vec::new();
        for file in [delta.old_file(), delta.new_file()] {
            if file.id().is_zero() {
                buffers.push(Vec::new());
                continue;
            }
            let content = diff_object_buffer(repository, file.id(), u32::from(file.mode()))?;
            if u32::from(file.mode()) != GITLINK_MODE {
                let (size, _) = repository
                    .read_object_header(file.id())
                    .map_err(|_| LocalGitFailure::Operation)?;
                truncated |=
                    delta.old_file().id() != delta.new_file().id() && size > MAX_DIFF_BYTES;
            }
            buffers.push(content);
        }
        let old_mode =
            (!delta.old_file().id().is_zero()).then_some(u32::from(delta.old_file().mode()));
        let new_mode =
            (!delta.new_file().id().is_zero()).then_some(u32::from(delta.new_file().mode()));
        let patch = Patch::from_buffers(
            &buffers[0],
            old_mode.map(|_| path),
            &buffers[1],
            new_mode.map(|_| path),
            None,
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        append_bounded(&mut bytes, patch, path, old_mode, new_mode, &mut truncated)?;
        if truncated {
            break;
        }
    }
    let result = render_patch_bytes(bytes, truncated)?;
    base_snapshot.validate(authority)?;
    head_snapshot.validate(authority)?;
    Ok(result)
}

pub(super) fn worktree_diff<FileSystem: WorkspaceFileSystem>(
    repository: &RepositoryShell,
    authority: &PinnedRepository,
    filesystem: &FileSystem,
    root: &WorkspaceRoot,
    untracked: Vec<PathBuf>,
) -> Result<DiffResult, LocalGitFailure> {
    let head_snapshot = StatusHeadSnapshot::capture(authority)?;
    let head_tree = head_snapshot
        .target
        .map(|target| tree_for_commit(repository, target))
        .transpose()?;
    let head_files = match head_tree.as_ref() {
        Some(tree) => {
            validate_tree_discovery(repository, tree)?;
            tree_files(repository, tree)?
        }
        None => BTreeMap::new(),
    };
    let index = repository.index().map_err(|_| LocalGitFailure::Operation)?;
    validate_index_objects(repository, &index)?;
    let index_files = index_files(&index);
    let index_backed_worktree_files = index_backed_worktree_files(&index);
    let conflicted_paths = conflicted_index_paths(&index);
    let mut diff_index_files = index_files.clone();
    diff_index_files.extend(index_backed_worktree_files.clone());
    let untracked_files = untracked
        .into_iter()
        .filter(|path| {
            matches!(
                filesystem.entry_kind(root, path),
                Ok(WorkspaceEntryKind::File | WorkspaceEntryKind::Symlink)
                    | Err(WorkspaceResolveError::Rejected(
                        WorkspacePathRejection::Symlink
                    ))
            )
        })
        .collect::<BTreeSet<_>>();
    let mut bytes = Vec::new();
    let mut truncated = false;
    let filemode = repository_filemode(repository)?;
    let paths = head_files
        .keys()
        .chain(diff_index_files.keys())
        .chain(conflicted_paths.iter())
        .chain(untracked_files.iter())
        .cloned()
        .collect::<BTreeSet<_>>();
    for path in paths {
        let mut content_truncated = false;
        let mut worktree_oid = None;
        if let Some((oid, mode)) = head_files.get(&path)
            && *mode != GITLINK_MODE
        {
            content_truncated |= repository
                .read_object_header(*oid)
                .map_err(|_| LocalGitFailure::Operation)?
                .0
                > MAX_DIFF_BYTES;
        }
        let old_buffer = match head_files.get(&path) {
            Some((oid, mode)) => Some((diff_object_buffer(repository, *oid, *mode)?, *mode)),
            None => None,
        };
        let new_buffer = if let Some((oid, mode)) = index_backed_worktree_files.get(&path) {
            Some((diff_object_buffer(repository, *oid, *mode)?, *mode))
        } else if index_files.contains_key(&path)
            || conflicted_paths.contains(&path)
            || untracked_files.contains(&path)
        {
            match filesystem.entry_kind(root, &path) {
                Ok(WorkspaceEntryKind::Directory) => {
                    if diff_index_files
                        .get(&path)
                        .is_some_and(|(_, mode)| *mode == GITLINK_MODE)
                    {
                        return Err(LocalGitFailure::Operation);
                    }
                    None
                }
                Ok(WorkspaceEntryKind::Symlink)
                | Err(WorkspaceResolveError::Rejected(WorkspacePathRejection::Symlink)) => {
                    let bytes = read_worktree_symlink(
                        authority,
                        &path,
                        repository.object_byte_limit(git2::ObjectType::Blob),
                    )?;
                    Some((bytes, 0o120000))
                }
                Ok(WorkspaceEntryKind::Other) => return Err(LocalGitFailure::Path),
                Err(WorkspaceResolveError::Io { source, .. })
                    if matches!(
                        source.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    None
                }
                Err(WorkspaceResolveError::Rejected(_)) => return Err(LocalGitFailure::Path),
                Err(WorkspaceResolveError::Io { .. }) => return Err(LocalGitFailure::Operation),
                Ok(WorkspaceEntryKind::File) => {
                    match crate::streamed_object::worktree_content(
                        filesystem,
                        root,
                        &path,
                        authority.max_object_bytes,
                    ) {
                        Ok((mut content, content_mode)) => {
                            content_truncated |= content.size > MAX_DIFF_BYTES;
                            worktree_oid = Some(content.oid(authority.object_format)?);
                            let observed_mode = if content_mode & 0o111 == 0 {
                                0o100644
                            } else {
                                0o100755
                            };
                            let indexed_mode = index_files.get(&path).map(|(_, mode)| *mode);
                            let mode = regular_file_mode(observed_mode, indexed_mode, filemode);
                            Some((content.prefix(MAX_DIFF_BYTES)?, mode))
                        }
                        Err(WorkspaceResolveError::Rejected(_)) => {
                            return Err(LocalGitFailure::Path);
                        }
                        Err(WorkspaceResolveError::Io { .. }) => {
                            return Err(LocalGitFailure::Operation);
                        }
                    }
                }
            }
        } else {
            None
        };
        if let (Some((old_oid, old_mode)), Some((_, new_mode))) =
            (head_files.get(&path), new_buffer.as_ref())
            && *old_mode == *new_mode
            && (worktree_oid == Some(*old_oid)
                || index_backed_worktree_files
                    .get(&path)
                    .is_some_and(|(oid, _)| oid == old_oid))
        {
            continue;
        }
        truncated |= content_truncated;
        let mut options = DiffOptions::new();
        options.force_text(true);
        let patch = match (old_buffer.as_ref(), new_buffer.as_ref()) {
            (Some((old, _old_mode)), Some((new, _new_mode))) => {
                Patch::from_buffers(old, Some(&path), new, Some(&path), Some(&mut options))
            }
            (Some((old, _mode)), None) => {
                Patch::from_buffers(old, Some(&path), b"", None, Some(&mut options))
            }
            (None, Some((new, _mode))) => {
                Patch::from_buffers(b"", None, new, Some(&path), Some(&mut options))
            }
            (None, None) => continue,
        }
        .map_err(|_| LocalGitFailure::Operation)?;
        let old_mode = head_files.get(&path).map(|(_, mode)| *mode);
        let new_mode = new_buffer.as_ref().map(|(_, mode)| *mode);
        append_bounded(&mut bytes, patch, &path, old_mode, new_mode, &mut truncated)?;
        if truncated {
            break;
        }
    }
    let result = render_patch_bytes(bytes, truncated)?;
    head_snapshot.validate(authority)?;
    Ok(result)
}

pub(super) fn diff_object_buffer(
    repository: &RepositoryShell,
    oid: git2::Oid,
    mode: u32,
) -> Result<Vec<u8>, LocalGitFailure> {
    if mode == GITLINK_MODE {
        return Ok(gitlink_buffer(oid));
    }
    if crate::bounded::validate_object_header(repository, oid)? != git2::ObjectType::Blob {
        return Err(LocalGitFailure::Operation);
    }
    repository.object_content(oid)?.prefix(MAX_DIFF_BYTES)
}

pub(super) fn gitlink_buffer(oid: git2::Oid) -> Vec<u8> {
    format!("Subproject commit {oid}\n").into_bytes()
}

pub(super) fn read_worktree_symlink(
    authority: &PinnedRepository,
    path: &Path,
    max_bytes: usize,
) -> Result<Vec<u8>, LocalGitFailure> {
    let leaf = path
        .file_name()
        .filter(|leaf| !leaf.is_empty())
        .ok_or(LocalGitFailure::Path)?;
    let mut directory = dup(&authority.root).map_err(|_| LocalGitFailure::Operation)?;
    for component in path.parent().unwrap_or_else(|| Path::new("")).components() {
        let Component::Normal(component) = component else {
            return Err(LocalGitFailure::Path);
        };
        directory = openat(
            &directory,
            component,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| LocalGitFailure::Path)?;
    }
    let mut buffer = vec![0_u8; max_bytes.min(4096).saturating_add(1)];
    let length =
        readlinkat_raw(&directory, leaf, &mut buffer).map_err(|_| LocalGitFailure::Operation)?;
    if length > max_bytes || length == buffer.len() {
        return Err(LocalGitFailure::Operation);
    }
    buffer.truncate(length);
    Ok(buffer)
}

pub(super) fn append_bounded(
    bytes: &mut Vec<u8>,
    mut patch: Patch<'_>,
    path: &Path,
    old_mode: Option<u32>,
    new_mode: Option<u32>,
    truncated: &mut bool,
) -> Result<(), LocalGitFailure> {
    let patch = patch.to_buf().map_err(|_| LocalGitFailure::Operation)?;
    let patch = patch_with_modes(path, &patch, old_mode, new_mode)?;
    let remaining = MAX_DIFF_BYTES.saturating_sub(bytes.len());
    if patch.len() <= remaining {
        bytes.extend_from_slice(&patch);
    } else {
        bytes.extend_from_slice(&patch[..remaining]);
        *truncated = true;
    }
    Ok(())
}

pub(super) fn patch_with_modes(
    path: &Path,
    patch: &[u8],
    old_mode: Option<u32>,
    new_mode: Option<u32>,
) -> Result<Vec<u8>, LocalGitFailure> {
    let mode = match (old_mode, new_mode) {
        (Some(old_mode), Some(new_mode)) if old_mode != new_mode => {
            format!("old mode {old_mode:06o}\nnew mode {new_mode:06o}\n")
        }
        (None, Some(new_mode)) => format!("new file mode {new_mode:06o}\n"),
        (Some(old_mode), None) => format!("deleted file mode {old_mode:06o}\n"),
        _ => return Ok(patch.to_vec()),
    };
    if patch.is_empty() {
        let old_path = quoted_diff_path(b"a/", path);
        let new_path = quoted_diff_path(b"b/", path);
        let mut rendered = Vec::new();
        rendered.extend_from_slice(b"diff --git ");
        rendered.extend_from_slice(&old_path);
        rendered.push(b' ');
        rendered.extend_from_slice(&new_path);
        rendered.push(b'\n');
        rendered.extend_from_slice(mode.as_bytes());
        return Ok(rendered);
    }
    let first_line = patch
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|position| position + 1)
        .ok_or(LocalGitFailure::Operation)?;
    let existing_mode_end = patch[first_line..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|position| first_line + position + 1)
        .filter(|end| {
            patch[first_line..*end].starts_with(b"new file mode ")
                || patch[first_line..*end].starts_with(b"deleted file mode ")
        })
        .unwrap_or(first_line);
    let mut rendered = Vec::with_capacity(patch.len().saturating_add(mode.len()));
    rendered.extend_from_slice(&patch[..first_line]);
    rendered.extend_from_slice(mode.as_bytes());
    rendered.extend_from_slice(&patch[existing_mode_end..]);
    Ok(rendered)
}

pub(super) fn quoted_diff_path(prefix: &[u8], path: &Path) -> Vec<u8> {
    let path = [prefix, path.as_os_str().as_bytes()].concat();
    if path
        .iter()
        .all(|byte| matches!(byte, b'!'..=b'~') && !matches!(byte, b'"' | b'\\'))
    {
        return path;
    }
    let mut quoted = Vec::with_capacity(path.len().saturating_add(2));
    quoted.push(b'"');
    for byte in path {
        match byte {
            b'\\' => quoted.extend_from_slice(b"\\\\"),
            b'"' => quoted.extend_from_slice(b"\\\""),
            b'\x07' => quoted.extend_from_slice(b"\\a"),
            b'\x08' => quoted.extend_from_slice(b"\\b"),
            b'\t' => quoted.extend_from_slice(b"\\t"),
            b'\n' => quoted.extend_from_slice(b"\\n"),
            b'\x0b' => quoted.extend_from_slice(b"\\v"),
            b'\x0c' => quoted.extend_from_slice(b"\\f"),
            b'\r' => quoted.extend_from_slice(b"\\r"),
            b' '..=b'~' => quoted.push(byte),
            _ => {
                quoted.push(b'\\');
                quoted.push(b'0' + ((byte >> 6) & 0x07));
                quoted.push(b'0' + ((byte >> 3) & 0x07));
                quoted.push(b'0' + (byte & 0x07));
            }
        }
    }
    quoted.push(b'"');
    quoted
}

pub(super) fn render_patch_bytes(
    bytes: Vec<u8>,
    mut truncated: bool,
) -> Result<DiffResult, LocalGitFailure> {
    let patch = match String::from_utf8(bytes) {
        Ok(patch) => patch,
        Err(error) => {
            truncated = true;
            let lossy = String::from_utf8_lossy(error.as_bytes());
            let (patch, _) = bounded_text(&lossy, MAX_DIFF_BYTES);
            patch
        }
    };
    Ok(DiffResult { patch, truncated })
}
