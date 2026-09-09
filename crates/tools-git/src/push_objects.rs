//! Descriptor-bound object capture for a push range; see git-authority-threat-model.md.

use crate::{
    descriptor::{
        FileIdentity, FileSnapshotIdentity, descriptor_path, file_identity, file_snapshot_identity,
    },
    failure::LocalGitFailure,
    layout::parse_full_object_id,
    limits::{
        MAX_LOOSE_OBJECT_HEADER_BYTES, MAX_OBJECT_BYTES, MAX_OBJECT_DATABASE_BYTES,
        MAX_REPOSITORY_INSPECTIONS, MAX_SHALLOW_ENTRIES,
    },
    pinning::{PinnedRepository, RepositoryShell},
};
use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use git2::{ObjectFormat, ObjectType, Odb, Oid};
use rustix::fs::{Mode, OFlags, openat};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    os::fd::AsFd,
    path::{Path, PathBuf},
    time::Instant,
};

pub(super) struct PushObjectSnapshot {
    pub(super) repository: RepositoryShell,
}

impl PushObjectSnapshot {
    #[cfg(test)]
    pub(super) fn capture(
        authority: &PinnedRepository,
        target: Oid,
        fence: Option<Oid>,
    ) -> Result<Self, LocalGitFailure> {
        Self::capture_before_deadline(
            authority,
            target,
            fence,
            Instant::now() + crate::push_executor::PUSH_PREPARATION_TIMEOUT,
        )
    }

    pub(super) fn capture_before_deadline(
        authority: &PinnedRepository,
        target: Oid,
        fence: Option<Oid>,
        deadline: Instant,
    ) -> Result<Self, LocalGitFailure> {
        let repository = authority.open_repository_shell()?;
        let database = repository.odb().map_err(|_| LocalGitFailure::Operation)?;
        let mut source = ObjectSource::open(authority, deadline)?;
        let mut excluded = BTreeSet::new();
        let mut boundaries = BTreeSet::new();
        if let Some(fence) = fence {
            retain_boundary(&repository, &mut source, fence, &mut excluded)?;
            boundaries.insert(fence);
        }
        let mut commits = vec![target];
        let mut visited = BTreeSet::new();
        let mut fence_ancestors: Option<BTreeSet<Oid>> = None;
        let mut trees = Vec::new();
        while let Some(commit) = commits.pop() {
            source.check_deadline()?;
            if !visited.insert(commit) {
                continue;
            }
            if visited
                .len()
                .saturating_add(fence_ancestors.as_ref().map_or(0, BTreeSet::len))
                > MAX_REPOSITORY_INSPECTIONS
            {
                return Err(LocalGitFailure::Repository);
            }
            if Some(commit) == fence {
                continue;
            }
            if fence_ancestors
                .as_ref()
                .is_some_and(|ancestors| ancestors.contains(&commit))
            {
                boundaries.insert(commit);
                if boundaries.len() > MAX_SHALLOW_ENTRIES {
                    return Err(LocalGitFailure::Repository);
                }
                retain_boundary(&repository, &mut source, commit, &mut excluded)?;
                continue;
            }
            source.capture(&database, commit)?;
            let commit = repository
                .find_commit(commit)
                .map_err(|_| LocalGitFailure::Operation)?;
            if commit.parent_count() > 1
                && fence_ancestors.is_none()
                && let Some(fence) = fence
            {
                fence_ancestors = Some(capture_fence_ancestors(authority, &mut source, fence)?);
            }
            trees.push(commit.tree_id());
            commits.extend(commit.parent_ids());
        }
        if fence.is_some_and(|fence| !visited.contains(&fence)) {
            return Err(LocalGitFailure::Operation);
        }
        if !boundaries.is_empty() {
            let shallow = boundaries
                .iter()
                .map(|boundary| format!("{boundary}\n"))
                .collect::<String>();
            fs::write(repository.path().join("shallow"), shallow)
                .map_err(|_| LocalGitFailure::Operation)?;
        }
        while let Some(tree) = trees.pop() {
            source.check_deadline()?;
            if !excluded.insert(tree) {
                continue;
            }
            source.capture(&database, tree)?;
            for entry in &repository
                .find_tree(tree)
                .map_err(|_| LocalGitFailure::Operation)?
            {
                match entry.kind() {
                    Some(ObjectType::Tree) => trees.push(entry.id()),
                    Some(ObjectType::Blob) => {
                        if excluded.insert(entry.id()) {
                            source.capture(&database, entry.id())?;
                        }
                    }
                    Some(ObjectType::Commit) => {}
                    _ => return Err(LocalGitFailure::Repository),
                }
            }
            if excluded.len() > MAX_REPOSITORY_INSPECTIONS {
                return Err(LocalGitFailure::Repository);
            }
        }
        source.validate(authority)?;
        drop(database);
        Ok(Self { repository })
    }
}

