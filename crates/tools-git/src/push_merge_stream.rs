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

pub(super) struct Hunks {
    file: BufWriter<File>,
    effects: u64,
    preview: usize,
    // At most one descriptor per indexed line (or byte of a bounded small diff).
    pub(super) edits: Vec<[u64; 4]>,
    pub(super) opaque: bool,
}
impl Hunks {
    pub(super) fn new() -> Result<Self, GitPushFailure> {
        Ok(Self {
            file: BufWriter::new(tempfile::tempfile().map_err(failed)?),
            effects: 0,
            preview: 0,
            edits: Vec::new(),
            opaque: false,
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
            bytes.len() as u64,
            bytes,
            matches!(bytes.first(), Some(b'+' | b'-')),
            matches!(bytes.first(), Some(b'<' | b'>' | b'=')),
        )
    }
    fn effect(
        &mut self,
        hash: [u8; 32],
        length: u64,
        preview: &[u8],
        text: bool,
        preview_only: bool,
    ) -> Result<(), GitPushFailure> {
        let kept = preview.len().min(MAX_MERGE_DETAIL_BYTES - self.preview);
        self.file
            .write_all(&[if preview_only {
                4
            } else if text {
                3
            } else {
                2
            }])
            .map_err(failed)?;
        self.file.write_all(&hash).map_err(failed)?;
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
    pub(super) fn permitted(&mut self, deadline: Instant) -> Result<Effects, GitPushFailure> {
        self.permitted_filtered(None, deadline)
    }
    pub(super) fn permitted_filtered(
        &mut self,
        text: Option<bool>,
        deadline: Instant,
    ) -> Result<Effects, GitPushFailure> {
        let mut effects = Effects::new(self.effects)?;
        let mut reader = self.reader()?;
        while let Some(record) = record(&mut reader)? {
            check(deadline)?;
            if let Record::Effect {
                hash,
                text: is_text,
                preview_only: false,
                ..
            } = record
                && text.is_none_or(|text| text == is_text)
            {
                effects.change(hash, true, deadline)?;
            }
        }
        Ok(effects)
    }
    pub(super) fn first_dropped(
        &mut self,
        permitted: &mut Effects,
        budget: usize,
        deadline: Instant,
    ) -> Result<Option<(String, bool)>, GitPushFailure> {
        self.first_dropped_filtered(permitted, budget, None, deadline)
    }
    pub(super) fn first_dropped_filtered(
        &mut self,
        permitted: &mut Effects,
        budget: usize,
        text: Option<bool>,
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
                    length,
                    bytes,
                    text: is_text,
                    preview_only,
                } => {
                    if !preview_only && text.is_some_and(|text| text != is_text) {
                        continue;
                    }
                    if !preview_only && !dropped && !permitted.change(hash, false, deadline)? {
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
        length: u64,
        bytes: Vec<u8>,
        text: bool,
        preview_only: bool,
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
    if !(2..=4).contains(&tag[0]) {
        return Err(GitPushFailure::Repository);
    }
    let mut hash = [0; 32];
    reader.read_exact(&mut hash).map_err(failed)?;
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
        length: u64::from_le_bytes(length),
        bytes,
        text: tag[0] == 3,
        preview_only: tag[0] == 4,
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

// At 48 bytes per record, each line index is at most 6 MiB; blobs remain unbounded.
const MAX_INDEXED_LINES: u64 = 131_072;

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
    fn new(content: ObjectContent, deadline: Instant) -> Result<Option<Self>, GitPushFailure> {
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
            if count == MAX_INDEXED_LINES {
                return Ok(None);
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
        Ok(Some(Self {
            content: reader.into_inner(),
            index: index.into_inner().map_err(failed)?,
            count,
        }))
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
        hunks.effect(
            hash.finalize().into(),
            line.length + 1,
            &preview,
            true,
            false,
        )?;
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

// Bounds random frontier operations and line comparisons across every split of
// one file diff. Exhaustion uses an atomic object effect, never inferred edits.
const MAX_MATCH_WORK: usize = 65_536;

#[derive(Debug, Eq, PartialEq)]
pub(super) enum TextDiff {
    Detailed,
    WholeObject,
}

pub(super) fn text_hunks(
    old: ObjectContent,
    new: ObjectContent,
    hunks: &mut Hunks,
    deadline: Instant,
) -> Result<TextDiff, GitPushFailure> {
    let checkpoint = hunks.file.stream_position().map_err(failed)?;
    let previous_effects = hunks.effects;
    let previous_preview = hunks.preview;
    let previous_edits = hunks.edits.len();
    let mut work = MAX_MATCH_WORK;
    let Some(old) = Lines::new(old, deadline)? else {
        return Ok(TextDiff::WholeObject);
    };
    let Some(new) = Lines::new(new, deadline)? else {
        return Ok(TextDiff::WholeObject);
    };
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
            if !start
                && let Some(last) = hunks.edits.last_mut()
                && last[1] == a
                && last[3] == c
            {
                last[1] = b;
                last[3] = d;
            } else {
                hunks.edits.push([a, b, c, d]);
            }
            for line in a..b {
                old.emit(line, b'-', hunks, deadline)?;
            }
            for line in c..d {
                new.emit(line, b'+', hunks, deadline)?;
            }
            continue;
        }
        let Some((x, y)) = bisect(&old, &new, [a, b, c, d], &mut work, deadline)? else {
            hunks.file.flush().map_err(failed)?;
            hunks.file.get_ref().set_len(checkpoint).map_err(failed)?;
            hunks
                .file
                .seek(std::io::SeekFrom::Start(checkpoint))
                .map_err(failed)?;
            hunks.effects = previous_effects;
            hunks.preview = previous_preview;
            hunks.edits.truncate(previous_edits);
            return Ok(TextDiff::WholeObject);
        };
        if (x == a && y == c) || (x == b && y == d) {
            return Err(GitPushFailure::Repository);
        }
        pending.push([x, b, y, d])?;
        pending.push([a, x, c, y])?;
    }
    Ok(TextDiff::Detailed)
}

// Edit descriptors are bounded by the line-index budget; bytes stay in files.
// Fixed spans are compared literally, with wildcard gaps only at parent conflicts.
pub(super) fn preserves_text(
    ancestor: ObjectContent,
    branch_content: ObjectContent,
    base_content: ObjectContent,
    result: ObjectContent,
    branch: &Hunks,
    base: &Hunks,
    deadline: Instant,
) -> Result<bool, GitPushFailure> {
    if branch.edits.is_empty() && base.edits.is_empty() {
        return if branch.opaque || base.opaque {
            Ok(true)
        } else {
            equal_files(
                &ancestor.file,
                0,
                ancestor.size as u64,
                &result.file,
                0,
                result.size as u64,
                deadline,
            )
        };
    }
    let Some(ancestor) = Lines::new(ancestor, deadline)? else {
        return Ok(false);
    };
    let Some(branch_lines) = Lines::new(branch_content, deadline)? else {
        return Ok(false);
    };
    let Some(base_lines) = Lines::new(base_content, deadline)? else {
        return Ok(false);
    };
    let mut edits: Vec<_> = branch
        .edits
        .iter()
        .map(|edit| (false, *edit))
        .chain(base.edits.iter().map(|edit| (true, *edit)))
        .collect();
    edits.sort_by_key(|(_, edit)| (edit[0], edit[1]));
    let mut fixed = tempfile::tempfile().map_err(failed)?;
    let (mut cursor, mut index, mut result_cursor) = (0, 0, 0);
    let mut conflicts = false;
    let mut search_work = MAX_MATCH_WORK;
    while index < edits.len() {
        check(deadline)?;
        let start = edits[index].1[0];
        let mut end = edits[index].1[1];
        let first = index;
        index += 1;
        while index < edits.len() && edits[index].1[0] <= end {
            end = end.max(edits[index].1[1]);
            index += 1;
        }
        append_lines(&ancestor, cursor, start, &mut fixed, deadline)?;
        let region = &edits[first..index];
        let replacement = |side| -> Result<File, GitPushFailure> {
            let mut bytes = tempfile::tempfile().map_err(failed)?;
            let mut cursor = start;
            for (_, [a, b, c, d]) in region.iter().filter(|(source, _)| *source == side) {
                append_lines(&ancestor, cursor, *a, &mut bytes, deadline)?;
                append_lines(
                    if side { &base_lines } else { &branch_lines },
                    *c,
                    *d,
                    &mut bytes,
                    deadline,
                )?;
                cursor = *b;
            }
            append_lines(&ancestor, cursor, end, &mut bytes, deadline)?;
            Ok(bytes)
        };
        let branch_changed = region.iter().any(|(side, _)| !side);
        let base_changed = region.iter().any(|(side, _)| *side);
        let branch_text = replacement(false)?;
        let base_text = replacement(true)?;
        let branch_size = branch_text.metadata().map_err(failed)?.len();
        let base_size = base_text.metadata().map_err(failed)?.len();
        if branch_changed
            && base_changed
            && !equal_files(
                &branch_text,
                0,
                branch_size,
                &base_text,
                0,
                base_size,
                deadline,
            )?
        {
            let size = fixed.metadata().map_err(failed)?.len();
            if conflicts {
                let Some(next) = find_span(
                    &fixed,
                    size,
                    &result.file,
                    result_cursor,
                    result.size as u64,
                    &mut search_work,
                    deadline,
                )?
                else {
                    return Ok(false);
                };
                result_cursor = next;
            } else {
                if size > result.size as u64
                    || !equal_files(&fixed, 0, size, &result.file, 0, size, deadline)?
                {
                    return Ok(false);
                }
                result_cursor = size;
            }
            conflicts = true;
            fixed.set_len(0).map_err(failed)?;
            fixed.rewind().map_err(failed)?;
        } else {
            let (file, size) = if branch_changed {
                (&branch_text, branch_size)
            } else {
                (&base_text, base_size)
            };
            append_bytes(file, 0, size, &mut fixed, deadline)?;
        }
        cursor = end;
    }
    append_lines(&ancestor, cursor, ancestor.count, &mut fixed, deadline)?;
    let size = fixed.metadata().map_err(failed)?.len();
    if !conflicts {
        equal_files(
            &fixed,
            0,
            size,
            &result.file,
            0,
            result.size as u64,
            deadline,
        )
    } else if size <= result.size as u64 - result_cursor {
        equal_files(
            &fixed,
            0,
            size,
            &result.file,
            result.size as u64 - size,
            size,
            deadline,
        )
    } else {
        Ok(false)
    }
}

fn append_lines(
    lines: &Lines,
    start: u64,
    end: u64,
    output: &mut File,
    deadline: Instant,
) -> Result<(), GitPushFailure> {
    if start == end {
        return Ok(());
    }
    let first = lines.line(start)?;
    let last = lines.line(end - 1)?;
    append_bytes(
        &lines.content,
        first.offset,
        last.offset + last.length - first.offset,
        output,
        deadline,
    )
}

fn append_bytes(
    input: &File,
    offset: u64,
    length: u64,
    output: &mut File,
    deadline: Instant,
) -> Result<(), GitPushFailure> {
    let mut buffer = [0; IO_BYTES];
    let mut copied = 0;
    while copied < length {
        check(deadline)?;
        let count = (length - copied).min(buffer.len() as u64) as usize;
        input
            .read_exact_at(&mut buffer[..count], offset + copied)
            .map_err(failed)?;
        output.write_all(&buffer[..count]).map_err(failed)?;
        copied += count as u64;
    }
    Ok(())
}

fn equal_files(
    left: &File,
    left_offset: u64,
    left_size: u64,
    right: &File,
    right_offset: u64,
    right_size: u64,
    deadline: Instant,
) -> Result<bool, GitPushFailure> {
    if left_size != right_size {
        return Ok(false);
    }
    let (mut a, mut b) = ([0; IO_BYTES], [0; IO_BYTES]);
    let mut offset = 0;
    while offset < left_size {
        check(deadline)?;
        let count = (left_size - offset).min(IO_BYTES as u64) as usize;
        left.read_exact_at(&mut a[..count], left_offset + offset)
            .map_err(failed)?;
        right
            .read_exact_at(&mut b[..count], right_offset + offset)
            .map_err(failed)?;
        if a[..count] != b[..count] {
            return Ok(false);
        }
        offset += count as u64;
    }
    Ok(true)
}

// KMP indexes only a fixed-size prefix. Full candidate comparisons share a page
// budget, so repeated near-matches cannot cause quadratic I/O or unbounded scratch.
fn find_span(
    pattern: &File,
    size: u64,
    result: &File,
    start: u64,
    end: u64,
    work: &mut usize,
    deadline: Instant,
) -> Result<Option<u64>, GitPushFailure> {
    if size == 0 {
        return Ok(Some(start));
    }
    if size > end - start {
        return Ok(None);
    }
    let mut prefix = vec![0; size.min(IO_BYTES as u64) as usize];
    pattern.read_exact_at(&mut prefix, 0).map_err(failed)?;
    let mut fallback = vec![0; prefix.len()];
    let mut matched = 0;
    for i in 1..prefix.len() {
        while matched > 0 && prefix[i] != prefix[matched] {
            matched = fallback[matched - 1];
        }
        if prefix[i] == prefix[matched] {
            matched += 1;
        }
        fallback[i] = matched;
    }
    matched = 0;
    let mut buffer = [0; IO_BYTES];
    let mut offset = start;
    while offset < end {
        check(deadline)?;
        let count = (end - offset).min(IO_BYTES as u64) as usize;
        result
            .read_exact_at(&mut buffer[..count], offset)
            .map_err(failed)?;
        for (i, byte) in buffer[..count].iter().enumerate() {
            while matched > 0 && *byte != prefix[matched] {
                matched = fallback[matched - 1];
            }
            if *byte == prefix[matched] {
                matched += 1;
            }
            if matched == prefix.len() {
                let candidate = offset + i as u64 + 1 - prefix.len() as u64;
                if size > end - candidate {
                    return Ok(None);
                }
                let pages = size.div_ceil(IO_BYTES as u64) as usize;
                let Some(remaining) = work.checked_sub(pages) else {
                    return Ok(None);
                };
                *work = remaining;
                if equal_files(pattern, 0, size, result, candidate, size, deadline)? {
                    return Ok(Some(candidate + size));
                }
                matched = fallback[matched - 1];
            }
        }
        offset += count as u64;
    }
    Ok(None)
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
    work: &mut usize,
    deadline: Instant,
) -> Result<Option<(u64, u64)>, GitPushFailure> {
    let n = i64::try_from(b - a).map_err(failed)?;
    let m = i64::try_from(e - c).map_err(failed)?;
    let max = n
        .checked_add(m)
        .and_then(|n| n.checked_add(1))
        .ok_or(GitPushFailure::Repository)?
        / 2;
    let forward = Frontier::new(max.min(MAX_MATCH_WORK as i64))?;
    let reverse = Frontier::new(max.min(MAX_MATCH_WORK as i64))?;
    let delta = n - m;
    let odd = delta % 2 != 0;
    let (mut forward_start, mut forward_end, mut reverse_start, mut reverse_end) = (0, 0, 0, 0);
    for distance in 0..=max {
        for k in (-distance + forward_start..=distance - forward_end).step_by(2) {
            check(deadline)?;
            let Some(remaining) = work.checked_sub(1) else {
                return Ok(None);
            };
            *work = remaining;
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
                let Some(remaining) = work.checked_sub(1) else {
                    return Ok(None);
                };
                *work = remaining;
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
                    return Ok(Some((a + x as u64, c + y as u64)));
                }
            }
        }
        for k in (-distance + reverse_start..=distance - reverse_end).step_by(2) {
            check(deadline)?;
            let Some(remaining) = work.checked_sub(1) else {
                return Ok(None);
            };
            *work = remaining;
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
                let Some(remaining) = work.checked_sub(1) else {
                    return Ok(None);
                };
                *work = remaining;
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
                    return Ok(Some((
                        a + forwards as u64,
                        c + (forwards - (delta - k)) as u64,
                    )));
                }
            }
        }
    }
    Err(GitPushFailure::Repository)
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
    fn conflict_gaps_preserve_literal_spans_including_matches_inside_a_line() {
        let deadline = Instant::now() + Duration::from_secs(30);
        let ancestor = "left old\nfixed\nright old\n";
        let branch = "left branch\nfixed\nright branch\n";
        let base = "left base\nfixed\nright base\n";
        let mut branch_hunks = Hunks::new().expect("branch hunks");
        let mut base_hunks = Hunks::new().expect("base hunks");
        text_hunks(
            content(ancestor),
            content(branch),
            &mut branch_hunks,
            deadline,
        )
        .expect("branch diff");
        text_hunks(content(ancestor), content(base), &mut base_hunks, deadline).expect("base diff");
        for (result, preserved) in [
            ("left resolved without newlinefixed\nright resolved\n", true),
            ("left resolved\nchanged\nright resolved\n", false),
        ] {
            assert_eq!(
                preserves_text(
                    content(ancestor),
                    content(branch),
                    content(base),
                    content(result),
                    &branch_hunks,
                    &base_hunks,
                    deadline
                )
                .expect("span comparison"),
                preserved
            );
        }
    }

    #[test]
    fn repeated_long_span_candidates_stop_at_the_shared_comparison_budget() {
        let deadline = Instant::now() + Duration::from_secs(30);
        let pattern = content(&format!("{}b", "a".repeat(IO_BYTES)));
        let result = content(&format!("{}b", "a".repeat(3 * IO_BYTES)));
        let mut work = 4;
        assert_eq!(
            find_span(
                &pattern.file,
                pattern.size as u64,
                &result.file,
                0,
                result.size as u64,
                &mut work,
                deadline
            )
            .expect("bounded search"),
            None
        );
        assert_eq!(work, 0);
        let mut work = 2;
        assert_eq!(
            find_span(
                &pattern.file,
                pattern.size as u64,
                &result.file,
                2 * IO_BYTES as u64,
                result.size as u64,
                &mut work,
                deadline
            )
            .expect("exact candidate"),
            Some(result.size as u64)
        );
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
        let mut allowed = allowed.permitted(deadline).expect("effect counts");
        assert_eq!(
            actual
                .first_dropped(&mut allowed, MAX_MERGE_DETAIL_BYTES, deadline)
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
            let mut allowed = allowed.permitted(deadline).expect("effect counts");
            assert_eq!(
                actual
                    .first_dropped(&mut allowed, MAX_MERGE_DETAIL_BYTES, deadline)
                    .expect("comparison"),
                None,
                "{old:?} -> {new:?}"
            );
            assert!(
                actual
                    .first_dropped(&mut allowed, MAX_MERGE_DETAIL_BYTES, deadline)
                    .expect("consumed counts")
                    .is_some()
            );
        }
    }
    #[test]
    fn disk_diff_matches_minimum_edit_counts_for_repeated_line_sequences() {
        let mut seed = 19u64;
        for case in 0..256 {
            let deadline = Instant::now() + Duration::from_secs(30);
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
            .permitted(deadline)
            .expect("counts");
        let (preview, truncated) = hunks
            .first_dropped(&mut empty, MAX_MERGE_DETAIL_BYTES, deadline)
            .expect("comparison")
            .expect("dropped changes");
        assert!(preview.starts_with("-old-0\n"));
        assert!(preview.len() <= MAX_MERGE_DETAIL_BYTES);
        assert!(truncated);
    }
    #[test]
    fn nearly_disjoint_large_files_fall_back_without_partial_line_effects() {
        let make = |prefix: &str| {
            let mut file = BufWriter::new(tempfile::tempfile().expect("generated text"));
            for line in 0..100_000 {
                if line == 50_000 {
                    writeln!(file, "shared middle line").expect("shared line");
                } else {
                    writeln!(file, "{prefix}-{line:08}").expect("distinct line");
                }
            }
            let mut file = file.into_inner().expect("flush");
            let size = file.metadata().expect("metadata").len() as usize;
            assert!(size > crate::limits::MAX_DIFF_BYTES);
            file.rewind().expect("rewind");
            ObjectContent {
                file,
                size,
                kind: git2::ObjectType::Blob,
            }
        };
        let mut hunks = Hunks::new().expect("hunks");
        hunks.single(b"mode change").expect("existing effect");
        let deadline = Instant::now() + Duration::from_secs(30);
        assert_eq!(
            text_hunks(make("old"), make("new"), &mut hunks, deadline).expect("bounded diff"),
            TextDiff::WholeObject
        );
        assert_eq!(hunks.effects, 1, "partial line effects are discarded");
        let mut allowed = Hunks::new().expect("allowed");
        allowed.single(b"mode change").expect("mode effect");
        let mut allowed = allowed.permitted(deadline).expect("allowed counts");
        assert_eq!(
            hunks
                .first_dropped(&mut allowed, MAX_MERGE_DETAIL_BYTES, deadline)
                .expect("comparison"),
            None
        );
    }

    fn newline_content(bytes: usize) -> ObjectContent {
        ObjectContent::decode(
            &mut std::io::repeat(b'\n').take(bytes as u64),
            bytes,
            git2::ObjectType::Blob,
            None,
        )
        .expect("generated newline-heavy blob")
    }

    #[test]
    fn newline_heavy_files_fall_back_before_reading_the_whole_blob() {
        for (old, new) in [
            (newline_content(256 * 1024), content("new\n")),
            (content("old\n"), newline_content(256 * 1024)),
        ] {
            let mut old_position = old.file.try_clone().expect("old position observer");
            let mut new_position = new.file.try_clone().expect("new position observer");
            let mut hunks = Hunks::new().expect("hunks");
            hunks.single(b"mode change").expect("existing effect");
            let deadline = Instant::now() + Duration::from_secs(30);
            assert_eq!(
                text_hunks(old, new, &mut hunks, deadline).expect("bounded indexing"),
                TextDiff::WholeObject
            );
            assert!(old_position.stream_position().expect("old position") < 256 * 1024);
            assert!(new_position.stream_position().expect("new position") < 256 * 1024);
            let mut allowed = Hunks::new().expect("allowed");
            allowed.single(b"mode change").expect("existing effect");
            let mut allowed = allowed.permitted(deadline).expect("allowed effects");
            assert_eq!(
                hunks
                    .first_dropped(&mut allowed, MAX_MERGE_DETAIL_BYTES, deadline)
                    .expect("existing effects survive without partial lines"),
                None
            );
        }
    }

    #[test]
    fn line_index_admits_its_exact_budget_with_or_without_a_terminal_newline() {
        for last in *b"\nx" {
            let content = newline_content(131_072);
            content
                .file
                .write_all_at(&[last], 131_071)
                .expect("last byte");
            let lines = Lines::new(content, Instant::now() + Duration::from_secs(30))
                .expect("line index")
                .expect("exactly 131072 lines fit");
            assert_eq!(lines.count, 131_072);
            assert_eq!(
                lines.index.metadata().expect("index size").len(),
                6 * 1024 * 1024
            );
        }
    }

    #[test]
    #[ignore = "generates two 1 GiB newline-heavy blobs to measure bounded line-index scratch"]
    fn newline_heavy_gigabyte_versions_bound_line_index_scratch() {
        let old = newline_content(1024 * 1024 * 1024);
        let new = newline_content(1024 * 1024 * 1024);
        new.file
            .write_all_at(b"x", 1024 * 1024 * 1024 - 1)
            .expect("changed final byte");
        let mut old_position = old.file.try_clone().expect("old position observer");
        let mut new_position = new.file.try_clone().expect("new position observer");
        let mut hunks = Hunks::new().expect("hunks");
        assert_eq!(
            text_hunks(
                old,
                new,
                &mut hunks,
                Instant::now() + Duration::from_secs(30)
            )
            .expect("bounded newline-heavy diff"),
            TextDiff::WholeObject
        );
        assert!(old_position.stream_position().expect("old position") < 256 * 1024);
        assert_eq!(new_position.stream_position().expect("new position"), 0);
        assert_eq!(hunks.effects, 0);
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
        assert!(hunks.permitted(expired).is_err());
    }
}
