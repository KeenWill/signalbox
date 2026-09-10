//! File-backed object decoding and publication with fixed-size I/O buffers.

use crate::failure::LocalGitFailure;
use flate2::{Compression, write::ZlibEncoder};
use git2::{ObjectFormat, ObjectType, Oid};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    time::Instant,
};

pub(super) const IO_BYTES: usize = 64 * 1024;

pub(super) struct ObjectContent {
    pub(super) file: File,
    pub(super) size: usize,
    pub(super) kind: ObjectType,
}

impl ObjectContent {
    pub(super) fn decode(
        reader: &mut impl Read,
        size: usize,
        kind: ObjectType,
        deadline: Option<Instant>,
    ) -> Result<Self, LocalGitFailure> {
        let mut file = tempfile::tempfile().map_err(failed)?;
        let mut reader = reader.take((size as u64).saturating_add(1));
        let mut buffer = [0; IO_BYTES];
        let mut copied = 0u64;
        loop {
            check_deadline(deadline)?;
            let count = reader.read(&mut buffer).map_err(failed)?;
            if count == 0 {
                break;
            }
            copied += count as u64;
            if copied > size as u64 {
                return Err(LocalGitFailure::Repository);
            }
            file.write_all(&buffer[..count]).map_err(failed)?;
        }
        if copied != size as u64 {
            return Err(LocalGitFailure::Repository);
        }
        file.rewind().map_err(failed)?;
        Ok(Self { file, size, kind })
    }

    pub(super) fn apply_delta(
        mut self,
        mut delta: File,
        limit: Option<usize>,
        deadline: Option<Instant>,
    ) -> Result<Self, LocalGitFailure> {
        delta.rewind().map_err(failed)?;
        let mut delta = std::io::BufReader::new(delta);
        let base_size = variable_size(&mut delta)?;
        let size = variable_size(&mut delta)?;
        if base_size != self.size || limit.is_some_and(|limit| size > limit || base_size > limit) {
            return Err(LocalGitFailure::Repository);
        }
        let mut file = tempfile::tempfile().map_err(failed)?;
        let mut written = 0usize;
        let mut buffer = [0u8; IO_BYTES];
        let mut opcode = [0u8; 1];
        while delta.read(&mut opcode).map_err(failed)? != 0 {
            check_deadline(deadline)?;
            let opcode = opcode[0];
            let (offset, length) = if opcode & 128 != 0 {
                let mut offset = 0usize;
                let mut length = 0usize;
                for bit in 0..4 {
                    if opcode & (1 << bit) != 0 {
                        offset |= usize::from(byte(&mut delta)?) << (bit * 8);
                    }
                }
                for bit in 0..3 {
                    if opcode & (1 << (bit + 4)) != 0 {
                        length |= usize::from(byte(&mut delta)?) << (bit * 8);
                    }
                }
                if length == 0 {
                    // Git pack-format defines a zero copy size as 64 KiB.
                    length = 0x10000;
                }
                if offset.checked_add(length).is_none_or(|end| end > self.size) {
                    return Err(LocalGitFailure::Repository);
                }
                (Some(offset), length)
            } else if opcode != 0 {
                (None, usize::from(opcode))
            } else {
                return Err(LocalGitFailure::Repository);
            };
            written = written
                .checked_add(length)
                .filter(|written| *written <= size)
                .ok_or(LocalGitFailure::Repository)?;
            let reader: &mut dyn Read = if let Some(offset) = offset {
                self.file
                    .seek(SeekFrom::Start(offset as u64))
                    .map_err(failed)?;
                &mut self.file
            } else {
                &mut delta
            };
            let mut remaining = length;
            while remaining != 0 {
                check_deadline(deadline)?;
                let length = remaining.min(buffer.len());
                reader.read_exact(&mut buffer[..length]).map_err(failed)?;
                file.write_all(&buffer[..length]).map_err(failed)?;
                remaining -= length;
            }
        }
        if written != size {
            return Err(LocalGitFailure::Repository);
        }
        file.rewind().map_err(failed)?;
        Ok(Self {
            file,
            size,
            kind: self.kind,
        })
    }

