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
        self.effect(hash, bytes.len() as u64, bytes)
    }
    fn effect(
        &mut self,
        hash: [u8; 32],
        length: u64,
        preview: &[u8],
    ) -> Result<(), GitPushFailure> {
        let kept = preview.len().min(MAX_MERGE_DETAIL_BYTES - self.preview);
        self.file.write_all(&[2]).map_err(failed)?;
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
        let slots = self
            .effects
            .checked_mul(2)
            .and_then(|n| n.checked_add(1))
            .and_then(u64::checked_next_power_of_two)
            .ok_or(GitPushFailure::Repository)?;
        let file = tempfile::tempfile().map_err(failed)?;
        file.set_len(slots.checked_mul(48).ok_or(GitPushFailure::Repository)?)
            .map_err(failed)?;
        let mut effects = Effects { file, slots };
        let mut reader = self.reader()?;
        while let Some(record) = record(&mut reader)? {
            check(deadline)?;
            if let Record::Effect { hash, .. } = record {
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
                } => {
                    if !dropped && !permitted.change(hash, false, deadline)? {
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
    }))
}
pub(super) struct Effects {
    file: File,
    slots: u64,
}
impl Effects {
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
        hunks.effect(hash.finalize().into(), line.length + 1, &preview)?;
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
    let mut prefix = 0;
    while prefix < old.count.min(new.count) && old.line(prefix)?.hash == new.line(prefix)?.hash {
        check(deadline)?;
        prefix += 1;
    }
    let mut old_end = old.count;
    let mut new_end = new.count;
    while old_end > prefix
        && new_end > prefix
        && old.line(old_end - 1)?.hash == new.line(new_end - 1)?.hash
    {
        check(deadline)?;
        old_end -= 1;
        new_end -= 1;
    }
    if old_end == prefix || new_end == prefix {
        if old_end != prefix || new_end != prefix {
            hunks.start()?;
        }
        for line in prefix..old_end {
            old.emit(line, b'-', hunks, deadline)?;
        }
        for line in prefix..new_end {
            new.emit(line, b'+', hunks, deadline)?;
        }
        return Ok(());
    }
    let n = i64::try_from(old_end - prefix).map_err(failed)?;
    let m = i64::try_from(new_end - prefix).map_err(failed)?;
    let max = n.checked_add(m).ok_or(GitPushFailure::Repository)?;
    let frontier = tempfile::tempfile().map_err(failed)?;
    frontier
        .set_len(
            u64::try_from(
                max.checked_mul(2)
                    .and_then(|n| n.checked_add(3))
                    .ok_or(GitPushFailure::Repository)?,
            )
            .map_err(failed)?
            .checked_mul(8)
            .ok_or(GitPushFailure::Repository)?,
        )
        .map_err(failed)?;
    let mut trace = BufWriter::new(tempfile::tempfile().map_err(failed)?);
    let mut distance = 0;
    'search: for d in 0..=max {
        for k in (-d..=d).step_by(2) {
            check(deadline)?;
            let mut x = if k == -d
                || (k != d && value(&frontier, max + k - 1)? < value(&frontier, max + k + 1)?)
            {
                value(&frontier, max + k + 1)?
            } else {
                value(&frontier, max + k - 1)? + 1
            };
            let mut y = x - k;
            while x < n
                && y < m
                && old.line(prefix + x as u64)?.hash == new.line(prefix + y as u64)?.hash
            {
                check(deadline)?;
                x += 1;
                y += 1;
            }
            frontier
                .write_all_at(&x.to_le_bytes(), ((max + k) as u64) * 8)
                .map_err(failed)?;
            trace.write_all(&x.to_le_bytes()).map_err(failed)?;
            if x >= n && y >= m {
                distance = d;
                break 'search;
            }
        }
    }
    let trace = trace.into_inner().map_err(failed)?;
    let mut operations = BufWriter::new(tempfile::tempfile().map_err(failed)?);
    let (mut x, mut y) = (n, m);
    for d in (1..=distance).rev() {
        check(deadline)?;
        let k = x - y;
        let previous =
            |k| -> Result<i64, GitPushFailure> { value(&trace, (d - 1) * d / 2 + (k + d - 1) / 2) };
        let previous_k = if k == -d || (k != d && previous(k - 1)? < previous(k + 1)?) {
            k + 1
        } else {
            k - 1
        };
        let previous_x = previous(previous_k)?;
        let previous_y = previous_x - previous_k;
        if x > previous_x && y > previous_y {
            operation(&mut operations, b'=', 0)?;
            x = previous_x + i64::from(previous_k == k - 1);
            y = previous_y + i64::from(previous_k == k + 1);
        }
        if x == previous_x {
            operation(&mut operations, b'+', prefix + (y - 1) as u64)?;
        } else {
            operation(&mut operations, b'-', prefix + (x - 1) as u64)?;
        }
        x = previous_x;
        y = previous_y;
    }
    let operations = operations.into_inner().map_err(failed)?;
    let mut position = operations.metadata().map_err(failed)?.len();
    let mut start = true;
    while position != 0 {
        check(deadline)?;
        position -= 9;
        let mut operation = [0; 9];
        operations
            .read_exact_at(&mut operation, position)
            .map_err(failed)?;
        if operation[0] == b'=' {
            start = true;
            continue;
        }
        if start {
            hunks.start()?;
            start = false;
        }
        let line = u64::from_le_bytes(operation[1..].try_into().map_err(failed)?);
        if operation[0] == b'-' {
            old.emit(line, b'-', hunks, deadline)?;
        } else {
            new.emit(line, b'+', hunks, deadline)?;
        }
    }
    Ok(())
}
fn value(file: &File, index: i64) -> Result<i64, GitPushFailure> {
    let mut bytes = [0; 8];
    file.read_exact_at(
        &mut bytes,
        u64::try_from(index)
            .map_err(failed)?
            .checked_mul(8)
            .ok_or(GitPushFailure::Repository)?,
    )
    .map_err(failed)?;
    Ok(i64::from_le_bytes(bytes))
}
fn operation(file: &mut impl Write, origin: u8, line: u64) -> Result<(), GitPushFailure> {
    file.write_all(&[origin]).map_err(failed)?;
    file.write_all(&line.to_le_bytes()).map_err(failed)
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
