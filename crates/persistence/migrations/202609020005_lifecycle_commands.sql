--
-- Session lifecycle §7: the command surface, finish conditions, and the
-- authenticated issuer on the durable-command envelope (§6).
--
-- Columns added to populated tables are backfilled from the evidence the
-- rows already carry before any constraint reads them.
--

SET check_function_bodies = false;

--
-- §6: every command records the principal that issued it. A module principal
-- is stamped by the in-daemon module path that composed the command; the
-- backfill derives it from the dispatch records that name those commands and
-- reads every other claim as the operator's.
--

ALTER TABLE durable_command
    ADD COLUMN issuer_kind text,
    ADD COLUMN issuer_module text;

UPDATE durable_command AS command
   SET issuer_kind = 'module', issuer_module = 'commissioned_dispatch'
  FROM commissioned_dispatch AS dispatch
 WHERE dispatch.create_command_id = command.command_id;

UPDATE durable_command AS command
   SET issuer_kind = 'module', issuer_module = 'repo_watch'
 WHERE command.issuer_kind IS NULL
   AND (
        EXISTS (SELECT 1 FROM repo_watch_dispatch_action AS action
                 WHERE action.create_command_id = command.command_id)
        OR EXISTS (SELECT 1 FROM repo_watch_dispatch_delivery AS delivery
                    WHERE delivery.submit_command_id = command.command_id)
        OR EXISTS (SELECT 1 FROM repo_watch_dispatch_delivery_intent AS intent
                    WHERE intent.submit_command_id = command.command_id)
        OR EXISTS (SELECT 1 FROM repo_watch_dispatch_start_lease_expiration AS lease
                    WHERE lease.goal_command_id = command.command_id)
        OR EXISTS (SELECT 1 FROM repo_watch_lifecycle_cutoff_goal AS cutoff
                    WHERE cutoff.goal_command_id = command.command_id)
        OR EXISTS (SELECT 1 FROM repo_watch_convergence_cutoff_goal AS cutoff
                    WHERE cutoff.goal_command_id = command.command_id)
        OR EXISTS (SELECT 1 FROM convergence_sweep_target AS target
                    WHERE target.pending_command_id = command.command_id)
        -- The goal a dispatch attaches names the dispatched session.
        OR EXISTS (SELECT 1 FROM goal_command AS goal
                     JOIN repo_watch_dispatch_action AS action
                       ON action.session_id = goal.session_id
                    WHERE goal.command_id = command.command_id
                      AND goal.operation_kind = 'attach')
       );

UPDATE durable_command
   SET issuer_kind = 'operator'
 WHERE issuer_kind IS NULL;

ALTER TABLE durable_command
    ALTER COLUMN issuer_kind SET NOT NULL;

ALTER TABLE durable_command
    ADD CONSTRAINT durable_command_issuer_shape CHECK (
        (issuer_kind = ANY (ARRAY[
            'core'::text, 'operator'::text, 'module'::text, 'watchdog'::text
        ]))
        AND ((issuer_kind = 'module'::text) = (issuer_module IS NOT NULL))
        AND ((issuer_module IS NULL) OR (issuer_module = ANY (ARRAY[
            'repo_watch'::text,
            'commissioned_dispatch'::text
        ])))
    );

--
-- One registry kind carries the seven §7 lifecycle operations, the way `goal`
-- carries its four.
--

-- Supersedes 202609010000_core.
ALTER TABLE durable_command
    DROP CONSTRAINT durable_command_kind_closed;

ALTER TABLE durable_command
    ADD CONSTRAINT durable_command_kind_closed CHECK (
        command_kind = ANY (ARRAY[
            'create_session'::text,
            'create_session_from_imported_frontier'::text,
            'replace_session_defaults'::text,
            'replace_session_metadata'::text,
            'submit_input'::text,
            'decide_tool_request'::text,
            'override_denied_tool_request'::text,
            'review_workflow'::text,
            'review_orchestration'::text,
            'compact_session'::text,
            'goal'::text,
            'update_session_placement'::text,
            'register_workspace'::text,
            'mint_git_remote'::text,
            'withdraw_git_remote'::text,
            'session_lifecycle'::text
        ])
    );

