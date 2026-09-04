-- Core start-gate release, lifecycle deadline transitions, and module park projection.

ALTER TABLE session_lifecycle_command
    DROP CONSTRAINT session_lifecycle_command_operation_closed,
    DROP CONSTRAINT session_lifecycle_command_operation_shape,
    DROP CONSTRAINT session_lifecycle_command_result_shape;

ALTER TABLE session_lifecycle_command
    ADD CONSTRAINT session_lifecycle_command_operation_closed CHECK (
        operation_kind = ANY (ARRAY[
            'release_start'::text,
            'stop'::text,
            'supersede'::text,
            'abandon'::text,
            'close_failed'::text,
            'resume'::text,
            'adopt'::text,
            'release'::text
        ])
    ),
    ADD CONSTRAINT session_lifecycle_command_operation_shape CHECK (
        ((operation_kind = 'stop'::text) = (stop_sticky IS NOT NULL))
        AND ((operation_kind = 'stop'::text) = (descendant_scope IS NOT NULL))
        AND ((operation_kind = 'supersede'::text) = (successor_session_id IS NOT NULL))
        AND ((failure_cause_kind IS NULL) OR (operation_kind = 'close_failed'::text))
        AND ((finish_condition_kind IS NULL) OR (operation_kind = 'adopt'::text))
        AND ((descendant_scope IS NULL) OR (descendant_scope = ANY (ARRAY[
            'parent_alone'::text, 'parent_and_descendants'::text
        ])))
    ),
    ADD CONSTRAINT session_lifecycle_command_result_shape CHECK (
        (result_kind = ANY (ARRAY['applied'::text, 'rejected'::text]))
        AND ((result_kind = 'rejected'::text) = (rejection_kind IS NOT NULL))
        AND ((result_kind = 'applied'::text) = (applied_effect_kind IS NOT NULL))
        AND ((applied_effect_kind IS NULL) OR (applied_effect_kind = ANY (ARRAY[
            'start_released'::text,
            'closed'::text,
            'closure_pending'::text,
            'resumed'::text,
            'ownership_changed'::text
        ])))
        AND ((applied_effect_kind IS NULL)
             OR ((operation_kind = 'release_start'::text)
                 AND (applied_effect_kind = 'start_released'::text))
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
    );

-- Admission is one deadline across both pre-activity states.
CREATE OR REPLACE FUNCTION guard_session_lifecycle_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'session lifecycle rows are never deleted'
            USING ERRCODE = '23514';
    END IF;
    IF OLD.state_kind = 'terminal' THEN
        RAISE EXCEPTION 'session lifecycle is terminal and cannot change'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.session_id IS DISTINCT FROM OLD.session_id THEN
        RAISE EXCEPTION 'session lifecycle identity is immutable'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

ALTER TABLE session_lifecycle
    DROP CONSTRAINT session_lifecycle_terminal_cause_closed,
    DROP CONSTRAINT session_lifecycle_terminal_shape,
    DROP CONSTRAINT session_lifecycle_pending_terminal_shape;

ALTER TABLE session_lifecycle
    ADD CONSTRAINT session_lifecycle_terminal_cause_closed CHECK (
        (terminal_cause_kind IS NULL)
        OR (terminal_cause_kind = ANY (ARRAY[
            'provider_transient'::text,
            'provider_quota_exhausted'::text,
            'provider_overloaded'::text,
            'infrastructure_failure'::text,
            'retry_budget_exhausted'::text,
            'context_compaction_wall'::text,
            'context_headroom_exhausted'::text,
            'broken_toolchain'::text,
            'moderation_block'::text,
            'admission_deadline_expired'::text,
            'stranded_queued_turn'::text
        ]))
    ),
    ADD CONSTRAINT session_lifecycle_terminal_shape CHECK (
        ((state_kind = 'terminal'::text)
            = ((ended_at IS NOT NULL) AND (terminal_outcome_kind IS NOT NULL)))
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
                    'admission_deadline_expired'::text,
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
        AND ((pending_terminal_superseded_by IS NULL)
             OR (pending_terminal_superseded_by <> session_id))
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
                    'admission_deadline_expired'::text,
                    'stranded_queued_turn'::text
                ]))
        )
    );

CREATE OR REPLACE FUNCTION arm_session_deadline() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    required text;
    armed text;
