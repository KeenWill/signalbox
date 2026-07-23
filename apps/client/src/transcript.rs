use std::{
    fs::File,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
};

use signalbox_process_protocol::{
    CanonicalUuid, ContentFragment, ServerFrame, ServerMessage, TranscriptEntry,
    TranscriptTextEntry, TurnState, decode_server_line, encode_server_line,
};

use crate::{connection::Connection, error::ClientError};

#[derive(Debug)]
pub(crate) struct TranscriptSnapshot {
    cursor: u64,
    spool: File,
}

impl TranscriptSnapshot {
    pub(crate) const fn cursor(&self) -> u64 {
        self.cursor
    }

    pub(crate) fn replay(&mut self) -> Result<SnapshotReplay<'_>, ClientError> {
        self.spool.seek(SeekFrom::Start(0))?;
        Ok(SnapshotReplay {
            reader: BufReader::new(&mut self.spool),
        })
    }

    pub(crate) fn turn_state(
        &mut self,
        selected_turn: CanonicalUuid,
    ) -> Result<Option<TurnState>, ClientError> {
        let mut replay = self.replay()?;
        for record in &mut replay {
            if let SnapshotRecord::Turn(turn) = record?
                && turn.turn_id == selected_turn
            {
                return Ok(Some(turn.state));
            }
        }
        Ok(None)
    }

    #[cfg(test)]
    pub(crate) fn from_messages(
        cursor: u64,
        messages: impl IntoIterator<Item = ServerMessage>,
    ) -> Result<Self, ClientError> {
        use signalbox_process_protocol::RequestId;

        let request_id = RequestId::try_new(1)
            .map_err(|_| ClientError::Protocol("test request identity was invalid"))?;
        let mut spool = tempfile::tempfile()?;
        for message in messages {
            let frame = ServerFrame::try_new(request_id, message)
                .map_err(signalbox_process_protocol::FrameEncodeError::Validation)?;
            append_frame(&mut spool, &frame)?;
        }
        spool.flush()?;
        Ok(Self { cursor, spool })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranscriptTurn {
    pub(crate) turn_id: CanonicalUuid,
    pub(crate) acceptance_position: u64,
    pub(crate) state: TurnState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SnapshotEntry {
    pub(crate) entry_index: u64,
    pub(crate) source_session_id: CanonicalUuid,
    pub(crate) entry_id: CanonicalUuid,
    pub(crate) kind: SnapshotEntryKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SnapshotEntryKind {
    Text(TranscriptTextEntry),
    Marker(TranscriptEntry),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SnapshotContent {
    pub(crate) entry_index: u64,
    pub(crate) fragment_index: u64,
    pub(crate) final_fragment: bool,
    pub(crate) content: ContentFragment,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SnapshotRecord {
    Turn(TranscriptTurn),
    Entry(SnapshotEntry),
    Content(SnapshotContent),
}

pub(crate) struct SnapshotIdentitySet(FixedDiskSet<32>);

impl SnapshotIdentitySet {
    pub(crate) fn new() -> Result<Self, ClientError> {
        Ok(Self(FixedDiskSet::new()?))
    }

    pub(crate) fn insert(
        &mut self,
        source_session_id: CanonicalUuid,
        entry_id: CanonicalUuid,
    ) -> Result<bool, ClientError> {
        Ok(self.0.insert(entry_key(source_session_id, entry_id))?)
    }
}

pub(crate) struct SnapshotReplay<'a> {
    reader: BufReader<&'a mut File>,
}

impl Iterator for SnapshotReplay<'_> {
    type Item = Result<SnapshotRecord, ClientError>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut line = Vec::new();
        match self.reader.read_until(b'\n', &mut line) {
            Ok(0) => None,
            Ok(_) => Some(
                decode_server_line(&line)
                    .map_err(ClientError::from)
                    .and_then(|frame| snapshot_record(frame.message().clone())),
            ),
            Err(error) => Some(Err(ClientError::Io(error))),
        }
    }
}

pub(crate) async fn read_snapshot(
    connection: &mut Connection,
    expected_session: CanonicalUuid,
) -> Result<TranscriptSnapshot, ClientError> {
    let (session_id, cursor) = match connection.message().await? {
        ServerMessage::TranscriptSnapshotStart { session_id, cursor }
            if session_id == expected_session =>
        {
            (session_id, cursor.value())
        }
        ServerMessage::Error {
            code,
            message,
            detail,
        } => return Err(ClientError::remote(code, message, detail)),
        _ => {
            return Err(ClientError::Protocol(
                "snapshot did not begin with its matching start frame",
            ));
        }
    };

    let mut spool = tempfile::tempfile()?;
    let mut turn_ids = FixedDiskSet::<16>::new()?;
    let mut entry_ids = FixedDiskSet::<32>::new()?;
    let mut prior_acceptance_position = None;
    let mut turn_count = 0_u64;
    let mut entry_count = 0_u64;
    let mut entries_started = false;
    loop {
        let frame = connection.frame().await?;
        match frame.message().clone() {
            ServerMessage::TranscriptTurn {
                turn_id,
                acceptance_position,
                ..
            } if !entries_started => {
                let position = acceptance_position.value();
                if position == 0
                    || prior_acceptance_position.is_some_and(|prior| prior >= position)
                    || !turn_ids.insert(uuid_key(turn_id))?
                {
                    return Err(ClientError::Protocol(
                        "snapshot turns were not unique acceptance-order projections",
                    ));
                }
                prior_acceptance_position = Some(position);
                append_frame(&mut spool, &frame)?;
                turn_count = turn_count
                    .checked_add(1)
                    .ok_or(ClientError::Protocol("snapshot turn count overflowed"))?;
            }
            ServerMessage::TranscriptEntry {
                entry_index,
                source_session_id,
                entry_id,
                ..
            } => {
                entries_started = true;
                require_entry_index(entry_index.value(), entry_count)?;
                if !entry_ids.insert(entry_key(source_session_id, entry_id))? {
                    return Err(ClientError::Protocol(
                        "snapshot repeated a source-qualified entry identity",
                    ));
                }
                append_frame(&mut spool, &frame)?;
                entry_count = entry_count
                    .checked_add(1)
                    .ok_or(ClientError::Protocol("snapshot entry count overflowed"))?;
            }
            ServerMessage::TranscriptTextEntry {
                entry_index,
                source_session_id,
                entry_id,
                ..
            } => {
                entries_started = true;
                require_entry_index(entry_index.value(), entry_count)?;
                if !entry_ids.insert(entry_key(source_session_id, entry_id))? {
                    return Err(ClientError::Protocol(
                        "snapshot repeated a source-qualified entry identity",
                    ));
                }
                append_frame(&mut spool, &frame)?;
                read_content(connection, &mut spool, entry_index.value()).await?;
                entry_count = entry_count
                    .checked_add(1)
                    .ok_or(ClientError::Protocol("snapshot entry count overflowed"))?;
            }
            ServerMessage::TranscriptSnapshotEnd {
                session_id: ending_session,
                cursor: ending_cursor,
                turn_count: ending_turn_count,
                entry_count: ending_entry_count,
            } if ending_session == session_id
                && ending_cursor.value() == cursor
                && ending_turn_count.value() == turn_count
                && ending_entry_count.value() == entry_count =>
            {
                spool.flush()?;
                return Ok(TranscriptSnapshot { cursor, spool });
            }
            ServerMessage::Error {
                code,
                message,
                detail,
            } => return Err(ClientError::remote(code, message, detail)),
            _ => {
                return Err(ClientError::Protocol(
                    "snapshot frame order or terminal counts were invalid",
                ));
            }
        }
    }
}

async fn read_content(
    connection: &mut Connection,
    spool: &mut File,
    entry_index: u64,
) -> Result<(), ClientError> {
    let mut expected_fragment = 0_u64;
    loop {
        let frame = connection.frame().await?;
        match frame.message() {
            ServerMessage::TranscriptContent {
                entry_index: fragment_entry,
                fragment_index,
                final_fragment,
                ..
            } if fragment_entry.value() == entry_index
                && fragment_index.value() == expected_fragment =>
            {
                append_frame(spool, &frame)?;
                if *final_fragment {
                    return Ok(());
                }
                expected_fragment = expected_fragment
                    .checked_add(1)
                    .ok_or(ClientError::Protocol("content fragment index overflowed"))?;
            }
            ServerMessage::Error {
                code,
                message,
                detail,
            } => return Err(ClientError::remote(*code, message.clone(), *detail)),
            _ => {
                return Err(ClientError::Protocol(
                    "text entry content fragments were invalid",
                ));
            }
        }
    }
}

fn append_frame(spool: &mut File, frame: &ServerFrame) -> Result<(), ClientError> {
    spool.write_all(&encode_server_line(frame)?)?;
    Ok(())
}

fn snapshot_record(message: ServerMessage) -> Result<SnapshotRecord, ClientError> {
    match message {
        ServerMessage::TranscriptTurn {
            turn_id,
            acceptance_position,
            state,
        } => Ok(SnapshotRecord::Turn(TranscriptTurn {
            turn_id,
            acceptance_position: acceptance_position.value(),
            state,
        })),
        ServerMessage::TranscriptEntry {
            entry_index,
            source_session_id,
            entry_id,
            entry,
        } => Ok(SnapshotRecord::Entry(SnapshotEntry {
            entry_index: entry_index.value(),
            source_session_id,
            entry_id,
            kind: SnapshotEntryKind::Marker(entry),
        })),
        ServerMessage::TranscriptTextEntry {
            entry_index,
            source_session_id,
            entry_id,
            entry,
        } => Ok(SnapshotRecord::Entry(SnapshotEntry {
            entry_index: entry_index.value(),
            source_session_id,
            entry_id,
            kind: SnapshotEntryKind::Text(entry),
        })),
        ServerMessage::TranscriptContent {
            entry_index,
            fragment_index,
            final_fragment,
            content_fragment,
        } => Ok(SnapshotRecord::Content(SnapshotContent {
            entry_index: entry_index.value(),
            fragment_index: fragment_index.value(),
            final_fragment,
            content: content_fragment,
        })),
        _ => Err(ClientError::Protocol(
            "snapshot spool contained a non-snapshot frame",
        )),
    }
}

fn require_entry_index(index: u64, entry_count: u64) -> Result<(), ClientError> {
    if index == entry_count {
        Ok(())
    } else {
        Err(ClientError::Protocol(
            "snapshot entry indices were not contiguous",
        ))
    }
}

fn uuid_key(value: CanonicalUuid) -> [u8; 16] {
    *value.into_uuid().as_bytes()
}

fn entry_key(source_session_id: CanonicalUuid, entry_id: CanonicalUuid) -> [u8; 32] {
    let mut key = [0_u8; 32];
    key[..16].copy_from_slice(source_session_id.into_uuid().as_bytes());
    key[16..].copy_from_slice(entry_id.into_uuid().as_bytes());
    key
}

struct FixedDiskSet<const WIDTH: usize> {
    file: File,
    len: u64,
    capacity: u64,
}

impl<const WIDTH: usize> FixedDiskSet<WIDTH> {
    fn new() -> io::Result<Self> {
        Self::with_capacity(16)
    }

    fn insert(&mut self, key: [u8; WIDTH]) -> io::Result<bool> {
        let next_len = self
            .len
            .checked_add(1)
            .ok_or_else(|| io::Error::other("disk identity count overflowed"))?;
        if next_len
            .checked_mul(10)
            .is_none_or(|scaled| scaled >= self.capacity.saturating_mul(7))
        {
            self.grow()?;
        }
        self.insert_without_grow(key)
    }

    fn with_capacity(capacity: u64) -> io::Result<Self> {
        let file = tempfile::tempfile()?;
        file.set_len(slot_offset::<WIDTH>(capacity)?)?;
        Ok(Self {
            file,
            len: 0,
            capacity,
        })
    }

    fn insert_without_grow(&mut self, key: [u8; WIDTH]) -> io::Result<bool> {
        let start = stable_hash(&key) % self.capacity;
        let mut probe = [0_u8; WIDTH];
        for displacement in 0..self.capacity {
            let index = (start + displacement) % self.capacity;
            if !self.read_slot(index, &mut probe)? {
                self.write_slot(index, &key)?;
                self.len = self
                    .len
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("disk identity count overflowed"))?;
                return Ok(true);
            }
            if probe == key {
                return Ok(false);
            }
        }
        Err(io::Error::other(
            "disk identity index was unexpectedly full",
        ))
    }

    fn grow(&mut self) -> io::Result<()> {
        let new_capacity = self
            .capacity
            .checked_mul(2)
            .ok_or_else(|| io::Error::other("disk identity capacity overflowed"))?;
        let mut replacement = Self::with_capacity(new_capacity)?;
        let mut key = [0_u8; WIDTH];
        for index in 0..self.capacity {
            if self.read_slot(index, &mut key)? && !replacement.insert_without_grow(key)? {
                return Err(io::Error::other(
                    "disk identity rehash encountered a duplicate",
                ));
            }
        }
        *self = replacement;
        Ok(())
    }

    fn read_slot(&mut self, index: u64, key: &mut [u8; WIDTH]) -> io::Result<bool> {
        self.file
            .seek(SeekFrom::Start(slot_offset::<WIDTH>(index)?))?;
        let mut occupied = [0_u8; 1];
        self.file.read_exact(&mut occupied)?;
        match occupied[0] {
            0 => Ok(false),
            1 => {
                self.file.read_exact(key)?;
                Ok(true)
            }
            _ => Err(io::Error::other(
                "disk identity index contained an invalid occupancy flag",
            )),
        }
    }

    fn write_slot(&mut self, index: u64, key: &[u8; WIDTH]) -> io::Result<()> {
        self.file
            .seek(SeekFrom::Start(slot_offset::<WIDTH>(index)?))?;
        self.file.write_all(&[1])?;
        self.file.write_all(key)
    }
}

fn slot_offset<const WIDTH: usize>(index: u64) -> io::Result<u64> {
    index
        .checked_mul(
            u64::try_from(WIDTH)
                .map_err(|_| io::Error::other("disk identity width overflowed"))?
                .checked_add(1)
                .ok_or_else(|| io::Error::other("disk identity slot width overflowed"))?,
        )
        .ok_or_else(|| io::Error::other("disk identity offset overflowed"))
}

fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::FixedDiskSet;

    #[test]
    fn disk_identity_set_preserves_exact_uniqueness() {
        let mut set = FixedDiskSet::<2>::new().expect("anonymous test file must open");
        for value in 0_u16..100 {
            assert!(
                set.insert(value.to_be_bytes())
                    .expect("growing insert must succeed")
            );
        }
        assert!(!set.insert([0, 2]).expect("duplicate lookup must succeed"));
    }
}
