-- Foreign keys that point forward across the domain files: each constraint
-- below references a table created in a later file than its own, so it can
-- only be added once both sides exist. Every other foreign key lives in the
-- file of the table that declares it.

--
-- Name: accepted_input accepted_input_consuming_call_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY accepted_input
    ADD CONSTRAINT accepted_input_consuming_call_fk FOREIGN KEY (consuming_model_call_id, expected_active_turn_id, session_id) REFERENCES model_call(model_call_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: accepted_input_content_part accepted_input_content_part_blob_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY accepted_input_content_part
    ADD CONSTRAINT accepted_input_content_part_blob_fk FOREIGN KEY (blob_digest) REFERENCES blob(digest) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: automatic_reconciliation automatic_model_call_reconcil_model_call_id_turn_id_sessio_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY automatic_reconciliation
    ADD CONSTRAINT automatic_model_call_reconcil_model_call_id_turn_id_sessio_fkey FOREIGN KEY (model_call_id, turn_id, session_id) REFERENCES model_call(model_call_id, turn_id, session_id);


--
-- Name: automatic_reconciliation automatic_reconciliation_tool_attempt; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY automatic_reconciliation
    ADD CONSTRAINT automatic_reconciliation_tool_attempt FOREIGN KEY (tool_attempt_id, turn_id, session_id) REFERENCES tool_attempt(attempt_id, turn_id, session_id);


--
-- Name: compact_session_command compact_session_command_automatic_turn_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY compact_session_command
    ADD CONSTRAINT compact_session_command_automatic_turn_fk FOREIGN KEY (automatic_for_turn_id, session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: compact_session_command compact_session_command_call_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY compact_session_command
    ADD CONSTRAINT compact_session_command_call_fk FOREIGN KEY (model_call_id, session_id) REFERENCES context_compaction_model_call(model_call_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: compact_session_command compact_session_command_compaction_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY compact_session_command
    ADD CONSTRAINT compact_session_command_compaction_fk FOREIGN KEY (result_context_compaction_id, session_id) REFERENCES context_compaction(context_compaction_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: compact_session_command compact_session_command_frontier_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY compact_session_command
    ADD CONSTRAINT compact_session_command_frontier_fk FOREIGN KEY (session_id, result_frontier_id) REFERENCES context_frontier(owning_session_id, context_frontier_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: compact_session_command compact_session_command_summary_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY compact_session_command
    ADD CONSTRAINT compact_session_command_summary_fk FOREIGN KEY (session_id, result_summary_entry_id) REFERENCES semantic_transcript_entry(source_session_id, semantic_entry_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: create_session_from_imported_frontier_command create_session_from_imported_frontier_command_frontier_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY create_session_from_imported_frontier_command
    ADD CONSTRAINT create_session_from_imported_frontier_command_frontier_fk FOREIGN KEY (imported_conversation_id, imported_frontier_entry_id, imported_frontier_position) REFERENCES imported_transcript_entry(imported_conversation_id, imported_transcript_entry_id, imported_entry_position) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_model_credential_record delegated_session_credential_relation_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_model_credential_record
    ADD CONSTRAINT delegated_session_credential_relation_fk FOREIGN KEY (provenance_tool_request_id, session_id) REFERENCES session_delegation(spawning_tool_request_id, child_session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: model_call model_call_instruction_manifest_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY model_call
    ADD CONSTRAINT model_call_instruction_manifest_fk FOREIGN KEY (turn_instruction_manifest_id, session_id, turn_id) REFERENCES turn_instruction_manifest(turn_instruction_manifest_id, session_id, turn_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: model_call_user_override model_call_user_override_recorded_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY model_call_user_override
    ADD CONSTRAINT model_call_user_override_recorded_fk FOREIGN KEY (denied_request_id) REFERENCES tool_approval_user_override(denied_request_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: outbox_event outbox_event_session_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY outbox_event
    ADD CONSTRAINT outbox_event_session_fk FOREIGN KEY (session_id) REFERENCES session(session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: semantic_transcript_entry semantic_transcript_entry_context_summary_call_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_context_summary_call_fk FOREIGN KEY (context_summary_producing_call_id, source_session_id) REFERENCES context_compaction_model_call(model_call_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: semantic_transcript_entry semantic_transcript_entry_delegated_task_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_delegated_task_fk FOREIGN KEY (delegated_task_spawning_tool_request_id, source_session_id, semantic_entry_id) REFERENCES session_delegation_initial_task(spawning_tool_request_id, child_session_id, semantic_entry_id) ON UPDATE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: semantic_transcript_entry semantic_transcript_entry_delegation_message_delivery_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_delegation_message_delivery_fk FOREIGN KEY (delegation_message_id, source_session_id) REFERENCES session_message_delivery(message_id, recipient_session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: semantic_transcript_entry semantic_transcript_entry_delegation_result_delivery_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_delegation_result_delivery_fk FOREIGN KEY (delegation_result_awaiting_tool_request_id, delegation_result_spawning_tool_request_id, source_session_id) REFERENCES session_child_result_delivery(awaiting_tool_request_id, spawning_tool_request_id, parent_session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: semantic_transcript_entry semantic_transcript_entry_imported_entry_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_imported_entry_fk FOREIGN KEY (imported_conversation_id, imported_transcript_entry_id) REFERENCES imported_transcript_entry(imported_conversation_id, imported_transcript_entry_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: semantic_transcript_entry semantic_transcript_entry_producing_call_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_producing_call_fk FOREIGN KEY (producing_model_call_id, source_session_id) REFERENCES model_call(model_call_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: semantic_transcript_entry semantic_transcript_entry_tool_result_attempt_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_tool_result_attempt_fk FOREIGN KEY (tool_result_attempt_id, source_session_id) REFERENCES tool_attempt(attempt_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: semantic_transcript_entry semantic_transcript_entry_tool_result_request_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_tool_result_request_fk FOREIGN KEY (tool_result_request_id, source_session_id) REFERENCES tool_request(request_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: semantic_transcript_entry semantic_transcript_entry_tool_use_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_tool_use_fk FOREIGN KEY (assistant_tool_request_id, producing_model_call_id, source_session_id) REFERENCES tool_request(request_id, producing_model_call_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session session_delegation_relation_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session
    ADD CONSTRAINT session_delegation_relation_fk FOREIGN KEY (spawning_tool_request_id, session_id) REFERENCES session_delegation(spawning_tool_request_id, child_session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session session_imported_frontier_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session
    ADD CONSTRAINT session_imported_frontier_fk FOREIGN KEY (imported_conversation_id, imported_frontier_entry_id, imported_frontier_position) REFERENCES imported_transcript_entry(imported_conversation_id, imported_transcript_entry_id, imported_entry_position) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_model_credential_record session_model_credential_record_provenance_tool_request_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_model_credential_record
    ADD CONSTRAINT session_model_credential_record_provenance_tool_request_id_fkey FOREIGN KEY (provenance_tool_request_id) REFERENCES tool_request(request_id);


--
-- Name: session_plan_event session_plan_event_provenance_attempt_id_provenance_reques_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_plan_event
    ADD CONSTRAINT session_plan_event_provenance_attempt_id_provenance_reques_fkey FOREIGN KEY (provenance_attempt_id, provenance_request_id, provenance_issuing_turn_attempt_id, provenance_dispatch_generation) REFERENCES tool_attempt(attempt_id, request_id, issuing_turn_attempt_id, dispatch_generation) ON DELETE RESTRICT;


--
-- Name: session_plan_event session_plan_event_provenance_attempt_id_provenance_turn_i_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_plan_event
    ADD CONSTRAINT session_plan_event_provenance_attempt_id_provenance_turn_i_fkey FOREIGN KEY (provenance_attempt_id, provenance_turn_id, session_id) REFERENCES tool_attempt(attempt_id, turn_id, session_id) ON DELETE RESTRICT;


--
-- Name: session session_scheduler_row_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session
    ADD CONSTRAINT session_scheduler_row_fk FOREIGN KEY (session_id) REFERENCES session_scheduler(session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session session_spawning_request_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session
    ADD CONSTRAINT session_spawning_request_fk FOREIGN KEY (spawning_tool_request_id) REFERENCES tool_request(request_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: tool_attempt tool_attempt_child_wait_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_attempt
    ADD CONSTRAINT tool_attempt_child_wait_fk FOREIGN KEY (request_id, wait_spawning_request_id, wait_child_session_id) REFERENCES session_delegation_wait(awaiting_tool_request_id, spawning_tool_request_id, child_session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_cancelled_outbox_event turn_cancelled_outbox_frontier_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_cancelled_outbox_event
    ADD CONSTRAINT turn_cancelled_outbox_frontier_fk FOREIGN KEY (session_id, terminal_frontier_id) REFERENCES context_frontier(owning_session_id, context_frontier_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_completed_outbox_event turn_completed_outbox_call_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_completed_outbox_event
    ADD CONSTRAINT turn_completed_outbox_call_fk FOREIGN KEY (model_call_id, turn_id, session_id) REFERENCES model_call(model_call_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_completed_outbox_event turn_completed_outbox_frontier_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_completed_outbox_event
    ADD CONSTRAINT turn_completed_outbox_frontier_fk FOREIGN KEY (session_id, terminal_frontier_id) REFERENCES context_frontier(owning_session_id, context_frontier_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_failed_outbox_event turn_failed_outbox_event_terminal_frontier_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_failed_outbox_event
    ADD CONSTRAINT turn_failed_outbox_event_terminal_frontier_fk FOREIGN KEY (session_id, terminal_frontier_id) REFERENCES context_frontier(owning_session_id, context_frontier_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_lifecycle turn_lifecycle_active_tool_round_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_active_tool_round_fk FOREIGN KEY (active_tool_round_call_id, turn_id, session_id) REFERENCES tool_round(producing_model_call_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_lifecycle turn_lifecycle_approval_tool_request_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_approval_tool_request_fk FOREIGN KEY (approval_tool_request_id, turn_id, session_id) REFERENCES tool_request(request_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_lifecycle turn_lifecycle_child_wait_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_child_wait_fk FOREIGN KEY (child_wait_request_id, turn_id, session_id) REFERENCES session_delegation_wait(awaiting_tool_request_id, parent_turn_id, parent_session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_lifecycle turn_lifecycle_recovery_call_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_recovery_call_fk FOREIGN KEY (recovery_model_call_id, turn_id, session_id) REFERENCES model_call(model_call_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_lifecycle turn_lifecycle_recovery_tool_attempt_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_recovery_tool_attempt_fk FOREIGN KEY (recovery_tool_attempt_id, turn_id, session_id) REFERENCES tool_attempt(attempt_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_lifecycle turn_lifecycle_runner_recovery_tool_attempt_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_runner_recovery_tool_attempt_fk FOREIGN KEY (runner_recovery_tool_attempt_id, session_id) REFERENCES tool_attempt(attempt_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_lifecycle turn_lifecycle_starting_frontier_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_starting_frontier_fk FOREIGN KEY (session_id, starting_frontier_id) REFERENCES context_frontier(owning_session_id, context_frontier_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_lifecycle turn_lifecycle_terminal_call_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_terminal_call_fk FOREIGN KEY (terminal_model_call_id, turn_id, session_id) REFERENCES model_call(model_call_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_lifecycle turn_lifecycle_terminal_frontier_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_terminal_frontier_fk FOREIGN KEY (session_id, terminal_frontier_id) REFERENCES context_frontier(owning_session_id, context_frontier_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_lifecycle turn_lifecycle_terminal_tool_attempt_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_terminal_tool_attempt_fk FOREIGN KEY (terminal_tool_attempt_id, turn_id, session_id) REFERENCES tool_attempt(attempt_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_reconciliation_required_outbox_event turn_reconciliation_required_outbox_call_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_reconciliation_required_outbox_event
    ADD CONSTRAINT turn_reconciliation_required_outbox_call_fk FOREIGN KEY (model_call_id, turn_id, session_id) REFERENCES model_call(model_call_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_reconciliation_required_outbox_event turn_reconciliation_required_outbox_frontier_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_reconciliation_required_outbox_event
    ADD CONSTRAINT turn_reconciliation_required_outbox_frontier_fk FOREIGN KEY (session_id, terminal_frontier_id) REFERENCES context_frontier(owning_session_id, context_frontier_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_reconciliation_required_outbox_event turn_reconciliation_required_outbox_tool_attempt_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_reconciliation_required_outbox_event
    ADD CONSTRAINT turn_reconciliation_required_outbox_tool_attempt_fk FOREIGN KEY (tool_attempt_id, turn_id, session_id) REFERENCES tool_attempt(attempt_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_refused_outbox_event turn_refused_outbox_call_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_refused_outbox_event
    ADD CONSTRAINT turn_refused_outbox_call_fk FOREIGN KEY (model_call_id, turn_id, session_id) REFERENCES model_call(model_call_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_refused_outbox_event turn_refused_outbox_frontier_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_refused_outbox_event
    ADD CONSTRAINT turn_refused_outbox_frontier_fk FOREIGN KEY (session_id, terminal_frontier_id) REFERENCES context_frontier(owning_session_id, context_frontier_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_runner_recovery_interrupt_effect turn_runner_recovery_interrup_interrupted_tool_attempt_id__fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_runner_recovery_interrupt_effect
    ADD CONSTRAINT turn_runner_recovery_interrup_interrupted_tool_attempt_id__fkey FOREIGN KEY (interrupted_tool_attempt_id, session_id) REFERENCES tool_attempt(attempt_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_runner_recovery_interrupt_effect turn_runner_recovery_interrup_session_id_placement_event_o_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_runner_recovery_interrupt_effect
    ADD CONSTRAINT turn_runner_recovery_interrup_session_id_placement_event_o_fkey FOREIGN KEY (session_id, placement_event_ordinal) REFERENCES runner_session_placement_record(session_id, event_ordinal) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_runner_recovery_interrupt_effect turn_runner_recovery_interrup_session_id_source_frontier_i_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_runner_recovery_interrupt_effect
    ADD CONSTRAINT turn_runner_recovery_interrup_session_id_source_frontier_i_fkey FOREIGN KEY (session_id, source_frontier_id) REFERENCES context_frontier(owning_session_id, context_frontier_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;
