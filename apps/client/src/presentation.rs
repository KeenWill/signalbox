use std::io::{self, Write};

use signalbox_process_protocol::{
    CanonicalUuid, CurrentModelCallState, ModelCallDisposition, ModelCallState, SessionEvent,
    TranscriptEntry, TranscriptTextEntry, TurnState,
};

use crate::{
    error::ClientError,
    transcript::{
        SnapshotEntry, SnapshotEntryKind, SnapshotIdentitySet, SnapshotRecord, TranscriptSnapshot,
        TranscriptTurn,
    },
};

#[derive(Clone, Copy)]
pub(crate) enum SnapshotSelection {
    All,
    Completed {
        turn_id: CanonicalUuid,
        model_call_id: CanonicalUuid,
    },
    Failed {
        turn_id: CanonicalUuid,
    },
    Refused {
        turn_id: CanonicalUuid,
        model_call_id: CanonicalUuid,
    },
}

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

    pub(crate) fn snapshot(
        &mut self,
        snapshot: &mut TranscriptSnapshot,
    ) -> Result<(), ClientError> {
        self.render_snapshot(snapshot, None, SnapshotSelection::All, true)
    }

    pub(crate) fn followed_snapshot(
        &mut self,
        snapshot: &mut TranscriptSnapshot,
        displayed: &mut SnapshotIdentitySet,
    ) -> Result<(), ClientError> {
        self.render_snapshot(snapshot, Some(displayed), SnapshotSelection::All, true)
    }

    pub(crate) fn terminal_material(
        &mut self,
        snapshot: &mut TranscriptSnapshot,
        displayed: &mut SnapshotIdentitySet,
        selection: SnapshotSelection,
    ) -> Result<(), ClientError> {
        self.render_snapshot(snapshot, Some(displayed), selection, false)
    }

    fn render_snapshot(
        &mut self,
        snapshot: &mut TranscriptSnapshot,
        mut displayed: Option<&mut SnapshotIdentitySet>,
        selection: SnapshotSelection,
        render_turns: bool,
    ) -> Result<(), ClientError> {
        let mut render_content = false;
        for record in snapshot.replay()? {
            match record? {
                SnapshotRecord::Turn(turn) if render_turns => self.snapshot_turn(&turn)?,
                SnapshotRecord::Turn(_) => {}
                SnapshotRecord::Entry(entry) => {
                    render_content = false;
                    let selected = selection.includes(&entry);
                    let undisplayed = if selected {
                        match displayed.as_deref_mut() {
                            Some(identities) => {
                                identities.insert(entry.source_session_id, entry.entry_id)?
                            }
                            None => true,
                        }
                    } else {
                        false
                    };
                    if undisplayed {
                        render_content = matches!(entry.kind, SnapshotEntryKind::Text(_));
                        self.snapshot_entry(&entry)?;
                    }
                }
                SnapshotRecord::Content(content) if render_content => {
                    let content_ends_with_newline = content.content.as_str().ends_with('\n');
                    self.text_fragment(
                        content.content.as_str(),
                        content.final_fragment,
                        content_ends_with_newline,
                    )?;
                    if content.final_fragment {
                        render_content = false;
                    }
                }
                SnapshotRecord::Content(_) => {}
            }
        }
        Ok(())
    }

    pub(crate) fn assistant_text_fragment(
        &mut self,
        fragment: &str,
        final_fragment: bool,
        content_ends_with_newline: bool,
    ) -> io::Result<()> {
        self.text_fragment(fragment, final_fragment, content_ends_with_newline)
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
        self.text_fragment(text, true, text.ends_with('\n'))
    }

    fn text_fragment(
        &mut self,
        fragment: &str,
        final_fragment: bool,
        content_ends_with_newline: bool,
    ) -> io::Result<()> {
        if self.raw {
            self.stdout.write_all(fragment.as_bytes())?;
            if final_fragment {
                self.stdout.flush()?;
            }
            return Ok(());
        }
        self.stdout.write_all(self.render(fragment).as_bytes())?;
        if final_fragment && !content_ends_with_newline {
            self.stdout.write_all(b"\n")?;
        }
        Ok(())
    }

    fn snapshot_turn(&mut self, turn: &TranscriptTurn) -> io::Result<()> {
        let turn_id = turn.turn_id;
        let position = turn.acceptance_position;
        match &turn.state {
            TurnState::Queued {
                accepted_input_id,
                content,
            } => {
                writeln!(
                    self.stdout,
                    "turn={turn_id} position={position} state=queued \
                     accepted_input={accepted_input_id}"
                )?;
                self.text(content.as_str())
            }
            TurnState::ActiveRunning {
                current_attempt_id,
                current_model_call,
            } => match current_model_call {
                Some(call) => writeln!(
                    self.stdout,
                    "turn={turn_id} position={position} state=active_running \
                     attempt={current_attempt_id} call={} call_state={}",
                    call.model_call_id(),
                    current_model_call_state(call.state())
                ),
                None => writeln!(
                    self.stdout,
                    "turn={turn_id} position={position} state=active_running \
                     attempt={current_attempt_id} call=none"
                ),
            },
            TurnState::ActiveAwaitingModelCallRecovery {
                ended_attempt_id,
                recovery_model_call_id,
            } => writeln!(
                self.stdout,
                "turn={turn_id} position={position} \
                 state=active_awaiting_model_call_recovery \
                 attempt={ended_attempt_id} call={recovery_model_call_id}"
            ),
            TurnState::Failed {
                terminal_frontier_id,
            } => writeln!(
                self.stdout,
                "turn={turn_id} position={position} state=failed \
                 frontier={terminal_frontier_id}"
            ),
            TurnState::Completed {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            } => writeln!(
                self.stdout,
                "turn={turn_id} position={position} state=completed \
                 frontier={terminal_frontier_id} attempt={terminal_attempt_id} \
                 call={terminal_model_call_id}"
            ),
            TurnState::Refused {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            } => writeln!(
                self.stdout,
                "turn={turn_id} position={position} state=refused \
                 frontier={terminal_frontier_id} attempt={terminal_attempt_id} \
                 call={terminal_model_call_id}"
            ),
        }
    }

    fn snapshot_entry(&mut self, entry: &SnapshotEntry) -> io::Result<()> {
        match &entry.kind {
            SnapshotEntryKind::Text(metadata) => {
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
                )
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

impl SnapshotSelection {
    fn includes(self, entry: &SnapshotEntry) -> bool {
        match (self, &entry.kind) {
            (Self::All, _) => true,
            (
                Self::Completed {
                    turn_id,
                    model_call_id,
                },
                SnapshotEntryKind::Text(TranscriptTextEntry::Assistant {
                    turn_id: entry_turn,
                    model_call_id: entry_call,
                }),
            )
            | (
                Self::Refused {
                    turn_id,
                    model_call_id,
                },
                SnapshotEntryKind::Text(TranscriptTextEntry::Assistant {
                    turn_id: entry_turn,
                    model_call_id: entry_call,
                }),
            ) => turn_id == *entry_turn && model_call_id == *entry_call,
            (
                Self::Completed { turn_id, .. },
                SnapshotEntryKind::Marker(TranscriptEntry::TurnCompleted {
                    turn_id: entry_turn,
                }),
            )
            | (
                Self::Failed { turn_id },
                SnapshotEntryKind::Marker(TranscriptEntry::TurnFailed {
                    turn_id: entry_turn,
                }),
            ) => turn_id == *entry_turn,
            (
                Self::Completed { .. } | Self::Failed { .. } | Self::Refused { .. },
                SnapshotEntryKind::Text(_)
                | SnapshotEntryKind::Marker(
                    TranscriptEntry::TurnCompleted { .. } | TranscriptEntry::TurnFailed { .. },
                ),
            ) => false,
        }
    }
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

const fn current_model_call_state(state: CurrentModelCallState) -> &'static str {
    match state {
        CurrentModelCallState::Prepared {} => "prepared",
        CurrentModelCallState::InFlight {} => "in_flight",
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
    use std::io::{self, Write};

    use signalbox_process_protocol::{
        CanonicalU64, CanonicalUuid, ContentFragment, InputContent, ServerMessage, TranscriptEntry,
        TranscriptTextEntry, TurnState,
    };
    use uuid::Uuid;

    use super::{Output, SnapshotSelection, control_safe};
    use crate::transcript::{SnapshotIdentitySet, TranscriptSnapshot};

    #[test]
    fn terminal_safe_text_preserves_line_feed_and_escapes_c0_del_and_c1() {
        assert_eq!(
            control_safe("a\n\t\u{1b}\u{7f}\u{85}z"),
            "a\n\\u{9}\\u{1b}\\u{7f}\\u{85}z"
        );
        assert_eq!(control_safe("café\u{1f980}"), "café\u{1f980}");
    }

    #[test]
    fn raw_assistant_text_flushes_without_adding_a_delimiter() {
        let mut stdout = FlushWriter::default();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, true);
        output
            .assistant_text_fragment("ok", true, false)
            .expect("in-memory output cannot fail");
        assert_eq!(stdout.bytes, b"ok");
        assert_eq!(stdout.flushes, 1);
        assert!(stderr.is_empty());
    }

    #[test]
    fn followed_snapshot_renders_queued_content_before_adopting_its_cursor() {
        let turn_id = wire_uuid(1);
        let accepted_input_id = wire_uuid(2);
        let mut snapshot = TranscriptSnapshot::from_messages(
            9,
            [ServerMessage::TranscriptTurn {
                turn_id,
                acceptance_position: CanonicalU64::new(1),
                state: TurnState::Queued {
                    accepted_input_id,
                    content: InputContent::new("queued owner text".to_owned()),
                },
            }],
        )
        .expect("test snapshot must spool");
        let mut displayed = SnapshotIdentitySet::new().expect("identity spool must open");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .followed_snapshot(&mut snapshot, &mut displayed)
            .expect("queued snapshot must render");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        assert!(rendered.contains("state=queued"));
        assert!(rendered.contains("queued owner text"));
        assert!(stderr.is_empty());
    }

    #[test]
    fn terminal_reread_excludes_material_from_later_buffered_events() {
        let selected_turn = wire_uuid(1);
        let selected_call = wire_uuid(2);
        let later_turn = wire_uuid(3);
        let later_call = wire_uuid(4);
        let mut snapshot = TranscriptSnapshot::from_messages(
            12,
            [
                ServerMessage::TranscriptTextEntry {
                    entry_index: CanonicalU64::new(0),
                    source_session_id: wire_uuid(10),
                    entry_id: wire_uuid(11),
                    entry: TranscriptTextEntry::Assistant {
                        turn_id: selected_turn,
                        model_call_id: selected_call,
                    },
                },
                ServerMessage::TranscriptContent {
                    entry_index: CanonicalU64::new(0),
                    fragment_index: CanonicalU64::new(0),
                    final_fragment: true,
                    content_fragment: ContentFragment::try_new("selected reply".to_owned())
                        .expect("short content is valid"),
                },
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(1),
                    source_session_id: wire_uuid(10),
                    entry_id: wire_uuid(12),
                    entry: TranscriptEntry::TurnCompleted {
                        turn_id: selected_turn,
                    },
                },
                ServerMessage::TranscriptTextEntry {
                    entry_index: CanonicalU64::new(2),
                    source_session_id: wire_uuid(10),
                    entry_id: wire_uuid(13),
                    entry: TranscriptTextEntry::Assistant {
                        turn_id: later_turn,
                        model_call_id: later_call,
                    },
                },
                ServerMessage::TranscriptContent {
                    entry_index: CanonicalU64::new(2),
                    fragment_index: CanonicalU64::new(0),
                    final_fragment: true,
                    content_fragment: ContentFragment::try_new("later reply".to_owned())
                        .expect("short content is valid"),
                },
            ],
        )
        .expect("test snapshot must spool");
        let mut displayed = SnapshotIdentitySet::new().expect("identity spool must open");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .terminal_material(
                &mut snapshot,
                &mut displayed,
                SnapshotSelection::Completed {
                    turn_id: selected_turn,
                    model_call_id: selected_call,
                },
            )
            .expect("selected terminal material must render");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        assert!(rendered.contains("selected reply"));
        assert!(rendered.contains("turn_completed"));
        assert!(!rendered.contains("later reply"));
        assert!(!rendered.contains(&later_turn.to_string()));
        assert!(stderr.is_empty());
    }

    #[derive(Default)]
    struct FlushWriter {
        bytes: Vec<u8>,
        flushes: usize,
    }

    impl Write for FlushWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    fn wire_uuid(value: u128) -> CanonicalUuid {
        CanonicalUuid::from_uuid(Uuid::from_u128(value))
    }
}