BEGIN
    required := session_deadline_kind_for_state(NEW.state_kind);

    IF NOT NEW.owned OR required IS NULL THEN
        DELETE FROM session_deadline WHERE session_id = NEW.session_id;
        RETURN NULL;
    END IF;

    SELECT deadline_kind INTO armed
      FROM session_deadline
     WHERE session_id = NEW.session_id;

    IF TG_OP = 'UPDATE'
       AND armed IS NOT DISTINCT FROM required
       AND OLD.owned = NEW.owned
       AND (
            OLD.state_entered_at = NEW.state_entered_at
            OR required = 'admission'
       )
    THEN
        RETURN NULL;
    END IF;

    INSERT INTO session_deadline
            (session_id, deadline_kind, on_expiry_kind, armed_at)
         VALUES (
            NEW.session_id,
            required,
            session_deadline_expiry_for_kind(required),
            statement_timestamp()
         )
    ON CONFLICT (session_id) DO UPDATE
       SET deadline_kind = EXCLUDED.deadline_kind,
           on_expiry_kind = EXCLUDED.on_expiry_kind,
           expires_at = NULL,
           armed_at = EXCLUDED.armed_at;

    RETURN NULL;
END;
$$;

CREATE OR REPLACE FUNCTION hold_session_start_gate() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.start_gate_held
       AND NEW.state_kind NOT IN ('created', 'terminal')
       AND NOT (
            NEW.state_kind = 'parked'
            AND NEW.parked_cause = 'module_park'
       )
    THEN
        RETURN OLD;
    END IF;
    RETURN NEW;
END;
$$;

-- A terminal repository-watch goal can spend the dispatch lineage's retry
-- budget and park every session in its batch. Lock that complete cohort in
-- session identity order before the ordinary goal projection locks its one
-- subject, so concurrent sibling terminations cannot each hold one lifecycle
-- row while the deferred park projection waits for the other.
CREATE OR REPLACE FUNCTION project_session_lifecycle_from_goal()
RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    authored_kind text;
    authored_module text;
BEGIN
    IF NEW.event_kind IN
        ('blocked', 'achieved', 'user_stopped', 'session_closed')
    THEN
        PERFORM lifecycle.session_id
          FROM session_lifecycle AS lifecycle
          JOIN (
                SELECT cohort.session_id
                  FROM repo_watch_dispatch_action AS subject
                  JOIN repo_watch_dispatch_action AS cohort
                    ON cohort.dispatch_id = subject.dispatch_id
                 WHERE subject.session_id = NEW.session_id
                   AND NOT EXISTS (
                        SELECT 1
                          FROM repo_watch_dispatch_release AS released
                         WHERE released.dispatch_id = subject.dispatch_id
                   )
          ) AS dispatch_subject USING (session_id)
         ORDER BY lifecycle.session_id
           FOR UPDATE OF lifecycle;
    END IF;

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

-- Rule retirement settles parked obligations. Lock their projected subjects
-- before the deactivation row takes its activation reference and before the
-- settlement trigger locks each obligation, preserving lifecycle-before-
-- obligation order against concurrent goal termination.
CREATE FUNCTION lock_repo_watch_deactivation_session_lifecycles()
RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM lifecycle.session_id
      FROM session_lifecycle AS lifecycle
      JOIN (
            SELECT obligation.external_blocking_session_id AS session_id
              FROM repo_watch_dispatch_obligation AS obligation
             WHERE obligation.repository = NEW.repository
               AND obligation.rule_id = NEW.rule_id
               AND obligation.rule_version = NEW.rule_version
               AND obligation.settled_kind IS NULL
               AND obligation.parked_at IS NOT NULL
               AND obligation.external_blocking_session_id IS NOT NULL
            UNION
            SELECT action.session_id
              FROM repo_watch_dispatch_obligation AS obligation
              JOIN repo_watch_dispatch_action AS action
                ON action.dispatch_id = obligation.blocking_dispatch_id
             WHERE obligation.repository = NEW.repository
               AND obligation.rule_id = NEW.rule_id
               AND obligation.rule_version = NEW.rule_version
               AND obligation.settled_kind IS NULL
               AND obligation.parked_at IS NOT NULL
      ) AS subject USING (session_id)
     ORDER BY lifecycle.session_id
       FOR UPDATE OF lifecycle;
    RETURN NEW;
END;
$$;

CREATE TRIGGER repo_watch_deactivation_locks_core_sessions
    BEFORE INSERT ON repo_watch_rule_deactivation
    FOR EACH ROW
    EXECUTE FUNCTION lock_repo_watch_deactivation_session_lifecycles();

-- A module obligation that directly names or wraps a live session projects
-- that park into the core lifecycle queue.
CREATE FUNCTION park_repo_watch_obligation_sessions() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    subject uuid;
    restored_state text;
