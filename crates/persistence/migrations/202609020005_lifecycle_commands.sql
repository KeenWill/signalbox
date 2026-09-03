--
-- Session lifecycle §7: the command surface, finish conditions, and the
-- authenticated issuer on the durable-command envelope (§6).

SET check_function_bodies = false;

--
-- §6: every command records the principal that issued it. A module principal
-- is stamped by the in-daemon module path that composed the command.
--

ALTER TABLE durable_command
    ADD COLUMN issuer_kind text NOT NULL,
    ADD COLUMN issuer_module text;

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

ALTER TABLE submit_input_command
    DROP CONSTRAINT submit_input_command_actor_kind_closed,
    DROP CONSTRAINT submit_input_command_actor_shape,
    ADD CONSTRAINT submit_input_command_actor_kind_closed CHECK (
        actor_kind = ANY (ARRAY[
            'user'::text, 'core'::text, 'model'::text,
            'recovery'::text, 'tool'::text
        ])
    ),
    ADD CONSTRAINT submit_input_command_actor_shape CHECK (
        ((actor_kind = ANY (ARRAY['user'::text, 'core'::text, 'recovery'::text]))
            AND actor_turn_id IS NULL
            AND actor_tool_request_id IS NULL)
        OR (actor_kind = 'model'::text
            AND actor_turn_id IS NOT NULL
            AND actor_tool_request_id IS NULL)
        OR (actor_kind = 'tool'::text
            AND actor_turn_id IS NULL
            AND actor_tool_request_id IS NOT NULL)
    );

ALTER TABLE replace_session_metadata_command
    DROP CONSTRAINT replace_session_metadata_command_actor_kind_closed,
    DROP CONSTRAINT replace_session_metadata_command_actor_shape,
    DROP CONSTRAINT replace_session_metadata_command_result_actor_kind_closed,
    DROP CONSTRAINT replace_session_metadata_command_result_actor_shape,
    ADD CONSTRAINT replace_session_metadata_command_actor_kind_closed CHECK (
        actor_kind = ANY (ARRAY[
            'user'::text, 'core'::text, 'model'::text,
            'recovery'::text, 'tool'::text
        ])
    ),
    ADD CONSTRAINT replace_session_metadata_command_actor_shape CHECK (
        ((actor_kind = ANY (ARRAY['user'::text, 'core'::text, 'recovery'::text]))
            AND actor_turn_id IS NULL
            AND actor_tool_request_id IS NULL)
        OR (actor_kind = 'model'::text
            AND actor_turn_id IS NOT NULL
            AND actor_tool_request_id IS NULL)
        OR (actor_kind = 'tool'::text
            AND actor_turn_id IS NULL
            AND actor_tool_request_id IS NOT NULL)
    ),
    ADD CONSTRAINT replace_session_metadata_command_result_actor_kind_closed CHECK (
        result_actor_kind IS NULL OR result_actor_kind = ANY (ARRAY[
            'user'::text, 'core'::text, 'model'::text,
            'recovery'::text, 'tool'::text
        ])
    ),
    ADD CONSTRAINT replace_session_metadata_command_result_actor_shape CHECK (
        (result_actor_kind IS NULL
            AND result_actor_turn_id IS NULL
            AND result_actor_tool_request_id IS NULL)
        OR ((result_actor_kind = ANY (ARRAY['user'::text, 'core'::text, 'recovery'::text]))
            AND result_actor_turn_id IS NULL
            AND result_actor_tool_request_id IS NULL)
        OR (result_actor_kind = 'model'::text
            AND result_actor_turn_id IS NOT NULL
            AND result_actor_tool_request_id IS NULL)
        OR (result_actor_kind = 'tool'::text
            AND result_actor_turn_id IS NULL
            AND result_actor_tool_request_id IS NOT NULL)
    );

ALTER TABLE session_metadata
    DROP CONSTRAINT session_metadata_actor_kind_closed,
    DROP CONSTRAINT session_metadata_actor_shape,
    ADD CONSTRAINT session_metadata_actor_kind_closed CHECK (
        actor_kind = ANY (ARRAY[
            'user'::text, 'core'::text, 'model'::text,
            'recovery'::text, 'tool'::text
        ])
    ),
    ADD CONSTRAINT session_metadata_actor_shape CHECK (
        ((actor_kind = ANY (ARRAY['user'::text, 'core'::text, 'recovery'::text]))
            AND actor_turn_id IS NULL
            AND actor_tool_request_id IS NULL)
        OR (actor_kind = 'model'::text
            AND actor_turn_id IS NOT NULL
            AND actor_tool_request_id IS NULL)
        OR (actor_kind = 'tool'::text
            AND actor_turn_id IS NULL
            AND actor_tool_request_id IS NOT NULL)
    );

--
-- The projector takes the issuer of a command-authored write (§6), and the
-- goal-event trigger below reads it from the envelope.
-- Supersedes 202609020002_session_lifecycle_satellite.
--

DROP FUNCTION IF EXISTS project_session_lifecycle(uuid, boolean);
DROP FUNCTION IF EXISTS project_session_lifecycle(uuid, boolean, boolean);
DROP FUNCTION IF EXISTS project_session_lifecycle(uuid, boolean, boolean, boolean);
DROP FUNCTION IF EXISTS project_session_lifecycle(uuid, boolean, text, text);

CREATE FUNCTION project_session_lifecycle(
    subject uuid,
    lifts_park boolean,
    issuer_kind text DEFAULT NULL,
    issuer_module text DEFAULT NULL,
    turn_progressed boolean DEFAULT false,
    queues_turn boolean DEFAULT false
) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    held session_lifecycle%ROWTYPE;
    live_phase text;
    goal_turn uuid;
    goal_request uuid;
    actor text;
    live_child_request uuid;
    child uuid;
    queued boolean;
    goal_kind text;
    goal_reason text;
    goal_generation numeric(20,0);
    cycles bigint;
    next_state text;
    next_waiting_kind text;
    next_waiting_waker text;
    next_recovering_op text;
    next_blocked_reason text;
    next_blocked_cycle bigint;
