use super::*;

impl<'a> Output<'a> {
    pub(super) fn snapshot_turn(&mut self, turn: &TranscriptTurn) -> io::Result<()> {
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

    pub(super) fn snapshot_entry(&mut self, entry: &SnapshotEntry) -> io::Result<()> {
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
            SnapshotEntryKind::Marker(TranscriptEntry::RunnerPlacementChanged {
                placement_revision,
            }) => writeln!(
                self.stdout,
                "runner_placement_changed revision={} source={} entry={}",
                placement_revision.value(),
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
}
