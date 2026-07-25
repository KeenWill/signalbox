use std::{
    collections::HashSet,
    io::{self, Write},
};

use signalbox_process_protocol::{
    CanonicalUuid, CurrentModelCallState, FailedModelCallDisposition, ModelCallDisposition,
    ModelCallState, SessionEvent, ToolBatchState, TranscriptEntry, TranscriptTextEntry, TurnState,
};

use crate::{
    error::ClientError,
    transcript::{
        SnapshotEntry, SnapshotEntryKind, SnapshotIdentitySet, SnapshotRecord, TranscriptSnapshot,
        TranscriptTurn,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SnapshotSelection {
    All,
    Completed {
        turn_id: CanonicalUuid,
        model_call_id: CanonicalUuid,
        terminal_entry_id: CanonicalUuid,
    },
    Failed {
        turn_id: CanonicalUuid,
        terminal_entry_id: CanonicalUuid,
    },
    Cancelled {
        turn_id: CanonicalUuid,
        terminal_entry_id: CanonicalUuid,
    },
    ToolBatchProposed {
        turn_id: CanonicalUuid,
        model_call_id: CanonicalUuid,
    },
    ToolBatchResults {
        turn_id: CanonicalUuid,
        model_call_id: CanonicalUuid,
    },
    ToolReconciliation {
        turn_id: CanonicalUuid,
        tool_attempt_id: CanonicalUuid,
        terminal_frontier_id: CanonicalUuid,
    },
}

#[derive(Default)]
struct SnapshotSelectionContext {
    requests: HashSet<CanonicalUuid>,
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
        let selection_context = selection.context(snapshot)?;
        let mut render_content = false;
        for record in snapshot.replay()? {
            match record? {
                SnapshotRecord::Turn(turn) if render_turns => self.snapshot_turn(&turn)?,
                SnapshotRecord::Turn(_) => {}
                SnapshotRecord::Entry(entry) => {
                    render_content = false;
                    let selected = selection.includes(&entry, &selection_context);
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
            SessionEvent::ToolBatchTransition {
                turn_id,
                model_call_id,
                state,
            } => match state {
                ToolBatchState::Proposed { frontier_id } => writeln!(
                    self.stdout,
                    "event={cursor} session={session_id} tool_batch_transition \
                     turn={turn_id} call={model_call_id} state=proposed frontier={frontier_id}"
                ),
                ToolBatchState::ResultsProjected { frontier_id } => writeln!(
                    self.stdout,
                    "event={cursor} session={session_id} tool_batch_transition \
                     turn={turn_id} call={model_call_id} state=results_projected \
                     frontier={frontier_id}"
                ),
                ToolBatchState::RecoveryRequired { tool_attempt_id } => writeln!(
                    self.stdout,
                    "event={cursor} session={session_id} tool_batch_transition \
                     turn={turn_id} call={model_call_id} state=recovery_required \
                     tool_attempt={tool_attempt_id}"
                ),
            },
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
            SessionEvent::TurnCancelled {
                turn_id,
                cancellation_entry_id,
                terminal_frontier_id,
            } => writeln!(
                self.stdout,
                "event={cursor} session={session_id} turn_cancelled turn={turn_id} \
                 entry={cancellation_entry_id} frontier={terminal_frontier_id}"
            ),
            SessionEvent::TurnReconciliationRequired {
                turn_id,
                model_call_id,
                terminal_frontier_id,
            } => writeln!(
                self.stdout,
                "event={cursor} session={session_id} turn_reconciliation_required \
                 turn={turn_id} operation=model_call operation_id={model_call_id} \
                 frontier={terminal_frontier_id}"
            ),
            SessionEvent::TurnToolReconciliationRequired {
                turn_id,
                tool_attempt_id,
                terminal_frontier_id,
            } => writeln!(
                self.stdout,
                "event={cursor} session={session_id} turn_tool_reconciliation_required \
                 turn={turn_id} operation=tool_attempt operation_id={tool_attempt_id} \
                 frontier={terminal_frontier_id}"
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
            TurnState::ActiveAwaitingToolApproval { tool_request_id } => writeln!(
                self.stdout,
                "turn={turn_id} position={position} state=active_awaiting_tool_approval \
                 request={tool_request_id}"
            ),
            TurnState::ActiveAwaitingToolRecovery {
                ended_attempt_id,
                recovery_tool_attempt_id,
            } => writeln!(
                self.stdout,
                "turn={turn_id} position={position} state=active_awaiting_tool_recovery \
                 attempt={ended_attempt_id} tool_attempt={recovery_tool_attempt_id}"
            ),
            TurnState::Failed {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call,
            } => match (terminal_attempt_id, terminal_model_call) {
                (None, None) => writeln!(
                    self.stdout,
                    "turn={turn_id} position={position} state=failed \
                     frontier={terminal_frontier_id} attempt=none call=none"
                ),
                (Some(attempt_id), None) => writeln!(
                    self.stdout,
                    "turn={turn_id} position={position} state=failed \
                     frontier={terminal_frontier_id} attempt={attempt_id} call=none"
                ),
                (Some(attempt_id), Some(call)) => writeln!(
                    self.stdout,
                    "turn={turn_id} position={position} state=failed \
                     frontier={terminal_frontier_id} attempt={attempt_id} call={} \
                     call_disposition={}",
                    call.model_call_id(),
                    failed_model_call_disposition(call.disposition())
                ),
                (None, Some(_)) => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "failed turn carried terminal call evidence without an attempt",
                )),
            },
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
            TurnState::Cancelled {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            } => match terminal_model_call_id {
                Some(model_call_id) => writeln!(
                    self.stdout,
                    "turn={turn_id} position={position} state=cancelled \
                     frontier={terminal_frontier_id} attempt={terminal_attempt_id} \
                     call={model_call_id}"
                ),
                None => writeln!(
                    self.stdout,
                    "turn={turn_id} position={position} state=cancelled \
                     frontier={terminal_frontier_id} attempt={terminal_attempt_id} call=none"
                ),
            },
            TurnState::ReconciliationRequired {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            } => writeln!(
                self.stdout,
                "turn={turn_id} position={position} state=reconciliation_required \
                 frontier={terminal_frontier_id} attempt={terminal_attempt_id} \
                 operation=model_call operation_id={terminal_model_call_id}"
            ),
            TurnState::ToolReconciliationRequired {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_tool_attempt_id,
            } => writeln!(
                self.stdout,
                "turn={turn_id} position={position} state=tool_reconciliation_required \
                 frontier={terminal_frontier_id} attempt={terminal_attempt_id} \
                 operation=tool_attempt operation_id={terminal_tool_attempt_id}"
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
            SnapshotEntryKind::Marker(TranscriptEntry::TurnCancelled { turn_id }) => {
                writeln!(
                    self.stdout,
                    "turn_cancelled turn={turn_id} source={} entry={}",
                    entry.source_session_id, entry.entry_id
                )
            }
            SnapshotEntryKind::Marker(TranscriptEntry::AssistantToolUse {
                turn_id,
                model_call_id,
                tool_request_id,
                tool_name,
                arguments,
            }) => writeln!(
                self.stdout,
                "assistant_tool_use turn={turn_id} call={model_call_id} \
                 request={tool_request_id} name={} arguments={} source={} entry={}",
                self.render(tool_name),
                self.render(arguments),
                entry.source_session_id,
                entry.entry_id
            ),
            SnapshotEntryKind::Marker(TranscriptEntry::ToolExecutionResult {
                tool_request_id,
                tool_attempt_id,
                content,
            }) => writeln!(
                self.stdout,
                "tool_execution_result request={tool_request_id} attempt={tool_attempt_id} \
                 content={} source={} entry={}",
                self.render(content),
                entry.source_session_id,
                entry.entry_id
            ),
            SnapshotEntryKind::Marker(TranscriptEntry::ToolDenied {
                tool_request_id,
                content,
            }) => writeln!(
                self.stdout,
                "tool_denied request={tool_request_id} content={} source={} entry={}",
                self.render(content),
                entry.source_session_id,
                entry.entry_id
            ),
            SnapshotEntryKind::Marker(TranscriptEntry::ToolClosed {
                tool_request_id,
                content,
            }) => writeln!(
                self.stdout,
                "tool_closed request={tool_request_id} content={} source={} entry={}",
                self.render(content),
                entry.source_session_id,
                entry.entry_id
            ),
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
    fn context(
        self,
        snapshot: &mut TranscriptSnapshot,
    ) -> Result<SnapshotSelectionContext, ClientError> {
        if matches!(self, Self::All) {
            return Ok(SnapshotSelectionContext::default());
        }
        let mut proposals = HashSet::new();
        let mut results = HashSet::new();
        let mut trailing_results = HashSet::new();
        let mut terminal_results = HashSet::new();
        let mut reconciliation_call = None;
        let mut reconciliation_proposals = HashSet::new();
        let mut anchor_found = false;
        for record in snapshot.replay()? {
            let record = record?;
            if let SnapshotRecord::Turn(turn) = &record {
                if matches!(
                    (self, &turn.state),
                    (
                        Self::ToolReconciliation {
                            turn_id,
                            tool_attempt_id,
                            terminal_frontier_id,
                        },
                        TurnState::ToolReconciliationRequired {
                            terminal_frontier_id: stored_frontier,
                            terminal_tool_attempt_id: stored_attempt,
                            ..
                        },
                    ) if turn_id == turn.turn_id
                        && tool_attempt_id == *stored_attempt
                        && terminal_frontier_id == *stored_frontier
                ) {
                    anchor_found = true;
                }
                continue;
            }
            let SnapshotRecord::Entry(entry) = record else {
                continue;
            };
            match &entry.kind {
                SnapshotEntryKind::Marker(TranscriptEntry::AssistantToolUse {
                    turn_id,
                    model_call_id,
                    tool_request_id,
                    ..
                }) => {
                    trailing_results.clear();
                    if self.matches_tool_batch(*turn_id, *model_call_id) {
                        proposals.insert(*tool_request_id);
                        if matches!(self, Self::ToolBatchProposed { .. }) {
                            anchor_found = true;
                        }
                    }
                    if matches!(
                        self,
                        Self::ToolReconciliation {
                            turn_id: selected_turn,
                            ..
                        } if selected_turn == *turn_id
                    ) {
                        if reconciliation_call != Some(*model_call_id) {
                            reconciliation_call = Some(*model_call_id);
                            reconciliation_proposals.clear();
                        }
                        reconciliation_proposals.insert(*tool_request_id);
                    }
                }
                SnapshotEntryKind::Marker(
                    TranscriptEntry::ToolExecutionResult {
                        tool_request_id, ..
                    }
                    | TranscriptEntry::ToolDenied {
                        tool_request_id, ..
                    }
                    | TranscriptEntry::ToolClosed {
                        tool_request_id, ..
                    },
                ) => {
                    results.insert(*tool_request_id);
                    trailing_results.insert(*tool_request_id);
                }
                _ if self.includes_terminal_marker(&entry) => {
                    anchor_found = true;
                    terminal_results.clone_from(&trailing_results);
                    trailing_results.clear();
                }
                _ => trailing_results.clear(),
            }
        }
        match self {
            Self::ToolBatchResults { .. } => {
                if proposals.is_empty()
                    || proposals.iter().any(|request| !results.contains(request))
                {
                    return Err(ClientError::Protocol(
                        "tool-result reread omitted the event's exact proposal or result set",
                    ));
                }
                Ok(SnapshotSelectionContext {
                    requests: proposals,
                })
            }
            Self::ToolBatchProposed { .. } if anchor_found => {
                Ok(SnapshotSelectionContext::default())
            }
            Self::Completed { .. } | Self::Failed { .. } | Self::Cancelled { .. }
                if anchor_found =>
            {
                Ok(SnapshotSelectionContext {
                    requests: terminal_results,
                })
            }
            Self::ToolReconciliation { .. }
                if anchor_found
                    && !reconciliation_proposals.is_empty()
                    && reconciliation_proposals
                        .iter()
                        .all(|request| results.contains(request)) =>
            {
                Ok(SnapshotSelectionContext {
                    requests: reconciliation_proposals,
                })
            }
            Self::ToolBatchProposed { .. } => Err(ClientError::Protocol(
                "tool-proposal reread omitted the event's exact proposal",
            )),
            Self::Completed { .. } | Self::Failed { .. } | Self::Cancelled { .. } => Err(
                ClientError::Protocol("terminal reread omitted the event's exact marker"),
            ),
            Self::ToolReconciliation { .. } => Err(ClientError::Protocol(
                "tool reconciliation reread omitted its exact terminal result suffix",
            )),
            Self::All => unreachable!("all entries need no selection context"),
        }
    }

    fn includes(self, entry: &SnapshotEntry, context: &SnapshotSelectionContext) -> bool {
        match (self, &entry.kind) {
            (Self::All, _) => true,
            (
                Self::Completed {
                    turn_id,
                    model_call_id,
                    ..
                }
                | Self::ToolBatchProposed {
                    turn_id,
                    model_call_id,
                },
                SnapshotEntryKind::Text(TranscriptTextEntry::Assistant {
                    turn_id: entry_turn,
                    model_call_id: entry_call,
                }),
            ) => turn_id == *entry_turn && model_call_id == *entry_call,
            (
                Self::ToolBatchProposed {
                    turn_id,
                    model_call_id,
                },
                SnapshotEntryKind::Marker(TranscriptEntry::AssistantToolUse {
                    turn_id: entry_turn,
                    model_call_id: entry_call,
                    ..
                }),
            ) => turn_id == *entry_turn && model_call_id == *entry_call,
            (
                Self::ToolBatchResults { .. }
                | Self::ToolReconciliation { .. }
                | Self::Failed { .. }
                | Self::Cancelled { .. },
                SnapshotEntryKind::Marker(
                    TranscriptEntry::ToolExecutionResult {
                        tool_request_id, ..
                    }
                    | TranscriptEntry::ToolDenied {
                        tool_request_id, ..
                    }
                    | TranscriptEntry::ToolClosed {
                        tool_request_id, ..
                    },
                ),
            ) => context.requests.contains(tool_request_id),
            (
                Self::Completed { .. } | Self::Failed { .. } | Self::Cancelled { .. },
                SnapshotEntryKind::Marker(_),
            ) => self.includes_terminal_marker(entry),
            (
                Self::Completed { .. }
                | Self::Failed { .. }
                | Self::Cancelled { .. }
                | Self::ToolBatchProposed { .. }
                | Self::ToolBatchResults { .. }
                | Self::ToolReconciliation { .. },
                SnapshotEntryKind::Text(_),
            ) => false,
            (
                Self::ToolBatchProposed { .. }
                | Self::ToolBatchResults { .. }
                | Self::ToolReconciliation { .. },
                SnapshotEntryKind::Marker(_),
            ) => false,
        }
    }

    fn matches_tool_batch(self, entry_turn: CanonicalUuid, entry_call: CanonicalUuid) -> bool {
        matches!(
            self,
            Self::ToolBatchProposed {
                turn_id,
                model_call_id,
            } | Self::ToolBatchResults {
                turn_id,
                model_call_id,
            } if turn_id == entry_turn && model_call_id == entry_call
        )
    }

    fn includes_terminal_marker(self, entry: &SnapshotEntry) -> bool {
        match (self, &entry.kind) {
            (
                Self::Completed {
                    turn_id,
                    terminal_entry_id,
                    ..
                },
                SnapshotEntryKind::Marker(TranscriptEntry::TurnCompleted {
                    turn_id: entry_turn,
                }),
            )
            | (
                Self::Failed {
                    turn_id,
                    terminal_entry_id,
                },
                SnapshotEntryKind::Marker(TranscriptEntry::TurnFailed {
                    turn_id: entry_turn,
                }),
            )
            | (
                Self::Cancelled {
                    turn_id,
                    terminal_entry_id,
                },
                SnapshotEntryKind::Marker(TranscriptEntry::TurnCancelled {
                    turn_id: entry_turn,
                }),
            ) => turn_id == *entry_turn && terminal_entry_id == entry.entry_id,
            (
                Self::All
                | Self::Completed { .. }
                | Self::Failed { .. }
                | Self::Cancelled { .. }
                | Self::ToolBatchProposed { .. }
                | Self::ToolBatchResults { .. }
                | Self::ToolReconciliation { .. },
                SnapshotEntryKind::Text(_)
                | SnapshotEntryKind::Marker(
                    TranscriptEntry::AssistantToolUse { .. }
                    | TranscriptEntry::ToolExecutionResult { .. }
                    | TranscriptEntry::ToolDenied { .. }
                    | TranscriptEntry::ToolClosed { .. }
                    | TranscriptEntry::TurnCompleted { .. }
                    | TranscriptEntry::TurnFailed { .. }
                    | TranscriptEntry::TurnCancelled { .. },
                ),
            ) => false,
        }
    }
}

fn model_call_state(state: ModelCallState) -> &'static str {
    match state {
        ModelCallState::Prepared {} => "prepared",
        ModelCallState::InFlight {} => "in_flight",
        ModelCallState::CancellationRequested {} => "cancellation_requested",
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
        CurrentModelCallState::CancellationRequested {} => "cancellation_requested",
    }
}

const fn failed_model_call_disposition(disposition: FailedModelCallDisposition) -> &'static str {
    match disposition {
        FailedModelCallDisposition::KnownFailed => "known_failed",
        FailedModelCallDisposition::Cancelled => "cancelled",
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

    use expect_test::expect;
    use signalbox_process_protocol::{
        CanonicalU64, CanonicalUuid, ContentFragment, CurrentModelCall, CurrentModelCallState,
        FailedModelCallDisposition, FailedTerminalModelCall, InputContent, ModelCallState,
        ServerMessage, SessionEvent, TranscriptEntry, TranscriptTextEntry, TurnState,
    };
    use uuid::Uuid;

    use super::{Output, SnapshotSelection, control_safe};
    use crate::{
        error::ClientError,
        transcript::{SnapshotIdentitySet, TranscriptSnapshot},
    };

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
                    terminal_entry_id: wire_uuid(12),
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

    #[test]
    fn tool_reconciliation_reread_uses_its_terminal_turn_batch() {
        let selected_turn = wire_uuid(1);
        let selected_call = wire_uuid(2);
        let selected_request = wire_uuid(3);
        let selected_attempt = wire_uuid(4);
        let selected_frontier = wire_uuid(5);
        let later_turn = wire_uuid(6);
        let later_call = wire_uuid(7);
        let later_request = wire_uuid(8);
        let mut snapshot = TranscriptSnapshot::from_messages(
            12,
            [
                ServerMessage::TranscriptTurn {
                    turn_id: selected_turn,
                    acceptance_position: CanonicalU64::new(1),
                    state: TurnState::ToolReconciliationRequired {
                        terminal_frontier_id: selected_frontier,
                        terminal_attempt_id: wire_uuid(9),
                        terminal_tool_attempt_id: selected_attempt,
                    },
                },
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(0),
                    source_session_id: wire_uuid(10),
                    entry_id: wire_uuid(11),
                    entry: TranscriptEntry::AssistantToolUse {
                        turn_id: selected_turn,
                        model_call_id: selected_call,
                        tool_request_id: selected_request,
                        tool_name: String::from("selected"),
                        arguments: String::from("{}"),
                    },
                },
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(1),
                    source_session_id: wire_uuid(10),
                    entry_id: wire_uuid(12),
                    entry: TranscriptEntry::ToolClosed {
                        tool_request_id: selected_request,
                        content: String::from("selected result"),
                    },
                },
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(2),
                    source_session_id: wire_uuid(10),
                    entry_id: wire_uuid(13),
                    entry: TranscriptEntry::AssistantToolUse {
                        turn_id: later_turn,
                        model_call_id: later_call,
                        tool_request_id: later_request,
                        tool_name: String::from("later"),
                        arguments: String::from("{}"),
                    },
                },
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(3),
                    source_session_id: wire_uuid(10),
                    entry_id: wire_uuid(14),
                    entry: TranscriptEntry::ToolClosed {
                        tool_request_id: later_request,
                        content: String::from("later result"),
                    },
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
                SnapshotSelection::ToolReconciliation {
                    turn_id: selected_turn,
                    tool_attempt_id: selected_attempt,
                    terminal_frontier_id: selected_frontier,
                },
            )
            .expect("the exact terminal tool batch renders");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        assert!(rendered.contains("selected result"));
        assert!(!rendered.contains("later result"));
        assert!(stderr.is_empty());
    }

    #[test]
    fn terminal_reread_rejects_a_missing_exact_marker_before_output() {
        let selected_turn = wire_uuid(1);
        let selected_call = wire_uuid(2);
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
                    content_fragment: ContentFragment::try_new("untrusted reply".to_owned())
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
            ],
        )
        .expect("test snapshot must spool");
        let mut displayed = SnapshotIdentitySet::new().expect("identity spool must open");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let error = Output::new(&mut stdout, &mut stderr, false)
            .terminal_material(
                &mut snapshot,
                &mut displayed,
                SnapshotSelection::Completed {
                    turn_id: selected_turn,
                    model_call_id: selected_call,
                    terminal_entry_id: wire_uuid(13),
                },
            )
            .expect_err("a side reread without the event marker must fail closed");

        assert!(matches!(
            error,
            ClientError::Protocol("terminal reread omitted the event's exact marker")
        ));
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
    }

    #[test]
    fn failed_terminal_reread_rejects_a_different_marker_identity() {
        let selected_turn = wire_uuid(1);
        let mut snapshot = TranscriptSnapshot::from_messages(
            12,
            [ServerMessage::TranscriptEntry {
                entry_index: CanonicalU64::new(0),
                source_session_id: wire_uuid(10),
                entry_id: wire_uuid(11),
                entry: TranscriptEntry::TurnFailed {
                    turn_id: selected_turn,
                },
            }],
        )
        .expect("test snapshot must spool");
        let mut displayed = SnapshotIdentitySet::new().expect("identity spool must open");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let error = Output::new(&mut stdout, &mut stderr, false)
            .terminal_material(
                &mut snapshot,
                &mut displayed,
                SnapshotSelection::Failed {
                    turn_id: selected_turn,
                    terminal_entry_id: wire_uuid(12),
                },
            )
            .expect_err("a failed reread must require the event marker");

        assert!(matches!(
            error,
            ClientError::Protocol("terminal reread omitted the event's exact marker")
        ));
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
    }

    #[test]
    fn snapshot_renders_cancellation_requested_call() {
        let rendered = render_snapshot_turn(TurnState::ActiveRunning {
            current_attempt_id: wire_uuid(2),
            current_model_call: Some(CurrentModelCall::new(
                wire_uuid(3),
                CurrentModelCallState::CancellationRequested {},
            )),
        });

        expect![[r#"
            turn=00000000-0000-0000-0000-000000000001 position=1 state=active_running attempt=00000000-0000-0000-0000-000000000002 call=00000000-0000-0000-0000-000000000003 call_state=cancellation_requested
        "#]]
        .assert_eq(&rendered);
    }

    #[test]
    fn snapshot_renders_failed_call_evidence() {
        let rendered = render_snapshot_turn(TurnState::Failed {
            terminal_frontier_id: wire_uuid(2),
            terminal_attempt_id: Some(wire_uuid(3)),
            terminal_model_call: Some(FailedTerminalModelCall::new(
                wire_uuid(4),
                FailedModelCallDisposition::Cancelled,
            )),
        });

        expect![[r#"
            turn=00000000-0000-0000-0000-000000000001 position=1 state=failed frontier=00000000-0000-0000-0000-000000000002 attempt=00000000-0000-0000-0000-000000000003 call=00000000-0000-0000-0000-000000000004 call_disposition=cancelled
        "#]]
        .assert_eq(&rendered);
    }

    #[test]
    fn snapshot_renders_cancelled_turn() {
        let rendered = render_snapshot_turn(TurnState::Cancelled {
            terminal_frontier_id: wire_uuid(2),
            terminal_attempt_id: wire_uuid(3),
            terminal_model_call_id: None,
        });

        expect![[r#"
            turn=00000000-0000-0000-0000-000000000001 position=1 state=cancelled frontier=00000000-0000-0000-0000-000000000002 attempt=00000000-0000-0000-0000-000000000003 call=none
        "#]]
        .assert_eq(&rendered);
    }

    #[test]
    fn snapshot_renders_reconciliation_required_turn() {
        let rendered = render_snapshot_turn(TurnState::ReconciliationRequired {
            terminal_frontier_id: wire_uuid(2),
            terminal_attempt_id: wire_uuid(3),
            terminal_model_call_id: wire_uuid(4),
        });

        expect![[r#"
            turn=00000000-0000-0000-0000-000000000001 position=1 state=reconciliation_required frontier=00000000-0000-0000-0000-000000000002 attempt=00000000-0000-0000-0000-000000000003 operation=model_call operation_id=00000000-0000-0000-0000-000000000004
        "#]]
        .assert_eq(&rendered);
    }

    #[test]
    fn follow_event_renders_cancellation_requested_call() {
        let rendered = render_event(SessionEvent::ModelCallTransition {
            turn_id: wire_uuid(2),
            model_call_id: wire_uuid(3),
            state: ModelCallState::CancellationRequested {},
        });

        expect![[r#"
            event=1 session=00000000-0000-0000-0000-000000000001 model_call_transition turn=00000000-0000-0000-0000-000000000002 call=00000000-0000-0000-0000-000000000003 state=cancellation_requested
        "#]]
        .assert_eq(&rendered);
    }

    #[test]
    fn follow_event_renders_cancelled_turn() {
        let rendered = render_event(SessionEvent::TurnCancelled {
            turn_id: wire_uuid(2),
            cancellation_entry_id: wire_uuid(3),
            terminal_frontier_id: wire_uuid(4),
        });

        expect![[r#"
            event=1 session=00000000-0000-0000-0000-000000000001 turn_cancelled turn=00000000-0000-0000-0000-000000000002 entry=00000000-0000-0000-0000-000000000003 frontier=00000000-0000-0000-0000-000000000004
        "#]]
        .assert_eq(&rendered);
    }

    #[test]
    fn follow_event_renders_reconciliation_required_turn() {
        let rendered = render_event(SessionEvent::TurnReconciliationRequired {
            turn_id: wire_uuid(2),
            model_call_id: wire_uuid(3),
            terminal_frontier_id: wire_uuid(4),
        });

        expect![[r#"
            event=1 session=00000000-0000-0000-0000-000000000001 turn_reconciliation_required turn=00000000-0000-0000-0000-000000000002 operation=model_call operation_id=00000000-0000-0000-0000-000000000003 frontier=00000000-0000-0000-0000-000000000004
        "#]]
        .assert_eq(&rendered);
    }

    #[test]
    fn cancelled_terminal_reread_selects_only_its_exact_marker() {
        let selected_turn = wire_uuid(1);
        let later_turn = wire_uuid(2);
        let mut snapshot = TranscriptSnapshot::from_messages(
            12,
            [
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(0),
                    source_session_id: wire_uuid(10),
                    entry_id: wire_uuid(11),
                    entry: TranscriptEntry::TurnCancelled {
                        turn_id: selected_turn,
                    },
                },
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(1),
                    source_session_id: wire_uuid(10),
                    entry_id: wire_uuid(12),
                    entry: TranscriptEntry::TurnCancelled {
                        turn_id: later_turn,
                    },
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
                SnapshotSelection::Cancelled {
                    turn_id: selected_turn,
                    terminal_entry_id: wire_uuid(11),
                },
            )
            .expect("selected cancellation marker must render");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        assert!(rendered.contains(&selected_turn.to_string()));
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

    #[track_caller]
    fn render_snapshot_turn(state: TurnState) -> String {
        let mut snapshot = TranscriptSnapshot::from_messages(
            1,
            [ServerMessage::TranscriptTurn {
                turn_id: wire_uuid(1),
                acceptance_position: CanonicalU64::new(1),
                state,
            }],
        )
        .expect("test snapshot must spool");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .snapshot(&mut snapshot)
            .expect("snapshot turn must render");
        assert!(stderr.is_empty());
        String::from_utf8(stdout).expect("rendered output is UTF-8")
    }

    #[track_caller]
    fn render_event(event: SessionEvent) -> String {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .event(1, wire_uuid(1), &event)
            .expect("event must render");
        assert!(stderr.is_empty());
        String::from_utf8(stdout).expect("rendered output is UTF-8")
    }

    fn wire_uuid(value: u128) -> CanonicalUuid {
        CanonicalUuid::from_uuid(Uuid::from_u128(value))
    }
}