BEGIN
    SELECT * INTO held FROM session_lifecycle WHERE session_id = subject;
    IF NOT FOUND THEN
        RETURN;
    END IF;

    -- Terminal is final in both directions. As a projection no-op, a later
    -- turn or goal write would land beneath a closed session and then
    -- activate: the deferred terminal-turn constraint fires on lifecycle
    -- writes, and nothing re-fires it for the turn.
    IF held.state_kind = 'terminal' THEN
        RAISE EXCEPTION
            'session % is terminal and admits no further turn or goal work',
            subject
            USING ERRCODE = '23514';
    END IF;

    -- `parked` overrides the mapping. One write lifts it, the operator's goal
    -- resume, and the caller says so: inferring it from the lineage would let
    -- a park taken after an earlier resume be lifted by the next unrelated
    -- turn write.
    IF held.state_kind = 'parked' AND NOT lifts_park THEN
        RETURN;
    END IF;

    SELECT live.active_phase_kind, live.child_wait_request_id
      INTO live_phase, live_child_request
      FROM turn_lifecycle AS live
     WHERE live.session_id = subject
       AND live.state_kind = 'active'
       AND NOT live.delegation_runtime_terminal
     ORDER BY live.acceptance_position DESC
     LIMIT 1;

    -- The goal lineage is read only when it can decide the state: a live turn
    -- outranks it, and this runs on every turn write.
    IF live_phase IS NULL THEN
        SELECT event.event_kind, event.blocked_reason, event.generation,
               event.model_turn_id, event.model_tool_request_id
          INTO goal_kind, goal_reason, goal_generation,
               goal_turn, goal_request
          FROM goal_event AS event
         WHERE event.session_id = subject
         ORDER BY event.event_ordinal DESC
         LIMIT 1;
    END IF;

    IF live_phase IS NOT NULL THEN
        CASE live_phase
            WHEN 'running' THEN
                next_state := 'active';
            WHEN 'awaiting_tool_approval' THEN
                next_state := 'waiting';
                next_waiting_kind := 'approval';
                next_waiting_waker := 'approval_decision';
            WHEN 'awaiting_child' THEN
                next_state := 'waiting';
                next_waiting_kind := 'child';
                next_waiting_waker := 'child_settlement';
                SELECT waiting.child_session_id INTO child
                  FROM session_delegation_wait AS waiting
                 WHERE waiting.awaiting_tool_request_id = live_child_request
                   AND waiting.parent_session_id = subject;
            WHEN 'awaiting_model_call_recovery' THEN
                next_state := 'recovering';
                next_recovering_op := 'model_call';
            WHEN 'awaiting_tool_recovery' THEN
                next_state := 'recovering';
                next_recovering_op := 'tool';
            WHEN 'awaiting_runner_recovery' THEN
                next_state := 'recovering';
                next_recovering_op := 'runner';
            ELSE
                RAISE EXCEPTION 'unmapped active turn phase %', live_phase
                    USING ERRCODE = '23514';
        END CASE;
    ELSIF goal_kind = 'blocked' THEN
        SELECT count(*) INTO cycles
          FROM goal_event AS resumed
         WHERE resumed.session_id = subject
           AND resumed.generation = goal_generation
           AND resumed.event_kind = 'resumed';
        next_state := 'blocked';
        next_blocked_reason := goal_reason;
        next_blocked_cycle := cycles;
    ELSE
        SELECT EXISTS (
            SELECT 1
              FROM turn_lifecycle AS pending
             WHERE pending.session_id = subject
               AND pending.state_kind = 'queued'
        ) INTO queued;
        queued := queued OR queues_turn;

        -- A creation stays `created` until its first turn is queued, and a
        -- dispatched session stays `dispatched` until one activates: the
        -- dispatch deadline is what covers a queued turn that never runs. A
        -- queued successor inside a live session never re-enters `dispatched`
        -- (§1) — the active stall deadline covers it, so the session reads
        -- `active` while the scheduler owes it a pass.
        IF held.state_kind = 'created' THEN
            next_state := CASE WHEN queued THEN 'dispatched' ELSE 'created' END;
        ELSIF held.state_kind = 'dispatched' THEN
            next_state := 'dispatched';
        ELSE
            next_state := 'active';
        END IF;
    END IF;

    IF held.state_kind = next_state
       AND held.waiting_kind IS NOT DISTINCT FROM next_waiting_kind
       AND held.waiting_subject_session_id IS NOT DISTINCT FROM child
       AND held.recovering_op IS NOT DISTINCT FROM next_recovering_op
       AND held.blocked_reason IS NOT DISTINCT FROM next_blocked_reason
       AND held.blocked_cycle IS NOT DISTINCT FROM next_blocked_cycle
    THEN
        -- The state did not move, but a turn transitioned under it -- one
        -- terminalized, or a successor activated -- and that is the progress
        -- the stall deadline measures. Queueing another turn is admission, not
        -- progress, and re-arming on it would let a stalled session postpone
        -- its own deadline. Only the deadline re-arms: the session entered
        -- `active` when its first turn started, and `state_entered_at` says so.
        IF turn_progressed AND held.state_kind = 'active' AND held.owned THEN
            UPDATE session_deadline
               SET armed_at = statement_timestamp(),
                   expires_at = NULL
             WHERE session_id = subject
               AND deadline_kind = 'active_stall';
        END IF;
        RETURN;
    END IF;

    -- A projected transition is core machinery, and the live turn is the
    -- agency behind it; the creating actor is not.
    -- The identity recorded here is model or tool agency and nothing else. A
    -- turn activating, a phase moving, a scheduler-authored block: those are
    -- daemon machinery that happens to concern a turn, and the reader takes a
    -- stored turn to mean the model acted. Only a model-declared goal event
    -- carries one. A goal event the user authored -- a lift, a stop, a
    -- supersede, a commission -- is the operator's, not daemon core's.
    IF issuer_kind IS NOT NULL AND issuer_kind <> 'core' THEN
        actor := issuer_kind;
        goal_turn := NULL;
        goal_request := NULL;
    ELSE
        actor := 'core';
        IF live_phase IS NOT NULL THEN
            goal_turn := NULL;
            goal_request := NULL;
        END IF;
    END IF;

    UPDATE session_lifecycle
       SET state_kind = next_state,
           state_entered_at = statement_timestamp(),
           actor_kind = actor,
           actor_module = CASE WHEN actor = 'module' THEN issuer_module END,
           actor_turn_id = goal_turn,
           actor_tool_request_id = CASE
               WHEN goal_turn IS NULL THEN goal_request
               ELSE NULL
           END,
           waiting_kind = next_waiting_kind,
           waiting_waker = next_waiting_waker,
           waiting_subject_session_id = child,
           recovering_op = next_recovering_op,
           blocked_reason = next_blocked_reason,
           blocked_cycle = next_blocked_cycle,
           parked_cause = NULL,
           parked_responder = NULL,
           parked_since = NULL,
           parked_standing_cause_kind = NULL
     WHERE session_id = subject;
