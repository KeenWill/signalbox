//! Descriptor-bound object capture for a push range; see git-authority-threat-model.md.

use crate::streamed_object::ObjectContent;
use crate::{
    descriptor::{
        FileIdentity, FileSnapshotIdentity, descriptor_path, file_identity, file_snapshot_identity,
    },
    failure::LocalGitFailure,
    layout::parse_full_object_id,
    limits::{MAX_LOOSE_OBJECT_HEADER_BYTES, MAX_REPOSITORY_INSPECTIONS, MAX_SHALLOW_ENTRIES},
    pinning::{PinnedRepository, RepositoryShell},
};
use flate2::read::ZlibDecoder;
use git2::{ObjectFormat, ObjectType, Odb, Oid};
use rustix::fs::{Mode, OFlags, openat};
use std::{
    collections::{BTreeSet, HashSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
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
        let mut source = ObjectSource::open(authority, Some(deadline))?;
        let mut excluded = BTreeSet::new();
        let mut boundaries = BTreeSet::new();
        if let Some(fence) = fence {
            retain_boundary(&repository, &mut source, fence, &mut excluded)?;
            boundaries.insert(fence);
        }
        let mut commits = vec![target];
        let mut visited = BTreeSet::new();
        let mut fence_ancestors: Option<(BTreeSet<Oid>, ObjectSource)> = None;
        let mut trees = Vec::new();
        while let Some(commit) = commits.pop() {
            source.check_deadline()?;
            if !visited.insert(commit) {
                continue;
            }
            if visited.len().saturating_add(
                fence_ancestors
                    .as_ref()
                    .map_or(0, |(ancestors, _)| ancestors.len()),
            ) > MAX_REPOSITORY_INSPECTIONS
            {
                return Err(LocalGitFailure::Repository);
            }
            if Some(commit) == fence {
                continue;
            }
            if fence_ancestors
                .as_ref()
                .is_some_and(|(ancestors, _)| ancestors.contains(&commit))
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
                fence_ancestors = Some(capture_fence_ancestors(authority, deadline, fence)?);
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
        if let Some((_, graph_source)) = &fence_ancestors {
            graph_source.validate(authority)?;
        }
        source.validate(authority)?;
        source.publish_private_packs(&repository.path().join("objects/pack"))?;
        drop(database);
        repository.retain_selected_source(source);
        Ok(Self { repository })
    }
}

fn capture_fence_ancestors(
    authority: &PinnedRepository,
    deadline: Instant,
    fence: Oid,
) -> Result<(BTreeSet<Oid>, ObjectSource), LocalGitFailure> {
    let graph = authority.open_repository_shell()?;
    let database = graph.odb().map_err(|_| LocalGitFailure::Operation)?;
    let mut source = ObjectSource::open(authority, Some(deadline))?;
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
        source.capture(&database, oid)?;
        let commit = graph
            .find_commit(oid)
            .map_err(|_| LocalGitFailure::Operation)?;
        pending.extend(commit.parent_ids());
    }
    Ok((ancestors, source))
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
    file: Option<File>,
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
    max_object_bytes: Option<usize>,
    pub(super) directory: tempfile::TempDir,
    deadline: Option<Instant>,
}

fn rejected<T>(_: T) -> LocalGitFailure {
    LocalGitFailure::Repository
}

fn open_child(root: &File, path: &Path) -> Result<Option<File>, LocalGitFailure> {
    let parent = path.parent().ok_or(LocalGitFailure::Repository)?;
    let directory = match openat(
        root,
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(directory) => directory,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(rejected(error)),
    };
    let file = match openat(
        directory.as_fd(),
        path.file_name().ok_or(LocalGitFailure::Repository)?,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(file) => File::from(file),
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(rejected(error)),
    };
    if !file.metadata().map_err(rejected)?.is_file() {
        return Err(LocalGitFailure::Repository);
    }
    Ok(Some(file))
}

