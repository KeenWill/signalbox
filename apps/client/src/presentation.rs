use std::{
    collections::HashSet,
    io::{self, Write},
    path::Path,
};

use signalbox_process_protocol::{
    CanonicalUuid, CurrentModelCallState, FailedModelCallDisposition, ImportedContentKind,
    ImportedSourceSpeaker, ImportedSpeaker, MetadataActor, MetadataLastWriter,
    ModelCallDisposition, ModelCallState, ReviewDiffSide, ReviewFindingSnapshot,
    ReviewFindingStatus, ReviewPassKind, ReviewPassLifecycle, ReviewRunLifecycle,
    ReviewRunSnapshot, ReviewSeverity, ReviewTargetSnapshot, ReviewTargetSubject, ReviewWorkflow,
    SessionEvent, ToolBatchState, ToolDecision, TranscriptEntry, TranscriptTextEntry, TurnState,
};

use crate::{
    ImportScanSummary,
    error::ClientError,
    transcript::{
        SnapshotEntry, SnapshotEntryKind, SnapshotIdentitySet, SnapshotRecord, TranscriptSnapshot,
        TranscriptTurn,
    },
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ReportedTokenTotal {
    tokens: u128,
    reported_calls: u64,
}

impl ReportedTokenTotal {
    fn add(
        &mut self,
        value: Option<signalbox_process_protocol::CanonicalU64>,
    ) -> Result<(), ClientError> {
        let Some(value) = value else {
            return Ok(());
        };
        self.tokens = self
            .tokens
            .checked_add(u128::from(value.value()))
            .ok_or(ClientError::Protocol("token usage total overflowed"))?;
        self.reported_calls = self
            .reported_calls
            .checked_add(1)
            .ok_or(ClientError::Protocol("token usage coverage overflowed"))?;
        Ok(())
    }

    fn label(self) -> String {
        if self.reported_calls == 0 {
            String::from("unreported")
        } else {
            self.tokens.to_string()
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TokenUsageTotal {
    terminal_calls: u64,
    input: ReportedTokenTotal,
    output: ReportedTokenTotal,
    cache_creation_input: ReportedTokenTotal,
    cache_read_input: ReportedTokenTotal,
}

impl TokenUsageTotal {
    fn add(
        &mut self,
        usage: signalbox_process_protocol::ModelCallTokenUsage,
    ) -> Result<(), ClientError> {
        self.terminal_calls = self
            .terminal_calls
            .checked_add(1)
            .ok_or(ClientError::Protocol(
                "terminal model-call count overflowed",
            ))?;
        self.input.add(usage.input_tokens)?;
        self.output.add(usage.output_tokens)?;
        self.cache_creation_input
            .add(usage.cache_creation_input_tokens)?;
        self.cache_read_input.add(usage.cache_read_input_tokens)
    }
}

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

/// One complete metadata summary as the search verb presents it.
pub(crate) struct SessionMetadataRow<'a> {
    pub(crate) session_id: CanonicalUuid,
    pub(crate) defaults_version: u64,
    pub(crate) selection: &'a str,
    pub(crate) dangerous_tool_auto_approval: bool,
    pub(crate) archived: bool,
    pub(crate) last_writer: Option<MetadataLastWriter>,
    pub(crate) tags: &'a [String],
    pub(crate) title: Option<&'a str>,
}

/// What one process-derived text field may carry unescaped, given where it
/// sits in the output that carries it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TextField {
    /// Flowing text that owns the lines it is written to, so U+000A is its
    /// content rather than a delimiter.
    Flowing,
    /// The last named value on its line: a line feed inside it would forge a
    /// following line, and nothing else delimits it.
    TrailingOnLine,
    /// A value delimited within its line: the space that ends its field and
    /// the comma that separates it from a sibling are escaped too, so the
    /// field states its exact values.
    DelimitedOnLine,
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
        writeln!(self.stderr, "{name}={value}")?;
        self.stderr.flush()
    }

    pub(crate) fn error(&mut self, error: &ClientError) -> io::Result<()> {
        let message = format!("error: {error}");
        self.stderr.write_all(self.render(&message).as_bytes())?;
        self.stderr.write_all(b"\n")
    }

    pub(crate) fn session_created(&mut self, session_id: CanonicalUuid) -> io::Result<()> {
        writeln!(self.stdout, "{session_id}")
    }

    pub(crate) fn session_defaults_replaced(
        &mut self,
        session_id: CanonicalUuid,
        defaults_version: u64,
        model_selection: &str,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "session={session_id} defaults_version={defaults_version} {model_selection}"
        )
    }

    pub(crate) fn conversation_import_inserted(
        &mut self,
        imported_conversation_id: CanonicalUuid,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "inserted imported_conversation_id={imported_conversation_id}"
        )
    }

    pub(crate) fn conversation_import_already_imported(
        &mut self,
        imported_conversation_id: CanonicalUuid,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "already_imported imported_conversation_id={imported_conversation_id}"
        )
    }

    pub(crate) fn conversation_import_scan_inserted(
        &mut self,
        path: &Path,
        imported_conversation_id: CanonicalUuid,
    ) -> io::Result<()> {
        let path = self.render(&format!("{path:?}"));
        writeln!(
            self.stdout,
            "imported path={path} imported_conversation_id={imported_conversation_id}"
        )
    }

    pub(crate) fn conversation_import_scan_already_imported(
        &mut self,
        path: &Path,
        imported_conversation_id: CanonicalUuid,
    ) -> io::Result<()> {
        let path = self.render(&format!("{path:?}"));
        writeln!(
            self.stdout,
            "already_imported path={path} imported_conversation_id={imported_conversation_id}"
        )
    }

    pub(crate) fn conversation_import_scan_skipped(
        &mut self,
        path: &Path,
        error: &ClientError,
    ) -> io::Result<()> {
        let path = self.render(&format!("{path:?}"));
        let reason = self.render_field(&error.to_string(), TextField::TrailingOnLine);
        writeln!(self.stdout, "skipped path={path} reason={reason}")
    }

    pub(crate) fn conversation_import_scan_summary(
        &mut self,
        summary: &ImportScanSummary,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "scan_summary imported={} already_imported={} skipped={}",
            summary.imported, summary.already_imported, summary.skipped
        )
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

    pub(crate) fn review_acknowledgement(&mut self, line: &str) -> io::Result<()> {
        self.stdout.write_all(self.render(line).as_bytes())?;
        self.stdout.write_all(b"\n")
    }

    pub(crate) fn review_target(&mut self, target: &ReviewTargetSnapshot) -> io::Result<()> {
        let subject = match target.subject {
            ReviewTargetSubject::ChangeRequest { number } => {
                format!("change_request:{}", number.value())
            }
            ReviewTargetSubject::Commit {} => String::from("commit"),
        };
        writeln!(
            self.stdout,
            "target={} subject={} parent={}",
            target.target_id,
            subject,
            target
                .stack_parent_target_id
                .map_or_else(|| String::from("-"), |id| id.to_string()),
        )?;
        self.review_text_field("provider", &target.provider)?;
        self.review_text_field("repository", &target.repository)?;
        self.review_text_field("head_revision", &target.head_revision)?;
        match target.base_revision.as_deref() {
            Some(base_revision) => {
                writeln!(self.stdout, "base_revision_present=true")?;
                self.review_text_field("base_revision", base_revision)
            }
            None => writeln!(self.stdout, "base_revision_present=false"),
        }
    }

    pub(crate) fn review_run(
        &mut self,
        run: &ReviewRunSnapshot,
        pass: Option<&signalbox_process_protocol::ReviewPassSnapshot>,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "run={} target={} workflow={} policy_version={} minimum_judge_confidence={} \
             minimum_publication_confidence={} state={} pass={}",
            run.run_id,
            run.target_id,
            review_workflow_label(run.workflow),
            run.policy_version.value(),
            run.minimum_judge_confidence.value(),
            run.minimum_publication_confidence.value(),
            review_run_state_label(run.state),
            run.pass_id
                .map_or_else(|| String::from("-"), |id| id.to_string()),
        )?;
        if let Some(pass) = pass {
            writeln!(
                self.stdout,
                "pass={} kind={} state={} session={} input={} origin_turn={} turn={} frontier={}",
                pass.pass_id,
                review_pass_kind_label(pass.kind),
                review_pass_state_label(pass.state),
                pass.session_id,
                pass.accepted_input_id,
                pass.origin_turn_id,
                pass.turn_id
                    .map_or_else(|| String::from("-"), |id| id.to_string()),
                pass.output_frontier_id
                    .map_or_else(|| String::from("-"), |id| id.to_string()),
            )?;
        }
        Ok(())
    }

    pub(crate) fn review_finding(&mut self, finding: &ReviewFindingSnapshot) -> io::Result<()> {
        writeln!(
            self.stdout,
            "finding={} target={} run={} pass={} status={} events={} line_start={} line_end={} \
             diff_side={} severity={} confidence={}",
            finding.finding.finding_id,
            finding.target_id,
            finding.run_id,
            finding.producing_pass_id,
            review_finding_status_label(finding.status),
            finding.event_count.value(),
            finding
                .finding
                .line_start
                .map_or_else(|| String::from("none"), |line| line.value().to_string()),
            finding
                .finding
                .line_end
                .map_or_else(|| String::from("none"), |line| line.value().to_string()),
            finding
                .finding
                .diff_side
                .map_or("none", review_diff_side_label),
            review_severity_label(finding.finding.severity),
            finding.finding.confidence.value(),
        )?;
        self.review_text_field("file_path", &finding.finding.file_path)?;
        self.review_text_field("title", &finding.finding.title)?;
        self.review_text_field("body", &finding.finding.body)?;
        self.review_text_field("category", &finding.finding.category)?;
        match finding.finding.recommended_fix.as_deref() {
            Some(recommended_fix) => {
                writeln!(self.stdout, "recommended_fix_present=true")?;
                self.review_text_field("recommended_fix", recommended_fix)
            }
            None => writeln!(self.stdout, "recommended_fix_present=false"),
        }
    }

    fn review_text_field(&mut self, name: &str, value: &str) -> io::Result<()> {
        write!(self.stdout, "{name}=")?;
        self.stdout.write_all(
            self.render_field(value, TextField::TrailingOnLine)
                .as_bytes(),
        )?;
        self.stdout.write_all(b"\n")
    }

    pub(crate) fn tool_request_decided(
        &mut self,
        tool_request_id: CanonicalUuid,
        decision: &ToolDecision,
    ) -> io::Result<()> {
        let decision = match decision {
            ToolDecision::Approve {} => "approve",
            ToolDecision::Deny { .. } => "deny",
        };
        writeln!(
            self.stdout,
            "tool_request={tool_request_id} decision={decision}"
        )
    }

    pub(crate) fn session_metadata_summary(
        &mut self,
        row: &SessionMetadataRow<'_>,
    ) -> io::Result<()> {
        let tags = row
            .tags
            .iter()
            .map(|tag| self.render_field(tag, TextField::DelimitedOnLine))
            .collect::<Vec<_>>()
            .join(",");
        let title = row
            .title
            .map(|title| self.render_field(title, TextField::TrailingOnLine));
        writeln!(
            self.stdout,
            "{} archived={} defaults_version={} {} dangerous_tool_auto_approval={} \
             last_writer={} updated_at_unix_micros={} tags={tags} title={}",
            row.session_id,
            row.archived,
            row.defaults_version,
            row.selection,
            dangerous_tool_auto_approval_label(row.dangerous_tool_auto_approval),
            last_writer_actor_label(row.last_writer),
            last_writer_micros_label(row.last_writer),
            title.unwrap_or_default()
        )
    }

    pub(crate) fn next_page_cursor(
        &mut self,
        next_after_session_id: CanonicalUuid,
    ) -> io::Result<()> {
        writeln!(self.stderr, "next_after_session_id={next_after_session_id}")?;
        self.stderr.flush()
    }

    pub(crate) fn snapshot(
        &mut self,
        snapshot: &mut TranscriptSnapshot,
    ) -> Result<(), ClientError> {
        self.render_snapshot(snapshot, None, SnapshotSelection::All, true)?;
        self.render_usage(snapshot)
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
                SnapshotRecord::ModelCallUsage(_) => {}
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

    fn render_usage(&mut self, snapshot: &mut TranscriptSnapshot) -> Result<(), ClientError> {
        let mut current_turn: Option<(CanonicalUuid, TokenUsageTotal)> = None;
        let mut session_total = TokenUsageTotal::default();
        for record in snapshot.replay()? {
            let SnapshotRecord::ModelCallUsage(evidence) = record? else {
                continue;
            };
            if current_turn
                .as_ref()
                .is_some_and(|(turn, _)| *turn != evidence.turn_id)
            {
                let (turn, total) = current_turn.take().ok_or(ClientError::Protocol(
                    "token usage turn grouping was invalid",
                ))?;
                self.usage_line(Some(turn), total)?;
            }
            let (_, turn_total) =
                current_turn.get_or_insert((evidence.turn_id, TokenUsageTotal::default()));
            turn_total.add(evidence.usage)?;
            session_total.add(evidence.usage)?;
        }
        if let Some((turn, total)) = current_turn {
            self.usage_line(Some(turn), total)?;
        }
        self.usage_line(None, session_total)?;
        Ok(())
    }

    fn usage_line(
        &mut self,
        turn: Option<CanonicalUuid>,
        total: TokenUsageTotal,
    ) -> io::Result<()> {
        let prefix = turn.map_or_else(
            || String::from("usage_total scope=session"),
            |turn| format!("usage turn={turn}"),
        );
        writeln!(
            self.stdout,
            "{prefix} terminal_calls={} input_tokens={} input_tokens_reported_calls={}/{} \
             output_tokens={} output_tokens_reported_calls={}/{} \
             cache_creation_input_tokens={} \
             cache_creation_input_tokens_reported_calls={}/{} cache_read_input_tokens={} \
             cache_read_input_tokens_reported_calls={}/{}",
            total.terminal_calls,
            total.input.label(),
            total.input.reported_calls,
            total.terminal_calls,
            total.output.label(),
            total.output.reported_calls,
            total.terminal_calls,
            total.cache_creation_input.label(),
            total.cache_creation_input.reported_calls,
            total.terminal_calls,
            total.cache_read_input.label(),
            total.cache_read_input.reported_calls,
            total.terminal_calls,
        )
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

    pub(crate) fn provider_text_delta(
        &mut self,
        session_id: CanonicalUuid,
        turn_id: CanonicalUuid,
        model_call_id: CanonicalUuid,
        part_index: u64,
        content: &str,
    ) -> io::Result<()> {
        let content = self.render_field(content, TextField::TrailingOnLine);
        writeln!(
            self.stdout,
            "provider_text_delta session={session_id} turn={turn_id} call={model_call_id} \
             part={part_index} content={content}"
        )?;
        self.stdout.flush()
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
                    TranscriptTextEntry::Imported {
                        imported_conversation_id,
                        imported_entry_id,
                        source_speaker,
                    } => format!(
                        "imported_{} imported_conversation={imported_conversation_id} \
                         imported_entry={imported_entry_id}",
                        imported_speaker_label(*source_speaker)
                    ),
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
            SnapshotEntryKind::Marker(TranscriptEntry::ModelIdentityChanged {
                turn_id,
                defaults_version,
                selected_model_id,
            }) => writeln!(
                self.stdout,
                "model_identity_changed turn={turn_id} defaults_version={} model={selected_model_id} \
                 source={} entry={}",
                defaults_version.value(),
                entry.source_session_id,
                entry.entry_id
            ),
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
            SnapshotEntryKind::Marker(TranscriptEntry::Imported {
                imported_conversation_id,
                imported_entry_id,
                source_speaker,
                content_kind,
            }) => writeln!(
                self.stdout,
                "imported_{} kind={} imported_conversation={imported_conversation_id} \
                 imported_entry={imported_entry_id} source={} entry={}",
                imported_speaker_label(*source_speaker),
                imported_content_kind(*content_kind),
                entry.source_session_id,
                entry.entry_id
            ),
        }
    }

    fn render(&self, value: &str) -> String {
        self.render_field(value, TextField::Flowing)
    }

    fn render_field(&self, value: &str, field: TextField) -> String {
        if self.raw {
            value.to_owned()
        } else {
            control_safe(value, field)
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
            Self::All => Ok(SnapshotSelectionContext::default()),
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
                    TranscriptEntry::ModelIdentityChanged { .. }
                    | TranscriptEntry::AssistantToolUse { .. }
                    | TranscriptEntry::ToolExecutionResult { .. }
                    | TranscriptEntry::ToolDenied { .. }
                    | TranscriptEntry::ToolClosed { .. }
                    | TranscriptEntry::TurnCompleted { .. }
                    | TranscriptEntry::TurnFailed { .. }
                    | TranscriptEntry::TurnCancelled { .. }
                    | TranscriptEntry::Imported { .. },
                ),
            ) => false,
        }
    }
}