fn capture_fence_ancestors(
    authority: &PinnedRepository,
    source: &mut ObjectSource,
    fence: Oid,
) -> Result<BTreeSet<Oid>, LocalGitFailure> {
    let mut pending = vec![fence];
    let mut ancestors = BTreeSet::new();
    while let Some(oid) = pending.pop() {
        source.check_deadline()?;
        if !ancestors.insert(oid) {
            continue;
        }
        if ancestors.len() > MAX_REPOSITORY_INSPECTIONS {
            return Err(LocalGitFailure::Repository);
        }
        let graph = authority.open_repository_shell()?;
        let database = graph.odb().map_err(|_| LocalGitFailure::Operation)?;
        source.capture(&database, oid)?;
        let commit = graph
            .find_commit(oid)
            .map_err(|_| LocalGitFailure::Operation)?;
        pending.extend(commit.parent_ids());
    }
    Ok(ancestors)
}

fn retain_boundary(
    repository: &RepositoryShell,
    source: &mut ObjectSource,
    boundary: Oid,
    excluded: &mut BTreeSet<Oid>,
) -> Result<(), LocalGitFailure> {
    let database = repository.odb().map_err(|_| LocalGitFailure::Operation)?;
    source.capture(&database, boundary)?;
    let commit = repository
        .find_commit(boundary)
        .map_err(|_| LocalGitFailure::Operation)?;
    let mut trees = vec![commit.tree_id()];
    while let Some(tree) = trees.pop() {
        source.check_deadline()?;
        if !excluded.insert(tree) {
            continue;
        }
        source.capture(&database, tree)?;
        for entry in &repository
            .find_tree(tree)
            .map_err(|_| LocalGitFailure::Operation)?
        {
            match entry.kind() {
                Some(ObjectType::Tree) => trees.push(entry.id()),
                Some(ObjectType::Blob) => {
                    excluded.insert(entry.id());
                }
                Some(ObjectType::Commit) => {}
                _ => return Err(LocalGitFailure::Repository),
            }
        }
        if excluded.len() > MAX_REPOSITORY_INSPECTIONS {
            return Err(LocalGitFailure::Repository);
        }
    }
    Ok(())
}

struct SourceFile {
    directory: usize,
    name: PathBuf,
    file: File,
    identity: FileSnapshotIdentity,
}

struct SourceDirectory {
    path: PathBuf,
    directory: File,
    identity: FileIdentity,
}

struct Pack {
    source: usize,
    index: usize,
    count: usize,
    width: usize,
    offsets_start: usize,
    large_offsets_start: usize,
    index_end: usize,
    end: usize,
}

pub(super) struct ObjectSource {
    objects: File,
    directories: Vec<SourceDirectory>,
    files: Vec<SourceFile>,
    packs: Vec<Pack>,
    format: ObjectFormat,
    captured_bytes: usize,
    deadline: Instant,
}

fn rejected<T>(_: T) -> LocalGitFailure {
    LocalGitFailure::Repository
}