    pub(super) fn store(
        &mut self,
        directory: &std::path::Path,
        format: ObjectFormat,
        deadline: Option<Instant>,
    ) -> Result<Oid, LocalGitFailure> {
        let header = format!("{} {}\0", self.kind.str(), self.size);
        self.file.rewind().map_err(failed)?;
        let mut output = tempfile::NamedTempFile::new_in(directory).map_err(failed)?;
        let mut encoder = ZlibEncoder::new(output.as_file_mut(), Compression::default());
        let mut hash = ObjectHash::new(format);
        encoder.write_all(header.as_bytes()).map_err(failed)?;
        hash.update(header.as_bytes());
        let mut buffer = [0u8; IO_BYTES];
        loop {
            check_deadline(deadline)?;
            let count = self.file.read(&mut buffer).map_err(failed)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
            encoder.write_all(&buffer[..count]).map_err(failed)?;
        }
        encoder.finish().map_err(failed)?;
        let oid = hash.finish()?;
        let hex = oid.to_string();
        let parent = directory.join(&hex[..2]);
        std::fs::create_dir_all(&parent).map_err(failed)?;
        match output.persist_noclobber(parent.join(&hex[2..])) {
            Ok(_) => {}
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err(LocalGitFailure::Operation),
        }
        Ok(oid)
    }
}

pub(super) enum ObjectHash {
    Sha1(Sha1),
    Sha256(Sha256),
}
impl ObjectHash {
    pub(super) fn new(format: ObjectFormat) -> Self {
        match format {
            ObjectFormat::Sha1 => Self::Sha1(Sha1::new()),
            ObjectFormat::Sha256 => Self::Sha256(Sha256::new()),
        }
    }
    pub(super) fn update(&mut self, bytes: &[u8]) {
        match self {
            Self::Sha1(hash) => hash.update(bytes),
            Self::Sha256(hash) => hash.update(bytes),
        }
    }
    pub(super) fn finish(self) -> Result<Oid, LocalGitFailure> {
        match self {
            Self::Sha1(hash) => Oid::from_bytes(&hash.finalize()),
            Self::Sha256(hash) => Oid::from_bytes(&hash.finalize()),
        }
        .map_err(failed)
    }
}

pub(super) fn variable_size(reader: &mut impl Read) -> Result<usize, LocalGitFailure> {
    let mut size = 0usize;
    for shift in (0..usize::BITS).step_by(7) {
        let byte = byte(reader)?;
        let value = usize::from(byte & 127);
        if value > usize::MAX >> shift {
            return Err(LocalGitFailure::Repository);
        }
        size |= value << shift;
        if byte & 128 == 0 {
            return Ok(size);
        }
    }
    Err(LocalGitFailure::Repository)
}
fn byte(reader: &mut impl Read) -> Result<u8, LocalGitFailure> {
    let mut byte = [0];
    reader.read_exact(&mut byte).map_err(failed)?;
    Ok(byte[0])
}
fn failed<T>(_: T) -> LocalGitFailure {
    LocalGitFailure::Repository
}

struct PackWriter<'a> {
    file: &'a mut File,
    hash: ObjectHash,
    crc: crc32fast::Hasher,
}
impl Write for PackWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let count = self.file.write(bytes)?;
        self.hash.update(&bytes[..count]);
        self.crc.update(&bytes[..count]);
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