const fn imported_speaker_label(source: ImportedSourceSpeaker) -> &'static str {
    match source {
        ImportedSourceSpeaker::NotAttested {} => "speaker_unattested",
        ImportedSourceSpeaker::AttestedAbsent {} => "speaker_absent",
        ImportedSourceSpeaker::Attested {
            speaker: ImportedSpeaker::User,
        } => "user",
        ImportedSourceSpeaker::Attested {
            speaker: ImportedSpeaker::Assistant,
        } => "assistant",
    }
}

const fn imported_content_kind(kind: ImportedContentKind) -> &'static str {
    match kind {
        ImportedContentKind::SourceEvent => "source_event",
        ImportedContentKind::SourceMessageBlock => "source_message_block",
        ImportedContentKind::Text => "text",
        ImportedContentKind::ToolCall => "tool_call",
        ImportedContentKind::ToolResult => "tool_result",
        ImportedContentKind::Thinking => "thinking",
        ImportedContentKind::RedactedThinking => "redacted_thinking",
        ImportedContentKind::Document => "document",
        ImportedContentKind::MessageContentAbsent => "message_content_absent",
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

const fn review_workflow_label(workflow: ReviewWorkflow) -> &'static str {
    match workflow {
        ReviewWorkflow::ImportExternalContext => "import_external_context",
        ReviewWorkflow::ReadOnlyReview => "read_only_review",
        ReviewWorkflow::JudgeFindings => "judge_findings",
        ReviewWorkflow::DedupeFindings => "dedupe_findings",
        ReviewWorkflow::PublishReview => "publish_review",
        ReviewWorkflow::FixFindings => "fix_findings",
        ReviewWorkflow::PropagateStack => "propagate_stack",
    }
}

