mod labels;
use labels::{
    RateCounts, bound_child_action, current_model_call_state, dangerous_tool_auto_approval_label,
    delegation_message_direction, delegation_outcome, delegation_provenance, delegation_reason,
    delegation_wait_mode, duration_label, failed_model_call_cause, failed_model_call_disposition,
    goal_blocked_reason_label, imported_content_kind, imported_speaker_attestation_label,
    imported_speaker_label, last_writer_micros_label, lifecycle_actor_label, model_call_state,
    operator_status_lifecycle_state_label, rate_label, review_diff_side_label,
    review_finding_status_label, review_orchestration_concern_status_label,
    review_orchestration_state_label, review_pass_kind_label, review_pass_state_label,
    review_run_state_label, review_severity_label, review_workflow_label, runner_connection_health,
    runner_projection_state, runner_sandbox_profile, runner_state_transition_state,
    session_closure_outcome_label,
};
pub(crate) use labels::{control_safe, last_writer_actor_label};
mod rows;
pub(crate) use rows::{
    ChatTurnStatus, ConversationRow, ImportedEntryRow, SessionMetadataRow, TextField,
};
mod cost;
#[cfg(test)]
pub(crate) use cost::{CostAggregateKey, DiskCostTotals};
use cost::{TokenUsageTotal, UsageAggregate, cost_label, usage_provenance_label};
mod selection;
pub(crate) use selection::SnapshotSelection;
#[cfg(test)]
use selection::SnapshotSelectionContext;

use std::{
    collections::HashSet,
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
    str::FromStr,
};

use rust_decimal::Decimal;
use signalbox_process_protocol::{
    BoundChildAction, CanonicalBlobDigest, CanonicalUuid, CurrentModelCallState,
    DelegationMessageDirection, DelegationOutcome, DelegationPolicy, DelegationProvenance,
    DelegationReason, DelegationWaitMode, DescendantTerminationScope, FailedModelCallCause,
    FailedModelCallDisposition, GoalBlockedProvenance, GoalBlockedReason, GoalHistoryEvent,
    GoalLifecycleState, ImportedContentKind, ImportedSourceSpeaker, ImportedSpeaker,
    ImportedTextPreview, LifecycleActorClass, MAX_RATE_VERSION_UTF8_BYTES, MetadataActor,
    MetadataLastWriter, ModelCallCostLabel, ModelCallDisposition, ModelCallState,
    OperatorStatusLifecycleDeadlineViolationMessage, OperatorStatusLifecycleState,
    OperatorStatusLifecycleWeekMessage, OperatorStatusMessage, ReviewDiffSide,
    ReviewFindingSnapshot, ReviewFindingStatus, ReviewOrchestrationConcernStatus,
    ReviewOrchestrationSnapshot, ReviewOrchestrationState, ReviewPassKind, ReviewPassLifecycle,
    ReviewRunLifecycle, ReviewRunSnapshot, ReviewSeverity, ReviewTargetSnapshot,
    ReviewTargetSubject, ReviewWorkflow, RunnerConnectionHealth, RunnerProjection,
    RunnerProjectionSelector, RunnerProjectionState, RunnerSandboxProfile,
    RunnerStateTransitionState, ServerMessage, SessionClosureOutcome, SessionEvent,
    ToolApprovalEventDecider, ToolApprovalEventDecision, ToolBatchState, ToolDecision,
    TranscriptEntry, TranscriptTextEntry, TurnState, UsageProvenance, UserInputContent,
    UserInputPart,
};

use crate::{
    ImportScanSummary,
    error::ClientError,
    transcript::{
        SnapshotEntry, SnapshotEntryKind, SnapshotIdentitySet, SnapshotRecord, TranscriptSnapshot,
        TranscriptTurn,
    },
};

pub(crate) struct ChildResultPresentation<'a> {
    pub(crate) await_request_id: CanonicalUuid,
    pub(crate) spawning_request_id: CanonicalUuid,
    pub(crate) child_session_id: CanonicalUuid,
    pub(crate) outcome: DelegationOutcome,
    pub(crate) content: Option<&'a String>,
    pub(crate) reason: DelegationReason,
    pub(crate) provenance: DelegationProvenance,
}

pub(crate) struct SessionSpawnedPresentation {
    pub(crate) tool_request_id: CanonicalUuid,
    pub(crate) child_session_id: CanonicalUuid,
    pub(crate) relationship: DelegationPolicy,
}

pub(crate) struct SessionAwaitRegisteredPresentation {
    pub(crate) tool_request_id: CanonicalUuid,
    pub(crate) child_session_id: CanonicalUuid,
    pub(crate) mode: DelegationWaitMode,
}

pub(crate) struct SessionMessageSentPresentation {
    pub(crate) tool_request_id: CanonicalUuid,
    pub(crate) peer_session_id: CanonicalUuid,
    pub(crate) message_id: CanonicalUuid,
    pub(crate) direction: DelegationMessageDirection,
    pub(crate) ordinal: u64,
    pub(crate) delivery_sequence: u64,
}

pub(crate) struct OperatorStatusPresentationCounts {
    pub(crate) lifecycle_weeks: u64,
    pub(crate) lifecycle_deadline_violations: u64,
}

pub(crate) enum BlobUploadPresentation {
    AlreadyPresent,
    Committed,
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

    pub(crate) fn flush(&mut self) -> io::Result<()> {
        self.stdout.flush()
    }

