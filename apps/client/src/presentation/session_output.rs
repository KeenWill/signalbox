use super::*;
use crate::conversation_import::ConversationImportDropFacts;
use signalbox_process_protocol::CanonicalU64;

impl<'a> Output<'a> {
    pub(crate) fn termination_receipt(
        &mut self,
        receipt: signalbox_process_protocol::TerminationReceipt,
    ) -> io::Result<()> {
        let scope = match receipt.descendant_scope {
            signalbox_process_protocol::DescendantTerminationScope::ParentAlone => "parent_alone",
            signalbox_process_protocol::DescendantTerminationScope::ParentAndDescendants => {
                "parent_and_descendants"
            }
        };
        writeln!(
            self.stdout,
            "descendant_scope={scope} descendant_count={}",
            receipt.descendant_count.value()
        )
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

    pub(crate) fn configuration_reloaded(
        &mut self,
        sections: &[signalbox_process_protocol::ReloadedSection],
    ) -> io::Result<()> {
        use signalbox_process_protocol::ReloadedSection;
        write!(self.stdout, "reloaded")?;
        for section in sections {
            write!(
                self.stdout,
                " {}",
                match section {
                    ReloadedSection::ModelCatalog => "model_catalog",
                    ReloadedSection::SessionTemplates => "session_templates",
                    ReloadedSection::RepoWatch => "repo_watch",
                }
            )?;
        }
        writeln!(self.stdout)
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
                "goal session={session_id} generation={generation} state=achieved turn={turn_id} request={}",
                tool_request_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "daemon".to_owned())
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
                    "event={ordinal} generation={generation} type=achieved turn={turn_id} request={}",
                    tool_request_id
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| "daemon".to_owned())
                )?;
                self.goal_text_field("report", report)
            }
            GoalHistoryEvent::UserStopped {
                command_id,
                settling_turn_id,
                abandoned_actions,
            } => {
                write!(
                    self.stdout,
                    "event={ordinal} generation={generation} type=user_stopped command={}",
                    command_id.into_uuid().hyphenated()
                )?;
                if let Some(turn) = settling_turn_id {
                    write!(self.stdout, " turn={turn}")?;
                }
                match abandoned_actions {
                    Some(count) => writeln!(self.stdout, " abandoned_actions={}", count.value()),
                    None => writeln!(self.stdout, " settlement=pending"),
                }
            }
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
        dropped: ConversationImportDropFacts,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "inserted imported_conversation_id={imported_conversation_id} dropped_record_count={} first_dropped_record_position={}",
            dropped.dropped_record_count.value(),
            optional_import_position(dropped.first_dropped_record_position),
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
        dropped: ConversationImportDropFacts,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "already_imported imported_conversation_id={imported_conversation_id} dropped_record_count={} first_dropped_record_position={}",
            dropped.dropped_record_count.value(),
            optional_import_position(dropped.first_dropped_record_position),
        )
    }

    pub(crate) fn conversation_import_scan_inserted(
        &mut self,
        path: &Path,
        imported_conversation_id: CanonicalUuid,
        dropped: ConversationImportDropFacts,
    ) -> io::Result<()> {
        let path = self.render(&format!("{path:?}"));
        writeln!(
            self.stdout,
            "imported path={path} imported_conversation_id={imported_conversation_id} dropped_record_count={} first_dropped_record_position={}",
            dropped.dropped_record_count.value(),
            optional_import_position(dropped.first_dropped_record_position),
        )
    }

    pub(crate) fn conversation_import_scan_already_imported(
        &mut self,
        path: &Path,
        imported_conversation_id: CanonicalUuid,
        dropped: ConversationImportDropFacts,
    ) -> io::Result<()> {
        let path = self.render(&format!("{path:?}"));
        writeln!(
            self.stdout,
            "already_imported path={path} imported_conversation_id={imported_conversation_id} dropped_record_count={} first_dropped_record_position={}",
            dropped.dropped_record_count.value(),
            optional_import_position(dropped.first_dropped_record_position),
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
    pub(crate) fn imported_conversation_entry_count(
        &mut self,
        entry_count: u64,
        dropped: ConversationImportDropFacts,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "entry_count={entry_count} dropped_record_count={} first_dropped_record_position={}",
            dropped.dropped_record_count.value(),
            optional_import_position(dropped.first_dropped_record_position),
        )
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
            session_supervision,
            outbox_quarantines,
        } = counts;
        writeln!(
            self.stdout,
            "status lifecycle_weeks={lifecycle_weeks} \
             nonterminal_past_deadline={lifecycle_deadline_violations} \
             session_supervision={session_supervision} outbox_quarantine={outbox_quarantines}"
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
            OperatorStatusMessage::UnavailableComponent(item) => {
                writeln!(
                    self.stdout,
                    "unavailable_component component={} cause={}",
                    self.render_field(&item.component, TextField::DelimitedOnLine),
                    item.cause
                )?;
                Ok(())
            }
            OperatorStatusMessage::SessionSupervision(item) => {
                writeln!(
                    self.stdout,
                    "session_supervision {}",
                    serde_json::to_string(item).map_err(std::io::Error::other)?
                )?;
                Ok(())
            }
            OperatorStatusMessage::RepositoryIngestion(item) => {
                writeln!(
                    self.stdout,
                    "repository_ingestion {}",
                    serde_json::to_string(item).map_err(std::io::Error::other)?
                )?;
                Ok(())
            }
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

    pub(crate) fn tool_denial_overridden(
        &mut self,
        tool_request_id: CanonicalUuid,
    ) -> io::Result<()> {
        writeln!(self.stdout, "tool_request={tool_request_id} override=armed")?;
        writeln!(
            self.stderr,
            "One-shot override armed; it applies to a later matching proposal after one extra model round."
        )
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

    pub(crate) fn assistant_text_fragment(
        &mut self,
        fragment: &str,
        final_fragment: bool,
        content_ends_with_newline: bool,
    ) -> io::Result<()> {
        self.text_fragment(fragment, final_fragment, content_ends_with_newline)
    }
}
fn optional_import_position(position: Option<CanonicalU64>) -> String {
    position.map_or_else(
        || String::from("none"),
        |position| position.value().to_string(),
    )
}