const fn review_run_state_label(state: ReviewRunLifecycle) -> &'static str {
    match state {
        ReviewRunLifecycle::Queued => "queued",
        ReviewRunLifecycle::Running => "running",
        ReviewRunLifecycle::Succeeded => "succeeded",
        ReviewRunLifecycle::Failed => "failed",
        ReviewRunLifecycle::Blocked => "blocked",
        ReviewRunLifecycle::Cancelled => "cancelled",
    }
}

const fn review_pass_kind_label(kind: ReviewPassKind) -> &'static str {
    match kind {
        ReviewPassKind::ImportExternalContext => "import_external_context",
        ReviewPassKind::ReadOnlyReview => "read_only_review",
        ReviewPassKind::Judge => "judge",
        ReviewPassKind::Dedupe => "dedupe",
        ReviewPassKind::Publish => "publish",
        ReviewPassKind::Fix => "fix",
        ReviewPassKind::PropagateStack => "propagate_stack",
    }
}

const fn review_pass_state_label(state: ReviewPassLifecycle) -> &'static str {
    match state {
        ReviewPassLifecycle::Queued => "queued",
        ReviewPassLifecycle::Running => "running",
        ReviewPassLifecycle::Succeeded => "succeeded",
        ReviewPassLifecycle::Failed => "failed",
        ReviewPassLifecycle::Blocked => "blocked",
        ReviewPassLifecycle::Cancelled => "cancelled",
    }
}