    pub(crate) fn blob_metadata(
        &mut self,
        digest: CanonicalBlobDigest,
        byte_length: u64,
        replica_count: u64,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "digest={digest} byte_length={byte_length} replica_count={replica_count}"
        )
    }

    pub(crate) fn chat_started(
        &mut self,
        session_id: CanonicalUuid,
        status: Option<ChatTurnStatus>,
        commands: &str,
    ) -> io::Result<()> {
        match status {
            Some(ChatTurnStatus::Active(turn_id)) => writeln!(
                self.stdout,
                "chat session={session_id} state=following turn={turn_id} commands={commands}"
            )?,
            Some(ChatTurnStatus::AwaitingApproval {
                turn_id,
                tool_request_id,
            }) => writeln!(
                self.stdout,
                "chat session={session_id} state=awaiting_approval turn={turn_id} request={tool_request_id} commands={commands}"
            )?,
            Some(ChatTurnStatus::Queued(turn_id)) => writeln!(
                self.stdout,
                "chat session={session_id} state=queued turn={turn_id} commands={commands}"
            )?,
            None => writeln!(
                self.stdout,
                "chat session={session_id} state=ready commands={commands}"
            )?,
        }
        self.stdout.flush()
    }

    pub(crate) fn chat_ready(&mut self, session_id: CanonicalUuid) -> io::Result<()> {
        writeln!(self.stdout, "chat session={session_id} state=ready")?;
        self.stdout.flush()
    }

    pub(crate) fn chat_queued(&mut self, turn_id: CanonicalUuid) -> io::Result<()> {
        writeln!(self.stdout, "chat state=queued turn={turn_id}")?;
        self.stdout.flush()
    }

    pub(crate) fn chat_activated(&mut self, turn_id: CanonicalUuid) -> io::Result<()> {
        writeln!(self.stdout, "chat state=streaming turn={turn_id}")?;
        self.stdout.flush()
    }

    pub(crate) fn chat_stopped(
        &mut self,
        stopped_turn_id: CanonicalUuid,
        successor_turn_id: CanonicalUuid,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "chat state=queued stopped_turn={stopped_turn_id} successor_turn={successor_turn_id}"
        )?;
        self.stdout.flush()
    }

    pub(crate) fn chat_awaiting_approval(
        &mut self,
        turn_id: CanonicalUuid,
        tool_request_id: CanonicalUuid,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "chat state=awaiting_approval turn={turn_id} request={tool_request_id}"
        )?;
        self.stdout.flush()
    }

    pub(crate) fn chat_usage(&mut self, message: &str, commands: &str) -> io::Result<()> {
        let message = self.render_field(message, TextField::TrailingOnLine);
        writeln!(self.stderr, "chat: {message}; commands: {commands}")?;
        self.stderr.flush()
    }

    pub(crate) fn chat_interrupt_offered(&mut self, commands: &str) -> io::Result<()> {
        writeln!(
            self.stderr,
            "chat: turn still running; use :stop TEXT to stop and continue, or press Ctrl-C again to exit leaving it running; commands: {commands}"
        )?;
        self.stderr.flush()
    }

    pub(crate) fn chat_approval_interrupt_offered(
        &mut self,
        tool_request_id: CanonicalUuid,
        commands: &str,
    ) -> io::Result<()> {
        writeln!(
            self.stderr,
            "chat: turn awaits approval request {tool_request_id}; use :approve ID or :deny ID REASON, or press Ctrl-C again to exit leaving it running; commands: {commands}"
        )?;
        self.stderr.flush()
    }

    pub(crate) fn chat_mutation_abandoned(&mut self) -> io::Result<()> {
        writeln!(
            self.stderr,
            "chat: exiting with an in-flight mutation whose outcome may be ambiguous; use the printed recovery values for any exact standalone retry"
        )?;
        self.stderr.flush()
    }

    pub(crate) fn chat_exiting(&mut self, status: Option<ChatTurnStatus>) -> io::Result<()> {
        match status {
            Some(
                ChatTurnStatus::Active(turn_id) | ChatTurnStatus::AwaitingApproval { turn_id, .. },
            ) => writeln!(
                self.stderr,
                "chat: exiting; turn {turn_id} remains running in the daemon"
            ),
            Some(ChatTurnStatus::Queued(turn_id)) => writeln!(
                self.stderr,
                "chat: exiting; turn {turn_id} remains queued in the daemon"
            ),
            None => writeln!(self.stderr, "chat: exiting; no turn is queued or running"),
        }?;
        self.stderr.flush()
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

    pub(crate) fn session_spawned(
        &mut self,
        receipt: SessionSpawnedPresentation,
    ) -> io::Result<()> {
        let SessionSpawnedPresentation {
            tool_request_id,
            child_session_id,
            relationship,
        } = receipt;
        match relationship {
            DelegationPolicy::Background {} => writeln!(
                self.stdout,
                "spawn_request={tool_request_id} child_session={child_session_id} relationship=background"
            ),
            DelegationPolicy::Bound {
                on_parent_stopped,
                on_parent_cancelled,
            } => writeln!(
                self.stdout,
                "spawn_request={tool_request_id} child_session={child_session_id} \
                 relationship=bound on_parent_stopped={} on_parent_cancelled={}",
                bound_child_action(on_parent_stopped),
                bound_child_action(on_parent_cancelled)
            ),
        }
    }

    pub(crate) fn session_await_registered(
        &mut self,
        receipt: SessionAwaitRegisteredPresentation,
    ) -> io::Result<()> {
        let SessionAwaitRegisteredPresentation {
            tool_request_id,
            child_session_id,
            mode,
        } = receipt;
        writeln!(
            self.stdout,
            "await_request={tool_request_id} child_session={child_session_id} mode={}",
            delegation_wait_mode(mode)
        )
    }

    pub(crate) fn child_result(&mut self, result: ChildResultPresentation<'_>) -> io::Result<()> {
        let ChildResultPresentation {
            await_request_id,
            spawning_request_id,
            child_session_id,
            outcome,
            content,
            reason,
            provenance,
        } = result;
        let content = content.map_or_else(
            || String::from("null"),
            |content| self.render_field(content, TextField::TrailingOnLine),
        );
        writeln!(
            self.stdout,
            "await_request={await_request_id} spawning_request={spawning_request_id} \
             child_session={child_session_id} delivery=foreground outcome={} reason={} \
             provenance={} content={content}",
            delegation_outcome(outcome),
            delegation_reason(reason),
            delegation_provenance(&provenance)
        )
    }

    pub(crate) fn session_message_sent(
        &mut self,
        receipt: SessionMessageSentPresentation,
    ) -> io::Result<()> {
        let SessionMessageSentPresentation {
            tool_request_id,
            peer_session_id,
            message_id,
            direction,
            ordinal,
            delivery_sequence,
        } = receipt;
        writeln!(
            self.stdout,
            "message_request={tool_request_id} peer_session={peer_session_id} \
             message={message_id} direction={} ordinal={ordinal} delivery_sequence={delivery_sequence}",
            delegation_message_direction(direction)
        )
    }

    pub(crate) fn goal_transition_applied(
        &mut self,
        session_id: CanonicalUuid,
        event_ordinal: u64,
        generation: u64,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "session={session_id} goal_event={event_ordinal} generation={generation}"
        )
    }

    pub(crate) fn goal_current(
        &mut self,
        session_id: CanonicalUuid,
        generation: u64,
        statement: &str,
        state: &GoalLifecycleState,
    ) -> io::Result<()> {
        match state {
            GoalLifecycleState::Pursuing {} => writeln!(
                self.stdout,
                "goal session={session_id} generation={generation} state=pursuing"
            )?,
            GoalLifecycleState::Blocked { reason, .. } => writeln!(
                self.stdout,
                "goal session={session_id} generation={generation} state=blocked reason={}",
                goal_blocked_reason_label(*reason)
            )?,
            GoalLifecycleState::Achieved {
                turn_id,
                tool_request_id,
            } => writeln!(
                self.stdout,
                "goal session={session_id} generation={generation} state=achieved turn={turn_id} request={tool_request_id}"
            )?,
            GoalLifecycleState::UserStopped {} => writeln!(
                self.stdout,
                "goal session={session_id} generation={generation} state=user_stopped"
            )?,
            GoalLifecycleState::Superseded { by_generation } => writeln!(
                self.stdout,
                "goal session={session_id} generation={generation} state=superseded by_generation={}",
                by_generation.value()
            )?,
            GoalLifecycleState::SessionClosed { outcome } => writeln!(
                self.stdout,
                "goal session={session_id} generation={generation} state=session_closed outcome={}",
                session_closure_outcome_label(*outcome)
            )?,
        }
        self.goal_text_field("statement", statement)?;
        match state {
            GoalLifecycleState::Blocked { need, .. } => self.goal_text_field("need", need)?,
            GoalLifecycleState::Pursuing {}
            | GoalLifecycleState::Achieved { .. }
            | GoalLifecycleState::UserStopped {}
            | GoalLifecycleState::Superseded { .. }
            | GoalLifecycleState::SessionClosed { .. } => {}
        }
        Ok(())
    }

    pub(crate) fn goal_history_event(
        &mut self,
        ordinal: u64,
        generation: u64,
        event: &GoalHistoryEvent,
    ) -> io::Result<()> {
        match event {
            GoalHistoryEvent::Commissioned {
                statement,
                command_id,
            } => {
                writeln!(
                    self.stdout,
                    "event={ordinal} generation={generation} type=commissioned command={}",
                    command_id.into_uuid().hyphenated()
                )?;
                self.goal_text_field("statement", statement)
            }
            GoalHistoryEvent::Blocked {
                reason,
                need,
                provenance,
            } => {
                match provenance {
                    GoalBlockedProvenance::Model {
                        turn_id,
                        tool_request_id,
                    } => writeln!(
                        self.stdout,
                        "event={ordinal} generation={generation} type=blocked reason={} source=model turn={turn_id} request={tool_request_id}",
                        goal_blocked_reason_label(*reason)
                    )?,
                    GoalBlockedProvenance::ExecutionFailure { turn_id } => writeln!(
                        self.stdout,
                        "event={ordinal} generation={generation} type=blocked reason={} source=scheduler turn={turn_id}",
                        goal_blocked_reason_label(*reason)
                    )?,
                }
                self.goal_text_field("need", need)
            }
            GoalHistoryEvent::Resumed {
                guidance,
                command_id,
            } => {
                writeln!(
                    self.stdout,
                    "event={ordinal} generation={generation} type=resumed command={} guidance_present={}",
                    command_id.into_uuid().hyphenated(),
                    guidance.is_some()
                )?;
                match guidance {
                    Some(guidance) => self.goal_text_field("guidance", guidance),
                    None => Ok(()),
                }
            }
            GoalHistoryEvent::Achieved {
                report,
                turn_id,
                tool_request_id,
            } => {
                writeln!(
                    self.stdout,
                    "event={ordinal} generation={generation} type=achieved turn={turn_id} request={tool_request_id}"
                )?;
                self.goal_text_field("report", report)
            }
            GoalHistoryEvent::UserStopped { command_id } => writeln!(
                self.stdout,
                "event={ordinal} generation={generation} type=user_stopped command={}",
                command_id.into_uuid().hyphenated()
            ),
            GoalHistoryEvent::Superseded {
                replacement_statement,
                command_id,
            } => {
                writeln!(
                    self.stdout,
                    "event={ordinal} generation={generation} type=superseded command={}",
                    command_id.into_uuid().hyphenated()
                )?;
                self.goal_text_field("replacement_statement", replacement_statement)
            }
            GoalHistoryEvent::SessionClosed { outcome, actor } => writeln!(
                self.stdout,
                "event={ordinal} generation={generation} type=session_closed outcome={} actor={}",
                session_closure_outcome_label(*outcome),
                lifecycle_actor_label(*actor)
            ),
        }
    }

    fn goal_text_field(&mut self, name: &str, value: &str) -> io::Result<()> {
        write!(self.stdout, "{name}=")?;
        self.stdout.write_all(
            self.render_field(value, TextField::TrailingOnLine)
                .as_bytes(),
        )?;
        self.stdout.write_all(b"\n")
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn session_compacted(
        &mut self,
        session_id: CanonicalUuid,
        context_compaction_id: CanonicalUuid,
        model_call_id: CanonicalUuid,
        through_position: u64,
        summary_entry_id: CanonicalUuid,
        result_frontier_id: CanonicalUuid,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "session={session_id} compaction={context_compaction_id} call={model_call_id} \
             through_position={through_position} summary_entry={summary_entry_id} \
             result_frontier={result_frontier_id}"
        )
    }

    pub(crate) fn template_summary(&mut self, name: &str, version: u64) -> io::Result<()> {
        writeln!(self.stdout, "name={name} version={version}")
    }

    pub(crate) fn steering_submitted(
        &mut self,
        accepted_input_id: CanonicalUuid,
        acceptance_position: u64,
        source_turn_id: CanonicalUuid,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "accepted_input={accepted_input_id} position={acceptance_position} source_turn={source_turn_id}"
        )
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

    pub(crate) fn blob_uploaded(
        &mut self,
        digest: CanonicalBlobDigest,
        byte_length: u64,
        outcome: BlobUploadPresentation,
    ) -> io::Result<()> {
        let status = match outcome {
            BlobUploadPresentation::AlreadyPresent => "already_present",
            BlobUploadPresentation::Committed => "committed",
        };
        writeln!(
            self.stdout,
            "{status} digest={digest} byte_length={byte_length}"
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

    /// Prints one selectable imported position with its attestation, kind, and
    /// bounded text preview.
    ///
    /// The preview is the last field on its line and the truncation marker
    /// precedes it, so preview text cannot forge either. An entry carrying no
    /// exact attested text omits both fields rather than printing a placeholder
    /// that empty attested text could not be told apart from.
    pub(crate) fn imported_conversation_entry(
        &mut self,
        row: &ImportedEntryRow<'_>,
    ) -> io::Result<()> {
        let prefix = format!(
            "position={} imported_entry={} speaker={} kind={}",
            row.position,
            row.imported_entry_id,
            imported_speaker_attestation_label(row.source_speaker),
            imported_content_kind(row.content_kind),
        );
        match row.text_preview {
            Some(preview) => {
                let text = self.render_field(preview.preview(), TextField::TrailingOnLine);
                writeln!(
                    self.stdout,
                    "{prefix} truncated={} text={text}",
                    preview.truncated()
                )
            }
            None => writeln!(self.stdout, "{prefix}"),
        }
    }

    /// Prints the imported conversation's total entry count, which is also its
    /// greatest selectable position.
    pub(crate) fn imported_conversation_entry_count(&mut self, entry_count: u64) -> io::Result<()> {
        writeln!(self.stdout, "entry_count={entry_count}")
    }

    /// Prints the concrete position a `latest` selection resolved to, before
    /// the durable command that consumes it can become ambiguous.
    pub(crate) fn resolved_through_position(&mut self, position: u64) -> io::Result<()> {
        self.recovery_value("through_position", &position.to_string())
    }

    pub(crate) fn operator_status_counts(
        &mut self,
        counts: OperatorStatusPresentationCounts,
    ) -> io::Result<()> {
        let OperatorStatusPresentationCounts {
            lifecycle_weeks,
            lifecycle_deadline_violations,
        } = counts;
        writeln!(
            self.stdout,
            "status lifecycle_weeks={lifecycle_weeks} \
             nonterminal_past_deadline={lifecycle_deadline_violations}"
        )
    }

    pub(crate) fn operator_status_item(
        &mut self,
        message: &ServerMessage,
    ) -> Result<(), ClientError> {
        let ServerMessage::OperatorStatus(message) = message else {
            return Err(ClientError::Protocol(
                "operator-status spool contained an unexpected frame",
            ));
        };
        match message.as_ref() {
            OperatorStatusMessage::LifecycleWeek(item) => {
                let OperatorStatusLifecycleWeekMessage {
                    week_start_date,
                    completion_failure_numerator,
                    completion_failure_denominator,
                    failed_unknown_count,
                    overflow_numerator,
                    overflow_denominator,
                    finish_given_overflow_numerator,
                    wall_numerator,
                    wall_denominator,
                    wall_occurrence_count,
                    classified_terminal_turn_count,
                    terminal_turn_count,
                    classified_known_failed_call_count,
                    known_failed_call_count,
                } = item.as_ref();
                writeln!(
                    self.stdout,
                    "lifecycle_week week={week_start_date} completion_failure={} \
                     failed_unknown={} overflow={} finish_given_overflow={} wall={} \
                     wall_occurrences={} \
                     turn_cause_completeness={} model_call_cause_completeness={}",
                    rate_label(RateCounts {
                        numerator: completion_failure_numerator.value(),
                        denominator: completion_failure_denominator.value(),
                    }),
                    rate_label(RateCounts {
                        numerator: failed_unknown_count.value(),
                        denominator: completion_failure_denominator.value(),
                    }),
                    rate_label(RateCounts {
                        numerator: overflow_numerator.value(),
                        denominator: overflow_denominator.value(),
                    }),
                    rate_label(RateCounts {
                        numerator: finish_given_overflow_numerator.value(),
                        denominator: overflow_numerator.value(),
                    }),
                    rate_label(RateCounts {
                        numerator: wall_numerator.value(),
                        denominator: wall_denominator.value(),
                    }),
                    wall_occurrence_count.value(),
                    rate_label(RateCounts {
                        numerator: classified_terminal_turn_count.value(),
                        denominator: terminal_turn_count.value(),
                    }),
                    rate_label(RateCounts {
                        numerator: classified_known_failed_call_count.value(),
                        denominator: known_failed_call_count.value(),
                    }),
                )?;
                Ok(())
            }
            OperatorStatusMessage::LifecycleDeadlineViolation(item) => {
                let OperatorStatusLifecycleDeadlineViolationMessage {
                    session_id,
                    state,
                    deadline_missing,
                    expired_for_seconds,
                } = item.as_ref();
                writeln!(
                    self.stdout,
                    "nonterminal_past_deadline session={session_id} state={} deadline={} \
                     expired={}",
                    operator_status_lifecycle_state_label(*state),
                    if *deadline_missing {
                        "missing"
                    } else {
                        "armed"
                    },
                    expired_for_seconds
                        .map_or_else(|| "n/a".to_owned(), |value| duration_label(value.value())),
                )?;
                Ok(())
            }
            OperatorStatusMessage::Start {} | OperatorStatusMessage::End(_) => Err(
                ClientError::Protocol("operator-status spool contained an unexpected frame"),
            ),
        }
    }

    pub(crate) fn operator_status_model_usage_omitted(&mut self) -> io::Result<()> {
        writeln!(
            self.stdout,
            "model_usage=omitted reason=no_cheap_status_aggregate"
        )
    }

    pub(crate) fn session_summary(
        &mut self,
        session_id: CanonicalUuid,
        defaults_version: u64,
        selection: &str,
        placement_version: u64,
        placement: &str,
        runner: Option<&RunnerProjection>,
    ) -> io::Result<()> {
        write!(
            self.stdout,
            "{session_id} defaults_version={defaults_version} {selection} \
             placement_version={placement_version} {placement}"
        )?;
        if let Some(runner) = runner {
            write!(self.stdout, " runner_selector=")?;
            match runner.selector() {
                RunnerProjectionSelector::Runner { runner_id } => {
                    write!(self.stdout, "runner runner_selector_runner={runner_id}")?;
                }
                RunnerProjectionSelector::CapabilityClass { name } => write!(
                    self.stdout,
                    "capability_class runner_selector_capability={}",
                    self.render_field(name.as_str(), TextField::DelimitedOnLine)
                )?,
            }
            if let Some(runner_id) = runner.runner_id() {
                write!(self.stdout, " runner={runner_id}")?;
            }
            write!(
                self.stdout,
                " runner_placement_revision={} runner_sandbox={}",
                runner.placement_revision().value(),
                runner_sandbox_profile(runner.sandbox_profile())
            )?;
            if let Some(profile) = runner.credential_profile() {
                write!(
                    self.stdout,
                    " runner_credential_profile={}",
                    self.render_field(profile.as_str(), TextField::DelimitedOnLine)
                )?;
            }
            if let Some(repository) = runner.repository() {
                write!(
                    self.stdout,
                    " runner_repository={}",
                    self.render_field(repository.as_str(), TextField::DelimitedOnLine)
                )?;
            }
            if let Some(directory) = runner.working_directory() {
                write!(
                    self.stdout,
                    " runner_working_directory={}",
                    self.render_field(directory.as_str(), TextField::DelimitedOnLine)
                )?;
            }
            if let Some(health) = runner.connection_health() {
                write!(
                    self.stdout,
                    " runner_connection_health={}",
                    runner_connection_health(health)
                )?;
            }
            write!(
                self.stdout,
                " runner_state={}",
                runner_projection_state(runner.state())
            )?;
        }
        writeln!(self.stdout)
    }

    pub(crate) fn session_placement_updated(
        &mut self,
        session_id: CanonicalUuid,
        placement_version: u64,
        placement: &str,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "session={session_id} placement_version={placement_version} {placement}"
        )
    }

    pub(crate) fn review_acknowledgement(&mut self, line: &str) -> io::Result<()> {
        self.stdout.write_all(self.render(line).as_bytes())?;
        self.stdout.write_all(b"\n")
    }

    pub(crate) fn review_orchestration(
        &mut self,
        snapshot: &ReviewOrchestrationSnapshot,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "attempt={} target={} state={} concerns={} findings={} judgment_members={} \
             judgment_effects_applied={} repairs_fixed={} publications_published={}",
            snapshot.attempt_id,
            snapshot.target_id,
            review_orchestration_state_label(snapshot.state),
            snapshot.concerns.len(),
            snapshot.counts.finding_count.value(),
            snapshot.counts.judgment_member_count.value(),
            snapshot.counts.judgment_effect_applied_count.value(),
            snapshot.counts.repair_fixed_count.value(),
            snapshot.counts.publication_published_count.value(),
        )?;
        self.text_field("concern_set_version", &snapshot.concern_set_version)?;
        writeln!(
            self.stdout,
            "template_import_digest={} template_judgment_digest={} template_repair_digest={} \
             template_publication_digest={}",
            snapshot.stage_template_digests.import.as_str(),
            snapshot.stage_template_digests.judgment.as_str(),
            snapshot.stage_template_digests.repair.as_str(),
            snapshot.stage_template_digests.publication.as_str(),
        )?;
        for (index, concern) in snapshot.concerns.iter().enumerate() {
            writeln!(
                self.stdout,
                "concern_index={index} status={} pass={} template_digest={}",
                review_orchestration_concern_status_label(concern.status),
                concern
                    .pass_id
                    .map_or_else(|| String::from("-"), |id| id.to_string()),
                concern.template_digest.as_str(),
            )?;
            self.text_field("concern_key", &concern.key)?;
        }
        Ok(())
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
        self.text_field("provider", &target.provider)?;
        self.text_field("repository", &target.repository)?;
        self.text_field("head_revision", &target.head_revision)?;
        match target.base_revision.as_deref() {
            Some(base_revision) => {
                writeln!(self.stdout, "base_revision_present=true")?;
                self.text_field("base_revision", base_revision)
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
             diff_side={} severity={} is_real_confidence={} severity_label_confidence={}",
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
            finding.finding.is_real_confidence.value(),
            finding.finding.severity_label_confidence.value(),
        )?;
        self.text_field("file_path", &finding.finding.file_path)?;
        self.text_field("title", &finding.finding.title)?;
        self.text_field("body", &finding.finding.body)?;
        self.text_field("category", &finding.finding.category)?;
        match finding.finding.recommended_fix.as_deref() {
            Some(recommended_fix) => {
                writeln!(self.stdout, "recommended_fix_present=true")?;
                self.text_field("recommended_fix", recommended_fix)
            }
            None => writeln!(self.stdout, "recommended_fix_present=false"),
        }
    }

    fn text_field(&mut self, name: &str, value: &str) -> io::Result<()> {
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

    pub(crate) fn conversation_summary(&mut self, row: &ConversationRow<'_>) -> io::Result<()> {
        match row {
            ConversationRow::Native {
                session_id,
                archived,
                defaults_version,
                title,
            } => {
                let title = title.map(|title| self.render_field(title, TextField::TrailingOnLine));
                writeln!(
                    self.stdout,
                    "origin=native session_id={session_id} archived={archived} \
                     defaults_version={defaults_version} title={}",
                    title.unwrap_or_default()
                )
            }
            ConversationRow::Imported {
                imported_conversation_id,
                format,
                entry_count,
                title,
            } => {
                let title = title.map(|title| self.render_field(title, TextField::TrailingOnLine));
                writeln!(
                    self.stdout,
                    "origin=imported imported_conversation_id={imported_conversation_id} \
                     format={format} entry_count={entry_count} title={}",
                    title.unwrap_or_default()
                )
            }
        }
    }

    pub(crate) fn next_conversation_cursor(
        &mut self,
        origin_label: &str,
        conversation_id: CanonicalUuid,
    ) -> io::Result<()> {
        writeln!(self.stderr, "next_after={origin_label}:{conversation_id}")?;
        self.stderr.flush()
    }

    pub(crate) fn snapshot(
        &mut self,
        snapshot: &mut TranscriptSnapshot,
    ) -> Result<(), ClientError> {
        let mut rendered_snapshot = tempfile::tempfile()?;
        {
            let mut staged = Output::new(&mut rendered_snapshot, &mut *self.stderr, self.raw);
            staged.snapshot_runner(snapshot.runner())?;
            staged.render_snapshot(snapshot, None, SnapshotSelection::All, true)?;
            staged.render_usage(snapshot)?;
        }
        rendered_snapshot.seek(SeekFrom::Start(0))?;
        io::copy(&mut rendered_snapshot, &mut self.stdout)?;
        Ok(())
    }

    pub(crate) fn followed_snapshot(
        &mut self,
        snapshot: &mut TranscriptSnapshot,
        displayed: &mut SnapshotIdentitySet,
    ) -> Result<(), ClientError> {
        self.snapshot_runner(snapshot.runner())?;
        self.render_snapshot(snapshot, Some(displayed), SnapshotSelection::All, true)
    }

    fn snapshot_runner(&mut self, runner: Option<&RunnerProjection>) -> io::Result<()> {
        let Some(runner) = runner else {
            return Ok(());
        };
        write!(self.stdout, "runner_snapshot selector=")?;
        match runner.selector() {
            RunnerProjectionSelector::Runner { runner_id } => {
                write!(self.stdout, "runner selector_runner={runner_id}")?;
            }
            RunnerProjectionSelector::CapabilityClass { name } => write!(
                self.stdout,
                "capability_class selector_capability={}",
                self.render_field(name.as_str(), TextField::DelimitedOnLine)
            )?,
        }
        if let Some(runner_id) = runner.runner_id() {
            write!(self.stdout, " runner={runner_id}")?;
        }
        write!(
            self.stdout,
            " placement_revision={} sandbox={}",
            runner.placement_revision().value(),
            runner_sandbox_profile(runner.sandbox_profile())
        )?;
        if let Some(profile) = runner.credential_profile() {
            write!(
                self.stdout,
                " credential_profile={}",
                self.render_field(profile.as_str(), TextField::DelimitedOnLine)
            )?;
        }
        if let Some(repository) = runner.repository() {
            write!(
                self.stdout,
                " repository={}",
                self.render_field(repository.as_str(), TextField::DelimitedOnLine)
            )?;
        }
        if let Some(directory) = runner.working_directory() {
            write!(
                self.stdout,
                " working_directory={}",
                self.render_field(directory.as_str(), TextField::DelimitedOnLine)
            )?;
        }
        if let Some(health) = runner.connection_health() {
            write!(
                self.stdout,
                " connection_health={}",
                runner_connection_health(health)
            )?;
        }
        writeln!(
            self.stdout,
            " state={}",
            runner_projection_state(runner.state())
        )
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
        let mut rendered_usage = tempfile::tempfile()?;
        let mut current_turn: Option<(CanonicalUuid, UsageAggregate)> = None;
        let mut session_total = UsageAggregate::new()?;
        for record in snapshot.replay()? {
            let SnapshotRecord::ModelCallUsage(evidence) = record? else {
                continue;
            };
            if current_turn
                .as_ref()
                .is_some_and(|(turn, _)| *turn != evidence.turn_id)
            {
                let (turn, mut total) = current_turn.take().ok_or(ClientError::Protocol(
                    "token usage turn grouping was invalid",
                ))?;
                self.usage_lines(&mut rendered_usage, Some(turn), &mut total)?;
            }
            if current_turn.is_none() {
                current_turn = Some((evidence.turn_id, UsageAggregate::new()?));
            }
            let (_, turn_total) = current_turn.as_mut().ok_or(ClientError::Protocol(
                "token usage turn grouping was invalid",
            ))?;
            turn_total.add(&evidence)?;
            session_total.add(&evidence)?;
        }
        if let Some((turn, mut total)) = current_turn {
            self.usage_lines(&mut rendered_usage, Some(turn), &mut total)?;
        }
        self.usage_lines(&mut rendered_usage, None, &mut session_total)?;
        rendered_usage.seek(SeekFrom::Start(0))?;
        io::copy(&mut rendered_usage, &mut self.stdout)?;
        Ok(())
    }

    fn usage_lines<OutputWriter: Write>(
        &self,
        stdout: &mut OutputWriter,
        turn: Option<CanonicalUuid>,
        total: &mut UsageAggregate,
    ) -> Result<(), ClientError> {
        Self::usage_line(stdout, turn, UsageProvenance::Reported, total.reported)?;
        Self::usage_line(stdout, turn, UsageProvenance::Estimated, total.estimated)?;
        for index in 0..total.costs.capacity {
            let Some((key, cost)) = total.costs.entry_at(index)? else {
                continue;
            };
            let prefix = turn.map_or_else(
                || String::from("cost_total scope=session"),
                |turn| format!("cost turn={turn}"),
            );
            let rate_version = self.render_field(&key.rate_version, TextField::DelimitedOnLine);
            writeln!(
                stdout,
                "{prefix} usage_provenance={} label={} rate_version={} usd={} costed_calls={}",
                usage_provenance_label(key.provenance),
                cost_label(key.label),
                rate_version,
                cost.amount_usd.normalize(),
                cost.calls,
            )?;
        }
        Ok(())
    }

    fn usage_line<OutputWriter: Write>(
        stdout: &mut OutputWriter,
        turn: Option<CanonicalUuid>,
        provenance: UsageProvenance,
        total: TokenUsageTotal,
    ) -> io::Result<()> {
        let prefix = turn.map_or_else(
            || String::from("usage_total scope=session"),
            |turn| format!("usage turn={turn}"),
        );
        writeln!(
            stdout,
            "{prefix} usage_provenance={} terminal_calls={} input_tokens={} \
             input_tokens_present_calls={}/{} \
             output_tokens={} output_tokens_present_calls={}/{} \
             cache_creation_input_tokens={} \
             cache_creation_input_tokens_present_calls={}/{} cache_read_input_tokens={} \
             cache_read_input_tokens_present_calls={}/{}",
            usage_provenance_label(provenance),
            total.terminal_calls,
            total.input.label(),
            total.input.present_calls,
            total.terminal_calls,
            total.output.label(),
            total.output.present_calls,
            total.terminal_calls,
            total.cache_creation_input.label(),
            total.cache_creation_input.present_calls,
            total.terminal_calls,
            total.cache_read_input.label(),
            total.cache_read_input.present_calls,
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
            SessionEvent::SessionModelSettingsChanged {
                command_id,
                prior_defaults_version,
                installed_defaults_version,
                adjustments,
                ..
            } => writeln!(
                self.stdout,
                "event={cursor} session={session_id} session_model_settings_changed \
                 command={command_id} prior_defaults_version={} \
                 installed_defaults_version={} adjustment_count={}",
                prior_defaults_version.value(),
                installed_defaults_version.value(),
                adjustments.len()
            ),
            SessionEvent::TurnModelSettingsResolved {
                accepted_input_id,
                turn_id,
                defaults_version,
                adjustments,
                ..
            } => writeln!(
                self.stdout,
                "event={cursor} session={session_id} turn_model_settings_resolved \
                 accepted_input={accepted_input_id} turn={turn_id} defaults_version={} \
                 adjustment_count={}",
                defaults_version.value(),
                adjustments.len()
            ),
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
                self.user_content(content)
            }
            SessionEvent::GoalTurnRetired { turn_id } => writeln!(
                self.stdout,
                "event={cursor} session={session_id} goal_turn_retired turn={turn_id}"
            ),
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
            SessionEvent::RunnerStateTransition {
                runner_id,
                placement_revision,
                sandbox_profile,
                working_directory,
                state,
            } => match working_directory {
                Some(working_directory) => writeln!(
                    self.stdout,
                    "event={cursor} session={session_id} runner_state_transition \
                     runner={runner_id} placement_revision={} sandbox={} \
                     working_directory={} state={}",
                    placement_revision.value(),
                    runner_sandbox_profile(*sandbox_profile),
                    self.render_field(working_directory.as_str(), TextField::DelimitedOnLine,),
                    runner_state_transition_state(*state),
                ),
                None => writeln!(
                    self.stdout,
                    "event={cursor} session={session_id} runner_state_transition \
                     runner={runner_id} placement_revision={} sandbox={} state={}",
                    placement_revision.value(),
                    runner_sandbox_profile(*sandbox_profile),
                    runner_state_transition_state(*state),
                ),
            },
            SessionEvent::ToolApprovalDecided {
                turn_id,
                tool_request_id,
                decision,
                decider,
                rationale,
            } => {
                let (decision, denial_reason) = match decision {
                    ToolApprovalEventDecision::Approve {} => ("approve", None),
                    ToolApprovalEventDecision::Deny { reason } => ("deny", reason.as_deref()),
                };
                match decider {
                    ToolApprovalEventDecider::User { command_id } => writeln!(
                        self.stdout,
                        "event={cursor} session={session_id} tool_approval_decided \
                         turn={turn_id} request={tool_request_id} decision={decision} \
                         decider=user command={command_id}"
                    )?,
                    ToolApprovalEventDecider::Delegate {
                        model_selection_id,
                        model_call_id,
                    } => writeln!(
                        self.stdout,
                        "event={cursor} session={session_id} tool_approval_decided \
                         turn={turn_id} request={tool_request_id} decision={decision} \
                         decider=delegate model_selection={model_selection_id} \
                         call={model_call_id}"
                    )?,
                    ToolApprovalEventDecider::UserOverride {
                        command_id,
                        overridden_tool_request_id,
                    } => writeln!(
                        self.stdout,
                        "event={cursor} session={session_id} tool_approval_decided \
                         turn={turn_id} request={tool_request_id} decision={decision} \
                         decider=user_override command={command_id} \
                         overridden_request={overridden_tool_request_id}"
                    )?,
                }
                if let Some(reason) = denial_reason {
                    self.text_field("denial_reason", reason)?;
                }
                if let Some(rationale) = rationale {
                    self.text_field("rationale", rationale)?;
                }
                Ok(())
            }
            SessionEvent::ContextCompacted {
                context_compaction_id,
                model_call_id,
                through_position,
                summary_entry_id,
                result_frontier_id,
            } => writeln!(
                self.stdout,
                "event={cursor} session={session_id} context_compacted \
                 compaction={context_compaction_id} call={model_call_id} \
                 through={} summary_entry={summary_entry_id} \
                 frontier={result_frontier_id}",
                through_position.value()
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
            SessionEvent::ChildSpawned {
                spawning_request_id,
                child_session_id,
                relationship,
            } => match relationship {
                DelegationPolicy::Background {} => writeln!(
                    self.stdout,
                    "event={cursor} session={session_id} delegation_child_spawned \
                     spawning_request={spawning_request_id} child={child_session_id} \
                     policy=background"
                ),
                DelegationPolicy::Bound {
                    on_parent_stopped,
                    on_parent_cancelled,
                } => writeln!(
                    self.stdout,
                    "event={cursor} session={session_id} delegation_child_spawned \
                     spawning_request={spawning_request_id} child={child_session_id} \
                     policy=bound on_parent_stopped={} on_parent_cancelled={}",
                    bound_child_action(*on_parent_stopped),
                    bound_child_action(*on_parent_cancelled)
                ),
            },
            SessionEvent::ChildWaiting {
                await_request_id,
                spawning_request_id,
                child_session_id,
                mode,
            } => writeln!(
                self.stdout,
                "event={cursor} session={session_id} delegation_child_waiting \
                 spawning_request={spawning_request_id} child={child_session_id} \
                 await_request={await_request_id} mode={}",
                match mode {
                    signalbox_process_protocol::DelegationWaitMode::Foreground => "foreground",
                    signalbox_process_protocol::DelegationWaitMode::Background => "background",
                }
            ),
            SessionEvent::ChildLifecycleDisposition {
                spawning_request_id,
                child_session_id,
                outcome,
                reason,
                provenance,
            } => writeln!(
                self.stdout,
                "event={cursor} session={session_id} delegation_child_lifecycle_disposition \
                 spawning_request={spawning_request_id} child={child_session_id} \
                 outcome={} reason={} provenance={}",
                delegation_outcome(*outcome),
                delegation_reason(*reason),
                delegation_provenance(provenance)
            ),
            SessionEvent::ChildResult {
                spawning_request_id,
                child_session_id,
                outcome,
                reason,
                provenance,
                content,
            } => {
                writeln!(
                    self.stdout,
                    "event={cursor} session={session_id} delegation_child_result \
                     spawning_request={spawning_request_id} child={child_session_id} \
                     outcome={} reason={} provenance={} content_present={}",
                    delegation_outcome(*outcome),
                    delegation_reason(*reason),
                    delegation_provenance(provenance),
                    content.is_some()
                )?;
                if let Some(content) = content {
                    self.text(content)
                } else {
                    Ok(())
                }
            }
            SessionEvent::SessionMessage {
                spawning_request_id,
                message_id,
                sender_session_id,
                recipient_session_id,
                ordinal,
                delivery_sequence,
                content,
            } => {
                writeln!(
                    self.stdout,
                    "event={cursor} session={session_id} delegation_session_message \
                     spawning_request={spawning_request_id} message={message_id} \
                     sender={sender_session_id} recipient={recipient_session_id} \
                     ordinal={} delivery_sequence={}",
                    ordinal.value(),
                    delivery_sequence.value()
                )?;
                self.text(content)
            }
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

    fn user_content(&mut self, content: &UserInputContent) -> io::Result<()> {
        match content.parts() {
            [UserInputPart::Text { text }] => self.text(text),
            parts => {
                self.user_content_parts_json(parts)?;
                writeln!(self.stdout)?;
                if self.raw {
                    self.stdout.flush()?;
                }
                Ok(())
            }
        }
    }

    fn user_content_parts_json(&mut self, parts: &[UserInputPart]) -> io::Result<()> {
        let serialized = serde_json::to_string(parts)?;
        if self.raw {
            return self.stdout.write_all(serialized.as_bytes());
        }
        for character in serialized.chars() {
            let code = character as u32;
            if (0x7f..=0x9f).contains(&code) {
                write!(self.stdout, "\\u{code:04x}")?;
            } else {
                write!(self.stdout, "{character}")?;
            }
        }
        Ok(())
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
                self.user_content(content)
            }
            TurnState::QueuedDelegated {
                spawning_request_id,
                parent_session_id,
                parent_turn_id,
                content,
            } => {
                writeln!(
                    self.stdout,
                    "turn={turn_id} position={position} state=queued_delegated \
                     spawning_request={spawning_request_id} \
                     parent_session={parent_session_id} parent_turn={parent_turn_id}"
                )?;
                self.text(content.as_str())
            }
            TurnState::QueuedDelegationWake {
                first_delivery_sequence,
                through_delivery_sequence,
            } => writeln!(
                self.stdout,
                "turn={turn_id} position={position} state=queued_delegation_wake \
                 deliveries={}-{}",
                first_delivery_sequence.value(),
                through_delivery_sequence.value()
            ),
            TurnState::DelegationTerminated {
                spawning_request_id,
                outcome,
                reason,
                provenance,
            } => writeln!(
                self.stdout,
                "turn={turn_id} position={position} state=delegation_terminated \
                 spawning_request={spawning_request_id} outcome={} reason={} provenance={}",
                delegation_outcome(*outcome),
                delegation_reason(*reason),
                delegation_provenance(provenance)
            ),
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
                automatic_reconciliation_attempts,
                operator_action_required,
            } => {
                let recovery = if *operator_action_required {
                    "operator_required"
                } else {
                    "automatic"
                };
                writeln!(
                    self.stdout,
                    "turn={turn_id} position={position} \
                     state=active_awaiting_model_call_recovery \
                     attempt={ended_attempt_id} call={recovery_model_call_id} \
                     recovery={recovery} recovery_attempts={}",
                    automatic_reconciliation_attempts.value()
                )
            }
            TurnState::ActiveAwaitingToolApproval { tool_request_id } => writeln!(
                self.stdout,
                "turn={turn_id} position={position} state=active_awaiting_tool_approval \
                 request={tool_request_id}"
            ),
            TurnState::ActiveAwaitingChild {
                await_request_id,
                spawning_request_id,
                child_session_id,
            } => writeln!(
                self.stdout,
                "turn={turn_id} position={position} state=active_awaiting_child \
                 request={await_request_id} spawning_request={spawning_request_id} \
                 child={child_session_id}"
            ),
            TurnState::ActiveAwaitingToolRecovery {
                ended_attempt_id,
                recovery_tool_attempt_id,
                automatic_reconciliation_attempts,
                operator_action_required,
            } => {
                let recovery = if *operator_action_required {
                    "operator_required"
                } else {
                    "automatic"
                };
                writeln!(
                    self.stdout,
                    "turn={turn_id} position={position} state=active_awaiting_tool_recovery \
                     attempt={ended_attempt_id} tool_attempt={recovery_tool_attempt_id} \
                     recovery={recovery} recovery_attempts={}",
                    automatic_reconciliation_attempts.value()
                )
            }
            TurnState::ActiveAwaitingRunnerRecovery {
                runner_id,
                placement_revision,
                tool_attempt_id,
            } => match tool_attempt_id {
                Some(tool_attempt_id) => writeln!(
                    self.stdout,
                    "turn={turn_id} position={position} state=active_awaiting_runner_recovery \
                     runner={runner_id} placement_revision={} tool_attempt={tool_attempt_id}",
                    placement_revision.value()
                ),
                None => writeln!(
                    self.stdout,
                    "turn={turn_id} position={position} state=active_awaiting_runner_recovery \
                     runner={runner_id} placement_revision={} tool_attempt=none",
                    placement_revision.value()
                ),
            },
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
                     call_disposition={} call_cause={}",
                    call.model_call_id(),
                    failed_model_call_disposition(call.disposition()),
                    call.cause().map_or("none", failed_model_call_cause)
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
            SnapshotEntryKind::User {
                accepted_input_id,
                turn_id,
                content,
            } => {
                write!(
                    self.stdout,
                    "user_content source_session={} entry={} accepted_input={accepted_input_id} turn={turn_id} parts=",
                    entry.source_session_id, entry.entry_id
                )?;
                self.user_content_parts_json(content.parts())?;
                writeln!(self.stdout)?;
                if self.raw {
                    self.stdout.flush()?;
                }
                Ok(())
            }
            SnapshotEntryKind::Text(metadata) => {
                let label = match metadata {
                    TranscriptTextEntry::Assistant { turn_id, .. } => {
                        format!("assistant turn={turn_id}")
                    }
                    TranscriptTextEntry::ContextSummary {
                        model_call_id,
                        first_source_session_id,
                        first_entry_id,
                        through_source_session_id,
                        through_entry_id,
                    } => format!(
                        "context_summary model_call={model_call_id} range={first_source_session_id}/{first_entry_id}..={through_source_session_id}/{through_entry_id}"
                    ),
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
            SnapshotEntryKind::Marker(TranscriptEntry::DelegatedTask {
                spawning_request_id,
                parent_session_id,
                parent_turn_id,
                content,
            }) => writeln!(
                self.stdout,
                "delegated_task spawning_request={spawning_request_id} \
                 parent_session={parent_session_id} parent_turn={parent_turn_id} \
                 content={} source={} entry={}",
                self.render(content),
                entry.source_session_id,
                entry.entry_id
            ),
            SnapshotEntryKind::Marker(TranscriptEntry::DelegationMessage {
                spawning_request_id,
                message_id,
                sender_session_id,
                recipient_session_id,
                ordinal,
                delivery_sequence,
                content,
            }) => writeln!(
                self.stdout,
                "delegation_message spawning_request={spawning_request_id} message={message_id} \
                 sender={sender_session_id} recipient={recipient_session_id} \
                 ordinal={} delivery_sequence={} content={} source={} entry={}",
                ordinal.value(),
                delivery_sequence.value(),
                self.render(content),
                entry.source_session_id,
                entry.entry_id
            ),
            SnapshotEntryKind::Marker(TranscriptEntry::DelegationResult {
                await_request_id,
                spawning_request_id,
                child_session_id,
                mode,
                delivery_sequence,
                outcome,
                content,
                reason,
                provenance,
            }) => writeln!(
                self.stdout,
                "delegation_result await_request={await_request_id} \
                 spawning_request={spawning_request_id} child={child_session_id} mode={} \
                 delivery_sequence={} outcome={} content={} reason={} provenance={} \
                 source={} entry={}",
                delegation_wait_mode(*mode),
                delivery_sequence
                    .map(|sequence| sequence.value().to_string())
                    .unwrap_or_else(|| String::from("none")),
                delegation_outcome(*outcome),
                content
                    .as_deref()
                    .map(|value| self.render(value))
                    .unwrap_or_else(|| String::from("none")),
                delegation_reason(*reason),
                delegation_provenance(provenance),
                entry.source_session_id,
                entry.entry_id
            ),
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
            SnapshotEntryKind::Marker(TranscriptEntry::ProviderCompaction {
                turn_id,
                model_call_id,
            }) => writeln!(
                self.stdout,
                "provider_compaction turn={turn_id} call={model_call_id} source={} entry={}",
                entry.source_session_id, entry.entry_id
            ),
            SnapshotEntryKind::Marker(TranscriptEntry::AssistantToolUse {
                turn_id,
                model_call_id,
                tool_request_id,
                tool_name,
                arguments,
                approval,
            }) => {
                writeln!(
                    self.stdout,
                    "assistant_tool_use turn={turn_id} call={model_call_id} \
                     request={tool_request_id} name={} arguments={} source={} entry={}",
                    self.render(tool_name),
                    self.render(arguments),
                    entry.source_session_id,
                    entry.entry_id
                )?;
                if let Some(approval) = approval {
                    let (decision, reason) = match &approval.decision {
                        ToolApprovalEventDecision::Approve {} => ("approve", None),
                        ToolApprovalEventDecision::Deny { reason } => ("deny", reason.as_deref()),
                    };
                    match &approval.decider {
                        ToolApprovalEventDecider::User { command_id } => writeln!(
                            self.stdout,
                            "tool_approval request={tool_request_id} decision={decision} \
                             decider=user command={command_id}"
                        )?,
                        ToolApprovalEventDecider::Delegate {
                            model_selection_id,
                            model_call_id,
                        } => writeln!(
                            self.stdout,
                            "tool_approval request={tool_request_id} decision={decision} \
                             decider=delegate model_selection={model_selection_id} \
                             call={model_call_id}"
                        )?,
                        ToolApprovalEventDecider::UserOverride {
                            command_id,
                            overridden_tool_request_id,
                        } => writeln!(
                            self.stdout,
                            "tool_approval request={tool_request_id} decision={decision} \
                             decider=user_override command={command_id} \
                             overridden_request={overridden_tool_request_id}"
                        )?,
                    }
                    if let Some(reason) = reason {
                        self.text_field("denial_reason", reason)?;
                    }
                    if let Some(rationale) = &approval.rationale {
                        self.text_field("rationale", rationale)?;
                    }
                }
                Ok(())
            }
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

#[cfg(test)]
mod tests;
