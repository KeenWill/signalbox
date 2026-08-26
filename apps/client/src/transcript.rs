use std::{
    fs::File,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
};

#[cfg(test)]
use signalbox_process_protocol::ProtocolVersion;
use signalbox_process_protocol::{
    CanonicalUuid, ContentFragment, ModelCallTokenUsage, RunnerProjection, ServerFrame,
    ServerMessage, TranscriptEntry, TranscriptTextEntry, TurnState, decode_server_line,
    encode_server_line,
};

use crate::{connection::Connection, error::ClientError};

#[derive(Debug)]
pub(crate) struct TranscriptSnapshot {
    cursor: u64,
    runner: Option<RunnerProjection>,
    spool: File,
}

impl TranscriptSnapshot {
    pub(crate) const fn cursor(&self) -> u64 {
        self.cursor
    }

    pub(crate) const fn runner(&self) -> Option<&RunnerProjection> {
        self.runner.as_ref()
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

    /// Returns the first acceptance-ordered queued turn, or `None` when no
    /// turn is queued.
    pub(crate) fn first_queued_turn(&mut self) -> Result<Option<CanonicalUuid>, ClientError> {
        let mut replay = self.replay()?;
        for record in &mut replay {
            if let SnapshotRecord::Turn(turn) = record?
                && matches!(turn.state, TurnState::Queued { .. })
            {
                return Ok(Some(turn.turn_id));
            }
        }
        Ok(None)
    }

    /// Returns the turn holding the session's single active slot, or `None`
    /// when every turn is queued or terminal.
    pub(crate) fn active_turn(&mut self) -> Result<Option<CanonicalUuid>, ClientError> {
        let mut replay = self.replay()?;
        for record in &mut replay {
            if let SnapshotRecord::Turn(turn) = record?
                && matches!(
                    turn.state,
                    TurnState::ActiveRunning { .. }
                        | TurnState::ActiveAwaitingModelCallRecovery { .. }
                        | TurnState::ActiveAwaitingToolApproval { .. }
                        | TurnState::ActiveAwaitingChild { .. }
                        | TurnState::ActiveAwaitingToolRecovery { .. }
                        | TurnState::ActiveAwaitingRunnerRecovery { .. }
                )
            {
                return Ok(Some(turn.turn_id));
            }
        }
        Ok(None)
    }

    #[cfg(test)]
    pub(crate) fn from_messages(
        cursor: u64,
        messages: impl IntoIterator<Item = ServerMessage>,
    ) -> Result<Self, ClientError> {
        Self::from_messages_with_runner(cursor, None, messages)
    }

    #[cfg(test)]
    pub(crate) fn from_messages_with_runner(
        cursor: u64,
        runner: Option<RunnerProjection>,
        messages: impl IntoIterator<Item = ServerMessage>,
    ) -> Result<Self, ClientError> {
        use signalbox_process_protocol::RequestId;

        let request_id = RequestId::try_new(1)
            .map_err(|_| ClientError::Protocol("test request identity was invalid"))?;
        let mut spool = tempfile::tempfile()?;
        for message in messages {
            let frame = ServerFrame::try_new_for_version(ProtocolVersion::One, request_id, message)
                .map_err(signalbox_process_protocol::FrameEncodeError::Validation)?;
            append_frame(&mut spool, &frame)?;
        }
        spool.flush()?;
        Ok(Self {
            cursor,
            runner,
            spool,
        })
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
    User {
        accepted_input_id: CanonicalUuid,
        turn_id: CanonicalUuid,
        content: signalbox_process_protocol::UserInputContent,
    },
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
pub(crate) struct SnapshotModelCallUsage {
    pub(crate) turn_id: CanonicalUuid,
    pub(crate) model_call_id: CanonicalUuid,
    pub(crate) usage_provenance: signalbox_process_protocol::UsageProvenance,
    pub(crate) usage: ModelCallTokenUsage,
    pub(crate) cost: Option<signalbox_process_protocol::ModelCallDollarCost>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SnapshotRecord {
    Turn(TranscriptTurn),
    ModelCallUsage(SnapshotModelCallUsage),
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
    let (session_id, cursor, runner) = match connection.message().await? {
        ServerMessage::TranscriptSnapshotStart {
            session_id,
            cursor,
            runner,
        } if session_id == expected_session => (session_id, cursor.value(), runner),
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
    let mut model_call_order = DiskModelCallUsageOrder::new()?;
    let mut model_call_ids = FixedDiskSet::<16>::new()?;
    let mut entry_ids = FixedDiskSet::<32>::new()?;
    let mut prior_acceptance_position = None;
    let mut turn_count = 0_u64;
    let mut model_call_count = 0_u64;
    let mut entry_count = 0_u64;
    let mut model_calls_started = false;
    let mut model_calls_ended = false;
    let mut entries_started = false;
    loop {
        let frame = connection.frame().await?;
        match frame.message().clone() {
            ServerMessage::TranscriptTurn {
                turn_id,
                acceptance_position,
                ..
            } if !model_calls_started && !entries_started => {
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
                model_call_order.push_turn(turn_id)?;
                append_frame(&mut spool, &frame)?;
                turn_count = turn_count
                    .checked_add(1)
                    .ok_or(ClientError::Protocol("snapshot turn count overflowed"))?;
            }
            ServerMessage::TranscriptModelCallUsage {
                model_call_index,
                turn_id,
                model_call_id,
                ..
            } if !model_calls_ended && !entries_started => {
                model_calls_started = true;
                if model_call_index.value() != model_call_count
                    || !model_call_order.accept(turn_id, model_call_id)?
                    || !model_call_ids.insert(uuid_key(model_call_id))?
                {
                    return Err(ClientError::Protocol(
                        "snapshot model-call usage identities, indices, or order were invalid",
                    ));
                }
                append_frame(&mut spool, &frame)?;
                model_call_count = model_call_count
                    .checked_add(1)
                    .ok_or(ClientError::Protocol(
                        "snapshot model-call usage count overflowed",
                    ))?;
            }
            ServerMessage::TranscriptModelCallsEnd {
                model_call_count: ending_model_call_count,
            } if !model_calls_ended
                && !entries_started
                && ending_model_call_count.value() == model_call_count =>
            {
                model_calls_started = true;
                model_calls_ended = true;
            }
            ServerMessage::TranscriptEntry {
                entry_index,
                source_session_id,
                entry_id,
                ..
            } if model_calls_ended => {
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
            ServerMessage::TranscriptUserEntry {
                entry_index,
                source_session_id,
                entry_id,
                ..
            } if model_calls_ended => {
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
            } if model_calls_ended => {
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
                && model_calls_ended
                && ending_turn_count.value() == turn_count
                && ending_entry_count.value() == entry_count =>
            {
                spool.flush()?;
                return Ok(TranscriptSnapshot {
                    cursor,
                    runner,
                    spool,
                });
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
            ..
        } => Ok(SnapshotRecord::Turn(TranscriptTurn {
            turn_id,
            acceptance_position: acceptance_position.value(),
            state,
        })),
        ServerMessage::TranscriptModelCallUsage {
            turn_id,
            model_call_id,
            usage_provenance,
            usage,
            cost,
            ..
        } => Ok(SnapshotRecord::ModelCallUsage(SnapshotModelCallUsage {
            turn_id,
            model_call_id,
            usage_provenance,
            usage,
            cost,
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
        ServerMessage::TranscriptUserEntry {
            entry_index,
            source_session_id,
            entry_id,
            accepted_input_id,
            turn_id,
            content,
        } => Ok(SnapshotRecord::Entry(SnapshotEntry {
            entry_index: entry_index.value(),
            source_session_id,
            entry_id,
            kind: SnapshotEntryKind::User {
                accepted_input_id,
                turn_id,
                content,
            },
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

#[derive(Clone, Copy)]
struct ModelCallUsagePosition {
    turn: [u8; 16],
    call: [u8; 16],
}

struct DiskModelCallUsageOrder {
    file: File,
    reading: bool,
    previous: Option<ModelCallUsagePosition>,
}

impl DiskModelCallUsageOrder {
    fn new() -> io::Result<Self> {
        Ok(Self {
            file: tempfile::tempfile()?,
            reading: false,
            previous: None,
        })
    }

    fn push_turn(&mut self, turn: CanonicalUuid) -> io::Result<()> {
        if self.reading {
            return Err(io::Error::other(
                "model-call usage order accepted a late turn",
            ));
        }
        self.file.write_all(&uuid_key(turn))
    }

    fn accept(&mut self, turn: CanonicalUuid, call: CanonicalUuid) -> io::Result<bool> {
        let turn = uuid_key(turn);
        let call = uuid_key(call);
        let accepted = match self.previous {
            None => self.advance_to(turn)?,
            Some(previous) if previous.turn == turn => previous.call < call,
            Some(_) => self.advance_to(turn)?,
        };
        if accepted {
            self.previous = Some(ModelCallUsagePosition { turn, call });
        }
        Ok(accepted)
    }

    fn advance_to(&mut self, selected: [u8; 16]) -> io::Result<bool> {
        if !self.reading {
            self.file.seek(SeekFrom::Start(0))?;
            self.reading = true;
        }
        let mut candidate = [0_u8; 16];
        loop {
            match self.file.read_exact(&mut candidate) {
                Ok(()) if candidate == selected => return Ok(true),
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
                Err(error) => return Err(error),
            }
        }
    }
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
    use signalbox_process_protocol::{CanonicalU64, CanonicalUuid, ServerMessage, TurnState};
    use uuid::Uuid;

    use super::{DiskModelCallUsageOrder, FixedDiskSet, TranscriptSnapshot};

    #[test]
    fn awaiting_child_snapshot_owns_the_active_turn_slot() {
        let turn = wire_uuid(1);
        let mut snapshot = TranscriptSnapshot::from_messages(
            1,
            [ServerMessage::TranscriptTurn {
                turn_id: turn,
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state: TurnState::ActiveAwaitingChild {
                    await_request_id: wire_uuid(2),
                    spawning_request_id: wire_uuid(3),
                    child_session_id: wire_uuid(4),
                },
            }],
        )
        .expect("the awaiting-child snapshot is valid");

        assert_eq!(
            snapshot.active_turn().expect("the snapshot replays"),
            Some(turn)
        );
    }

    #[test]
    fn disk_usage_order_rejects_a_return_to_an_earlier_turn() {
        let first_turn = wire_uuid(1);
        let second_turn = wire_uuid(2);
        let mut order = DiskModelCallUsageOrder::new().expect("anonymous order file must open");
        order.push_turn(first_turn).expect("first turn must spool");
        order
            .push_turn(second_turn)
            .expect("second turn must spool");

        assert!(
            order
                .accept(first_turn, wire_uuid(11))
                .expect("first usage lookup must succeed")
        );
        assert!(
            order
                .accept(second_turn, wire_uuid(12))
                .expect("second usage lookup must succeed")
        );
        assert!(
            !order
                .accept(first_turn, wire_uuid(13))
                .expect("backward usage lookup must succeed")
        );
    }

    #[test]
    fn disk_usage_order_rejects_descending_calls_within_one_turn() {
        let turn = wire_uuid(1);
        let mut order = DiskModelCallUsageOrder::new().expect("anonymous order file must open");
        order.push_turn(turn).expect("turn must spool");

        assert!(
            order
                .accept(turn, wire_uuid(12))
                .expect("first usage lookup must succeed")
        );
        assert!(
            !order
                .accept(turn, wire_uuid(11))
                .expect("descending usage lookup must succeed")
        );
    }

    #[test]
    fn disk_identity_set_grows_at_its_load_boundary() {
        let mut set = FixedDiskSet::<2>::new().expect("anonymous test file must open");
        assert_inserted(&mut set, [0, 0]);
        assert_inserted(&mut set, [0, 1]);
        assert_inserted(&mut set, [0, 2]);
        assert_inserted(&mut set, [0, 3]);
        assert_inserted(&mut set, [0, 4]);
        assert_inserted(&mut set, [0, 5]);
        assert_inserted(&mut set, [0, 6]);
        assert_inserted(&mut set, [0, 7]);
        assert_inserted(&mut set, [0, 8]);
        assert_inserted(&mut set, [0, 9]);
        assert_inserted(&mut set, [0, 10]);
        assert_eq!(set.capacity, 16);

        assert_inserted(&mut set, [0, 11]);

        assert_eq!(set.capacity, 32);
        assert_eq!(set.len, 12);
    }

    #[test]
    fn disk_identity_set_rejects_an_exact_duplicate() {
        let mut set = FixedDiskSet::<2>::new().expect("anonymous test file must open");
        assert_inserted(&mut set, [0, 2]);

        assert!(!set.insert([0, 2]).expect("duplicate lookup must succeed"));
        assert_eq!(set.len, 1);
    }

    fn wire_uuid(value: u128) -> CanonicalUuid {
        CanonicalUuid::from_uuid(Uuid::from_u128(value))
    }

    #[track_caller]
    fn assert_inserted(set: &mut FixedDiskSet<2>, key: [u8; 2]) {
        assert!(set.insert(key).expect("disk identity insert must succeed"));
    }
}
