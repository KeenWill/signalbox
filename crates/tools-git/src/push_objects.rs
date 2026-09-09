//! Descriptor-bound object capture for a push range; see git-authority-threat-model.md.

use crate::{
    descriptor::{FileSnapshotIdentity, descriptor_path, file_identity, file_snapshot_identity},
    failure::LocalGitFailure,
    layout::parse_full_object_id,
    limits::{
        MAX_LOOSE_OBJECT_HEADER_BYTES, MAX_OBJECT_BYTES, MAX_OBJECT_DATABASE_BYTES,
        MAX_REPOSITORY_INSPECTIONS,
    },
    pinning::{PinnedRepository, RepositoryShell, parse_pack_index},
};
use flate2::read::ZlibDecoder;
use git2::{ObjectFormat, ObjectType, Odb, Oid};
use rustix::fs::{Mode, OFlags, openat};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    os::fd::AsFd,
    path::{Path, PathBuf},
};

pub(super) struct PushObjectSnapshot {
    pub(super) repository: RepositoryShell,
}

impl PushObjectSnapshot {
    pub(super) fn capture(
        authority: &PinnedRepository,
        target: Oid,
        fence: Option<Oid>,
    ) -> Result<Self, LocalGitFailure> {
        let repository = authority.open_repository_shell()?;
        let database = repository.odb().map_err(|_| LocalGitFailure::Operation)?;
        let mut source = ObjectSource::open(authority)?;
        let mut excluded = HashSet::new();
        if let Some(fence) = fence {
            source.capture(&database, fence)?;
            let commit = repository
                .find_commit(fence)
                .map_err(|_| LocalGitFailure::Operation)?;
            let mut trees = vec![commit.tree_id()];
            while let Some(tree) = trees.pop() {
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
            fs::write(repository.path().join("shallow"), format!("{fence}\n"))
                .map_err(|_| LocalGitFailure::Operation)?;
        }
        let mut commits = vec![target];
        let mut visited = HashSet::new();
        let mut trees = Vec::new();
        while let Some(commit) = commits.pop() {
            if !visited.insert(commit) {
                continue;
            }
            if visited.len() > MAX_REPOSITORY_INSPECTIONS {
                return Err(LocalGitFailure::Repository);
            }
            if Some(commit) == fence {
                continue;
            }
            source.capture(&database, commit)?;
            let commit = repository
                .find_commit(commit)
                .map_err(|_| LocalGitFailure::Operation)?;
            trees.push(commit.tree_id());
            commits.extend(commit.parent_ids());
        }
        if fence.is_some_and(|fence| !visited.contains(&fence)) {
            return Err(LocalGitFailure::Operation);
        }
        while let Some(tree) = trees.pop() {
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

struct SourceFile {
    path: PathBuf,
    file: File,
    identity: FileSnapshotIdentity,
}

struct Pack {
    source: usize,
    entries: BTreeMap<Oid, (usize, usize)>,
    offsets: BTreeMap<usize, Oid>,
}

struct ObjectSource {
    objects: File,
    files: Vec<SourceFile>,
    packs: Vec<Pack>,
    format: ObjectFormat,
    captured_bytes: usize,
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
    fn open(authority: &PinnedRepository) -> Result<Self, LocalGitFailure> {
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
        let mut source = Self {
            objects,
            files: Vec::new(),
            packs: Vec::new(),
            format: authority.object_format,
            captured_bytes: 0,
        };
        let mut scanned = 0usize;
        let mut index_bytes = 0usize;
        let mut object_count = 0usize;
        for entry in fs::read_dir(descriptor_path(&pack_directory)).map_err(rejected)? {
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
            let length = usize::try_from(source.files[index].identity.length).map_err(rejected)?;
            index_bytes = index_bytes
                .checked_add(length)
                .filter(|n| *n <= MAX_OBJECT_DATABASE_BYTES)
                .ok_or(LocalGitFailure::Repository)?;
            let indexed =
                parse_pack_index(&source.read(index, 0, length)?, checksum, source.format)?;
            object_count += indexed.len();
            if object_count > MAX_REPOSITORY_INSPECTIONS {
                return Err(LocalGitFailure::Repository);
            }
            let pack = source.open_file(&PathBuf::from("pack").join(format!("{name}.pack")))?;
            let length = usize::try_from(source.files[pack].identity.length).map_err(rejected)?;
            let end = length
                .checked_sub(checksum.as_bytes().len())
                .ok_or(LocalGitFailure::Repository)?;
            let header = source.read(pack, 0, 12)?;
            if &header[..4] != b"PACK"
                || !matches!(&header[4..8], [0, 0, 0, 2] | [0, 0, 0, 3])
                || u32::from_be_bytes(header[8..12].try_into().map_err(rejected)?) as usize
                    != indexed.len()
                || source.read(pack, end, checksum.as_bytes().len())? != checksum.as_bytes()
            {
                return Err(LocalGitFailure::Repository);
            }
            let offsets: BTreeMap<_, _> = indexed
                .iter()
                .map(|(oid, offset)| (*offset, *oid))
                .collect();
            if offsets.len() != indexed.len() {
                return Err(LocalGitFailure::Repository);
            }
            let mut entries = BTreeMap::new();
            let ordered: Vec<_> = offsets.iter().collect();
            for (position, &(&offset, &oid)) in ordered.iter().enumerate() {
                let next = ordered
                    .get(position + 1)
                    .map_or(end, |&(&offset, _)| offset);
                if offset < 12 || next <= offset || next > end {
                    return Err(LocalGitFailure::Repository);
                }
                entries.insert(oid, (offset, next - offset));
            }
            source.packs.push(Pack {
                source: pack,
                entries,
                offsets,
            });
        }
        Ok(source)
    }

    fn open_file(&mut self, path: &Path) -> Result<usize, LocalGitFailure> {
        let file = open_child(&self.objects, path)?;
        let identity = file_snapshot_identity(&file.metadata().map_err(rejected)?);
        let index = self.files.len();
        self.files.push(SourceFile {
            path: path.to_owned(),
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

    fn capture(&mut self, database: &Odb<'_>, oid: Oid) -> Result<(), LocalGitFailure> {
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

    fn capture_pack(&mut self, database: &Odb<'_>, oid: Oid) -> Result<(), LocalGitFailure> {
        let pack_index = self
            .packs
            .iter()
            .position(|pack| pack.entries.contains_key(&oid))
            .ok_or(LocalGitFailure::Repository)?;
        let mut pending = vec![oid];
        let mut selected = HashSet::new();
        let mut data = Vec::new();
        while let Some(oid) = pending.pop() {
            if !selected.insert(oid) {
                continue;
            }
            if selected.len() > MAX_REPOSITORY_INSPECTIONS {
                return Err(LocalGitFailure::Repository);
            }
            let pack = &self.packs[pack_index];
            let &(offset, length) = pack.entries.get(&oid).ok_or(LocalGitFailure::Repository)?;
            let source = pack.source;
            self.charge(length)?;
            let bytes = self.read(source, offset, length)?;
            let pack = &self.packs[pack_index];
            let mut remaining = bytes.as_slice();
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
            let header_end = bytes.len() - remaining.len();
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
                    let base = offset
                        .checked_sub(distance)
                        .filter(|n| *n < offset)
                        .ok_or(LocalGitFailure::Repository)?;
                    Some(*pack.offsets.get(&base).ok_or(LocalGitFailure::Repository)?)
                }
                7 => {
                    let width = oid.as_bytes().len();
                    let base =
                        Oid::from_bytes(remaining.get(..width).ok_or(LocalGitFailure::Repository)?)
                            .map_err(rejected)?;
                    remaining = &remaining[width..];
                    Some(base)
                }
                _ => return Err(LocalGitFailure::Repository),
            };
            if let Some(base) = base {
                let mut header = ZlibDecoder::new(remaining).take(size as u64);
                let base_size = variable_size(&mut header)?;
                let target_size = variable_size(&mut header)?;
                if base_size > MAX_OBJECT_BYTES || target_size > MAX_OBJECT_BYTES {
                    return Err(LocalGitFailure::Repository);
                }
                self.charge(target_size)?;
                pending.push(base);
                data.push((first & !0x70) | 0x70);
                data.extend_from_slice(&bytes[1..header_end]);
                data.extend_from_slice(base.as_bytes());
                data.extend_from_slice(remaining);
            } else {
                data.extend_from_slice(&bytes);
            }
            self.charge(size)?;
        }
        // Git pack-format: selected delta dependencies form a self-contained pack.
        // OFS_DELTA is emitted as REF_DELTA so source offsets need not be retained.
        let mut pack = b"PACK\0\0\0\x02".to_vec();
        pack.extend_from_slice(&(selected.len() as u32).to_be_bytes());
        pack.extend_from_slice(&data);
        let checksum = match self.format {
            ObjectFormat::Sha1 => Sha1::digest(&pack).to_vec(),
            ObjectFormat::Sha256 => Sha256::digest(&pack).to_vec(),
        };
        pack.extend_from_slice(&checksum);
        let mut writer = database.packwriter().map_err(rejected)?;
        writer.write_all(&pack).map_err(rejected)?;
        writer.commit().map_err(rejected)?;
        for oid in selected {
            if !database.exists(oid) {
                return Err(LocalGitFailure::Repository);
            }
        }
        Ok(())
    }

    fn validate(&self, authority: &PinnedRepository) -> Result<(), LocalGitFailure> {
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
        for entry in &self.files {
            let current = open_child(&self.objects, &entry.path)?;
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