const fn review_diff_side_label(side: ReviewDiffSide) -> &'static str {
    match side {
        ReviewDiffSide::Left => "left",
        ReviewDiffSide::Right => "right",
    }
}

const fn review_severity_label(severity: ReviewSeverity) -> &'static str {
    match severity {
        ReviewSeverity::Info => "info",
        ReviewSeverity::Low => "low",
        ReviewSeverity::Medium => "medium",
        ReviewSeverity::High => "high",
        ReviewSeverity::Critical => "critical",
    }
}

const fn review_finding_status_label(status: ReviewFindingStatus) -> &'static str {
    match status {
        ReviewFindingStatus::Open => "open",
        ReviewFindingStatus::Accepted => "accepted",
        ReviewFindingStatus::Rejected => "rejected",
        ReviewFindingStatus::Duplicate => "duplicate",
        ReviewFindingStatus::Superseded => "superseded",
        ReviewFindingStatus::Stale => "stale",
        ReviewFindingStatus::Posted => "posted",
        ReviewFindingStatus::Fixed => "fixed",
        ReviewFindingStatus::BlockedWithReason => "blocked_with_reason",
    }
}

const fn dangerous_tool_auto_approval_label(dangerous_tool_auto_approval: bool) -> &'static str {
    if dangerous_tool_auto_approval {
        "approve-all"
    } else {
        "disabled"
    }
}

