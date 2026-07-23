use std::{
    collections::HashSet,
    io::{self, Write},
};

use signalbox_process_protocol::{
    CanonicalUuid, ModelCallDisposition, ModelCallState, SessionEvent, TranscriptEntry,
    TranscriptTextEntry,
};

use crate::{
    error::ClientError,
    transcript::{SnapshotEntry, SnapshotEntryKind, TranscriptSnapshot},
};

pub(crate) type TranscriptEntryIdentity = (CanonicalUuid, CanonicalUuid);

pub(crate) struct Output<'a> {
    stdout: &'a mut dyn Write,
    stderr: &'a mut dyn Write,
    raw: bool,
}

impl<'a> Output<'a> {
    pub(crate) fn new(stdout: &'a mut dyn Write, stderr: &'a mut dyn Write, raw: bool) -> Self {
        Self {
            stdout,
            stderr,
            raw,
        }
    }

    pub(crate) fn recovery_value(&mut self, name: &str, value: &str) -> io::Result<()> {
        writeln!(self.stderr, "{name}={value}")
    }

    pub(crate) fn error(&mut self, error: &ClientError) -> io::Result<()> {
        let message = format!("error: {error}");
        self.stderr.write_all(self.render(&message).as_bytes())?;
        self.stderr.write_all(b"\n")
    }

    pub(crate) fn session_created(&mut self, session_id: CanonicalUuid) -> io::Result<()> {
        writeln!(self.stdout, "{session_id}")
    }

    pub(crate) fn session_summary(
        &mut self,
        session_id: CanonicalUuid,
        defaults_version: u64,
        selection: &str,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "{session_id} defaults_version={defaults_version} {selection}"
        )
    }

    pub(crate) fn snapshot(&mut self, snapshot: &TranscriptSnapshot) -> io::Result<()> {
        for entry in snapshot.entries() {
            self.snapshot_entry(entry)?;
        }
        Ok(())
    }

    pub(crate) fn snapshot_new_entries(
        &mut self,
        snapshot: &TranscriptSnapshot,
        displayed: &mut HashSet<TranscriptEntryIdentity>,
    ) -> io::Result<()> {
        for entry in snapshot.entries() {
            if displayed.insert(transcript_entry_identity(entry)) {
                self.snapshot_entry(entry)?;
            }
        }
        Ok(())
    }

    pub(crate) fn assistant_text(&mut self, text: &str) -> io::Result<()> {
        self.text(text)
    }

    pub(crate) fn event(
        &mut self,
        cursor: u64,
        session_id: CanonicalUuid,
        event: &SessionEvent,
    ) -> io::Result<()> {
        match event {
            SessionEvent::SessionCreated {} => {
                writeln!(
                    self.stdout,
                    "event={cursor} session={session_id} session_created"
                )
            }
            SessionEvent::InputAccepted {
                accepted_input_id,
                turn_id,
                acceptance_position,
                content,
            } => {
                writeln!(
                    self.stdout,
                    "event={cursor} session={session_id} input_accepted \
                     accepted_input={accepted_input_id} turn={turn_id} position={}",
                    acceptance_position.value()
                )?;
                self.text(content.as_str())
            }
            SessionEvent::TurnActivated {
                turn_id,
                current_attempt_id,
            } => writeln!(
                self.stdout,
                "event={cursor} session={session_id} turn_activated \
                 turn={turn_id} attempt={current_attempt_id}"
            ),
            SessionEvent::ModelCallTransition {
                turn_id,
                model_call_id,
                state,
            } => writeln!(
                self.stdout,
                "event={cursor} session={session_id} model_call_transition \
                 turn={turn_id} call={model_call_id} state={}",
                model_call_state(*state)
            ),
            SessionEvent::TurnCompleted {
                turn_id,
                model_call_id,
                completion_entry_id,
                terminal_frontier_id,
            } => writeln!(
                self.stdout,
                "event={cursor} session={session_id} turn_completed turn={turn_id} \
                 call={model_call_id} entry={completion_entry_id} \
                 frontier={terminal_frontier_id}"
            ),
            SessionEvent::TurnFailed {
                turn_id,
                failure_entry_id,
                terminal_frontier_id,
            } => writeln!(
                self.stdout,
                "event={cursor} session={session_id} turn_failed turn={turn_id} \
                 entry={failure_entry_id} frontier={terminal_frontier_id}"
            ),
            SessionEvent::TurnRefused {
                turn_id,
                model_call_id,
                terminal_frontier_id,
            } => writeln!(
                self.stdout,
                "event={cursor} session={session_id} turn_refused turn={turn_id} \
                 call={model_call_id} frontier={terminal_frontier_id}"
            ),
        }
    }

    fn text(&mut self, text: &str) -> io::Result<()> {
        self.stdout.write_all(self.render(text).as_bytes())?;
        if !text.ends_with('\n') {
            self.stdout.write_all(b"\n")?;
        }
        Ok(())
    }

    fn snapshot_entry(&mut self, entry: &SnapshotEntry) -> io::Result<()> {
        match &entry.kind {
            SnapshotEntryKind::Text { metadata, content } => {
                let label = match metadata {
                    TranscriptTextEntry::User { turn_id, .. } => {
                        format!("user turn={turn_id}")
                    }
                    TranscriptTextEntry::Assistant { turn_id, .. } => {
                        format!("assistant turn={turn_id}")
                    }
                };
                writeln!(
                    self.stdout,
                    "{label} source={} entry={}",
                    entry.source_session_id, entry.entry_id
                )?;
                self.text(content)
            }
            SnapshotEntryKind::Marker(TranscriptEntry::TurnCompleted { turn_id }) => {
                writeln!(
                    self.stdout,
                    "turn_completed turn={turn_id} source={} entry={}",
                    entry.source_session_id, entry.entry_id
                )
            }
            SnapshotEntryKind::Marker(TranscriptEntry::TurnFailed { turn_id }) => {
                writeln!(
                    self.stdout,
                    "turn_failed turn={turn_id} source={} entry={}",
                    entry.source_session_id, entry.entry_id
                )
            }
        }
    }

    fn render(&self, value: &str) -> String {
        if self.raw {
            value.to_owned()
        } else {
            control_safe(value)
        }
    }
}