BEGIN
    -- Releasing or settling a parked obligation restores its subjects unless
    -- a different parked obligation still names them.
    IF OLD.parked_at IS NOT NULL
       AND OLD.settled_kind IS NULL
       AND (NEW.parked_at IS NULL OR NEW.settled_kind IS NOT NULL)
    THEN
        FOR subject IN
            SELECT OLD.external_blocking_session_id
             WHERE OLD.external_blocking_session_id IS NOT NULL
            UNION
            SELECT action.session_id
              FROM repo_watch_dispatch_action AS action
             WHERE action.dispatch_id = OLD.blocking_dispatch_id
        LOOP
            IF EXISTS (
                SELECT 1
                  FROM session_lifecycle
                 WHERE session_id = subject
                   AND state_kind = 'parked'
                   AND parked_cause = 'module_park'
                   AND parked_responder = 'repo_watch'
            ) AND NOT EXISTS (
                SELECT 1
                  FROM repo_watch_dispatch_obligation AS obligation
                 WHERE obligation.obligation_id <> NEW.obligation_id
                   AND obligation.parked_at IS NOT NULL
                   AND obligation.settled_kind IS NULL
                   AND (
                        obligation.external_blocking_session_id = subject
                        OR EXISTS (
                            SELECT 1
                              FROM repo_watch_dispatch_action AS action
                             WHERE action.dispatch_id = obligation.blocking_dispatch_id
                               AND action.session_id = subject
                        )
                   )
            ) THEN
                UPDATE session_lifecycle AS lifecycle
                   SET state_kind = CASE
                           WHEN lifecycle.start_gate_held THEN 'created'
                           WHEN NOT EXISTS (
                               SELECT 1
                                 FROM turn_lifecycle
                                WHERE session_id = subject
                           ) THEN 'created'
                           WHEN NOT EXISTS (
                               SELECT 1
                                 FROM turn_lifecycle
                                WHERE session_id = subject
                                  AND start_lineage_kind IS NOT NULL
                           ) THEN 'dispatched'
                           ELSE 'active'
                       END,
                       state_entered_at = statement_timestamp(),
                       actor_kind = 'module',
                       actor_module = 'repo_watch',
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
                       parked_since = NULL,
                       parked_standing_cause_kind = NULL
                 WHERE lifecycle.session_id = subject
             RETURNING lifecycle.state_kind INTO restored_state;
                IF restored_state = 'active' THEN
                    PERFORM project_session_lifecycle(
                        subject, false, 'module', 'repo_watch'
                    );
                END IF;
            END IF;
        END LOOP;
    END IF;

    IF NEW.parked_at IS NOT NULL AND NEW.settled_kind IS NULL THEN
        FOR subject IN
            SELECT NEW.external_blocking_session_id
             WHERE NEW.external_blocking_session_id IS NOT NULL
            UNION
            SELECT action.session_id
              FROM repo_watch_dispatch_action AS action
             WHERE action.dispatch_id = NEW.blocking_dispatch_id
        LOOP
            UPDATE session_lifecycle
               SET state_kind = 'parked',
                   state_entered_at = statement_timestamp(),
                   actor_kind = 'module',
                   actor_module = 'repo_watch',
                   actor_turn_id = NULL,
                   actor_tool_request_id = NULL,
                   waiting_kind = NULL,
                   waiting_waker = NULL,
                   waiting_subject_session_id = NULL,
                   recovering_op = NULL,
                   blocked_reason = NULL,
                   blocked_cycle = NULL,
                   parked_cause = 'module_park',
                   parked_responder = 'repo_watch',
                   parked_since = statement_timestamp(),
                   parked_standing_cause_kind = NULL
             WHERE session_id = subject
               AND owned
               AND state_kind IN (
                    'created', 'dispatched', 'active', 'waiting', 'recovering', 'blocked'
               )
               AND pending_terminal_outcome_kind IS NULL;
        END LOOP;
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER repo_watch_obligation_parks_core_session
    AFTER UPDATE OF parked_at, settled_kind
    ON repo_watch_dispatch_obligation
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    WHEN (OLD.parked_at IS DISTINCT FROM NEW.parked_at
          OR OLD.settled_kind IS DISTINCT FROM NEW.settled_kind)
    EXECUTE FUNCTION park_repo_watch_obligation_sessions();

-- Blocker replacement and park release must agree which lifecycle subjects an
-- obligation names before either path locks them. This identity-scoped lock is
-- independent of the mutable obligation row, so both paths can serialize
-- before preserving lifecycle-before-obligation row-lock order.
CREATE FUNCTION repo_watch_dispatch_obligation_lock_key(
    candidate_obligation_id uuid
) RETURNS text
    LANGUAGE sql IMMUTABLE
    AS $$
    SELECT concat_ws(
        E'\x1f',
        'repo-watch-obligation',
        candidate_obligation_id::text
    )