const fn last_writer_actor_label(last_writer: Option<MetadataLastWriter>) -> &'static str {
    match last_writer {
        Some(last_writer) => match last_writer.actor() {
            MetadataActor::Owner {} => "owner",
        },
        None => "none",
    }
}

fn last_writer_micros_label(last_writer: Option<MetadataLastWriter>) -> String {
    match last_writer {
        Some(last_writer) => last_writer.updated_at_unix_micros().value().to_string(),
        None => String::from("none"),
    }
}

fn control_safe(value: &str, field: TextField) -> String {
    let mut rendered = String::with_capacity(value.len());
    for character in value.chars() {
        let code = character as u32;
        let preserved_line_feed = character == '\n' && field == TextField::Flowing;
        let control = code <= 0x1f || (0x7f..=0x9f).contains(&code);
        // A delimited field escapes the introducer too, so every backslash in
        // its output opens an escape this renderer wrote and the field decodes
        // back to the exact values it was given.
        let delimiter =
            matches!(character, ' ' | ',' | '\\') && field == TextField::DelimitedOnLine;
        if delimiter || (control && !preserved_line_feed) {
            rendered.push_str(&format!("\\u{{{code:x}}}"));
        } else {
            rendered.push(character);
        }
    }
    rendered
}

#[cfg(test)]
mod tests {
    use std::{
        io::{self, Write},
        path::Path,
    };

    use expect_test::expect;
    use signalbox_process_protocol::{
        CanonicalU64, CanonicalUuid, ContentFragment, CurrentModelCall, CurrentModelCallState,
        ErrorCode, ErrorDetail, FailedModelCallDisposition, FailedTerminalModelCall,
        ImportedContentKind, ImportedSourceSpeaker, ImportedSpeaker, InputContent, MetadataActor,
        MetadataLastWriter, ModelCallState, ModelCallTokenUsage, ReviewDiffSide,
        ReviewFindingInput, ReviewFindingSnapshot, ReviewFindingStatus, ReviewSeverity,
        ReviewTargetSnapshot, ReviewTargetSubject, ServerMessage, SessionEvent, TranscriptEntry,
        TranscriptTextEntry, TurnState,
    };
    use uuid::Uuid;

    use super::{Output, SessionMetadataRow, SnapshotSelection, TextField, control_safe};
    use crate::{
        error::ClientError,
        transcript::{SnapshotIdentitySet, TranscriptSnapshot},
    };

    #[test]
    fn review_target_names_an_absent_base_revision() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .review_target(&review_target_snapshot(None))
            .expect("in-memory output cannot fail");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            target=00000000-0000-0000-0000-000000000001 subject=commit parent=-
            provider=example-host
            repository=owner/repository
            head_revision=head
            base_revision_present=false
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn review_target_preserves_a_literal_dash_base_revision() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .review_target(&review_target_snapshot(Some(String::from("-"))))
            .expect("in-memory output cannot fail");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            target=00000000-0000-0000-0000-000000000001 subject=commit parent=-
            provider=example-host
            repository=owner/repository
            head_revision=head
            base_revision_present=true
            base_revision=-
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn review_finding_renders_its_complete_snapshot() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .review_finding(&review_finding_snapshot())
            .expect("in-memory output cannot fail");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            finding=00000000-0000-0000-0000-000000000004 target=00000000-0000-0000-0000-000000000001 run=00000000-0000-0000-0000-000000000002 pass=00000000-0000-0000-0000-000000000003 status=open events=2 line_start=7 line_end=9 diff_side=right severity=high confidence=9000
            file_path=src/lib.rs
            title=Retain evidence
            body=First line\u{a}Second line
            category=correctness
            recommended_fix_present=true
            recommended_fix=Bind the exact\u{a}pass.
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn terminal_safe_text_preserves_line_feed_and_escapes_c0_del_and_c1() {
        assert_eq!(
            control_safe("a\n\t\u{1b}\u{7f}\u{85}z", TextField::Flowing),
            "a\n\\u{9}\\u{1b}\\u{7f}\\u{85}z"
        );
        assert_eq!(
            control_safe("café\u{1f980}", TextField::Flowing),
            "café\u{1f980}"
        );
    }