-- Supersedes 202609010000_core.
ALTER TABLE durable_command
    DROP CONSTRAINT durable_command_storage_version_supported;

ALTER TABLE durable_command
    ADD CONSTRAINT durable_command_storage_version_supported CHECK (
        ((command_kind = 'create_session'::text)
            AND (storage_version = ANY (ARRAY[1, 2, 3, 4, 5, 6, 7])))
        OR ((command_kind = 'replace_session_defaults'::text)
            AND (storage_version = ANY (ARRAY[1, 2, 3, 4])))
        OR ((command_kind = 'create_session_from_imported_frontier'::text)
            AND (storage_version = ANY (ARRAY[1, 2, 3, 5])))
        OR ((command_kind = 'submit_input'::text) AND (storage_version = 3))
        OR ((command_kind = ANY (ARRAY[
                'replace_session_metadata'::text,
                'decide_tool_request'::text,
                'override_denied_tool_request'::text,
                'review_workflow'::text,
                'review_orchestration'::text,
                'compact_session'::text,
                'goal'::text,
                'update_session_placement'::text,
                'register_workspace'::text,
                'mint_git_remote'::text,
                'withdraw_git_remote'::text,
                'session_lifecycle'::text
            ])) AND (storage_version = 1))
    );

CREATE TABLE session_lifecycle_command (
    command_id uuid NOT NULL,
    command_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    operation_kind text NOT NULL,
    stop_sticky boolean,
    descendant_scope text,
    successor_session_id uuid,
    failure_cause_kind text,
    finish_condition_kind text,
    finish_condition text,
    result_kind text NOT NULL,
    rejection_kind text,

    CONSTRAINT session_lifecycle_command_pkey PRIMARY KEY (command_id),
    CONSTRAINT session_lifecycle_command_kind_closed
        CHECK (command_kind = 'session_lifecycle'::text),
    CONSTRAINT session_lifecycle_command_storage_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT session_lifecycle_command_operation_closed CHECK (
        operation_kind = ANY (ARRAY[
            'stop'::text,
            'supersede'::text,
            'abandon'::text,
            'close_failed'::text,
            'resume'::text,
            'adopt'::text,
            'release'::text
        ])
    ),
    -- Each operation carries exactly its own members.
    CONSTRAINT session_lifecycle_command_operation_shape CHECK (
        ((operation_kind = 'stop'::text)
            = ((stop_sticky IS NOT NULL) AND (descendant_scope IS NOT NULL)))
        AND ((operation_kind = 'supersede'::text) = (successor_session_id IS NOT NULL))
        AND ((failure_cause_kind IS NULL) OR (operation_kind = 'close_failed'::text))
        AND ((finish_condition_kind IS NULL) OR (operation_kind = 'adopt'::text))
        AND ((descendant_scope IS NULL) OR (descendant_scope = ANY (ARRAY[
            'parent_alone'::text, 'parent_and_descendants'::text
        ])))
        AND ((successor_session_id IS NULL) OR (successor_session_id <> session_id))
    ),
    CONSTRAINT session_lifecycle_command_failure_cause_closed CHECK (
        (failure_cause_kind IS NULL)
        OR (failure_cause_kind = ANY (ARRAY[
            'provider_transient'::text,
            'provider_quota_exhausted'::text,
            'provider_overloaded'::text,
            'infrastructure_failure'::text,
            'retry_budget_exhausted'::text,
            'context_compaction_wall'::text,
            'context_headroom_exhausted'::text,
            'broken_toolchain'::text,
            'moderation_block'::text
        ]))
    ),
    CONSTRAINT session_lifecycle_command_finish_condition_shape CHECK (
        ((finish_condition_kind IS NULL)
            OR (finish_condition_kind = ANY (ARRAY['external_gate'::text, 'declared'::text])))
        AND ((finish_condition_kind = 'declared'::text) = (finish_condition IS NOT NULL))
        AND ((finish_condition IS NULL)
             OR ((octet_length(finish_condition) >= 1)
                 AND (octet_length(finish_condition) <= 1048576)))
    ),
    CONSTRAINT session_lifecycle_command_result_shape CHECK (
        (result_kind = ANY (ARRAY['applied'::text, 'rejected'::text]))
        AND ((result_kind = 'rejected'::text) = (rejection_kind IS NOT NULL))
    ),
    CONSTRAINT session_lifecycle_command_rejection_closed CHECK (
        (rejection_kind IS NULL)
        OR (rejection_kind = ANY (ARRAY[
            'session_not_found'::text,
            'transition_not_admitted'::text,
            'requires_parked'::text,
            'release_while_parked'::text,
            'ownership_unchanged'::text,
            'finish_condition_required'::text,
            'finish_condition_already_declared'::text,
            'standing_cause_mismatch'::text,
            'successor_not_found'::text,
            'goal_resume_required'::text,
            'goal_outcome_mismatch'::text,
            'pending_terminal_conflict'::text
        ]))
    ),
    -- Each rejection belongs to the operations that can raise it.
    CONSTRAINT session_lifecycle_command_rejection_operation CHECK (
        (result_kind = 'applied'::text)
        OR (rejection_kind = ANY (ARRAY[
            'session_not_found'::text, 'transition_not_admitted'::text
        ]))
        OR ((operation_kind = ANY (ARRAY[
                'stop'::text, 'supersede'::text, 'abandon'::text, 'close_failed'::text
            ])) AND (rejection_kind = ANY (ARRAY[
                'pending_terminal_conflict'::text, 'goal_outcome_mismatch'::text
            ])))
        OR ((operation_kind = ANY (ARRAY[
                'abandon'::text, 'close_failed'::text, 'resume'::text
            ])) AND (rejection_kind = 'requires_parked'::text))
        OR ((operation_kind = 'close_failed'::text)
            AND (rejection_kind = 'standing_cause_mismatch'::text))
        OR ((operation_kind = 'supersede'::text)
            AND (rejection_kind = 'successor_not_found'::text))
        OR ((operation_kind = 'resume'::text)
            AND (rejection_kind = 'goal_resume_required'::text))
        OR ((operation_kind = 'adopt'::text) AND (rejection_kind = ANY (ARRAY[
                'ownership_unchanged'::text,
                'finish_condition_required'::text,
                'finish_condition_already_declared'::text
            ])))
        OR ((operation_kind = 'release'::text) AND (rejection_kind = ANY (ARRAY[
                'ownership_unchanged'::text, 'release_while_parked'::text
            ])))
    ),
    CONSTRAINT session_lifecycle_command_registry_fk
        FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command (command_id, command_kind, storage_version)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT session_lifecycle_command_successor_fk
        FOREIGN KEY (successor_session_id) REFERENCES session (session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
);

CREATE INDEX session_lifecycle_command_by_session
    ON session_lifecycle_command (session_id, command_id);

CREATE TRIGGER session_lifecycle_command_is_append_only
    BEFORE DELETE OR UPDATE ON session_lifecycle_command
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

-- A rejected command may name a session that does not exist, so the session
-- reference is checked by the applying transaction rather than a key.

CREATE OR REPLACE FUNCTION require_durable_command_typed_record() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE matching_records bigint;
BEGIN
    IF NEW.command_kind <> 'review_orchestration' AND EXISTS (
        SELECT 1 FROM review_orchestration_command_recovery
         WHERE command_id = NEW.command_id
    ) THEN
        RAISE EXCEPTION 'durable command % is reserved by review orchestration recovery', NEW.command_id
            USING ERRCODE = '23505';
    END IF;
    CASE NEW.command_kind
        WHEN 'create_session' THEN SELECT count(*) INTO matching_records FROM create_session_command WHERE command_id = NEW.command_id;
        WHEN 'create_session_from_imported_frontier' THEN SELECT count(*) INTO matching_records FROM create_session_from_imported_frontier_command WHERE command_id = NEW.command_id;
        WHEN 'replace_session_defaults' THEN SELECT count(*) INTO matching_records FROM replace_session_defaults_command WHERE command_id = NEW.command_id;
        WHEN 'replace_session_metadata' THEN SELECT count(*) INTO matching_records FROM replace_session_metadata_command WHERE command_id = NEW.command_id;
        WHEN 'submit_input' THEN SELECT count(*) INTO matching_records FROM submit_input_command WHERE command_id = NEW.command_id;
        WHEN 'decide_tool_request' THEN SELECT count(*) INTO matching_records FROM decide_tool_request_command WHERE command_id = NEW.command_id;
        WHEN 'override_denied_tool_request' THEN SELECT count(*) INTO matching_records FROM override_denied_tool_request_command WHERE command_id = NEW.command_id;
        WHEN 'review_workflow' THEN SELECT count(*) INTO matching_records FROM review_workflow_command WHERE command_id = NEW.command_id;
        WHEN 'review_orchestration' THEN SELECT (SELECT count(*) FROM review_orchestration_command WHERE command_id = NEW.command_id) + (SELECT count(*) FROM review_orchestration_command_intent WHERE command_id = NEW.command_id) INTO matching_records;
        WHEN 'compact_session' THEN SELECT count(*) INTO matching_records FROM compact_session_command WHERE command_id = NEW.command_id;
        WHEN 'goal' THEN SELECT count(*) INTO matching_records FROM goal_command WHERE command_id = NEW.command_id;
        WHEN 'update_session_placement' THEN SELECT count(*) INTO matching_records FROM update_session_placement_command WHERE command_id = NEW.command_id;
        WHEN 'register_workspace' THEN SELECT count(*) INTO matching_records FROM workspace WHERE command_id = NEW.command_id;
        WHEN 'mint_git_remote' THEN SELECT count(*) INTO matching_records FROM configured_git_remote_mint WHERE command_id = NEW.command_id;
        WHEN 'withdraw_git_remote' THEN SELECT count(*) INTO matching_records FROM configured_git_remote_withdrawal WHERE command_id = NEW.command_id;
        WHEN 'session_lifecycle' THEN SELECT count(*) INTO matching_records FROM session_lifecycle_command WHERE command_id = NEW.command_id;
        ELSE RAISE EXCEPTION 'unsupported durable command kind %', NEW.command_kind USING ERRCODE = '23514';
    END CASE;
    IF matching_records <> 1 THEN
        RAISE EXCEPTION 'durable command % requires exactly one % typed record', NEW.command_id, NEW.command_kind USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;

--
-- §7 create_session: `start_gate`, `ownership`, `finish_condition`, and the
-- recorded rejection. Existing rows opened their gate, took the ownership
-- their cause implies, and — for dispatched work — declared the external
-- gate the dispatch serves.
--

ALTER TABLE create_session_command
    ADD COLUMN start_gate text,
    ADD COLUMN ownership text,
    ADD COLUMN finish_condition_kind text,
    ADD COLUMN finish_condition text,
    ADD COLUMN rejection_kind text;

UPDATE create_session_command
   SET start_gate = 'open',
       ownership = CASE creation_cause
           WHEN 'module_dispatched' THEN 'owned'
           ELSE 'unmonitored'
       END,
       finish_condition_kind = CASE creation_cause
           WHEN 'module_dispatched' THEN 'external_gate'
           ELSE NULL
       END;

ALTER TABLE create_session_command
    ALTER COLUMN start_gate SET NOT NULL,
    ALTER COLUMN ownership SET NOT NULL,
    ALTER COLUMN created_session_id DROP NOT NULL;

-- Supersedes 202609010001_sessions.
ALTER TABLE create_session_command
    DROP CONSTRAINT create_session_command_result_kind_closed;

ALTER TABLE create_session_command
    ADD CONSTRAINT create_session_command_start_gate_closed
        CHECK (start_gate = ANY (ARRAY['open'::text, 'held'::text])),
    ADD CONSTRAINT create_session_command_ownership_closed
        CHECK (ownership = ANY (ARRAY['owned'::text, 'unmonitored'::text])),
    ADD CONSTRAINT create_session_command_finish_condition_shape CHECK (
        ((finish_condition_kind IS NULL)
            OR (finish_condition_kind = ANY (ARRAY['external_gate'::text, 'declared'::text])))
        AND ((finish_condition_kind = 'declared'::text) = (finish_condition IS NOT NULL))
        AND ((finish_condition IS NULL)
             OR ((octet_length(finish_condition) >= 1)
                 AND (octet_length(finish_condition) <= 1048576)))
    ),
    ADD CONSTRAINT create_session_command_result_shape CHECK (
        ((result_kind = 'applied'::text)
            AND (created_session_id IS NOT NULL)
            AND (rejection_kind IS NULL))
        OR ((result_kind = 'rejected'::text)
            AND (created_session_id IS NULL)
            AND (rejection_kind = ANY (ARRAY[
                'finish_condition_required'::text,
                'held_gate_requires_ownership'::text
            ])))
    ),
    -- The validations §7 states, as the shape an applied row must have.
    ADD CONSTRAINT create_session_command_applied_admission CHECK (
        (result_kind = 'rejected'::text)
        OR (((ownership = 'unmonitored'::text) OR (finish_condition_kind IS NOT NULL))
            AND ((start_gate = 'open'::text) OR (ownership = 'owned'::text)))
    );

--
-- The satellite carries the finish condition the session owes and, beside
-- the pending outcome, the actor that decided it: the turn-settlement
-- transaction records terminal with that actor.
--

ALTER TABLE session_lifecycle
    ADD COLUMN finish_condition_kind text,
    ADD COLUMN finish_condition text,
    ADD COLUMN pending_terminal_actor_kind text,
    ADD COLUMN pending_terminal_actor_module text;

UPDATE session_lifecycle AS lifecycle
   SET finish_condition_kind = 'external_gate'
  FROM session
 WHERE session.session_id = lifecycle.session_id
   AND session.creation_cause = 'module_dispatched';

ALTER TABLE session_lifecycle
    ADD CONSTRAINT session_lifecycle_finish_condition_shape CHECK (
        ((finish_condition_kind IS NULL)
            OR (finish_condition_kind = ANY (ARRAY['external_gate'::text, 'declared'::text])))
        AND ((finish_condition_kind = 'declared'::text) = (finish_condition IS NOT NULL))
        AND ((finish_condition IS NULL)
             OR ((octet_length(finish_condition) >= 1)
                 AND (octet_length(finish_condition) <= 1048576)))
    ),
    ADD CONSTRAINT session_lifecycle_pending_terminal_actor_shape CHECK (
        ((pending_terminal_outcome_kind IS NULL) = (pending_terminal_actor_kind IS NULL))
        AND ((pending_terminal_actor_kind IS NULL) OR (pending_terminal_actor_kind = ANY (ARRAY[
            'core'::text, 'operator'::text, 'module'::text, 'watchdog'::text
        ])))
        AND ((pending_terminal_actor_kind = 'module'::text)
             = (pending_terminal_actor_module IS NOT NULL))
        AND ((pending_terminal_actor_module IS NULL)
             OR (pending_terminal_actor_module = ANY (ARRAY[
                 'repo_watch'::text, 'commissioned_dispatch'::text
             ])))
    );

--
-- §2: a session-level stop settles an open goal generation with the session's
-- outcome; `user_stopped` stays the goal command's own event.
--

-- Supersedes 202609020002_session_lifecycle_satellite.
ALTER TABLE goal_event
    DROP CONSTRAINT goal_event_session_closed_shape;

ALTER TABLE goal_event
    ADD CONSTRAINT goal_event_session_closed_shape CHECK (
        ((event_kind = 'session_closed'::text)
            = ((session_outcome_kind IS NOT NULL)
               AND (closure_actor_kind IS NOT NULL)))
        AND ((session_outcome_kind IS NULL) OR (session_outcome_kind = ANY (ARRAY[
            'failed_retryable'::text,
            'failed_structural'::text,
            'failed_unknown'::text,
            'stopped'::text,
            'superseded'::text,
            'abandoned'::text,
            'retired'::text
        ])))
        AND ((closure_actor_kind IS NULL) OR (closure_actor_kind = ANY (ARRAY[
            'core'::text, 'operator'::text, 'module'::text, 'watchdog'::text
        ])))
        AND ((closure_actor_kind IS NULL)
             OR ((closure_actor_kind = 'module'::text)
                 = (closure_actor_module IS NOT NULL)))
        AND ((event_kind = 'session_closed'::text)
             OR ((session_outcome_kind IS NULL)
                 AND (closure_actor_kind IS NULL)
                 AND (closure_actor_module IS NULL)))
        AND ((closure_actor_module IS NULL) OR (closure_actor_module = ANY (ARRAY[
            'repo_watch'::text,
            'commissioned_dispatch'::text
        ])))
    );

--
-- §2 finish check: every declaration's verdict is recorded against the exact
-- request, so a failed check can surface as the need text of the failure
-- that follows.
--

CREATE TABLE goal_finish_check (
    session_id uuid NOT NULL,
    tool_request_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    generation numeric(20,0) NOT NULL,
    verdict_kind text NOT NULL,
    detail text,
    recorded_at timestamp with time zone DEFAULT statement_timestamp() NOT NULL,

    CONSTRAINT goal_finish_check_pkey PRIMARY KEY (tool_request_id),
    CONSTRAINT goal_finish_check_verdict_shape CHECK (
        (verdict_kind = ANY (ARRAY['passed'::text, 'failed'::text, 'unverified'::text]))
        AND ((verdict_kind = 'failed'::text) = (detail IS NOT NULL))
        AND ((detail IS NULL)
             OR ((octet_length(detail) >= 1) AND (octet_length(detail) <= 1048576)))
    ),
    CONSTRAINT goal_finish_check_generation_positive CHECK (
        (generation >= (1)::numeric) AND (generation <= '18446744073709551615'::numeric)
    ),
    CONSTRAINT goal_finish_check_request_fk
        FOREIGN KEY (tool_request_id, session_id)
        REFERENCES tool_request (request_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    CONSTRAINT goal_finish_check_turn_fk
        FOREIGN KEY (turn_id, session_id)
        REFERENCES turn_lifecycle (turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
);

CREATE INDEX goal_finish_check_by_turn
    ON goal_finish_check (session_id, turn_id, recorded_at);

CREATE TRIGGER goal_finish_check_is_append_only
    BEFORE DELETE OR UPDATE ON goal_finish_check
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

--
-- §10: a closure retires the queued turns it strands.
--

-- Supersedes 202609020004_event_vocabulary.
ALTER TABLE turn_lifecycle
    DROP CONSTRAINT turn_lifecycle_terminal_cause_closed;

ALTER TABLE turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_terminal_cause_closed CHECK (
        (terminal_cause_kind IS NULL)
        OR (terminal_cause_kind = ANY (ARRAY[
            'completed'::text,
            'model_refusal'::text,
            'interrupt_applied'::text,
            'model_call_ambiguous'::text,
            'tool_attempt_ambiguous'::text,
            'model_call_failed'::text,
            'model_target_unavailable'::text,
            'attachment_preparation_failed'::text,
            'capability_preparation_failed'::text,
            'tool_round_limit_reached'::text,
            'tool_attempt_lost'::text,
            'credential_pool_exhausted'::text,
            'headless_approval_escalation'::text,
            'abandoned_at_restart'::text,
            'watchdog_stale_turn'::text,
            'context_headroom_exhausted'::text,
            'context_compaction_wall'::text,
            'context_compaction_failed'::text,
            'reported_usage_context_compaction_exhausted'::text,
            'reported_usage_context_still_exceeded'::text,
            'unclassified_failure'::text,
            'goal_turn_ineligible'::text,
            'session_closed'::text
        ]))
    );

-- Supersedes 202609020004_event_vocabulary.
ALTER TABLE turn_lifecycle
    DROP CONSTRAINT turn_lifecycle_terminal_cause_matches_disposition;

ALTER TABLE turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_terminal_cause_matches_disposition CHECK (
        (terminal_cause_kind IS NULL)
        OR ((terminal_disposition_kind = 'completed'::text)
            AND (terminal_cause_kind = 'completed'::text))
        OR ((terminal_disposition_kind = 'refused'::text)
            AND (terminal_cause_kind = 'model_refusal'::text))
        OR ((terminal_disposition_kind = 'cancelled'::text)
            AND (terminal_cause_kind = 'interrupt_applied'::text))
        OR ((terminal_disposition_kind = 'retired'::text)
            AND (terminal_cause_kind = ANY (ARRAY[
                'goal_turn_ineligible'::text, 'session_closed'::text
            ])))
        OR ((terminal_disposition_kind = 'reconciliation_required'::text)
            AND (terminal_cause_kind = ANY (ARRAY[
                'model_call_ambiguous'::text, 'tool_attempt_ambiguous'::text
            ])))
        OR ((terminal_disposition_kind = 'failed'::text)
            AND (terminal_cause_kind = ANY (ARRAY[
                'model_call_failed'::text,
                'model_target_unavailable'::text,
                'attachment_preparation_failed'::text,
                'capability_preparation_failed'::text,
                'tool_round_limit_reached'::text,
                'tool_attempt_lost'::text,
                'credential_pool_exhausted'::text,
                'headless_approval_escalation'::text,
                'abandoned_at_restart'::text,
                'watchdog_stale_turn'::text,
                'context_headroom_exhausted'::text,
                'context_compaction_wall'::text,
                'context_compaction_failed'::text,
                'reported_usage_context_compaction_exhausted'::text,
                'reported_usage_context_still_exceeded'::text,
                'unclassified_failure'::text
            ])))
    );

--
-- §1/§2 settlement. A closure that finds a live turn commits its outcome to
-- the handoff and hands the turn to the committed interrupt machinery; the
-- transaction that terminalizes the session's last live turn records the
-- terminal state, retiring the queued turns the closure stranded.
--
-- Queued turns are retired one statement at a time: each retirement re-fires
-- this trigger, and the nested invocation that finds no live turn left is the
-- one that settles.
--

CREATE FUNCTION settle_session_pending_terminal(subject uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    held session_lifecycle%ROWTYPE;
    queued uuid;
    last_kind text;
    last_generation numeric(20, 0);
    last_ordinal numeric(20, 0);
BEGIN
    SELECT * INTO held FROM session_lifecycle WHERE session_id = subject;
    IF NOT FOUND
       OR held.state_kind = 'terminal'
       OR held.pending_terminal_outcome_kind IS NULL
    THEN
        RETURN;
    END IF;

    IF EXISTS (
        SELECT 1 FROM turn_lifecycle
         WHERE session_id = subject AND state_kind = 'active'
    ) THEN
        RETURN;
    END IF;

    SELECT turn_id INTO queued
      FROM turn_lifecycle
     WHERE session_id = subject AND state_kind = 'queued'
     ORDER BY acceptance_position
     LIMIT 1;
    IF FOUND THEN
        UPDATE turn_lifecycle
           SET state_kind = 'terminal',
               terminal_disposition_kind = 'retired',
               terminal_cause_kind = 'session_closed'
         WHERE session_id = subject AND turn_id = queued;
        WITH header AS (
            INSERT INTO outbox_event
                (event_kind, storage_version, session_id, turn_disposition)
            VALUES ('turn_terminal', 1, subject, 'retired')
            RETURNING event_sequence, event_kind, storage_version, session_id
        )
        INSERT INTO turn_terminal_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             turn_id, disposition_kind)
        SELECT event_sequence, event_kind, storage_version, session_id,
               queued, 'retired'
          FROM header;
        -- The retirement's own trigger settled if it was the last turn.
        RETURN;
    END IF;

    SELECT event_kind, generation, event_ordinal
      INTO last_kind, last_generation, last_ordinal
      FROM goal_event
     WHERE session_id = subject
     ORDER BY event_ordinal DESC
     LIMIT 1;
    IF FOUND AND last_kind IN ('commissioned', 'resumed', 'blocked', 'superseded') THEN
        IF held.pending_terminal_outcome_kind = 'achieved_verified' THEN
            RAISE EXCEPTION
                'session % cannot settle achieved_verified over an open goal', subject
                USING ERRCODE = '23514';
        END IF;
        INSERT INTO goal_event
            (session_id, event_ordinal, generation, event_kind,
             session_outcome_kind, closure_actor_kind, closure_actor_module)
        VALUES (
            subject,
            last_ordinal + 1,
            last_generation + CASE WHEN last_kind = 'superseded' THEN 1 ELSE 0 END,
            'session_closed',
            held.pending_terminal_outcome_kind,
            held.pending_terminal_actor_kind,
            held.pending_terminal_actor_module
        );
        WITH header AS (
            INSERT INTO outbox_event (event_kind, storage_version, session_id)
            VALUES ('goal_changed', 1, subject)
            RETURNING event_sequence, event_kind, storage_version, session_id
        )
        INSERT INTO goal_changed_outbox_event
            (event_sequence, event_kind, storage_version, session_id, event_ordinal)
        SELECT event_sequence, event_kind, storage_version, session_id,
               last_ordinal + 1
          FROM header;
    END IF;

    UPDATE session_lifecycle
       SET state_kind = 'terminal',
           state_entered_at = statement_timestamp(),
           actor_kind = pending_terminal_actor_kind,
           actor_module = pending_terminal_actor_module,
           actor_turn_id = NULL,
           actor_tool_request_id = NULL,
           waiting_kind = NULL,
           waiting_waker = NULL,
           waiting_subject_session_id = NULL,
           recovering_op = NULL,
           blocked_reason = NULL,
           blocked_cycle = NULL,
           parked_cause = NULL,
           parked_responder = NULL,
           ended_at = statement_timestamp(),
           terminal_outcome_kind = pending_terminal_outcome_kind,
           terminal_cause_kind = pending_terminal_cause_kind,
           terminal_stop_sticky = pending_terminal_stop_sticky,
           terminal_superseded_by = pending_terminal_superseded_by,
           pending_terminal_outcome_kind = NULL,
           pending_terminal_cause_kind = NULL,
           pending_terminal_stop_sticky = NULL,
           pending_terminal_superseded_by = NULL,
           pending_terminal_actor_kind = NULL,
           pending_terminal_actor_module = NULL
     WHERE session_id = subject;

    IF held.pending_terminal_outcome_kind = 'abandoned' THEN
        INSERT INTO session_cleanup_obligation (session_id, outcome_kind)
        VALUES (subject, 'abandoned')
        ON CONFLICT (session_id) DO NOTHING;
    END IF;
END;
$$;

CREATE FUNCTION settle_session_pending_terminal_from_turn() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.state_kind = 'terminal' THEN
        PERFORM settle_session_pending_terminal(NEW.session_id);
    END IF;
    RETURN NULL;
END;
$$;

-- Named to fire after `turn_lifecycle_projects_session_state`.
CREATE TRIGGER turn_lifecycle_settles_pending_terminal
    AFTER UPDATE OF state_kind ON turn_lifecycle
    FOR EACH ROW EXECUTE FUNCTION settle_session_pending_terminal_from_turn();