fn open_child(root: &File, path: &Path) -> Result<File, LocalGitFailure> {
    let parent = path.parent().ok_or(LocalGitFailure::Repository)?;
    let directory = openat(
        root,
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(rejected)?;
    let file = File::from(
        openat(
            directory.as_fd(),
            path.file_name().ok_or(LocalGitFailure::Repository)?,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(rejected)?,
    );
    if !file.metadata().map_err(rejected)?.is_file() {
        return Err(LocalGitFailure::Repository);
    }
    Ok(file)
}

impl ObjectSource {
    pub(super) fn open(
        authority: &PinnedRepository,
        deadline: Instant,
    ) -> Result<Self, LocalGitFailure> {
        authority.validate_object_layout()?;
        let objects = File::from(
            openat(
                &authority.git_directory,
                "objects",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(rejected)?,
        );
        let pack_directory = File::from(
            openat(
                &objects,
                "pack",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(rejected)?,
        );
        let pack_path = descriptor_path(&pack_directory);
        let mut source = Self {
            objects,
            directories: vec![SourceDirectory {
                path: PathBuf::from("pack"),
                identity: file_identity(&pack_directory.metadata().map_err(rejected)?),
                directory: pack_directory,
            }],
            files: Vec::new(),
            packs: Vec::new(),
            format: authority.object_format,
            captured_bytes: 0,
            deadline,
        };
        let mut scanned = 0usize;
        for entry in fs::read_dir(pack_path).map_err(rejected)? {
            source.check_deadline()?;
            scanned += 1;
            if scanned > MAX_REPOSITORY_INSPECTIONS {
                return Err(LocalGitFailure::Repository);
            }
            let name = entry.map_err(rejected)?.file_name();
            let Some(name) = name.to_str().and_then(|name| name.strip_suffix(".idx")) else {
                continue;
            };
            let checksum = name
                .strip_prefix("pack-")
                .and_then(|name| parse_full_object_id(name, source.format))
                .ok_or(LocalGitFailure::Repository)?;
            let index = source.open_file(&PathBuf::from("pack").join(format!("{name}.idx")))?;
            let width = checksum.as_bytes().len();
            let index_length =
                usize::try_from(source.files[index].identity.length).map_err(rejected)?;
            let index_end = index_length
                .checked_sub(width * 2)
                .ok_or(LocalGitFailure::Repository)?;
            if source.read(index, 0, 8)? != b"\xfftOc\0\0\0\x02" {
                return Err(LocalGitFailure::Repository);
            }
            let count = source.read_u32(index, 8 + 255 * 4)? as usize;
            let offsets_start = count
                .checked_mul(width + 4)
                .and_then(|bytes| bytes.checked_add(8 + 256 * 4))
                .ok_or(LocalGitFailure::Repository)?;
            let large_offsets_start = count
                .checked_mul(4)
                .and_then(|bytes| offsets_start.checked_add(bytes))
                .ok_or(LocalGitFailure::Repository)?;
            if large_offsets_start > index_end
                || (index_end - large_offsets_start) % 8 != 0
                || source.read(index, index_end, width)? != checksum.as_bytes()
            {
                return Err(LocalGitFailure::Repository);
            }
            let pack = source.open_file(&PathBuf::from("pack").join(format!("{name}.pack")))?;
            let length = usize::try_from(source.files[pack].identity.length).map_err(rejected)?;
            let end = length
                .checked_sub(width)
                .ok_or(LocalGitFailure::Repository)?;
            let header = source.read(pack, 0, 12)?;
            if end < 12
                || &header[..4] != b"PACK"
                || !matches!(&header[4..8], [0, 0, 0, 2] | [0, 0, 0, 3])
                || u32::from_be_bytes(header[8..12].try_into().map_err(rejected)?) as usize != count
                || source.read(pack, end, width)? != checksum.as_bytes()
            {
                return Err(LocalGitFailure::Repository);
            }
            source.packs.push(Pack {
                source: pack,
                index,
                count,
                width,
                offsets_start,
                large_offsets_start,
                index_end,
                end,
            });
        }
        Ok(source)
    }

    // Stop timed-out blocking work between bounded reads and decodes.
    fn check_deadline(&self) -> Result<(), LocalGitFailure> {
        if Instant::now() >= self.deadline {
            return Err(LocalGitFailure::Repository);
        }
        Ok(())
    }

    fn open_file(&mut self, path: &Path) -> Result<usize, LocalGitFailure> {
        let parent = path.parent().ok_or(LocalGitFailure::Repository)?;
        let directory = if let Some(index) = self
            .directories
            .iter()
            .position(|directory| directory.path == parent)
        {
            index
        } else {
            let directory = File::from(
                openat(
                    &self.objects,
                    parent,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(rejected)?,
            );
            let identity = file_identity(&directory.metadata().map_err(rejected)?);
            let index = self.directories.len();
            self.directories.push(SourceDirectory {
                path: parent.to_owned(),
                directory,
                identity,
            });
            if self.directories.len() > MAX_REPOSITORY_INSPECTIONS {
                return Err(LocalGitFailure::Repository);
            }
            index
        };
        let file = File::from(
            openat(
                self.directories[directory].directory.as_fd(),
                path.file_name().ok_or(LocalGitFailure::Repository)?,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(rejected)?,
        );
        if !file.metadata().map_err(rejected)?.is_file() {
            return Err(LocalGitFailure::Repository);
        }
        let identity = file_snapshot_identity(&file.metadata().map_err(rejected)?);
        let index = self.files.len();
        self.files.push(SourceFile {
            directory,
            name: PathBuf::from(path.file_name().ok_or(LocalGitFailure::Repository)?),
            file,
            identity,
        });
        if self.files.len() > MAX_REPOSITORY_INSPECTIONS {
            return Err(LocalGitFailure::Repository);
        }
        Ok(index)
    }

    fn read(
        &self,
        source: usize,
        offset: usize,
        length: usize,
    ) -> Result<Vec<u8>, LocalGitFailure> {
        self.check_deadline()?;
        if length > MAX_OBJECT_DATABASE_BYTES {
            return Err(LocalGitFailure::Repository);
        }
        let entry = &self.files[source];
        let mut file = entry.file.try_clone().map_err(rejected)?;
        file.seek(SeekFrom::Start(offset as u64))
            .map_err(rejected)?;
        let mut bytes = vec![0; length];
        file.read_exact(&mut bytes).map_err(rejected)?;
        if file_snapshot_identity(&file.metadata().map_err(rejected)?) != entry.identity {
            return Err(LocalGitFailure::Repository);
        }
        Ok(bytes)
    }

    fn charge(&mut self, bytes: usize) -> Result<(), LocalGitFailure> {
        self.captured_bytes = self
            .captured_bytes
            .checked_add(bytes)
            .filter(|n| *n <= MAX_OBJECT_DATABASE_BYTES)
            .ok_or(LocalGitFailure::Repository)?;
        Ok(())
    }

    pub(super) fn capture(&mut self, database: &Odb<'_>, oid: Oid) -> Result<(), LocalGitFailure> {
        self.check_deadline()?;
        if database.exists(oid) {
            return Ok(());
        }
        let hex = oid.to_string();
        let path = PathBuf::from(&hex[..2]).join(&hex[2..]);
        if open_child(&self.objects, &path).is_ok() {
            let file = self.open_file(&path)?;
            let length = usize::try_from(self.files[file].identity.length).map_err(rejected)?;
            if length > MAX_OBJECT_BYTES * 2 {
                return Err(LocalGitFailure::Repository);
            }
            self.charge(length)?;
            let compressed = self.read(file, 0, length)?;
            let mut decoder = ZlibDecoder::new(compressed.as_slice());
            let mut content = Vec::new();
            Read::by_ref(&mut decoder)
                .take((MAX_OBJECT_BYTES + MAX_LOOSE_OBJECT_HEADER_BYTES + 1) as u64)
                .read_to_end(&mut content)
                .map_err(rejected)?;
            let header = content
                .iter()
                .position(|b| *b == 0)
                .filter(|n| *n <= MAX_LOOSE_OBJECT_HEADER_BYTES)
                .ok_or(LocalGitFailure::Repository)?;
            let (kind, size) = std::str::from_utf8(&content[..header])
                .map_err(rejected)?
                .split_once(' ')
                .ok_or(LocalGitFailure::Repository)?;
            let kind = ObjectType::from_str(kind).ok_or(LocalGitFailure::Repository)?;
            let declared_size = size;
            let size = declared_size.parse::<usize>().map_err(rejected)?;
            if size.to_string() != declared_size
                || !matches!(
                    kind,
                    ObjectType::Blob | ObjectType::Tree | ObjectType::Commit | ObjectType::Tag
                )
            {
                return Err(LocalGitFailure::Repository);
            }
            if size > MAX_OBJECT_BYTES
                || content.len() != header + 1 + size
                || decoder.total_in() != length as u64
            {
                return Err(LocalGitFailure::Repository);
            }
            self.charge(size)?;
            if database
                .write(kind, &content[header + 1..])
                .map_err(rejected)?
                != oid
            {
                return Err(LocalGitFailure::Repository);
            }
        } else {
            self.capture_pack(database, oid)?;
        }
        Ok(())
    }

    fn read_u32(&self, source: usize, offset: usize) -> Result<u32, LocalGitFailure> {
        Ok(u32::from_be_bytes(
            self.read(source, offset, 4)?.try_into().map_err(rejected)?,
        ))
    }

    fn packed_offset(&self, pack: &Pack, oid: Oid) -> Result<Option<usize>, LocalGitFailure> {
        // Index entries are lookup hints; the captured object's computed ID is authoritative.
        let mut low = 0;
        let mut high = pack.count;
        while low < high {
            let middle = low + (high - low) / 2;
            let candidate = self.read(pack.index, 8 + 256 * 4 + middle * pack.width, pack.width)?;
            match candidate.as_slice().cmp(oid.as_bytes()) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => {
                    let offset = self.read_u32(pack.index, pack.offsets_start + middle * 4)?;
                    let offset = if offset & 0x8000_0000 == 0 {
                        u64::from(offset)
                    } else {
                        let position = (offset & 0x7fff_ffff) as usize;
                        if position >= (pack.index_end - pack.large_offsets_start) / 8 {
                            return Err(LocalGitFailure::Repository);
                        }
                        u64::from_be_bytes(
                            self.read(pack.index, pack.large_offsets_start + position * 8, 8)?
                                .try_into()
                                .map_err(rejected)?,
                        )
                    };
                    let offset = usize::try_from(offset).map_err(rejected)?;
                    if offset < 12 || offset >= pack.end {
                        return Err(LocalGitFailure::Repository);
                    }
                    return Ok(Some(offset));
                }
            }
        }
        Ok(None)
    }

    fn capture_pack(&mut self, database: &Odb<'_>, oid: Oid) -> Result<(), LocalGitFailure> {
        let mut location = None;
        for (index, pack) in self.packs.iter().enumerate() {
            if let Some(offset) = self.packed_offset(pack, oid)? {
                location = Some((index, offset));
                break;
            }
        }
        let (pack_index, mut offset) = location.ok_or(LocalGitFailure::Repository)?;
        let mut selected = HashSet::new();
        let mut entries = Vec::new();
        loop {
            self.check_deadline()?;
            if !selected.insert(offset) || selected.len() > MAX_REPOSITORY_INSPECTIONS {
                return Err(LocalGitFailure::Repository);
            }
            let pack = &self.packs[pack_index];
            if offset < 12 || offset >= pack.end {
                return Err(LocalGitFailure::Repository);
            }
            let mut file = self.files[pack.source].file.try_clone().map_err(rejected)?;
            file.seek(SeekFrom::Start(offset as u64))
                .map_err(rejected)?;
            let remaining_budget = MAX_OBJECT_DATABASE_BYTES - self.captured_bytes;
            let mut remaining = file.take((pack.end - offset).min(remaining_budget) as u64);
            let first = byte(&mut remaining)?;
            let kind = (first >> 4) & 7;
            let size = if first & 0x80 == 0 {
                usize::from(first & 15)
            } else {
                variable_size(&mut remaining)?
                    .checked_mul(16)
                    .and_then(|n| n.checked_add(usize::from(first & 15)))
                    .ok_or(LocalGitFailure::Repository)?
            };
            if size > MAX_OBJECT_BYTES {
                return Err(LocalGitFailure::Repository);
            }
            let base = match kind {
                1..=4 => None,
                6 => {
                    let mut part = byte(&mut remaining)?;
                    let mut distance = usize::from(part & 127);
                    while part & 128 != 0 {
                        part = byte(&mut remaining)?;
                        distance = distance
                            .checked_add(1)
                            .and_then(|n| n.checked_mul(128))
                            .and_then(|n| n.checked_add(usize::from(part & 127)))
                            .ok_or(LocalGitFailure::Repository)?;
                    }
                    Some(
                        offset
                            .checked_sub(distance)
                            .filter(|n| *n < offset)
                            .ok_or(LocalGitFailure::Repository)?,
                    )
                }
                7 => {
                    let mut bytes = vec![0; pack.width];
                    remaining.read_exact(&mut bytes).map_err(rejected)?;
                    let base = Oid::from_bytes(&bytes).map_err(rejected)?;
                    Some(
                        self.packed_offset(pack, base)?
                            .ok_or(LocalGitFailure::Repository)?,
                    )
                }
                _ => return Err(LocalGitFailure::Repository),
            };
            let mut decoder = ZlibDecoder::new(remaining);
            let mut content = Vec::new();
            Read::by_ref(&mut decoder)
                .take(size as u64 + 1)
                .read_to_end(&mut content)
                .map_err(rejected)?;
            if content.len() != size {
                return Err(LocalGitFailure::Repository);
            }
            self.charge(decoder.total_in() as usize)?;
            self.charge(size)?;
            if base.is_some() {
                let mut header = content.as_slice();
                let base_size = variable_size(&mut header)?;
                let target_size = variable_size(&mut header)?;
                if base_size > MAX_OBJECT_BYTES || target_size > MAX_OBJECT_BYTES {
                    return Err(LocalGitFailure::Repository);
                }
                self.charge(target_size)?;
            }
            entries.push((kind, content));
            match base {
                Some(base) => offset = base,
                None => break,
            }
        }
        // Emit the selected dependency chain base-first with new OFS_DELTA offsets.
        let mut pack = b"PACK\0\0\0\x02".to_vec();
        pack.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        let mut previous_offset = 0;
        for (kind, content) in entries.into_iter().rev() {
            self.check_deadline()?;
            let offset = pack.len();
            let mut size = content.len();
            let first = ((if kind >= 6 { 6 } else { kind }) << 4) | (size & 15) as u8;
            size >>= 4;
            pack.push(first | if size == 0 { 0 } else { 128 });
            while size != 0 {
                let part = (size & 127) as u8;
                size >>= 7;
                pack.push(part | if size == 0 { 0 } else { 128 });
            }
            if kind >= 6 {
                let mut distance = offset - previous_offset;
                let mut encoded = vec![(distance & 127) as u8];
                while distance > 127 {
                    distance = (distance >> 7) - 1;
                    encoded.push(128 | (distance & 127) as u8);
                }
                pack.extend(encoded.into_iter().rev());
            }
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&content).map_err(rejected)?;
            pack.extend(encoder.finish().map_err(rejected)?);
            previous_offset = offset;
        }
        let checksum = match self.format {
            ObjectFormat::Sha1 => Sha1::digest(&pack).to_vec(),
            ObjectFormat::Sha256 => Sha256::digest(&pack).to_vec(),
        };
        pack.extend_from_slice(&checksum);
        let mut writer = database.packwriter().map_err(rejected)?;
        writer.write_all(&pack).map_err(rejected)?;
        writer.commit().map_err(rejected)?;
        if !database.exists(oid) {
            return Err(LocalGitFailure::Repository);
        }
        Ok(())
    }

    pub(super) fn validate(&self, authority: &PinnedRepository) -> Result<(), LocalGitFailure> {
        authority.validate_object_layout()?;
        let current = File::from(
            openat(
                &authority.git_directory,
                "objects",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(rejected)?,
        );
        if file_identity(&current.metadata().map_err(rejected)?)
            != file_identity(&self.objects.metadata().map_err(rejected)?)
        {
            return Err(LocalGitFailure::Repository);
        }
        for entry in &self.directories {
            self.check_deadline()?;
            let current = File::from(
                openat(
                    &self.objects,
                    &entry.path,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(rejected)?,
            );
            if file_identity(&current.metadata().map_err(rejected)?) != entry.identity
                || file_identity(&entry.directory.metadata().map_err(rejected)?) != entry.identity
            {
                return Err(LocalGitFailure::Repository);
            }
        }
        for entry in &self.files {
            self.check_deadline()?;
            let current = File::from(
                openat(
                    self.directories[entry.directory].directory.as_fd(),
                    &entry.name,
                    OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(rejected)?,
            );
            if file_snapshot_identity(&current.metadata().map_err(rejected)?) != entry.identity
                || file_snapshot_identity(&entry.file.metadata().map_err(rejected)?)
                    != entry.identity
            {
                return Err(LocalGitFailure::Repository);
            }
        }
        Ok(())
    }
}

fn byte(reader: &mut impl Read) -> Result<u8, LocalGitFailure> {
    let mut byte = [0];
    reader.read_exact(&mut byte).map_err(rejected)?;
    Ok(byte[0])
}

fn variable_size(reader: &mut impl Read) -> Result<usize, LocalGitFailure> {
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
