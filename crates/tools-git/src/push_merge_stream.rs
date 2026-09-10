//! Disk-backed merge effects and line matching with bounded resident buffers.

use crate::{
    push_executor::{GitPushFailure, MAX_MERGE_DETAIL_BYTES},
    streamed_object::{IO_BYTES, ObjectContent},
};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{BufRead, BufReader, BufWriter, Read, Seek, Write},
    os::unix::fs::FileExt,
    time::Instant,
};

fn failed<T>(_: T) -> GitPushFailure {
    GitPushFailure::Repository
}
pub(super) fn check(deadline: Instant) -> Result<(), GitPushFailure> {
    if Instant::now() >= deadline {
        Err(GitPushFailure::Repository)
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(super) enum EffectsToMatch {
    #[cfg(test)]
    All,
    Text,
    Metadata,
}
impl EffectsToMatch {
    fn includes(self, origin: u8) -> bool {
        match self {
            #[cfg(test)]
            Self::All => true,
            Self::Text => matches!(origin, b'+' | b'-'),
            Self::Metadata => !matches!(origin, b'+' | b'-' | b'<' | b'>' | b'='),
        }
    }
}

pub(super) struct Hunks {
    file: BufWriter<File>,
    effects: u64,
    preview: usize,
}
impl Hunks {
    pub(super) fn new() -> Result<Self, GitPushFailure> {
        Ok(Self {
            file: BufWriter::new(tempfile::tempfile().map_err(failed)?),
            effects: 0,
            preview: 0,
        })
    }
    pub(super) fn start(&mut self) -> Result<(), GitPushFailure> {
        self.preview = 0;
        self.file.write_all(&[1]).map_err(failed)
    }
    pub(super) fn bytes(&mut self, bytes: &[u8]) -> Result<(), GitPushFailure> {
        let hash: [u8; 32] = Sha256::digest(bytes).into();
        self.effect(
            hash,
            bytes.first().copied().unwrap_or_default(),
            bytes.len() as u64,
            bytes,
        )
    }
    fn effect(
        &mut self,
        hash: [u8; 32],
        origin: u8,
        length: u64,
        preview: &[u8],
    ) -> Result<(), GitPushFailure> {
        let kept = preview.len().min(MAX_MERGE_DETAIL_BYTES - self.preview);
        self.file.write_all(&[2]).map_err(failed)?;
        self.file.write_all(&hash).map_err(failed)?;
        self.file.write_all(&[origin]).map_err(failed)?;
        self.file.write_all(&length.to_le_bytes()).map_err(failed)?;
        self.file
            .write_all(&(kept as u32).to_le_bytes())
            .map_err(failed)?;
        self.file.write_all(&preview[..kept]).map_err(failed)?;
        self.preview += kept;
        self.effects = self
            .effects
            .checked_add(1)
            .ok_or(GitPushFailure::Repository)?;
        Ok(())
    }
    pub(super) fn single(&mut self, bytes: &[u8]) -> Result<(), GitPushFailure> {
        self.start()?;
        self.bytes(bytes)
    }
    fn reader(&mut self) -> Result<BufReader<File>, GitPushFailure> {
        self.file.flush().map_err(failed)?;
        let mut file = self.file.get_ref().try_clone().map_err(failed)?;
        file.rewind().map_err(failed)?;
        Ok(BufReader::new(file))
    }
    pub(super) fn permitted(
        &mut self,
        selection: EffectsToMatch,
        deadline: Instant,
    ) -> Result<Effects, GitPushFailure> {
        let mut effects = Effects::new(self.effects)?;
        let mut reader = self.reader()?;
        while let Some(record) = record(&mut reader)? {
            check(deadline)?;
            if let Record::Effect { hash, origin, .. } = record
                && selection.includes(origin)
            {
                effects.change(hash, true, deadline)?;
            }
        }
        Ok(effects)
    }
    pub(super) fn first_dropped(
        &mut self,
        permitted: &mut Effects,
        selection: EffectsToMatch,
        budget: usize,
        deadline: Instant,
    ) -> Result<Option<(String, bool)>, GitPushFailure> {
        let mut reader = self.reader()?;
        let mut preview = Vec::new();
        let mut truncated = false;
        let mut dropped = false;
        while let Some(record) = record(&mut reader)? {
            check(deadline)?;
            match record {
                Record::Start => {
                    if dropped {
                        break;
                    }
                    preview.clear();
                    truncated = false;
                }
                Record::Effect {
                    hash,
                    origin,
                    length,
                    bytes,
                } => {
                    if !dropped
                        && selection.includes(origin)
                        && !permitted.change(hash, false, deadline)?
                    {
                        dropped = true;
                    }
                    let kept = bytes.len().min(budget - preview.len());
                    preview.extend_from_slice(&bytes[..kept]);
                    truncated |= length > kept as u64;
                }
            }
        }
        if !dropped {
            return Ok(None);
        }
        let (text, shortened) = crate::bounded::bounded_bytes(&preview, budget);
        Ok(Some((text, truncated || shortened)))
    }
}
enum Record {
    Start,
    Effect {
        hash: [u8; 32],
        origin: u8,
        length: u64,
        bytes: Vec<u8>,
    },
}
fn record(reader: &mut impl Read) -> Result<Option<Record>, GitPushFailure> {
    let mut tag = [0];
    if reader.read(&mut tag).map_err(failed)? == 0 {
        return Ok(None);
    }
    if tag[0] == 1 {
        return Ok(Some(Record::Start));
    }
    if tag[0] != 2 {
        return Err(GitPushFailure::Repository);
    }
    let mut hash = [0; 32];
    reader.read_exact(&mut hash).map_err(failed)?;
    let mut origin = [0];
    reader.read_exact(&mut origin).map_err(failed)?;
    let mut length = [0; 8];
    reader.read_exact(&mut length).map_err(failed)?;
    let mut size = [0; 4];
    reader.read_exact(&mut size).map_err(failed)?;
    let size = u32::from_le_bytes(size) as usize;
    if size > MAX_MERGE_DETAIL_BYTES {
        return Err(GitPushFailure::Repository);
    }
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes).map_err(failed)?;
    Ok(Some(Record::Effect {
        hash,
        origin: origin[0],
        length: u64::from_le_bytes(length),
        bytes,
    }))
}
pub(super) struct Effects {
    file: File,
    slots: u64,
}
impl Effects {
    fn new(count: u64) -> Result<Self, GitPushFailure> {
        let slots = count
            .checked_mul(2)
            .and_then(|n| n.checked_add(1))
            .and_then(u64::checked_next_power_of_two)
            .ok_or(GitPushFailure::Repository)?;
        let file = tempfile::tempfile().map_err(failed)?;
        file.set_len(slots.checked_mul(48).ok_or(GitPushFailure::Repository)?)
            .map_err(failed)?;
        Ok(Self { file, slots })
    }
    fn change(
        &mut self,
        hash: [u8; 32],
        add: bool,
        deadline: Instant,
    ) -> Result<bool, GitPushFailure> {
        let mut slot = u64::from_le_bytes(hash[..8].try_into().map_err(failed)?) & (self.slots - 1);
        loop {
            check(deadline)?;
            let mut record = [0; 48];
            self.file
                .read_exact_at(&mut record, slot * 48)
                .map_err(failed)?;
            if record[0] == 0 || record[8..40] == hash {
                if !add && record[0] == 0 {
                    return Ok(false);
                }
                let count = u64::from_le_bytes(record[40..48].try_into().map_err(failed)?);
                if !add && count == 0 {
                    return Ok(false);
                }
                record[0] = 1;
                record[8..40].copy_from_slice(&hash);
                record[40..48].copy_from_slice(
                    &if add {
                        count.checked_add(1).ok_or(GitPushFailure::Repository)?
                    } else {
                        count - 1
                    }
                    .to_le_bytes(),
                );
                self.file.write_all_at(&record, slot * 48).map_err(failed)?;
                return Ok(true);
            }
            slot = (slot + 1) & (self.slots - 1);
        }
    }
}

struct Lines {
    content: File,
    index: File,
    count: u64,
}
#[derive(Clone, Copy)]
struct Line {
    offset: u64,
    length: u64,
    hash: [u8; 32],
}
impl Lines {
    fn new(content: ObjectContent, deadline: Instant) -> Result<Self, GitPushFailure> {
        let mut reader = BufReader::with_capacity(IO_BYTES, content.file);
        reader.rewind().map_err(failed)?;
        let mut index = BufWriter::new(tempfile::tempfile().map_err(failed)?);
        let (mut count, mut offset, mut length) = (0u64, 0u64, 0u64);
        let mut hash = Sha256::new();
        loop {
            check(deadline)?;
            let buffer = reader.fill_buf().map_err(failed)?;
            if buffer.is_empty() {
                break;
            }
            let used = bstr::ByteSlice::find_byte(buffer, b'\n').map_or(buffer.len(), |i| i + 1);
            hash.update(&buffer[..used]);
            length += used as u64;
            if buffer[used - 1] == b'\n' {
                write_line(&mut index, offset, length, hash.finalize_reset().into())?;
                count += 1;
                offset += length;
                length = 0;
            }
            reader.consume(used);
        }
        if length != 0 {
            write_line(&mut index, offset, length, hash.finalize().into())?;
            count += 1;
        }
        Ok(Self {
            content: reader.into_inner(),
            index: index.into_inner().map_err(failed)?,
            count,
        })
    }
    fn line(&self, number: u64) -> Result<Line, GitPushFailure> {
        let mut record = [0; 48];
        self.index
            .read_exact_at(
                &mut record,
                number.checked_mul(48).ok_or(GitPushFailure::Repository)?,
            )
            .map_err(failed)?;
        Ok(Line {
            offset: u64::from_le_bytes(record[..8].try_into().map_err(failed)?),
            length: u64::from_le_bytes(record[8..16].try_into().map_err(failed)?),
            hash: record[16..].try_into().map_err(failed)?,
        })
    }
    fn emit(
        &self,
        number: u64,
        origin: u8,
        hunks: &mut Hunks,
        deadline: Instant,
    ) -> Result<(), GitPushFailure> {
        let line = self.line(number)?;
        let mut hash = Sha256::new();
        hash.update([origin]);
        let mut buffer = [0; IO_BYTES];
        let mut offset = 0;
        let mut preview = vec![origin];
        while offset < line.length {
            check(deadline)?;
            let count = (line.length - offset).min(buffer.len() as u64) as usize;
            self.content
                .read_exact_at(&mut buffer[..count], line.offset + offset)
                .map_err(failed)?;
            hash.update(&buffer[..count]);
            let kept = count.min(MAX_MERGE_DETAIL_BYTES - preview.len());
            preview.extend_from_slice(&buffer[..kept]);
            offset += count as u64;
        }
        hunks.effect(hash.finalize().into(), origin, line.length + 1, &preview)?;
        let mut last = [0];
        self.content
            .read_exact_at(&mut last, line.offset + line.length - 1)
            .map_err(failed)?;
        if last[0] != b'\n' {
            // libgit2's EOF line origin distinguishes additions and removals.
            hunks.bytes(if origin == b'-' {
                b">\n\\ No newline at end of file\n"
            } else {
                b"<\n\\ No newline at end of file\n"
            })?;
        }
        Ok(())
    }
}
fn write_line(
    writer: &mut impl Write,
    offset: u64,
    length: u64,
    hash: [u8; 32],
) -> Result<(), GitPushFailure> {
    writer.write_all(&offset.to_le_bytes()).map_err(failed)?;
    writer.write_all(&length.to_le_bytes()).map_err(failed)?;
    writer.write_all(&hash).map_err(failed)
}

pub(super) fn text_hunks(
    old: ObjectContent,
    new: ObjectContent,
    hunks: &mut Hunks,
    deadline: Instant,
) -> Result<(), GitPushFailure> {
    let old = Lines::new(old, deadline)?;
    let new = Lines::new(new, deadline)?;
    // Pending ranges and both search frontiers live on disk, including for
    // highly unbalanced splits. Only the current range occupies resident memory.
    let mut pending = Ranges::new()?;
    pending.push([0, old.count, 0, new.count])?;
    let mut start = true;
    while let Some([mut a, mut b, mut c, mut d]) = pending.pop()? {
        check(deadline)?;
        if a == b && c == d {
            start = true;
            continue;
        }
        while a < b && c < d && old.line(a)?.hash == new.line(c)?.hash {
            check(deadline)?;
            a += 1;
            c += 1;
            start = true;
        }
        let mut suffix = false;
        while a < b && c < d && old.line(b - 1)?.hash == new.line(d - 1)?.hash {
            check(deadline)?;
            b -= 1;
            d -= 1;
            suffix = true;
        }
        if suffix {
            pending.push([0; 4])?;
        }
        if a == b && c == d {
            continue;
        }
        if a == b || c == d || !have_common_line(&old, &new, [a, b, c, d], deadline)? {
            if start {
                hunks.start()?;
                start = false;
            }
            for line in a..b {
                old.emit(line, b'-', hunks, deadline)?;
            }
            for line in c..d {
                new.emit(line, b'+', hunks, deadline)?;
            }
            continue;
        }
        let (x, y) = bisect(&old, &new, [a, b, c, d], deadline)?;
        if (x == a && y == c) || (x == b && y == d) {
            return Err(GitPushFailure::Repository);
        }
        pending.push([x, b, y, d])?;
        pending.push([a, x, c, y])?;
    }
    Ok(())
}

struct Ranges {
    file: File,
    count: u64,
}
impl Ranges {
    fn new() -> Result<Self, GitPushFailure> {
        Ok(Self {
            file: tempfile::tempfile().map_err(failed)?,
            count: 0,
        })
    }
    fn push(&mut self, range: [u64; 4]) -> Result<(), GitPushFailure> {
        let offset = self
            .count
            .checked_mul(32)
            .ok_or(GitPushFailure::Repository)?;
        for (index, value) in range.into_iter().enumerate() {
            self.file
                .write_all_at(&value.to_le_bytes(), offset + index as u64 * 8)
                .map_err(failed)?;
        }
        self.count += 1;
        Ok(())
    }
    fn pop(&mut self) -> Result<Option<[u64; 4]>, GitPushFailure> {
        if self.count == 0 {
            return Ok(None);
        }
        self.count -= 1;
        let mut range = [0; 4];
        for (index, value) in range.iter_mut().enumerate() {
            let mut bytes = [0; 8];
            self.file
                .read_exact_at(&mut bytes, self.count * 32 + index as u64 * 8)
                .map_err(failed)?;
            *value = u64::from_le_bytes(bytes);
        }
        Ok(Some(range))
    }
}

fn have_common_line(
    old: &Lines,
    new: &Lines,
    [a, b, c, d]: [u64; 4],
    deadline: Instant,
) -> Result<bool, GitPushFailure> {
    // A disjoint replacement needs no edit-distance search. This keeps the
    // all-different case linear in source lines and scratch space.
    let mut hashes = Effects::new(b - a)?;
    for line in a..b {
        hashes.change(old.line(line)?.hash, true, deadline)?;
    }
    for line in c..d {
        if hashes.change(new.line(line)?.hash, false, deadline)? {
            return Ok(true);
        }
    }
    Ok(false)
}

struct Frontier {
    file: File,
    offset: i64,
}
impl Frontier {
    fn new(distance: i64) -> Result<Self, GitPushFailure> {
        let file = tempfile::tempfile().map_err(failed)?;
        let slots = distance
            .checked_mul(2)
            .and_then(|n| n.checked_add(3))
            .ok_or(GitPushFailure::Repository)?;
        file.set_len(
            u64::try_from(slots)
                .map_err(failed)?
                .checked_mul(8)
                .ok_or(GitPushFailure::Repository)?,
        )
        .map_err(failed)?;
        let result = Self {
            file,
            offset: distance + 1,
        };
        result.set(1, 0)?;
        Ok(result)
    }
    fn position(&self, k: i64) -> Result<u64, GitPushFailure> {
        u64::try_from(self.offset + k)
            .map_err(failed)?
            .checked_mul(8)
            .ok_or(GitPushFailure::Repository)
    }
    fn get(&self, k: i64) -> Result<i64, GitPushFailure> {
        let mut bytes = [0; 8];
        self.file
            .read_exact_at(&mut bytes, self.position(k)?)
            .map_err(failed)?;
        // Sparse zero pages represent unreachable diagonals.
        Ok(i64::from_le_bytes(bytes) - 1)
    }
    fn set(&self, k: i64, x: i64) -> Result<(), GitPushFailure> {
        self.file
            .write_all_at(&(x + 1).to_le_bytes(), self.position(k)?)
            .map_err(failed)
    }
}

fn bisect(
    old: &Lines,
    new: &Lines,
    [a, b, c, e]: [u64; 4],
    deadline: Instant,
) -> Result<(u64, u64), GitPushFailure> {
    let n = i64::try_from(b - a).map_err(failed)?;
    let m = i64::try_from(e - c).map_err(failed)?;
    let max = n
        .checked_add(m)
        .and_then(|n| n.checked_add(1))
        .ok_or(GitPushFailure::Repository)?
        / 2;
    let forward = Frontier::new(max)?;
    let reverse = Frontier::new(max)?;
    let delta = n - m;
    let odd = delta % 2 != 0;
    let (mut forward_start, mut forward_end, mut reverse_start, mut reverse_end) = (0, 0, 0, 0);
    for distance in 0..=max {
        for k in (-distance + forward_start..=distance - forward_end).step_by(2) {
            check(deadline)?;
            let mut x =
                if k == -distance || (k != distance && forward.get(k - 1)? < forward.get(k + 1)?) {
                    forward.get(k + 1)?
                } else {
                    forward.get(k - 1)? + 1
                };
            let mut y = x - k;
            while x >= 0
                && y >= 0
                && x < n
                && y < m
                && old.line(a + x as u64)?.hash == new.line(c + y as u64)?.hash
            {
                check(deadline)?;
                x += 1;
                y += 1;
            }
            forward.set(k, x)?;
            if x > n {
                forward_end += 2;
            } else if y > m {
                forward_start += 2;
            } else if odd && (delta - k).abs() < distance {
                let backwards = reverse.get(delta - k)?;
                if backwards >= 0 && x >= n - backwards {
                    return Ok((a + x as u64, c + y as u64));
                }
            }
        }
        for k in (-distance + reverse_start..=distance - reverse_end).step_by(2) {
            check(deadline)?;
            let mut x =
                if k == -distance || (k != distance && reverse.get(k - 1)? < reverse.get(k + 1)?) {
                    reverse.get(k + 1)?
                } else {
                    reverse.get(k - 1)? + 1
                };
            let mut y = x - k;
            while x >= 0
                && y >= 0
                && x < n
                && y < m
                && old.line(b - x as u64 - 1)?.hash == new.line(e - y as u64 - 1)?.hash
            {
                check(deadline)?;
                x += 1;
                y += 1;
            }
            reverse.set(k, x)?;
            if x > n {
                reverse_end += 2;
            } else if y > m {
                reverse_start += 2;
            } else if !odd && (delta - k).abs() <= distance {
                let forwards = forward.get(delta - k)?;
                if forwards >= 0 && forwards >= n - x {
                    return Ok((a + forwards as u64, c + (forwards - (delta - k)) as u64));
                }
            }
        }
    }
    Err(GitPushFailure::Repository)
}

fn changed_ranges(old: &Lines, new: &Lines, deadline: Instant) -> Result<Ranges, GitPushFailure> {
    let mut ranges = Ranges::new()?;
    let old_size = old.content.metadata().map_err(failed)?.len();
    let new_size = new.content.metadata().map_err(failed)?.len();
    if old_size <= crate::limits::MAX_DIFF_BYTES as u64
        && new_size <= crate::limits::MAX_DIFF_BYTES as u64
    {
        let mut a = vec![0; old_size as usize];
        let mut b = vec![0; new_size as usize];
        old.content.read_exact_at(&mut a, 0).map_err(failed)?;
        new.content.read_exact_at(&mut b, 0).map_err(failed)?;
        let mut options = git2::DiffOptions::new();
        options.context_lines(0).interhunk_lines(0);
        let patch =
            git2::Patch::from_buffers(&a, None, &b, None, Some(&mut options)).map_err(failed)?;
        for index in 0..patch.num_hunks() {
            check(deadline)?;
            let (h, _) = patch.hunk(index).map_err(failed)?;
            let a = h.old_start().saturating_sub(u32::from(h.old_lines() != 0)) as u64;
            let c = h.new_start().saturating_sub(u32::from(h.new_lines() != 0)) as u64;
            ranges.push([a, a + h.old_lines() as u64, c, c + h.new_lines() as u64])?;
        }
    } else {
        visit_changes(old, new, deadline, |range| ranges.push(range))?;
    }
    Ok(ranges)
}
fn visit_changes(
    old: &Lines,
    new: &Lines,
    deadline: Instant,
    mut emit: impl FnMut([u64; 4]) -> Result<(), GitPushFailure>,
) -> Result<(), GitPushFailure> {
    // Pending ranges and both search frontiers live on disk, including for
    // highly unbalanced splits. Only the current range occupies resident memory.
    let mut pending = Ranges::new()?;
    pending.push([0, old.count, 0, new.count])?;

    while let Some([mut a, mut b, mut c, mut d]) = pending.pop()? {
        check(deadline)?;
        if a == b && c == d {
            continue;
        }
        while a < b && c < d && old.line(a)?.hash == new.line(c)?.hash {
            check(deadline)?;
            a += 1;
            c += 1;
        }
        let mut suffix = false;
        while a < b && c < d && old.line(b - 1)?.hash == new.line(d - 1)?.hash {
            check(deadline)?;
            b -= 1;
            d -= 1;
            suffix = true;
        }
        if suffix {
            pending.push([0; 4])?;
        }
        if a == b && c == d {
            continue;
        }
        if a == b || c == d || !have_common_line(old, new, [a, b, c, d], deadline)? {
            emit([a, b, c, d])?;
            continue;
        }
        let (x, y) = bisect(old, new, [a, b, c, d], deadline)?;
        if (x == a && y == c) || (x == b && y == d) {
            return Err(GitPushFailure::Repository);
        }
        pending.push([x, b, y, d])?;
        pending.push([a, x, c, y])?;
    }
    Ok(())
}

fn range_at(ranges: &Ranges, number: u64) -> Result<Option<[u64; 4]>, GitPushFailure> {
    if number == ranges.count {
        return Ok(None);
    }
    let mut bytes = [0; 32];
    ranges
        .file
        .read_exact_at(&mut bytes, number * 32)
        .map_err(failed)?;
    let mut values = [0; 4];
    for (value, bytes) in values.iter_mut().zip(bytes.as_chunks::<8>().0.iter()) {
        *value = u64::from_le_bytes(*bytes);
    }
    Ok(Some(values))
}
fn append_hashes(
    out: &mut File,
    lines: &Lines,
    start: u64,
    end: u64,
    deadline: Instant,
) -> Result<(), GitPushFailure> {
    for line in start..end {
        check(deadline)?;
        out.write_all(&lines.line(line)?.hash).map_err(failed)?;
    }
    Ok(())
}
fn replacement_hashes(
    ancestor: &Lines,
    side: &Lines,
    edits: &Ranges,
    selected: std::ops::Range<u64>,
    region: std::ops::Range<u64>,
    deadline: Instant,
) -> Result<File, GitPushFailure> {
    let mut out = tempfile::tempfile().map_err(failed)?;
    let mut cursor = region.start;
    for index in selected {
        let [a, b, c, d] = range_at(edits, index)?.ok_or(GitPushFailure::Repository)?;
        append_hashes(&mut out, ancestor, cursor, a, deadline)?;
        append_hashes(&mut out, side, c, d, deadline)?;
        cursor = b;
    }
    append_hashes(&mut out, ancestor, cursor, region.end, deadline)?;
    Ok(out)
}
fn same_file(a: &File, b: &File, deadline: Instant) -> Result<bool, GitPushFailure> {
    let size = a.metadata().map_err(failed)?.len();
    if size != b.metadata().map_err(failed)?.len() {
        return Ok(false);
    }
    let mut offset = 0;
    let mut x = [0; IO_BYTES];
    let mut y = [0; IO_BYTES];
    while offset < size {
        check(deadline)?;
        let n = (size - offset).min(IO_BYTES as u64) as usize;
        a.read_exact_at(&mut x[..n], offset).map_err(failed)?;
        b.read_exact_at(&mut y[..n], offset).map_err(failed)?;
        if x[..n] != y[..n] {
            return Ok(false);
        }
        offset += n as u64;
    }
    Ok(true)
}
fn copy_hashes(from: &mut File, to: &mut File, deadline: Instant) -> Result<(), GitPushFailure> {
    from.rewind().map_err(failed)?;
    let mut buffer = [0; IO_BYTES];
    loop {
        check(deadline)?;
        let n = from.read(&mut buffer).map_err(failed)?;
        if n == 0 {
            break;
        }
        to.write_all(&buffer[..n]).map_err(failed)?;
    }
    Ok(())
}
// Fixed line-hash spans are separated by conflict wildcards; all storage stays on disk.
pub(super) fn preserves_nonconflicting_text(
    ancestor: ObjectContent,
    branch: ObjectContent,
    base: ObjectContent,
    result: ObjectContent,
    deadline: Instant,
) -> Result<bool, GitPushFailure> {
    let ancestor = Lines::new(ancestor, deadline)?;
    let branch = Lines::new(branch, deadline)?;
    let base = Lines::new(base, deadline)?;
    let result = Lines::new(result, deadline)?;
    let own = changed_ranges(&ancestor, &branch, deadline)?;
    let main = changed_ranges(&ancestor, &base, deadline)?;
    let mut fixed = tempfile::tempfile().map_err(failed)?;
    let mut spans = Ranges::new()?;
    let (mut i, mut j, mut cursor, mut span_start) = (0, 0, 0, 0);
    loop {
        check(deadline)?;
        let a = range_at(&own, i)?;
        let b = range_at(&main, j)?;
        let start = match (a, b) {
            (None, None) => break,
            (Some(a), None) => a[0],
            (None, Some(b)) => b[0],
            (Some(a), Some(b)) => a[0].min(b[0]),
        };
        let mut end = start;
        let (first_i, first_j) = (i, j);
        loop {
            let before = (i, j);
            while let Some(r) = range_at(&own, i)? {
                if r[0] > end {
                    break;
                }
                end = end.max(r[1]);
                i += 1;
            }
            while let Some(r) = range_at(&main, j)? {
                if r[0] > end {
                    break;
                }
                end = end.max(r[1]);
                j += 1;
            }
            if before == (i, j) {
                break;
            }
        }
        append_hashes(&mut fixed, &ancestor, cursor, start, deadline)?;
        let mut a = replacement_hashes(&ancestor, &branch, &own, first_i..i, start..end, deadline)?;
        let mut b = replacement_hashes(&ancestor, &base, &main, first_j..j, start..end, deadline)?;
        if i != first_i && j != first_j && !same_file(&a, &b, deadline)? {
            let span_end = fixed.stream_position().map_err(failed)? / 32;
            spans.push([span_start, span_end, 0, 0])?;
            span_start = span_end;
        } else {
            copy_hashes(
                if i != first_i { &mut a } else { &mut b },
                &mut fixed,
                deadline,
            )?;
        }
        cursor = end;
    }
    append_hashes(&mut fixed, &ancestor, cursor, ancestor.count, deadline)?;
    let end = fixed.stream_position().map_err(failed)? / 32;
    spans.push([span_start, end, 0, 0])?;
    let mut cursor = 0;
    for index in 0..spans.count {
        let [a, b, _, _] = range_at(&spans, index)?.ok_or(GitPushFailure::Repository)?;
        let count = b - a;
        if count > result.count.saturating_sub(cursor) {
            return Ok(false);
        }
        let maximum = result.count - count;
        let mut candidate = if index == 0 {
            cursor
        } else if index + 1 == spans.count {
            maximum
        } else {
            cursor
        };
        let mut matched = false;
        while candidate <= maximum {
            check(deadline)?;
            let mut equal = true;
            for line in 0..count {
                check(deadline)?;
                let mut hash = [0; 32];
                fixed
                    .read_exact_at(&mut hash, (a + line) * 32)
                    .map_err(failed)?;
                if hash != result.line(candidate + line)?.hash {
                    equal = false;
                    break;
                }
            }
            if equal {
                matched = true;
                cursor = candidate + count;
                break;
            }
            if index == 0 || index + 1 == spans.count {
                break;
            }
            candidate += 1;
        }
        if !matched {
            return Ok(false);
        }
    }
    Ok(cursor == result.count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn content(text: &str) -> ObjectContent {
        ObjectContent::decode(
            &mut text.as_bytes(),
            text.len(),
            git2::ObjectType::Blob,
            None,
        )
        .expect("fixture content")
    }
    #[test]
    fn disk_merge_check_protects_main_lines_beyond_the_preview() {
        // The distinction follows the complete line, past the retained preview.
        let main_line = format!("{}\n", "a".repeat(MAX_MERGE_DETAIL_BYTES + 1));
        struct Case {
            name: &'static str,
            resolution: String,
            expected_dropped: Option<()>,
        }
        let cases = [
            Case {
                name: "regrouped branch",
                resolution: format!("{main_line}branch regrouped\n"),
                expected_dropped: None,
            },
            Case {
                name: "altered main line suffix",
                resolution: format!("{}!\nbranch regrouped\n", main_line.trim_end()),
                expected_dropped: Some(()),
            },
            Case {
                name: "restored main removal",
                resolution: format!("{main_line}old\n"),
                expected_dropped: Some(()),
            },
        ];
        for case in cases {
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut base = Hunks::new().expect("base hunks");
            text_hunks(content("old\n"), content(&main_line), &mut base, deadline)
                .expect("main diff");
            let mut result = Hunks::new().expect("result hunks");
            text_hunks(
                content("old\n"),
                content(&case.resolution),
                &mut result,
                deadline,
            )
            .expect("merged diff");
            let mut retained = result
                .permitted(EffectsToMatch::Text, deadline)
                .expect("result effects");

            let dropped = base
                .first_dropped(
                    &mut retained,
                    EffectsToMatch::Text,
                    MAX_MERGE_DETAIL_BYTES,
                    deadline,
                )
                .expect("main-side check");

            assert_eq!(dropped.map(|_| ()), case.expected_dropped, "{}", case.name);
        }
    }

    #[test]
    fn disk_diff_preserves_separate_hunks_and_effect_multiplicity() {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut actual = Hunks::new().expect("hunks");
        text_hunks(
            content("old\nshared\nold tail\n"),
            content("new\nshared\nnew tail\n"),
            &mut actual,
            deadline,
        )
        .expect("disk diff");
        let mut allowed = Hunks::new().expect("allowed effects");
        allowed.start().expect("hunk");
        allowed.bytes(b"-old\n").expect("removal");
        allowed.bytes(b"+new\n").expect("addition");
        let mut allowed = allowed
            .permitted(EffectsToMatch::All, deadline)
            .expect("effect counts");
        assert_eq!(
            actual
                .first_dropped(
                    &mut allowed,
                    EffectsToMatch::All,
                    MAX_MERGE_DETAIL_BYTES,
                    deadline
                )
                .expect("comparison"),
            Some(("-old tail\n+new tail\n".to_owned(), false))
        );
    }
    #[test]
    fn disk_diff_matches_insertions_removals_repeated_lines_and_missing_newlines() {
        let deadline = Instant::now() + Duration::from_secs(30);
        let cases: &[(&str, &str, &[&[u8]])] = &[
            ("a\nb\nc\n", "a\nx\nb\ny\nc\n", &[b"+x\n", b"+y\n"]),
            ("a\nb\nc\n", "a\nc\n", &[b"-b\n"]),
            ("a\n", "a\na\na\n", &[b"+a\n", b"+a\n"]),
            (
                "before",
                "after",
                &[
                    b"-before",
                    b">\n\\ No newline at end of file\n",
                    b"+after",
                    b"<\n\\ No newline at end of file\n",
                ],
            ),
        ];
        for (old, new, effects) in cases {
            let mut actual = Hunks::new().expect("actual hunks");
            text_hunks(content(old), content(new), &mut actual, deadline).expect("disk diff");
            let mut allowed = Hunks::new().expect("expected effects");
            allowed.start().expect("hunk");
            for effect in *effects {
                allowed.bytes(effect).expect("expected line");
            }
            let mut allowed = allowed
                .permitted(EffectsToMatch::All, deadline)
                .expect("effect counts");
            assert_eq!(
                actual
                    .first_dropped(
                        &mut allowed,
                        EffectsToMatch::All,
                        MAX_MERGE_DETAIL_BYTES,
                        deadline
                    )
                    .expect("comparison"),
                None,
                "{old:?} -> {new:?}"
            );
            assert!(
                actual
                    .first_dropped(
                        &mut allowed,
                        EffectsToMatch::All,
                        MAX_MERGE_DETAIL_BYTES,
                        deadline
                    )
                    .expect("consumed counts")
                    .is_some()
            );
        }
    }
    #[test]
    fn disk_diff_matches_minimum_edit_counts_for_repeated_line_sequences() {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut seed = 19u64;
        for case in 0..256 {
            let mut sequence = |length: usize| -> Vec<u8> {
                (0..length)
                    .map(|_| {
                        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                        b'a' + ((seed >> 32) % 4) as u8
                    })
                    .collect()
            };
            let old = sequence(case % 13);
            let new = sequence((case / 13) % 13);
            // A small dynamic-programming oracle checks the independent minimum
            // edit count, including ambiguous alignments of repeated lines.
            let mut lengths = [[0; 14]; 14];
            for (i, &left) in old.iter().enumerate() {
                for (j, &right) in new.iter().enumerate() {
                    lengths[i + 1][j + 1] = if left == right {
                        lengths[i][j] + 1
                    } else {
                        lengths[i][j + 1].max(lengths[i + 1][j])
                    };
                }
            }
            let text = |lines: &[u8]| -> String {
                lines
                    .iter()
                    .map(|&byte| format!("{}\n", char::from(byte)))
                    .collect()
            };
            let mut hunks = Hunks::new().expect("hunks");
            text_hunks(
                content(&text(&old)),
                content(&text(&new)),
                &mut hunks,
                deadline,
            )
            .expect("divide-and-conquer diff");
            assert_eq!(
                hunks.effects as usize,
                old.len() + new.len() - 2 * lengths[old.len()][new.len()],
                "case {case}: {old:?} -> {new:?}"
            );
        }
    }

    #[test]
    fn disk_diff_handles_fifty_thousand_mutually_different_lines() {
        let make = |prefix: &str| {
            let mut file = tempfile::tempfile().expect("generated text");
            for line in 0..50_000 {
                writeln!(file, "{prefix}-{line}").expect("line");
            }
            let size = file.metadata().expect("metadata").len() as usize;
            file.rewind().expect("rewind");
            ObjectContent {
                file,
                size,
                kind: git2::ObjectType::Blob,
            }
        };
        let old = make("old");
        let new = make("new");
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut hunks = Hunks::new().expect("hunks");
        text_hunks(old, new, &mut hunks, deadline).expect("linear disjoint replacement");
        assert_eq!(hunks.effects, 100_000);
        let mut empty = Hunks::new()
            .expect("empty")
            .permitted(EffectsToMatch::All, deadline)
            .expect("counts");
        let (preview, truncated) = hunks
            .first_dropped(
                &mut empty,
                EffectsToMatch::All,
                MAX_MERGE_DETAIL_BYTES,
                deadline,
            )
            .expect("comparison")
            .expect("dropped changes");
        assert!(preview.starts_with("-old-0\n"));
        assert!(preview.len() <= MAX_MERGE_DETAIL_BYTES);
        assert!(truncated);
    }
    #[test]
    fn streamed_merge_work_stops_at_its_deadline() {
        let expired = Instant::now() - Duration::from_secs(1);
        assert!(
            ObjectContent::decode(
                &mut std::io::empty(),
                0,
                git2::ObjectType::Blob,
                Some(expired)
            )
            .is_err()
        );
        let directory = tempfile::tempdir().expect("publication directory");
        assert!(
            content("new\n")
                .store(directory.path(), git2::ObjectFormat::Sha1, Some(expired))
                .is_err()
        );
        let mut hunks = Hunks::new().expect("hunks");
        assert!(text_hunks(content("old\n"), content("new\n"), &mut hunks, expired).is_err());
        hunks.single(b"+new\n").expect("effect");
        assert!(hunks.permitted(EffectsToMatch::All, expired).is_err());
    }
}