pub(super) fn write_pack(
    objects: &[Oid],
    mut content_for: impl FnMut(Oid) -> Result<ObjectContent, LocalGitFailure>,
    format: ObjectFormat,
    directory: &std::path::Path,
) -> Result<(std::path::PathBuf, std::path::PathBuf), LocalGitFailure> {
    // Git pack-format and index-format v2. One publication batch owns one pair.
    if objects.len() > crate::limits::MAX_REPOSITORY_INSPECTIONS {
        return Err(LocalGitFailure::Operation);
    }
    let mut temporary = tempfile::NamedTempFile::new_in(directory).map_err(failed)?;
    let mut header = b"PACK\0\0\0\x02".to_vec();
    header.extend_from_slice(&u32::try_from(objects.len()).map_err(failed)?.to_be_bytes());
    temporary.write_all(&header).map_err(failed)?;
    let mut hash = ObjectHash::new(format);
    hash.update(&header);
    let mut writer = PackWriter {
        file: temporary.as_file_mut(),
        hash,
        crc: crc32fast::Hasher::new(),
    };
    let mut entries = Vec::with_capacity(objects.len());
    for &oid in objects {
        let offset = writer.file.stream_position().map_err(failed)?;
        let mut content = content_for(oid)?;
        let mut size = content.size;
        let kind = match content.kind {
            ObjectType::Commit => 1u8,
            ObjectType::Tree => 2,
            ObjectType::Blob => 3,
            ObjectType::Tag => 4,
            ObjectType::Any => return Err(LocalGitFailure::Repository),
        };
        let mut first = kind << 4 | (size & 15) as u8;
        size >>= 4;
        if size != 0 {
            first |= 128;
        }
        writer.write_all(&[first]).map_err(failed)?;
        while size != 0 {
            let mut byte = (size & 127) as u8;
            size >>= 7;
            if size != 0 {
                byte |= 128;
            }
            writer.write_all(&[byte]).map_err(failed)?;
        }
        let mut encoder = ZlibEncoder::new(writer, Compression::default());
        content.file.rewind().map_err(failed)?;
        std::io::copy(&mut content.file, &mut encoder).map_err(failed)?;
        writer = encoder.finish().map_err(failed)?;
        let crc = std::mem::replace(&mut writer.crc, crc32fast::Hasher::new()).finalize();
        entries.push((oid, crc, offset));
    }
    let checksum = writer.hash.finish()?;
    temporary.write_all(checksum.as_bytes()).map_err(failed)?;
    let stem = format!("pack-{checksum}");
    let pack = directory.join(format!("{stem}.pack"));
    temporary.persist(&pack).map_err(failed)?;
    entries.sort_unstable_by_key(|(oid, _, _)| *oid);
    let mut index = b"\xfftOc\0\0\0\x02".to_vec();
    let mut count = 0usize;
    for prefix in 0..256 {
        while count < entries.len() && usize::from(entries[count].0.as_bytes()[0]) <= prefix {
            count += 1;
        }
        index.extend_from_slice(&u32::try_from(count).map_err(failed)?.to_be_bytes());
    }
    for (oid, _, _) in &entries {
        index.extend_from_slice(oid.as_bytes());
    }
    for (_, crc, _) in &entries {
        index.extend_from_slice(&crc.to_be_bytes());
    }
    let mut large_offset_index = 0u32;
    // Index v2 uses the high bit as an indirection into its 64-bit offset table.
    const LARGE_OFFSET: u64 = 1 << 31;
    for (_, _, offset) in &entries {
        let encoded = if *offset < LARGE_OFFSET {
            *offset as u32
        } else {
            let encoded = (LARGE_OFFSET as u32) | large_offset_index;
            large_offset_index += 1;
            encoded
        };
        index.extend_from_slice(&encoded.to_be_bytes());
    }
    for (_, _, offset) in &entries {
        if *offset >= LARGE_OFFSET {
            index.extend_from_slice(&offset.to_be_bytes());
        }
    }
    index.extend_from_slice(checksum.as_bytes());
    let mut hash = ObjectHash::new(format);
    hash.update(&index);
    index.extend_from_slice(hash.finish()?.as_bytes());
    let index_path = directory.join(format!("{stem}.idx"));
    std::fs::write(&index_path, index).map_err(failed)?;
    Ok((pack, index_path))
}

pub(super) fn worktree_content<FileSystem: signalbox_tools_workspace::WorkspaceFileSystem>(
    filesystem: &FileSystem,
    root: &signalbox_tools_workspace::WorkspaceRoot,
    path: &std::path::Path,
    limit: Option<usize>,
) -> Result<(ObjectContent, u32), signalbox_tools_workspace::WorkspaceResolveError> {
    use signalbox_tools_workspace::WorkspaceResolveError;
    let io_error = |source| WorkspaceResolveError::Io {
        path: path.to_owned(),
        source,
    };
    let mut source = filesystem.open_file_stream(root, path)?;
    let size = usize::try_from(source.len())
        .map_err(|_| io_error(std::io::Error::other("file size does not fit host")))?;
    if limit.is_some_and(|limit| size > limit) {
        return Err(io_error(std::io::Error::other(
            "configured object limit exceeded",
        )));
    }
    let mode = source.mode();
    let mut file = tempfile::tempfile().map_err(io_error)?;
    let mut buffer = [0u8; IO_BYTES];
    let mut copied = 0usize;
    loop {
        let count = source.read(&mut buffer).map_err(io_error)?;
        if count == 0 {
            break;
        }
        file.write_all(&buffer[..count]).map_err(io_error)?;
        copied += count;
    }
    if copied != size {
        return Err(io_error(std::io::Error::other("incomplete worktree read")));
    }
    file.rewind().map_err(io_error)?;
    Ok((
        ObjectContent {
            file,
            size,
            kind: ObjectType::Blob,
        },
        mode,
    ))
}