    #[test]
    fn terminal_safe_trailing_field_escapes_line_feed_and_keeps_its_spaces() {
        assert_eq!(
            control_safe("a\n\t\u{1b}\u{7f}\u{85}z", TextField::TrailingOnLine),
            "a\\u{a}\\u{9}\\u{1b}\\u{7f}\\u{85}z"
        );
        assert_eq!(
            control_safe("café, and a space\u{1f980}", TextField::TrailingOnLine),
            "café, and a space\u{1f980}"
        );
    }

    #[test]
    fn provider_text_delta_is_terminal_safe_and_flushed_immediately() {
        let session_id = wire_uuid(1);
        let turn_id = wire_uuid(2);
        let model_call_id = wire_uuid(3);
        let part_index = 4;
        let mut stdout = FlushWriter::default();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .provider_text_delta(
                session_id,
                turn_id,
                model_call_id,
                part_index,
                "first\nforged event\u{1b}",
            )
            .expect("in-memory output cannot fail");

        assert_eq!(
            String::from_utf8(stdout.bytes).expect("rendered output is UTF-8"),
            format!(
                "provider_text_delta session={session_id} turn={turn_id} \
                 call={model_call_id} part={part_index} \
                 content=first\\u{{a}}forged event\\u{{1b}}\n"
            )
        );
        assert_eq!(stdout.flushes, 1);
        assert!(stderr.is_empty());
    }

    #[test]
    fn terminal_safe_delimited_field_escapes_the_space_and_comma_that_delimit_it() {
        assert_eq!(
            control_safe("a\n\t\u{1b}\u{7f}\u{85}z", TextField::DelimitedOnLine),
            "a\\u{a}\\u{9}\\u{1b}\\u{7f}\\u{85}z"
        );
        assert_eq!(
            control_safe("café, and a space\u{1f980}", TextField::DelimitedOnLine),
            "café\\u{2c}\\u{20}and\\u{20}a\\u{20}space\u{1f980}"
        );
    }

    #[test]
    fn terminal_safe_delimited_field_distinguishes_written_escape_text_from_an_escape() {
        let comma = control_safe(",", TextField::DelimitedOnLine);
        let escape_text_for_a_comma = control_safe("\\u{2c}", TextField::DelimitedOnLine);

        assert_eq!(comma, "\\u{2c}");
        assert_eq!(escape_text_for_a_comma, "\\u{5c}u{2c}");
        assert_ne!(comma, escape_text_for_a_comma);
    }

    #[test]
    fn scan_failure_reason_cannot_forge_an_outcome_line() {
        let error = ClientError::remote(
            ErrorCode::Unavailable,
            String::from("first line\nscan_summary imported=99 already_imported=99 skipped=0"),
            ErrorDetail::none(),
        );
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .conversation_import_scan_skipped(Path::new("conversation.jsonl"), &error)
            .expect("in-memory output cannot fail");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            skipped path="conversation.jsonl" reason=unavailable: first line\u{a}scan_summary imported=99 already_imported=99 skipped=0
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn search_renders_one_complete_written_metadata_row() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .session_metadata_summary(&SessionMetadataRow {
                session_id: wire_uuid(1),
                defaults_version: 2,
                selection: "model=00000000-0000-0000-0000-000000000003",
                dangerous_tool_auto_approval: true,
                archived: true,
                last_writer: Some(MetadataLastWriter::new(
                    CanonicalU64::new(1_753_484_400_000_000),
                    MetadataActor::Owner {},
                )),
                tags: &[String::from("daily"), String::from("plan")],
                title: Some("Active plan"),
            })
            .expect("in-memory output cannot fail");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            00000000-0000-0000-0000-000000000001 archived=true defaults_version=2 model=00000000-0000-0000-0000-000000000003 dangerous_tool_auto_approval=approve-all last_writer=owner updated_at_unix_micros=1753484400000000 tags=daily,plan title=Active plan
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn search_renders_the_unwritten_metadata_row_with_named_absences() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .session_metadata_summary(&SessionMetadataRow {
                session_id: wire_uuid(1),
                defaults_version: 1,
                selection: "alias=00000000-0000-0000-0000-000000000002",
                dangerous_tool_auto_approval: false,
                archived: false,
                last_writer: None,
                tags: &[],
                title: None,
            })
            .expect("in-memory output cannot fail");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            00000000-0000-0000-0000-000000000001 archived=false defaults_version=1 alias=00000000-0000-0000-0000-000000000002 dangerous_tool_auto_approval=disabled last_writer=none updated_at_unix_micros=none tags= title=
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn search_title_and_tags_cannot_forge_another_row() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .session_metadata_summary(&SessionMetadataRow {
                session_id: wire_uuid(1),
                defaults_version: 1,
                selection: "model=00000000-0000-0000-0000-000000000003",
                dangerous_tool_auto_approval: false,
                archived: false,
                last_writer: None,
                tags: &[String::from("first\nsecond")],
                title: Some("forged\n00000000-0000-0000-0000-000000000002 archived=false"),
            })
            .expect("in-memory output cannot fail");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            00000000-0000-0000-0000-000000000001 archived=false defaults_version=1 model=00000000-0000-0000-0000-000000000003 dangerous_tool_auto_approval=disabled last_writer=none updated_at_unix_micros=none tags=first\u{a}second title=forged\u{a}00000000-0000-0000-0000-000000000002 archived=false
        "#]]
        .assert_eq(&rendered);
        assert_eq!(rendered.lines().count(), 1);
        assert!(stderr.is_empty());
    }