impl ObjectSource {
    pub(super) fn open(
        authority: &PinnedRepository,
        deadline: Option<Instant>,
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
            max_object_bytes: authority.max_object_bytes,
            directory: tempfile::tempdir().map_err(rejected)?,
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
            crate::pack_read_bounds::validate_pack_header(&header, end, count)?;
            if source.read(pack, end, width)? != checksum.as_bytes() {
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
        self.check_deadline_at(Instant::now())
    }

    pub(super) fn check_deadline_at(&self, now: Instant) -> Result<(), LocalGitFailure> {
        if self.deadline.is_some_and(|deadline| now >= deadline) {
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
            file: Some(file),
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
        let entry = &self.files[source];
        let mut file = entry
            .file
            .as_ref()
            .ok_or(LocalGitFailure::Repository)?
            .try_clone()
            .map_err(rejected)?;
        file.seek(SeekFrom::Start(offset as u64))
            .map_err(rejected)?;
        let mut bytes = vec![0; length];
        file.read_exact(&mut bytes).map_err(rejected)?;
        if file_snapshot_identity(&file.metadata().map_err(rejected)?) != entry.identity {
            return Err(LocalGitFailure::Repository);
        }
        Ok(bytes)
    }

    fn publish_private_packs(&self, destination: &Path) -> Result<(), LocalGitFailure> {
        let mut objects = Vec::new();
        for directory in fs::read_dir(self.directory.path()).map_err(rejected)? {
            let directory = directory.map_err(rejected)?;
            for entry in fs::read_dir(directory.path()).map_err(rejected)? {
                self.check_deadline()?;
                let entry = entry.map_err(rejected)?;
                let hex = format!(
                    "{}{}",
                    directory.file_name().to_string_lossy(),
                    entry.file_name().to_string_lossy()
                );
                let oid =
                    parse_full_object_id(&hex, self.format).ok_or(LocalGitFailure::Repository)?;
                objects.push(oid);
                if objects.len() > MAX_REPOSITORY_INSPECTIONS {
                    return Err(LocalGitFailure::Repository);
                }
            }
        }
        if !objects.is_empty() {
            crate::streamed_object::write_pack(
                &objects,
                |oid| {
                    self.check_deadline()?;
                    let hex = oid.to_string();
                    let path = self.directory.path().join(&hex[..2]).join(&hex[2..]);
                    self.decode_loose(File::open(path).map_err(rejected)?)
                },
                self.format,
                destination,
                self.deadline,
            )?;
        }
        Ok(())
    }

    pub(super) fn contains(&mut self, oid: Oid) -> Result<bool, LocalGitFailure> {
        self.check_deadline()?;
        let hex = oid.to_string();
        let mut present =
            open_child(&self.objects, &PathBuf::from(&hex[..2]).join(&hex[2..]))?.is_some();
        if !present {
            for pack in &self.packs {
                if self.packed_offset(pack, oid)?.is_some() {
                    present = true;
                    break;
                }
            }
        }
        if present {
            let database = Odb::new_ext(self.format).map_err(rejected)?;
            // A generated private copy does not establish that the live copy is valid.
            self.capture_live(&database, oid)?;
        }
        Ok(present)
    }

    pub(super) fn attach(&self, database: &Odb<'_>) -> Result<(), LocalGitFailure> {
        // libgit2 deduplicates disk backends by directory identity.
        database
            .add_disk_alternate(
                self.directory
                    .path()
                    .to_str()
                    .ok_or(LocalGitFailure::Repository)?,
            )
            .map_err(rejected)
    }

    pub(super) fn store(
        &mut self,
        database: &Odb<'_>,
        content: &mut ObjectContent,
    ) -> Result<Oid, LocalGitFailure> {
        if content.size > crate::limits::object_byte_limit(self.max_object_bytes, content.kind) {
            return Err(LocalGitFailure::Repository);
        }
        self.attach(database)?;
        content.store(self.directory.path(), self.format, self.deadline)
    }

    pub(super) fn content(&self, oid: Oid) -> Result<Option<ObjectContent>, LocalGitFailure> {
        let hex = oid.to_string();
        let path = self.directory.path().join(&hex[..2]).join(&hex[2..]);
        match File::open(path) {
            Ok(file) => self.decode_loose(file).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(rejected(error)),
        }
    }

    fn decode_loose(&self, file: File) -> Result<ObjectContent, LocalGitFailure> {
        let compressed_size = file.metadata().map_err(rejected)?.len();
        let mut decoder = ZlibDecoder::new(std::io::BufReader::new(file));
        let mut header = Vec::new();
        loop {
            let byte = byte(&mut decoder)?;
            if byte == 0 {
                break;
            }
            if header.len() == MAX_LOOSE_OBJECT_HEADER_BYTES {
                return Err(LocalGitFailure::Repository);
            }
            header.push(byte);
        }
        let (kind, size) = std::str::from_utf8(&header)
            .map_err(rejected)?
            .split_once(' ')
            .ok_or(LocalGitFailure::Repository)?;
        let kind = ObjectType::from_str(kind)
            .filter(|kind| {
                matches!(
                    kind,
                    ObjectType::Blob | ObjectType::Tree | ObjectType::Commit | ObjectType::Tag
                )
            })
            .ok_or(LocalGitFailure::Repository)?;
        let declared = size;
        let size = size.parse::<usize>().map_err(rejected)?;
        if size.to_string() != declared
            || size > crate::limits::object_byte_limit(self.max_object_bytes, kind)
        {
            return Err(LocalGitFailure::Repository);
        }
        let content = ObjectContent::decode(&mut decoder, size, kind, self.deadline)?;
        if decoder.total_in() != compressed_size {
            return Err(LocalGitFailure::Repository);
        }
        Ok(content)
    }

    pub(super) fn capture(&mut self, database: &Odb<'_>, oid: Oid) -> Result<(), LocalGitFailure> {
        self.check_deadline()?;
        self.attach(database)?;
        if database.exists(oid) {
            return Ok(());
        }
        self.capture_live(database, oid)
    }

    fn capture_live(&mut self, database: &Odb<'_>, oid: Oid) -> Result<(), LocalGitFailure> {
        let hex = oid.to_string();
        let path = PathBuf::from(&hex[..2]).join(&hex[2..]);
        if open_child(&self.objects, &path)?.is_some() {
            let source = self.open_file(&path)?;
            let file = self.files[source]
                .file
                .as_ref()
                .ok_or(LocalGitFailure::Repository)?
                .try_clone()
                .map_err(rejected)?;
            let mut content = self.decode_loose(file)?;
            if self.store(database, &mut content)? != oid {
                return Err(LocalGitFailure::Repository);
            }
            // Keep the source identity while releasing verified loose-object descriptors.
            self.files[source].file = None;
            return Ok(());
        }
        self.capture_pack(database, oid)
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
        let mut content = loop {
            self.check_deadline()?;
            if !selected.insert(offset) || selected.len() > MAX_REPOSITORY_INSPECTIONS {
                return Err(LocalGitFailure::Repository);
            }
            let pack = &self.packs[pack_index];
            if offset < 12 || offset >= pack.end {
                return Err(LocalGitFailure::Repository);
            }
            let mut file = self.files[pack.source]
                .file
                .as_ref()
                .ok_or(LocalGitFailure::Repository)?
                .try_clone()
                .map_err(rejected)?;
            file.seek(SeekFrom::Start(offset as u64))
                .map_err(rejected)?;
            let mut remaining = file.take((pack.end - offset) as u64);
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
            crate::pack_read_bounds::validate_decoded_size(size, self.max_object_bytes)?;
            let object_kind = match kind {
                1 => ObjectType::Commit,
                2 => ObjectType::Tree,
                3 | 6 | 7 => ObjectType::Blob,
                4 => ObjectType::Tag,
                _ => return Err(LocalGitFailure::Repository),
            };
            if size > crate::limits::object_byte_limit(self.max_object_bytes, object_kind) {
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
            if let Some(base) = base {
                entries.push((pack.end as u64 - remaining.limit(), size));
                offset = base;
            } else {
                let mut decoder = ZlibDecoder::new(std::io::BufReader::new(remaining));
                break ObjectContent::decode(&mut decoder, size, object_kind, self.deadline)?;
            }
        };
        let pack = &self.packs[pack_index];
        while let Some((offset, size)) = entries.pop() {
            self.check_deadline()?;
            let mut file = self.files[pack.source]
                .file
                .as_ref()
                .ok_or(LocalGitFailure::Repository)?
                .try_clone()
                .map_err(rejected)?;
            file.seek(SeekFrom::Start(offset)).map_err(rejected)?;
            let mut decoder =
                ZlibDecoder::new(std::io::BufReader::new(file.take(pack.end as u64 - offset)));
            let delta = ObjectContent::decode(&mut decoder, size, ObjectType::Blob, self.deadline)?;
            drop(decoder);
            let limit = crate::limits::object_byte_limit(self.max_object_bytes, content.kind);
            content = content.apply_delta(delta.file, Some(limit), self.deadline)?;
        }
        if self.store(database, &mut content)? != oid {
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
            if file_snapshot_identity(&current.metadata().map_err(rejected)?) != entry.identity {
                return Err(LocalGitFailure::Repository);
            }
            if let Some(file) = &entry.file
                && file_snapshot_identity(&file.metadata().map_err(rejected)?) != entry.identity
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
