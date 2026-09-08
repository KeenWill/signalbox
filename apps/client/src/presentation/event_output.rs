use super::*;

impl<'a> Output<'a> {
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
            SessionEvent::TurnCredentialPoolExhausted {
                turn_id,
                terminal_frontier_id,
                terminal_attempt_id,
                failure_entry_id,
                pool_policy_id,
                policy_members,
                members,
            } => {
                write!(
                    self.stdout,
                    "event={cursor} session={session_id} turn_credential_pool_exhausted turn={turn_id} frontier={terminal_frontier_id} attempt={terminal_attempt_id} entry={failure_entry_id} pool_policy={pool_policy_id} policy_members="
                )?;
                self.json_value(policy_members)?;
                write!(self.stdout, " members=")?;
                self.json_value(members)?;
                writeln!(self.stdout)
            }
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

    pub(super) fn text(&mut self, text: &str) -> io::Result<()> {
        self.text_fragment(text, true, text.ends_with('\n'))
    }

    pub(super) fn user_content(&mut self, content: &UserInputContent) -> io::Result<()> {
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

    pub(super) fn user_content_parts_json(&mut self, parts: &[UserInputPart]) -> io::Result<()> {
        self.json_value(parts)
    }

    pub(super) fn json_value<T: serde::Serialize + ?Sized>(&mut self, value: &T) -> io::Result<()> {
        let serialized = serde_json::to_string(value)?;
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

    pub(super) fn text_fragment(
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
}