    #[test]
    fn search_tags_state_their_exact_boundaries() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .session_metadata_summary(&SessionMetadataRow {
                session_id: wire_uuid(1),
                defaults_version: 1,
                selection: "model=00000000-0000-0000-0000-000000000003",
                dangerous_tool_auto_approval: false,
                archived: false,
                last_writer: None,
                tags: &[String::from("one,tag title=forged"), String::from("second")],
                title: Some("Active plan"),
            })
            .expect("in-memory output cannot fail");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            00000000-0000-0000-0000-000000000001 archived=false defaults_version=1 model=00000000-0000-0000-0000-000000000003 dangerous_tool_auto_approval=disabled last_writer=none updated_at_unix_micros=none tags=one\u{2c}tag\u{20}title=forged,second title=Active plan
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn search_prints_its_continuation_cursor_to_standard_error() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .next_page_cursor(wire_uuid(1))
            .expect("in-memory output cannot fail");

        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).expect("rendered output is UTF-8"),
            "next_after_session_id=00000000-0000-0000-0000-000000000001\n"
        );
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
    fn s28_imported_snapshot_renders_attested_text() {
        let mut snapshot = TranscriptSnapshot::from_messages(
            9,
            [
                ServerMessage::TranscriptTextEntry {
                    entry_index: CanonicalU64::new(0),
                    source_session_id: wire_uuid(1),
                    entry_id: wire_uuid(2),
                    entry: TranscriptTextEntry::Imported {
                        imported_conversation_id: wire_uuid(3),
                        imported_entry_id: wire_uuid(4),
                        source_speaker: ImportedSourceSpeaker::Attested {
                            speaker: ImportedSpeaker::User,
                        },
                    },
                },
                ServerMessage::TranscriptContent {
                    entry_index: CanonicalU64::new(0),
                    fragment_index: CanonicalU64::new(0),
                    final_fragment: true,
                    content_fragment: ContentFragment::try_new("exact imported text".to_owned())
                        .expect("short content is valid"),
                },
            ],
        )
        .expect("test snapshot must spool");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .snapshot(&mut snapshot)
            .expect("imported snapshot must render");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            imported_user imported_conversation=00000000-0000-0000-0000-000000000003 imported_entry=00000000-0000-0000-0000-000000000004 source=00000000-0000-0000-0000-000000000001 entry=00000000-0000-0000-0000-000000000002
            exact imported text
            usage_total scope=session terminal_calls=0 input_tokens=unreported input_tokens_reported_calls=0/0 output_tokens=unreported output_tokens_reported_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_reported_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_reported_calls=0/0
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn s28_imported_snapshot_renders_conservative_nontext() {
        let mut snapshot = TranscriptSnapshot::from_messages(
            9,
            [ServerMessage::TranscriptEntry {
                entry_index: CanonicalU64::new(0),
                source_session_id: wire_uuid(1),
                entry_id: wire_uuid(5),
                entry: TranscriptEntry::Imported {
                    imported_conversation_id: wire_uuid(3),
                    imported_entry_id: wire_uuid(6),
                    source_speaker: ImportedSourceSpeaker::NotAttested {},
                    content_kind: ImportedContentKind::ToolCall,
                },
            }],
        )
        .expect("test snapshot must spool");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .snapshot(&mut snapshot)
            .expect("imported snapshot must render");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            imported_speaker_unattested kind=tool_call imported_conversation=00000000-0000-0000-0000-000000000003 imported_entry=00000000-0000-0000-0000-000000000006 source=00000000-0000-0000-0000-000000000001 entry=00000000-0000-0000-0000-000000000005
            usage_total scope=session terminal_calls=0 input_tokens=unreported input_tokens_reported_calls=0/0 output_tokens=unreported output_tokens_reported_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_reported_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_reported_calls=0/0
        "#]]
        .assert_eq(&rendered);
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
            usage_total scope=session terminal_calls=0 input_tokens=unreported input_tokens_reported_calls=0/0 output_tokens=unreported output_tokens_reported_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_reported_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_reported_calls=0/0
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
            usage_total scope=session terminal_calls=0 input_tokens=unreported input_tokens_reported_calls=0/0 output_tokens=unreported output_tokens_reported_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_reported_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_reported_calls=0/0
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
            usage_total scope=session terminal_calls=0 input_tokens=unreported input_tokens_reported_calls=0/0 output_tokens=unreported output_tokens_reported_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_reported_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_reported_calls=0/0
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
            usage_total scope=session terminal_calls=0 input_tokens=unreported input_tokens_reported_calls=0/0 output_tokens=unreported output_tokens_reported_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_reported_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_reported_calls=0/0
        "#]]
        .assert_eq(&rendered);
    }

    #[test]
    fn transcript_without_terminal_calls_renders_a_session_usage_total() {
        let mut snapshot =
            TranscriptSnapshot::from_messages(1, std::iter::empty::<ServerMessage>())
                .expect("test snapshot must spool");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .snapshot(&mut snapshot)
            .expect("empty usage snapshot must render");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            usage_total scope=session terminal_calls=0 input_tokens=unreported input_tokens_reported_calls=0/0 output_tokens=unreported output_tokens_reported_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_reported_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_reported_calls=0/0
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn transcript_usage_preserves_zero_absence_and_partial_coverage() {
        let first_turn = wire_uuid(1);
        let second_turn = wire_uuid(2);
        let mut snapshot = TranscriptSnapshot::from_messages(
            1,
            [
                ServerMessage::TranscriptModelCallUsage {
                    model_call_index: CanonicalU64::new(0),
                    turn_id: first_turn,
                    model_call_id: wire_uuid(11),
                    usage: ModelCallTokenUsage {
                        input_tokens: Some(CanonicalU64::new(10)),
                        output_tokens: Some(CanonicalU64::new(0)),
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: Some(CanonicalU64::new(4)),
                    },
                },
                ServerMessage::TranscriptModelCallUsage {
                    model_call_index: CanonicalU64::new(1),
                    turn_id: first_turn,
                    model_call_id: wire_uuid(12),
                    usage: ModelCallTokenUsage {
                        input_tokens: None,
                        output_tokens: None,
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: None,
                    },
                },
                ServerMessage::TranscriptModelCallUsage {
                    model_call_index: CanonicalU64::new(2),
                    turn_id: second_turn,
                    model_call_id: wire_uuid(13),
                    usage: ModelCallTokenUsage {
                        input_tokens: None,
                        output_tokens: None,
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: None,
                    },
                },
            ],
        )
        .expect("test snapshot must spool");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .snapshot(&mut snapshot)
            .expect("usage snapshot must render");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            usage turn=00000000-0000-0000-0000-000000000001 terminal_calls=2 input_tokens=10 input_tokens_reported_calls=1/2 output_tokens=0 output_tokens_reported_calls=1/2 cache_creation_input_tokens=unreported cache_creation_input_tokens_reported_calls=0/2 cache_read_input_tokens=4 cache_read_input_tokens_reported_calls=1/2
            usage turn=00000000-0000-0000-0000-000000000002 terminal_calls=1 input_tokens=unreported input_tokens_reported_calls=0/1 output_tokens=unreported output_tokens_reported_calls=0/1 cache_creation_input_tokens=unreported cache_creation_input_tokens_reported_calls=0/1 cache_read_input_tokens=unreported cache_read_input_tokens_reported_calls=0/1
            usage_total scope=session terminal_calls=3 input_tokens=10 input_tokens_reported_calls=1/3 output_tokens=0 output_tokens_reported_calls=1/3 cache_creation_input_tokens=unreported cache_creation_input_tokens_reported_calls=0/3 cache_read_input_tokens=4 cache_read_input_tokens_reported_calls=1/3
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
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
    fn review_target_snapshot(base_revision: Option<String>) -> ReviewTargetSnapshot {
        ReviewTargetSnapshot {
            target_id: wire_uuid(1),
            provider: String::from("example-host"),
            repository: String::from("owner/repository"),
            subject: ReviewTargetSubject::Commit {},
            head_revision: String::from("head"),
            base_revision,
            stack_parent_target_id: None,
        }
    }

    fn review_finding_snapshot() -> ReviewFindingSnapshot {
        ReviewFindingSnapshot {
            target_id: wire_uuid(1),
            run_id: wire_uuid(2),
            producing_pass_id: wire_uuid(3),
            finding: ReviewFindingInput {
                finding_id: wire_uuid(4),
                file_path: String::from("src/lib.rs"),
                line_start: Some(CanonicalU64::new(7)),
                line_end: Some(CanonicalU64::new(9)),
                diff_side: Some(ReviewDiffSide::Right),
                title: String::from("Retain evidence"),
                body: String::from("First line\nSecond line"),
                severity: ReviewSeverity::High,
                confidence: CanonicalU64::new(9_000),
                category: String::from("correctness"),
                recommended_fix: Some(String::from("Bind the exact\npass.")),
            },
            status: ReviewFindingStatus::Open,
            event_count: CanonicalU64::new(2),
        }
    }
}