END;
$$;

CREATE OR REPLACE FUNCTION project_session_lifecycle_from_turn() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM project_session_lifecycle(
        NEW.session_id,
        false,
        NULL,
        NULL,
        TG_OP = 'UPDATE'
    );
    RETURN NULL;
END;
$$;

--
-- A goal event a command authored projects with that command's issuer (§6):
-- daemon core's automatic resume is core's, a module's composed stop is the
-- module's, and only the operator's own commands read as the operator's.
-- Supersedes 202609020002_session_lifecycle_satellite.sql.
--

CREATE OR REPLACE FUNCTION project_session_lifecycle_from_goal() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    authored_kind text;
    authored_module text;
BEGIN
    IF NEW.user_command_id IS NOT NULL THEN
        SELECT command.issuer_kind, command.issuer_module
          INTO STRICT authored_kind, authored_module
          FROM durable_command AS command
         WHERE command.command_id = NEW.user_command_id;
    END IF;
    PERFORM project_session_lifecycle(
        NEW.session_id,
        NEW.event_kind = 'resumed',
        authored_kind,
        authored_module,
        false,
        NEW.event_kind IN ('commissioned', 'resumed', 'superseded')
    );
    RETURN NULL;
END;
$$;

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
    -- The successor as the command named it: a rejection keeps an unknown or
    -- self-naming successor on record, so no reference constrains it.
    successor_session_id uuid,
    failure_cause_kind text,
    finish_condition_kind text,
    finish_condition text,
    result_kind text NOT NULL,
    rejection_kind text,
    applied_effect_kind text,
    live_turn_id uuid,

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
        ((operation_kind = 'stop'::text) = (stop_sticky IS NOT NULL))
        AND ((operation_kind = 'stop'::text) = (descendant_scope IS NOT NULL))
        AND ((operation_kind = 'supersede'::text) = (successor_session_id IS NOT NULL))
        AND ((failure_cause_kind IS NULL) OR (operation_kind = 'close_failed'::text))
        AND ((finish_condition_kind IS NULL) OR (operation_kind = 'adopt'::text))
        AND ((descendant_scope IS NULL) OR (descendant_scope = ANY (ARRAY[
            'parent_alone'::text, 'parent_and_descendants'::text
        ])))
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
        AND ((finish_condition_kind IS NOT DISTINCT FROM 'declared'::text)
             = (finish_condition IS NOT NULL))
        AND ((finish_condition IS NULL)
             OR ((octet_length(finish_condition) >= 1)
                 AND (octet_length(finish_condition) <= 1048576)))
    ),
    CONSTRAINT session_lifecycle_command_result_shape CHECK (
        (result_kind = ANY (ARRAY['applied'::text, 'rejected'::text]))
        AND ((result_kind = 'rejected'::text) = (rejection_kind IS NOT NULL))
        AND ((result_kind = 'applied'::text) = (applied_effect_kind IS NOT NULL))
        AND ((applied_effect_kind IS NULL) OR (applied_effect_kind = ANY (ARRAY[
            'closed'::text, 'closure_pending'::text, 'resumed'::text, 'ownership_changed'::text
        ])))
        AND ((applied_effect_kind IS NULL)
             OR ((operation_kind = ANY (ARRAY[
                    'stop'::text, 'supersede'::text,
                    'abandon'::text, 'close_failed'::text
                 ])) AND (applied_effect_kind = ANY (ARRAY[
                    'closed'::text, 'closure_pending'::text
                 ])))
             OR ((operation_kind = 'resume'::text)
                 AND (applied_effect_kind = 'resumed'::text))
             OR ((operation_kind = ANY (ARRAY['adopt'::text, 'release'::text]))
                 AND (applied_effect_kind = 'ownership_changed'::text)))
        AND ((applied_effect_kind IS NOT DISTINCT FROM 'closure_pending'::text)
             = (live_turn_id IS NOT NULL))
    ),
    CONSTRAINT session_lifecycle_command_rejection_closed CHECK (
        (rejection_kind IS NULL)
        OR (rejection_kind = ANY (ARRAY[
            'session_not_found'::text,
            'transition_not_admitted'::text,
            'requires_parked'::text,
            'release_while_parked'::text,
            'ownership_unchanged'::text,
            'finish_condition_already_declared'::text,
            'standing_cause_mismatch'::text,
            'successor_not_found'::text,
            'successor_is_self'::text,
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
            AND (rejection_kind = ANY (ARRAY[
                'successor_not_found'::text, 'successor_is_self'::text
            ])))
        OR ((operation_kind = 'resume'::text)
            AND (rejection_kind = ANY (ARRAY[
                'goal_resume_required'::text, 'pending_terminal_conflict'::text
            ])))
        OR ((operation_kind = 'adopt'::text) AND (rejection_kind = ANY (ARRAY[
                'ownership_unchanged'::text,
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
    CONSTRAINT session_lifecycle_command_live_turn_fk
        FOREIGN KEY (live_turn_id, session_id) REFERENCES turn_lifecycle (turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
);

CREATE INDEX session_lifecycle_command_by_session
    ON session_lifecycle_command (session_id, command_id);

CREATE TRIGGER session_lifecycle_command_is_append_only
    BEFORE DELETE OR UPDATE ON session_lifecycle_command
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE CONSTRAINT TRIGGER applied_lifecycle_command_requires_delegation_cascade
    AFTER INSERT OR UPDATE ON session_lifecycle_command
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION require_applied_lifecycle_command_delegation_cascade();

-- A rejected command may name a session that does not exist, so the session
-- reference is checked by the applying transaction rather than a key.

CREATE OR REPLACE FUNCTION durable_command_belongs_to_session(checked_command_id uuid, checked_session_id uuid) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    SELECT EXISTS (SELECT 1 FROM create_session_command
        WHERE command_id = checked_command_id AND created_session_id = checked_session_id)
    OR EXISTS (SELECT 1 FROM create_session_from_imported_frontier_command
        WHERE command_id = checked_command_id AND created_session_id = checked_session_id)
    OR EXISTS (SELECT 1 FROM replace_session_defaults_command
        WHERE command_id = checked_command_id AND session_id = checked_session_id)
    OR EXISTS (SELECT 1 FROM replace_session_metadata_command
        WHERE command_id = checked_command_id AND session_id = checked_session_id)
    OR EXISTS (SELECT 1 FROM submit_input_command
        WHERE command_id = checked_command_id AND session_id = checked_session_id)
    OR EXISTS (
        SELECT 1 FROM decide_tool_request_command AS command
        JOIN tool_request AS request ON request.request_id = command.request_id
        WHERE command.command_id = checked_command_id
          AND request.session_id = checked_session_id
    )
    OR EXISTS (SELECT 1 FROM compact_session_command
        WHERE command_id = checked_command_id AND session_id = checked_session_id)
    OR EXISTS (SELECT 1 FROM goal_command
        WHERE command_id = checked_command_id AND session_id = checked_session_id)
    OR EXISTS (SELECT 1 FROM session_lifecycle_command
        WHERE command_id = checked_command_id AND session_id = checked_session_id)
$$;

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
-- §7 create_session: `start_gate`, `ownership`, `finish_condition`.
--

ALTER TABLE create_session_command
    ADD COLUMN start_gate text NOT NULL,
    ADD COLUMN ownership text NOT NULL,
    ADD COLUMN finish_condition_kind text,
    ADD COLUMN finish_condition text;

ALTER TABLE create_session_command
    ADD CONSTRAINT create_session_command_start_gate_closed
        CHECK (start_gate = ANY (ARRAY['open'::text, 'held'::text])),
    ADD CONSTRAINT create_session_command_ownership_closed
        CHECK (ownership = ANY (ARRAY['owned'::text, 'unmonitored'::text])),
    ADD CONSTRAINT create_session_command_finish_condition_shape CHECK (
        ((finish_condition_kind IS NULL)
            OR (finish_condition_kind = ANY (ARRAY['external_gate'::text, 'declared'::text])))
        AND ((finish_condition_kind IS NOT DISTINCT FROM 'declared'::text)
             = (finish_condition IS NOT NULL))
        AND ((finish_condition IS NULL)
             OR ((octet_length(finish_condition) >= 1)
                 AND (octet_length(finish_condition) <= 1048576)))
    );

--
-- The satellite carries the finish condition the session owes and, beside
-- the pending outcome, the actor that decided it: the turn-settlement
-- transaction records terminal with that actor.
--

ALTER TABLE session_lifecycle
    ADD COLUMN start_gate_held boolean NOT NULL,
    ADD COLUMN finish_condition_kind text,
    ADD COLUMN finish_condition text;

ALTER TABLE session_lifecycle
    ADD CONSTRAINT session_lifecycle_finish_condition_shape CHECK (
        ((finish_condition_kind IS NULL)
            OR (finish_condition_kind = ANY (ARRAY['external_gate'::text, 'declared'::text])))
        AND ((finish_condition_kind IS NOT DISTINCT FROM 'declared'::text)
             = (finish_condition IS NOT NULL))
        AND ((finish_condition IS NULL)
             OR ((octet_length(finish_condition) >= 1)
                 AND (octet_length(finish_condition) <= 1048576)))
    );

ALTER TABLE ONLY session_lifecycle
    ADD CONSTRAINT session_lifecycle_pending_actor_turn_fk
        FOREIGN KEY (pending_terminal_actor_turn_id, session_id)
        REFERENCES turn_lifecycle(turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE ONLY session_lifecycle
    ADD CONSTRAINT session_lifecycle_pending_actor_tool_request_fk
        FOREIGN KEY (pending_terminal_actor_tool_request_id, session_id)
        REFERENCES tool_request(request_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

--
-- §7 start gate: a held gate keeps the session `created` until `release_start`
-- or expiry; the projection's move out of `created` is discarded while the
-- gate is held, and the created state arms the start-gate deadline instead of
-- the first-input one. Releasing the gate and gating activation land with the
-- deadline engine.
--

CREATE FUNCTION hold_session_start_gate() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF OLD.state_kind = 'created'
       AND OLD.start_gate_held
       AND NEW.state_kind NOT IN ('created', 'terminal')
    THEN
        RETURN OLD;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER session_lifecycle_start_gate_holds
    BEFORE UPDATE OF state_kind ON session_lifecycle
    FOR EACH ROW EXECUTE FUNCTION hold_session_start_gate();

--
-- A goal command on a session whose closure is pending is refused
-- `session_closing`: the committed closure settles the generation.
--

-- Supersedes 202609010006_goals.
ALTER TABLE goal_command
    DROP CONSTRAINT goal_command_rejection_kind_check;

ALTER TABLE goal_command
    ADD CONSTRAINT goal_command_rejection_kind_check CHECK (
        (rejection_kind IS NULL)
        OR (rejection_kind = ANY (ARRAY[
            'session_not_found'::text,
            'session_closing'::text,
            'goal_already_attached'::text,
            'goal_not_attached'::text,
            'unknown_model_alias'::text,
            'requires_blocked'::text,
            'requires_pursuing_or_blocked'::text,
            'generation_exhausted'::text,
            'event_ordinal_exhausted'::text,
            'acceptance_position_exhausted'::text
        ]))
    );

-- Supersedes 202609010006_goals.
ALTER TABLE goal_command
    DROP CONSTRAINT goal_command_rejection_operation;

ALTER TABLE goal_command
    ADD CONSTRAINT goal_command_rejection_operation CHECK (
        (result_kind = 'applied'::text)
        OR (rejection_kind = ANY (ARRAY['session_not_found'::text, 'session_closing'::text]))
        OR ((operation_kind = 'attach'::text) AND (rejection_kind = ANY (ARRAY[
            'goal_already_attached'::text, 'unknown_model_alias'::text,
            'generation_exhausted'::text, 'event_ordinal_exhausted'::text,
            'acceptance_position_exhausted'::text
        ])))
        OR ((operation_kind = 'resume'::text) AND (rejection_kind = ANY (ARRAY[
            'goal_not_attached'::text, 'unknown_model_alias'::text,
            'requires_blocked'::text, 'event_ordinal_exhausted'::text,
            'acceptance_position_exhausted'::text
        ])))
        OR ((operation_kind = 'stop'::text) AND (rejection_kind = ANY (ARRAY[
            'goal_not_attached'::text, 'requires_pursuing_or_blocked'::text,
            'event_ordinal_exhausted'::text
        ])))
        OR ((operation_kind = 'supersede'::text) AND (rejection_kind = ANY (ARRAY[
            'goal_not_attached'::text, 'unknown_model_alias'::text,
            'requires_pursuing_or_blocked'::text, 'generation_exhausted'::text,
            'event_ordinal_exhausted'::text, 'acceptance_position_exhausted'::text
        ])))
    );

--
-- §2: a session-level stop settles an open goal generation with the session's
-- outcome; `user_stopped` stays the goal command's own event. An achieved
-- generation that was never verified admits every later closure.
--

-- Supersedes 202609020002_session_lifecycle_satellite.
CREATE OR REPLACE FUNCTION require_terminal_session_settles_its_goal() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    last_kind text;
    last_outcome text;
BEGIN
    IF NEW.state_kind <> 'terminal' THEN
        RETURN NULL;
    END IF;

    SELECT event_kind, session_outcome_kind INTO last_kind, last_outcome
      FROM goal_event
     WHERE session_id = NEW.session_id
     ORDER BY event_ordinal DESC
     LIMIT 1;

    IF NOT FOUND THEN
        RETURN NULL;
    END IF;

    IF last_kind NOT IN ('achieved', 'user_stopped', 'session_closed') THEN
        RAISE EXCEPTION
            'terminal session % leaves its goal generation live at %',
            NEW.session_id, last_kind
            USING ERRCODE = '23514';
    END IF;

    IF NOT (
        last_kind = 'achieved'
        OR (last_kind = 'user_stopped' AND NEW.terminal_outcome_kind = 'stopped')
        OR (last_kind = 'session_closed'
            AND last_outcome = NEW.terminal_outcome_kind)
    ) THEN
        RAISE EXCEPTION
            'terminal session % records % over a goal settled as %',
            NEW.session_id, NEW.terminal_outcome_kind,
            COALESCE(last_outcome, last_kind)
            USING ERRCODE = '23514';
    END IF;

    RETURN NULL;
END;
$$;

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
        -- Column by column: requiring only that the pair is absent together
        -- lets an ordinary event carry one of them, which replay ignores.
        AND ((event_kind = 'session_closed'::text)
             OR ((session_outcome_kind IS NULL)
                 AND (closure_actor_kind IS NULL)
                 AND (closure_actor_module IS NULL)
                 AND (closure_actor_turn_id IS NULL)
                 AND (closure_actor_tool_request_id IS NULL)))
        AND ((closure_actor_module IS NULL) OR (closure_actor_module = ANY (ARRAY[
            'repo_watch'::text,
            'commissioned_dispatch'::text
        ])))
        -- A core closure keeps at most one acting identity, and no other
        -- classification keeps any: the reader rejects the combinations this
        -- would otherwise let commit, leaving the goal unreadable.
        AND ((closure_actor_turn_id IS NULL) OR (closure_actor_tool_request_id IS NULL))
        AND ((closure_actor_kind = 'core'::text)
             OR ((closure_actor_turn_id IS NULL)
                 AND (closure_actor_tool_request_id IS NULL)))
    );

--
-- A failing finish check blocks the goal with its result as the need (§2).
-- Supersedes 202609010006_goals.
--

ALTER TABLE goal_event
    DROP CONSTRAINT goal_event_blocked_reason_check;

ALTER TABLE goal_event
    ADD CONSTRAINT goal_event_blocked_reason_check CHECK (
        (blocked_reason IS NULL)
        OR (blocked_reason = ANY (ARRAY[
            'user_input_required'::text,
            'external_change_required'::text,
            'authorization_required'::text,
            'execution_failure'::text,
            'finish_check_failed'::text
        ]))
    );

-- Supersedes 202609010006_goals.
CREATE OR REPLACE FUNCTION enforce_goal_model_declaration_request() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    stored_tool_name text;
    stored_arguments_kind text;
    stored_arguments jsonb;
    declared_text text;
    expected_arguments jsonb;
BEGIN
    IF NEW.model_tool_request_id IS NULL THEN
        RETURN NEW;
    END IF;

    IF NOT goal_event_names_current_goal_turn(
        NEW.session_id, NEW.generation, NEW.model_turn_id
    ) THEN
        RAISE EXCEPTION 'goal model event must name the current goal turn'
            USING ERRCODE = '23514',
                CONSTRAINT = 'goal_event_model_current_turn';
    END IF;

    SELECT
        request.tool_name,
        request.arguments_kind,
        CASE
            WHEN request.arguments_kind = 'json'
                THEN request.arguments_text::jsonb
        END,
        declaration.assistant_text_value
      INTO stored_tool_name, stored_arguments_kind, stored_arguments,
           declared_text
      FROM tool_request AS request
      JOIN semantic_transcript_entry AS tool_use
        ON tool_use.source_session_id = request.session_id
       AND tool_use.producing_model_call_id = request.producing_model_call_id
       AND tool_use.payload_kind = 'assistant_tool_use'
       AND tool_use.assistant_tool_request_id = request.request_id
      JOIN semantic_transcript_entry AS declaration
        ON declaration.source_session_id = tool_use.source_session_id
       AND declaration.producing_model_call_id = tool_use.producing_model_call_id
       AND declaration.payload_kind = 'assistant_text'
       AND declaration.assistant_response_part_ordinal + 1 =
           tool_use.assistant_response_part_ordinal
     WHERE request.request_id = NEW.model_tool_request_id
       AND request.session_id = NEW.session_id
       AND request.turn_id = NEW.model_turn_id
       AND NOT EXISTS (
           SELECT 1
             FROM semantic_transcript_entry AS later_part
            WHERE later_part.source_session_id = tool_use.source_session_id
              AND later_part.producing_model_call_id =
                  tool_use.producing_model_call_id
              AND later_part.assistant_response_part_ordinal >
                  tool_use.assistant_response_part_ordinal
       );

    -- A failing finish check blocks the goal from an `achieved` declaration:
    -- the request declared achievement, and the need is the check's result.
    expected_arguments := CASE
        WHEN NEW.event_kind = 'achieved'
          OR (NEW.event_kind = 'blocked' AND NEW.blocked_reason = 'finish_check_failed')
            THEN jsonb_build_object('transition', 'achieved')
        WHEN NEW.event_kind = 'blocked' THEN jsonb_build_object(
            'transition', 'blocked',
            'reason', NEW.blocked_reason
        )
    END;

    IF stored_tool_name IS DISTINCT FROM 'goal_declare'
        OR stored_arguments_kind IS DISTINCT FROM 'json'
        OR stored_arguments IS DISTINCT FROM expected_arguments
        OR (NOT (NEW.event_kind = 'blocked' AND NEW.blocked_reason = 'finish_check_failed')
            AND declared_text IS DISTINCT FROM COALESCE(NEW.report, NEW.need))
    THEN
        RAISE EXCEPTION 'goal model event lacks its exact declaration request'
            USING ERRCODE = '23514',
                CONSTRAINT = 'goal_event_model_declaration_request';
    END IF;
    RETURN NEW;
END;
$$;

--
-- `achieved_declared` (§2, ruled 2026-09-02): an achievement no finish check
-- verifies closes the session with it; `finish_check_failed` is the block a
-- failing check appends. Supersedes 202609020002_session_lifecycle_satellite.
--

ALTER TABLE session_lifecycle
    DROP CONSTRAINT session_lifecycle_terminal_outcome_closed,
    DROP CONSTRAINT session_lifecycle_terminal_shape,
    DROP CONSTRAINT session_lifecycle_pending_terminal_shape,
    DROP CONSTRAINT session_lifecycle_blocked_reason_closed;

ALTER TABLE session_lifecycle
    ADD CONSTRAINT session_lifecycle_terminal_outcome_closed CHECK (
        (terminal_outcome_kind IS NULL)
        OR (terminal_outcome_kind = ANY (ARRAY[
            'achieved_verified'::text,
            'achieved_declared'::text,
            'failed_retryable'::text,
            'failed_structural'::text,
            'failed_unknown'::text,
            'stopped'::text,
            'superseded'::text,
            'abandoned'::text,
            'retired'::text
        ]))
    ),
    ADD CONSTRAINT session_lifecycle_terminal_shape CHECK (
        ((state_kind = 'terminal'::text)
            = ((ended_at IS NOT NULL) AND (terminal_outcome_kind IS NOT NULL)))
        -- Guarded against a null outcome: `NULL = 'stopped'` is NULL, and a
        -- CHECK accepts NULL.
        AND ((terminal_outcome_kind IS NULL)
             OR ((terminal_outcome_kind = 'stopped'::text)
                 = (terminal_stop_sticky IS NOT NULL)))
        AND ((state_kind = 'terminal'::text) = (ended_at IS NOT NULL))
        AND ((state_kind = 'terminal'::text) = (terminal_outcome_kind IS NOT NULL))
        AND ((terminal_outcome_kind IS NOT NULL)
             OR ((terminal_stop_sticky IS NULL)
                 AND (terminal_superseded_by IS NULL)
                 AND (terminal_cause_kind IS NULL)
                 AND (ended_at IS NULL)))
        AND ((terminal_superseded_by IS NULL)
             OR (terminal_outcome_kind = 'superseded'::text))
        AND ((terminal_superseded_by IS NULL)
             OR (terminal_superseded_by <> session_id))
        AND (
            (terminal_outcome_kind IS NULL AND terminal_cause_kind IS NULL)
            OR (terminal_outcome_kind = ANY (ARRAY[
                    'achieved_verified'::text,
                    'achieved_declared'::text,
                    'failed_unknown'::text,
                    'stopped'::text,
                    'superseded'::text,
                    'abandoned'::text
                ]) AND terminal_cause_kind IS NULL)
            OR (terminal_outcome_kind = 'failed_retryable'::text
                AND terminal_cause_kind IS NOT NULL
                AND terminal_cause_kind = ANY (ARRAY[
                    'provider_transient'::text,
                    'provider_quota_exhausted'::text,
                    'provider_overloaded'::text,
                    'infrastructure_failure'::text,
                    'retry_budget_exhausted'::text
                ]))
            OR (terminal_outcome_kind = 'failed_structural'::text
                AND terminal_cause_kind IS NOT NULL
                AND terminal_cause_kind = ANY (ARRAY[
                    'context_compaction_wall'::text,
                    'context_headroom_exhausted'::text,
                    'broken_toolchain'::text,
                    'moderation_block'::text
                ]))
            OR (terminal_outcome_kind = 'retired'::text
                AND terminal_cause_kind IS NOT NULL
                AND terminal_cause_kind = ANY (ARRAY[
                    'dispatch_deadline_expired'::text,
                    'start_gate_deadline_expired'::text,
                    'stranded_queued_turn'::text
                ]))
        )
    ),
    ADD CONSTRAINT session_lifecycle_pending_terminal_shape CHECK (
        ((pending_terminal_outcome_kind IS NULL)
            OR (state_kind <> 'terminal'::text))
        AND ((pending_terminal_outcome_kind IS NOT NULL)
             OR ((pending_terminal_cause_kind IS NULL)
                 AND (pending_terminal_stop_sticky IS NULL)
                 AND (pending_terminal_superseded_by IS NULL)
                 AND (pending_terminal_actor_kind IS NULL)))
        -- A committed handoff always names its actor, and the actor's shape is
        -- the state actor's: one module name, or one acting identity, never
        -- both and never a bare classification that owes one.
        AND ((pending_terminal_outcome_kind IS NULL)
             = (pending_terminal_actor_kind IS NULL))
        AND ((pending_terminal_actor_kind IS NULL)
             OR (pending_terminal_actor_kind = ANY (ARRAY[
                    'core'::text,
                    'operator'::text,
                    'module'::text,
                    'watchdog'::text
                ])))
        AND ((pending_terminal_actor_kind IS NOT DISTINCT FROM 'module'::text)
             = (pending_terminal_actor_module IS NOT NULL))
        AND ((pending_terminal_actor_module IS NULL)
             OR (pending_terminal_actor_module = ANY (ARRAY[
                    'repo_watch'::text,
                    'commissioned_dispatch'::text
                ])))
        AND ((pending_terminal_actor_turn_id IS NULL)
             OR (pending_terminal_actor_tool_request_id IS NULL))
        AND ((pending_terminal_actor_kind IS NOT DISTINCT FROM 'core'::text)
             OR ((pending_terminal_actor_turn_id IS NULL)
                 AND (pending_terminal_actor_tool_request_id IS NULL)))
        AND ((pending_terminal_outcome_kind IS NULL)
             OR (pending_terminal_outcome_kind = ANY (ARRAY[
                    'achieved_verified'::text,
                    'achieved_declared'::text,
                    'failed_retryable'::text,
                    'failed_structural'::text,
                    'failed_unknown'::text,
                    'stopped'::text,
                    'superseded'::text,
                    'abandoned'::text,
                    'retired'::text
                ])))
        AND ((pending_terminal_outcome_kind IS NULL)
             OR ((pending_terminal_outcome_kind = 'stopped'::text)
                 = (pending_terminal_stop_sticky IS NOT NULL)))
        AND ((pending_terminal_superseded_by IS NULL)
             OR (pending_terminal_outcome_kind = 'superseded'::text))
        -- The pending shape carries the terminal shape's rules; a handoff
        -- settlement would reject can never be recorded.
        AND ((pending_terminal_superseded_by IS NULL)
             OR (pending_terminal_superseded_by <> session_id))
        -- The cause is scoped to its outcome here too: a handoff whose cause
        -- the terminal shape rejects can be committed and never settled.
        AND (
            (pending_terminal_outcome_kind IS NULL
                AND pending_terminal_cause_kind IS NULL)
            OR (pending_terminal_outcome_kind = ANY (ARRAY[
                    'achieved_verified'::text,
                    'achieved_declared'::text,
                    'failed_unknown'::text,
                    'stopped'::text,
                    'superseded'::text,
                    'abandoned'::text
                ]) AND pending_terminal_cause_kind IS NULL)
            OR (pending_terminal_outcome_kind = 'failed_retryable'::text
                AND pending_terminal_cause_kind IS NOT NULL
                AND pending_terminal_cause_kind = ANY (ARRAY[
                    'provider_transient'::text,
                    'provider_quota_exhausted'::text,
                    'provider_overloaded'::text,
                    'infrastructure_failure'::text,
                    'retry_budget_exhausted'::text
                ]))
            OR (pending_terminal_outcome_kind = 'failed_structural'::text
                AND pending_terminal_cause_kind IS NOT NULL
                AND pending_terminal_cause_kind = ANY (ARRAY[
                    'context_compaction_wall'::text,
                    'context_headroom_exhausted'::text,
                    'broken_toolchain'::text,
                    'moderation_block'::text
                ]))
            OR (pending_terminal_outcome_kind = 'retired'::text
                AND pending_terminal_cause_kind IS NOT NULL
                AND pending_terminal_cause_kind = ANY (ARRAY[
                    'dispatch_deadline_expired'::text,
                    'start_gate_deadline_expired'::text,
                    'stranded_queued_turn'::text
                ]))
        )
    ),
    ADD CONSTRAINT session_lifecycle_blocked_reason_closed CHECK (
        (blocked_reason IS NULL)
        OR (blocked_reason = ANY (ARRAY[
            'user_input_required'::text,
            'external_change_required'::text,
            'authorization_required'::text,
            'execution_failure'::text,
            'finish_check_failed'::text
        ]))
    );

-- Supersedes 202609020004_event_vocabulary.
ALTER TABLE session_terminal_outbox_event
    DROP CONSTRAINT session_terminal_outbox_outcome_closed;

ALTER TABLE session_terminal_outbox_event
    ADD CONSTRAINT session_terminal_outbox_outcome_closed CHECK (
        terminal_outcome_kind = ANY (ARRAY[
            'achieved_verified'::text, 'achieved_declared'::text, 'failed_retryable'::text,
            'failed_structural'::text, 'failed_unknown'::text, 'stopped'::text,
            'superseded'::text, 'abandoned'::text, 'retired'::text
        ])
    );

-- Supersedes 202609020003_lifecycle_metrics: `achieved_declared` finishes an overflow.
CREATE OR REPLACE VIEW session_lifecycle_weekly_metric AS
WITH wall_occurrence AS (
    -- F9's immediate half: a wall belongs to the week it happened in. §2 parks
    -- a session on a wall and suspends its turn, so the park is the evidence
    -- and `parked_since` the instant; terminalization carries both forward.
    -- The park therefore dates the occurrence wherever it exists, and a later
    -- terminal turn naming the same wall never moves it. A turn cause is the
    -- next evidence, for a wall that ended a turn without parking the session,
    -- at that row's write week; a session closed on a wall its turn never
    -- named is the last, at its closure. The sources are the ones the
    -- numerator counts, so a walled session always has an occurrence to show.
    -- One session's wall is one occurrence.
    SELECT session_row.session_id,
           COALESCE(
               (SELECT lifecycle.parked_since
                  FROM session_lifecycle AS lifecycle
                 WHERE lifecycle.session_id = session_row.session_id
                   AND lifecycle.parked_standing_cause_kind
                       = 'context_compaction_wall'::text
                   AND lifecycle.parked_since IS NOT NULL),
               (SELECT min(turn.recorded_at)
                  FROM turn_lifecycle AS turn
                 WHERE turn.session_id = session_row.session_id
                   AND turn.terminal_cause_kind = 'context_compaction_wall'::text),
               (SELECT lifecycle.ended_at
                  FROM session_lifecycle AS lifecycle
                 WHERE lifecycle.session_id = session_row.session_id
                   AND lifecycle.terminal_cause_kind
                       = 'context_compaction_wall'::text)
           ) AS occurred_at
      FROM session AS session_row
), weeks AS (
    SELECT cohort_week AS week FROM session_lifecycle_terminal_cohort
     UNION
    SELECT dispatch_week AS week FROM session_lifecycle_dispatch_cohort
     UNION
    SELECT recorded_week AS week FROM session_lifecycle_terminal_turn_cause
     UNION
    SELECT recorded_week AS week FROM session_lifecycle_known_failed_call_cause
     UNION
    SELECT session_lifecycle_metric_week(occurred_at) AS week
      FROM wall_occurrence
     WHERE occurred_at IS NOT NULL
), terminal AS (
    SELECT cohort.cohort_week AS week,
           count(*) AS cohort_size,
           count(*) FILTER (WHERE cohort.counts_in_denominator)
               AS completion_failure_denominator,
           count(*) FILTER (WHERE cohort.counts_in_numerator)
               AS completion_failure_numerator,
           count(*) FILTER (WHERE cohort.terminal_outcome_kind = 'failed_unknown'::text)
               AS failed_unknown,
           count(*) FILTER (WHERE incidence.recorded_context_headroom_exhausted)
               AS overflow,
           count(*) FILTER (WHERE incidence.recorded_context_headroom_exhausted
                              AND cohort.terminal_outcome_kind = ANY (ARRAY[
                                  'achieved_verified'::text, 'achieved_declared'::text]))
               AS overflow_finished
      FROM session_lifecycle_terminal_cohort AS cohort
      JOIN session_lifecycle_cause_incidence AS incidence
        ON incidence.session_id = cohort.session_id
     GROUP BY cohort.cohort_week
), dispatched AS (
    SELECT cohort.dispatch_week AS week,
           count(*) AS cohort_size,
           count(*) FILTER (WHERE cohort.wall) AS wall
      FROM session_lifecycle_dispatch_cohort AS cohort
     GROUP BY cohort.dispatch_week
), walls_recorded AS (
    SELECT session_lifecycle_metric_week(occurrence.occurred_at) AS week,
           count(*) AS occurrences
      FROM wall_occurrence AS occurrence
     WHERE occurrence.occurred_at IS NOT NULL
     GROUP BY session_lifecycle_metric_week(occurrence.occurred_at)
), turn_causes AS (
    SELECT cause.recorded_week AS week,
           count(*) AS terminal_turns,
           count(*) FILTER (WHERE cause.classified) AS classified_turns
      FROM session_lifecycle_terminal_turn_cause AS cause
     GROUP BY cause.recorded_week
), call_causes AS (
    SELECT cause.recorded_week AS week,
           count(*) AS known_failed_calls,
           count(*) FILTER (WHERE cause.classified) AS classified_calls
      FROM session_lifecycle_known_failed_call_cause AS cause
     GROUP BY cause.recorded_week
)
SELECT weeks.week,
       COALESCE(terminal.cohort_size, 0) AS terminal_cohort_size,
       COALESCE(terminal.completion_failure_denominator, 0)
           AS completion_failure_denominator,
       COALESCE(terminal.completion_failure_numerator, 0)
           AS completion_failure_numerator,
       COALESCE(terminal.failed_unknown, 0) AS failed_unknown_count,
       COALESCE(terminal.overflow, 0) AS overflow_count,
       COALESCE(terminal.overflow_finished, 0) AS overflow_finished_count,
       COALESCE(dispatched.cohort_size, 0) AS dispatch_cohort_size,
       COALESCE(dispatched.wall, 0) AS wall_count,
       COALESCE(walls_recorded.occurrences, 0) AS wall_occurrence_count,
       COALESCE(turn_causes.terminal_turns, 0) AS terminal_turn_count,
       COALESCE(turn_causes.classified_turns, 0) AS classified_terminal_turn_count,
       COALESCE(call_causes.known_failed_calls, 0) AS known_failed_call_count,
       COALESCE(call_causes.classified_calls, 0) AS classified_known_failed_call_count
  FROM weeks
  LEFT JOIN terminal ON terminal.week = weeks.week
  LEFT JOIN dispatched ON dispatched.week = weeks.week
  LEFT JOIN walls_recorded ON walls_recorded.week = weeks.week
  LEFT JOIN turn_causes ON turn_causes.week = weeks.week
  LEFT JOIN call_causes ON call_causes.week = weeks.week;

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
        OR ((terminal_disposition_kind IS NOT NULL) AND (FALSE
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
                'model_call_ambiguous'::text,
                'tool_attempt_ambiguous'::text
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
            ])))))
    );

--
-- §1/§2 settlement. A closure that finds a live turn commits its outcome to
-- the handoff and hands the turn to the committed interrupt machinery; the
-- transaction that terminalizes the session's last live turn records the
-- terminal state, retiring the queued turns the closure stranded. Deferred to
-- commit, so the causal turn's own terminal event precedes the retirements
-- and the session's terminal event; the queue drains in one invocation, and
-- the invocations its own retirements queue find the session terminal.
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
         WHERE session_id = subject
           AND state_kind = 'active'
           AND NOT delegation_runtime_terminal
    ) THEN
        RETURN;
    END IF;

    LOOP
        SELECT turn_id INTO queued
          FROM turn_lifecycle
         WHERE session_id = subject AND state_kind = 'queued'
         ORDER BY acceptance_position
         LIMIT 1;
        EXIT WHEN NOT FOUND;
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
    END LOOP;

    SELECT event_kind, generation, event_ordinal
      INTO last_kind, last_generation, last_ordinal
      FROM goal_event
     WHERE session_id = subject
     ORDER BY event_ordinal DESC
     LIMIT 1;
    IF FOUND AND last_kind = 'user_stopped'
       AND held.pending_terminal_outcome_kind <> 'stopped'
    THEN
        RAISE EXCEPTION
            'session % cannot settle % over a goal the user stopped',
            subject, held.pending_terminal_outcome_kind
            USING ERRCODE = '23514';
    END IF;
    IF FOUND AND last_kind IN ('commissioned', 'resumed', 'blocked', 'superseded') THEN
        IF held.pending_terminal_outcome_kind IN ('achieved_verified', 'achieved_declared') THEN
            RAISE EXCEPTION
                'session % cannot settle an achievement over an open goal', subject
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
           actor_turn_id = pending_terminal_actor_turn_id,
           actor_tool_request_id = pending_terminal_actor_tool_request_id,
           waiting_kind = NULL,
           waiting_waker = NULL,
           waiting_subject_session_id = NULL,
           recovering_op = NULL,
           blocked_reason = NULL,
           blocked_cycle = NULL,
           parked_cause = NULL,
           parked_responder = NULL,
           parked_since = NULL,
           parked_standing_cause_kind = NULL,
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
           pending_terminal_actor_module = NULL,
           pending_terminal_actor_turn_id = NULL,
           pending_terminal_actor_tool_request_id = NULL
     WHERE session_id = subject;

END;
$$;

CREATE FUNCTION settle_session_pending_terminal_from_turn() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.state_kind = 'terminal' OR NEW.delegation_runtime_terminal THEN
        PERFORM settle_session_pending_terminal(NEW.session_id);
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER turn_lifecycle_settles_pending_terminal
    AFTER UPDATE OF state_kind, delegation_runtime_terminal ON turn_lifecycle
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION settle_session_pending_terminal_from_turn();