$$;

-- Park release restores the projected lifecycle subject. Stabilize its
-- identity, then acquire that subject before the obligation row so lifecycle
-- commands that settle obligations use the same lifecycle-before-obligation
-- order.
CREATE OR REPLACE FUNCTION repo_watch_release_dispatch_obligation_park_for_progress(
    parked_obligation_id uuid,
    progress_event_id uuid
) RETURNS boolean
    LANGUAGE plpgsql
    AS $$
DECLARE
    parked_attempts bigint;
BEGIN
    PERFORM pg_advisory_xact_lock(
        hashtextextended(
            repo_watch_dispatch_obligation_lock_key(parked_obligation_id),
            0
        )
    );

    PERFORM lifecycle.session_id
      FROM session_lifecycle AS lifecycle
      JOIN (
            SELECT obligation.external_blocking_session_id AS session_id
              FROM repo_watch_dispatch_obligation AS obligation
             WHERE obligation.obligation_id = parked_obligation_id
               AND obligation.external_blocking_session_id IS NOT NULL
            UNION
            SELECT action.session_id
              FROM repo_watch_dispatch_obligation AS obligation
              JOIN repo_watch_dispatch_action AS action
                ON action.dispatch_id = obligation.blocking_dispatch_id
             WHERE obligation.obligation_id = parked_obligation_id
      ) AS subject USING (session_id)
     ORDER BY lifecycle.session_id
       FOR UPDATE OF lifecycle;

    SELECT obligation.failed_attempts
      INTO parked_attempts
      FROM repo_watch_dispatch_obligation AS obligation
     WHERE obligation.obligation_id = parked_obligation_id
       AND obligation.settled_kind IS NULL
       AND obligation.parked_at IS NOT NULL
       FOR UPDATE;

    IF NOT FOUND THEN
        RETURN false;
    END IF;

    PERFORM repo_watch_record_dispatch_obligation_park_transition(
        parked_obligation_id,
        'released',
        parked_attempts,
        'pull_request_progress',
        progress_event_id,
        NULL::text
    );

    UPDATE repo_watch_dispatch_obligation
       SET parked_at = NULL,
           parked_state_event_id = NULL,
           failed_attempts = 0,
           last_failed_attempt_at = NULL
     WHERE obligation_id = parked_obligation_id;

    RETURN true;
END;
$$;

CREATE OR REPLACE FUNCTION repo_watch_release_parked_dispatch_obligation(
    parked_obligation_id uuid,
    releasing_actor text
) RETURNS boolean
    LANGUAGE plpgsql
    AS $$
DECLARE
    parked_attempts bigint;
BEGIN
    PERFORM pg_advisory_xact_lock(
        hashtextextended(
            repo_watch_dispatch_obligation_lock_key(parked_obligation_id),
            0
        )
    );

    PERFORM lifecycle.session_id
      FROM session_lifecycle AS lifecycle
      JOIN (
            SELECT obligation.external_blocking_session_id AS session_id
              FROM repo_watch_dispatch_obligation AS obligation
             WHERE obligation.obligation_id = parked_obligation_id
               AND obligation.external_blocking_session_id IS NOT NULL
            UNION
            SELECT action.session_id
              FROM repo_watch_dispatch_obligation AS obligation
              JOIN repo_watch_dispatch_action AS action
                ON action.dispatch_id = obligation.blocking_dispatch_id
             WHERE obligation.obligation_id = parked_obligation_id
      ) AS subject USING (session_id)
     ORDER BY lifecycle.session_id
       FOR UPDATE OF lifecycle;

    SELECT obligation.failed_attempts
      INTO parked_attempts
      FROM repo_watch_dispatch_obligation AS obligation
     WHERE obligation.obligation_id = parked_obligation_id
       AND obligation.settled_kind IS NULL
       AND obligation.parked_at IS NOT NULL
       FOR UPDATE;

    IF NOT FOUND THEN
        RETURN false;
    END IF;

    PERFORM repo_watch_record_dispatch_obligation_park_transition(
        parked_obligation_id,
        'released',
        parked_attempts,
        'operator',
        NULL,
        releasing_actor
    );

    UPDATE repo_watch_dispatch_obligation
       SET parked_at = NULL,
           parked_state_event_id = NULL,
           failed_attempts = 0,
           last_failed_attempt_at = NULL
     WHERE obligation_id = parked_obligation_id;

    RETURN true;
END;
$$;