fn transcript_entry_identity(entry: &SnapshotEntry) -> TranscriptEntryIdentity {
    (entry.source_session_id, entry.entry_id)
}

fn model_call_state(state: ModelCallState) -> &'static str {
    match state {
        ModelCallState::Prepared {} => "prepared",
        ModelCallState::InFlight {} => "in_flight",
        ModelCallState::Terminal { disposition } => match disposition {
            ModelCallDisposition::Completed => "terminal:completed",
            ModelCallDisposition::KnownFailed => "terminal:known_failed",
            ModelCallDisposition::Refused => "terminal:refused",
            ModelCallDisposition::Cancelled => "terminal:cancelled",
            ModelCallDisposition::Ambiguous => "terminal:ambiguous",
        },
    }
}

fn control_safe(value: &str) -> String {
    let mut rendered = String::with_capacity(value.len());
    for character in value.chars() {
        let code = character as u32;
        if character != '\n' && (code <= 0x1f || (0x7f..=0x9f).contains(&code)) {
            rendered.push_str(&format!("\\u{{{code:x}}}"));
        } else {
            rendered.push(character);
        }
    }
    rendered
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use signalbox_process_protocol::{CanonicalUuid, TranscriptEntry};
    use uuid::Uuid;

    use super::{control_safe, transcript_entry_identity};
    use crate::transcript::{SnapshotEntry, SnapshotEntryKind};

    #[test]
    fn terminal_safe_text_preserves_line_feed_and_escapes_c0_del_and_c1() {
        assert_eq!(
            control_safe("a\n\t\u{1b}\u{7f}\u{85}z"),
            "a\n\\u{9}\\u{1b}\\u{7f}\\u{85}z"
        );
        assert_eq!(control_safe("café\u{1f980}"), "café\u{1f980}");
    }

    #[test]
    fn follow_dedup_qualifies_entry_identity_by_source_session() {
        let entry_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let entries = [
            SnapshotEntry {
                source_session_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
                entry_id,
                kind: SnapshotEntryKind::Marker(TranscriptEntry::TurnCompleted { turn_id }),
            },
            SnapshotEntry {
                source_session_id: CanonicalUuid::from_uuid(Uuid::from_u128(4)),
                entry_id,
                kind: SnapshotEntryKind::Marker(TranscriptEntry::TurnCompleted { turn_id }),
            },
        ];
        let mut displayed = HashSet::new();
        assert!(displayed.insert(transcript_entry_identity(&entries[0])));
        assert!(displayed.insert(transcript_entry_identity(&entries[1])));
    }
}
