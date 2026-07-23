use std::collections::HashSet;

use signalbox_process_protocol::{
    CanonicalUuid, ServerMessage, TranscriptEntry, TranscriptTextEntry, TurnState,
};

use crate::{connection::Connection, error::ClientError};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranscriptSnapshot {
    cursor: u64,
    turns: Vec<TranscriptTurn>,
    entries: Vec<SnapshotEntry>,
}

impl TranscriptSnapshot {
    pub(crate) const fn cursor(&self) -> u64 {
        self.cursor
    }

    pub(crate) fn entries(&self) -> &[SnapshotEntry] {
        &self.entries
    }

    pub(crate) fn turn_state(&self, turn_id: CanonicalUuid) -> Option<&TurnState> {
        self.turns
            .iter()
            .find(|turn| turn.turn_id == turn_id)
            .map(|turn| &turn.state)
    }

    pub(crate) fn assistant_text(&self, selected_turn: CanonicalUuid) -> Result<&str, ClientError> {
        let mut matching = self.entries.iter().filter_map(|entry| match &entry.kind {
            SnapshotEntryKind::Text {
                metadata: TranscriptTextEntry::Assistant { turn_id, .. },
                content,
            } if *turn_id == selected_turn => Some(content.as_str()),
            SnapshotEntryKind::Text { .. }
            | SnapshotEntryKind::Marker(TranscriptEntry::TurnCompleted { .. })
            | SnapshotEntryKind::Marker(TranscriptEntry::TurnFailed { .. }) => None,
        });
        let text = matching.next().ok_or(ClientError::Protocol(
            "completed turn had no assistant transcript entry",
        ))?;
        if matching.next().is_some() {
            return Err(ClientError::Protocol(
                "completed turn had multiple assistant transcript entries",
            ));
        }
        Ok(text)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranscriptTurn {
    pub(crate) turn_id: CanonicalUuid,
    pub(crate) state: TurnState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SnapshotEntry {
    pub(crate) source_session_id: CanonicalUuid,
    pub(crate) entry_id: CanonicalUuid,
    pub(crate) kind: SnapshotEntryKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SnapshotEntryKind {
    Text {
        metadata: TranscriptTextEntry,
        content: String,
    },
    Marker(TranscriptEntry),
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

    let mut turns = Vec::new();
    let mut turn_ids = HashSet::new();
    let mut entries = Vec::new();
    let mut entries_started = false;
    loop {
        match connection.message().await? {
            ServerMessage::TranscriptTurn {
                turn_id,
                acceptance_position,
                state,
            } if !entries_started => {
                let expected_position = u64::try_from(turns.len())
                    .ok()
                    .and_then(|count| count.checked_add(1))
                    .ok_or(ClientError::Protocol("snapshot turn count overflowed"))?;
                if acceptance_position.value() != expected_position || !turn_ids.insert(turn_id) {
                    return Err(ClientError::Protocol(
                        "snapshot turns were not unique acceptance-order projections",
                    ));
                }
                turns.push(TranscriptTurn { turn_id, state });
            }
            ServerMessage::TranscriptEntry {
                entry_index,
                source_session_id,
                entry_id,
                entry,
            } => {
                entries_started = true;
                require_entry_index(entry_index.value(), entries.len())?;
                entries.push(SnapshotEntry {
                    source_session_id,
                    entry_id,
                    kind: SnapshotEntryKind::Marker(entry),
                });
            }
            ServerMessage::TranscriptTextEntry {
                entry_index,
                source_session_id,
                entry_id,
                entry,
            } => {
                entries_started = true;
                require_entry_index(entry_index.value(), entries.len())?;
                let content = read_content(connection, entry_index.value()).await?;
                entries.push(SnapshotEntry {
                    source_session_id,
                    entry_id,
                    kind: SnapshotEntryKind::Text {
                        metadata: entry,
                        content,
                    },
                });
            }
            ServerMessage::TranscriptSnapshotEnd {
                session_id: ending_session,
                cursor: ending_cursor,
                turn_count,
                entry_count,
            } if ending_session == session_id
                && ending_cursor.value() == cursor
                && usize_matches(turn_count.value(), turns.len())
                && usize_matches(entry_count.value(), entries.len()) =>
            {
                return Ok(TranscriptSnapshot {
                    cursor,
                    turns,
                    entries,
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
    entry_index: u64,
) -> Result<String, ClientError> {
    let mut content = String::new();
    let mut expected_fragment = 0_u64;
    loop {
        match connection.message().await? {
            ServerMessage::TranscriptContent {
                entry_index: fragment_entry,
                fragment_index,
                final_fragment,
                content_fragment,
            } if fragment_entry.value() == entry_index
                && fragment_index.value() == expected_fragment =>
            {
                content.push_str(content_fragment.as_str());
                if final_fragment {
                    return Ok(content);
                }
                expected_fragment = expected_fragment
                    .checked_add(1)
                    .ok_or(ClientError::Protocol("content fragment index overflowed"))?;
            }
            ServerMessage::Error {
                code,
                message,
                detail,
            } => return Err(ClientError::remote(code, message, detail)),
            _ => {
                return Err(ClientError::Protocol(
                    "text entry content fragments were invalid",
                ));
            }
        }
    }
}

fn require_entry_index(index: u64, entry_count: usize) -> Result<(), ClientError> {
    if usize_matches(index, entry_count) {
        Ok(())
    } else {
        Err(ClientError::Protocol(
            "snapshot entry indices were not contiguous",
        ))
    }
}

fn usize_matches(value: u64, expected: usize) -> bool {
    usize::try_from(value) == Ok(expected)
}