impl ObjectContent {
    pub(super) fn oid(&mut self, format: ObjectFormat) -> Result<Oid, LocalGitFailure> {
        let mut hash = ObjectHash::new(format);
        hash.update(format!("{} {}\0", self.kind.str(), self.size).as_bytes());
        self.file.rewind().map_err(failed)?;
        let mut buffer = [0u8; IO_BYTES];
        loop {
            let count = self.file.read(&mut buffer).map_err(failed)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        hash.finish()
    }
    pub(super) fn prefix(&mut self, limit: usize) -> Result<Vec<u8>, LocalGitFailure> {
        self.file.rewind().map_err(failed)?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut self.file)
            .take(limit as u64)
            .read_to_end(&mut bytes)
            .map_err(failed)?;
        Ok(bytes)
    }
}

pub(super) fn checkout_paths(
    repository: &crate::pinning::RepositoryShell,
    tree: &git2::Tree<'_>,
    paths: &std::collections::BTreeSet<std::path::PathBuf>,
    destination: &std::path::Path,
    mut updated: impl FnMut(&std::path::Path) -> Result<(), LocalGitFailure>,
) -> Result<(), LocalGitFailure> {
    use rustix::fs::{AtFlags, Mode, OFlags, mkdirat, openat, unlinkat};
    use std::{
        ffi::OsStr,
        os::unix::fs::PermissionsExt,
        path::{Component, Path},
    };
    let root = File::open(destination).map_err(failed)?;
    let files = crate::bounded::tree_files(repository, tree)?;
    for (path, (_, mode)) in &files {
        if paths
            .iter()
            .any(|selected| path == selected || path.starts_with(selected))
            && !matches!(mode, 0o100644 | 0o100755)
        {
            return Err(LocalGitFailure::Operation);
        }
    }
    for path in paths {
        if !files.contains_key(path) {
            match crate::rollback::open_worktree_parent(&root, path) {
                Ok((parent, leaf)) => match unlinkat(&parent, &leaf, AtFlags::empty()) {
                    Ok(()) => updated(path)?,
                    Err(rustix::io::Errno::NOENT) => {}
                    Err(_) => return Err(LocalGitFailure::Operation),
                },
                Err(_) if !destination.join(path).exists() => {}
                Err(error) => return Err(error),
            }
        }
    }
    for (path, (oid, mode)) in files {
        if !paths
            .iter()
            .any(|selected| path == *selected || path.starts_with(selected))
        {
            continue;
        }
        let mut parent = rustix::io::dup(&root).map_err(failed)?;
        for component in path.parent().unwrap_or_else(|| Path::new("")).components() {
            let Component::Normal(name) = component else {
                return Err(LocalGitFailure::Path);
            };
            match mkdirat(&parent, name, Mode::from_raw_mode(0o755)) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(error) => return Err(failed(error)),
            }
            parent = openat(
                &parent,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(failed)?;
        }
        let leaf = path.file_name().unwrap_or_else(|| OsStr::new(""));
        let mut content = repository.object_content(oid)?;
        let descriptor = openat(
            &parent,
            leaf,
            OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            crate::descriptor::mode_from_metadata_bits(mode & 0o777),
        )
        .map_err(failed)?;
        let mut target = File::from(descriptor);
        // Record each touched path even when a later write fails, for rollback ownership.
        updated(&path)?;
        std::io::copy(&mut content.file, &mut target).map_err(failed)?;
        target
            .set_permissions(std::fs::Permissions::from_mode(mode & 0o777))
            .map_err(failed)?;
        updated(&path)?;
    }
    Ok(())
}

fn check_deadline(deadline: Option<Instant>) -> Result<(), LocalGitFailure> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        Err(LocalGitFailure::Repository)
    } else {
        Ok(())
    }
}
