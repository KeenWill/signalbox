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
    IF OLD.state_kind = 'terminal'
       AND NOT (
            NEW.state_kind = 'terminal'
            AND OLD.terminal_cause_kind IN (
                'dispatch_deadline_expired', 'start_gate_deadline_expired'
            )
            AND NEW.terminal_cause_kind = 'admission_deadline_expired'
            AND to_jsonb(NEW) - 'terminal_cause_kind'
                = to_jsonb(OLD) - 'terminal_cause_kind'
       )
    THEN
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

DO $rewrite_retirement_cause$
DECLARE
    constraint_names text[] := ARRAY[
        'session_lifecycle_terminal_cause_closed',
        'session_lifecycle_terminal_shape',
        'session_lifecycle_pending_terminal_shape'
    ];
    rewritten_definitions text[] := ARRAY[]::text[];
    constraint_name text;
    definition text;
    rewritten text;
    position integer;
BEGIN
    FOREACH constraint_name IN ARRAY constraint_names
    LOOP
        SELECT pg_get_constraintdef(oid) INTO definition
          FROM pg_constraint
         WHERE conrelid = 'session_lifecycle'::regclass
           AND conname = constraint_name;
        rewritten := replace(
            definition,
            '''dispatch_deadline_expired''::text, ''start_gate_deadline_expired''::text',
            '''admission_deadline_expired''::text'
        );
        IF rewritten = definition
           OR rewritten LIKE '%dispatch_deadline_expired%'
           OR rewritten LIKE '%start_gate_deadline_expired%'
           OR rewritten NOT LIKE '%admission_deadline_expired%'
        THEN
            RAISE EXCEPTION 'constraint % did not carry the prior retirement causes',
                constraint_name;
        END IF;
        rewritten_definitions := array_append(rewritten_definitions, rewritten);
    END LOOP;

    FOREACH constraint_name IN ARRAY constraint_names
    LOOP
        EXECUTE format('ALTER TABLE session_lifecycle DROP CONSTRAINT %I', constraint_name);
    END LOOP;

    UPDATE session_lifecycle
       SET terminal_cause_kind = CASE
               WHEN terminal_cause_kind IN (
                   'dispatch_deadline_expired', 'start_gate_deadline_expired'
               ) THEN 'admission_deadline_expired'
               ELSE terminal_cause_kind
           END,
           pending_terminal_cause_kind = CASE
               WHEN pending_terminal_cause_kind IN (
                   'dispatch_deadline_expired', 'start_gate_deadline_expired'
               ) THEN 'admission_deadline_expired'
               ELSE pending_terminal_cause_kind
           END
     WHERE terminal_cause_kind IN (
               'dispatch_deadline_expired', 'start_gate_deadline_expired'
           )
        OR pending_terminal_cause_kind IN (
               'dispatch_deadline_expired', 'start_gate_deadline_expired'
           );

    SET CONSTRAINTS ALL IMMEDIATE;
    FOR position IN 1..array_length(constraint_names, 1)
    LOOP
        EXECUTE format(
            'ALTER TABLE session_lifecycle ADD CONSTRAINT %I %s',
            constraint_names[position],
            rewritten_definitions[position]
        );
    END LOOP;
END
$rewrite_retirement_cause$;

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
    -- A parked obligation may be refreshed with a different blocker, released,
    -- or settled. Restore subjects it no longer wraps before projecting any
    -- replacement, unless a different parked obligation still names them.
    IF OLD.parked_at IS NOT NULL AND OLD.settled_kind IS NULL THEN
        FOR subject IN
            (
                SELECT OLD.external_blocking_session_id
                 WHERE OLD.external_blocking_session_id IS NOT NULL
                UNION
                SELECT action.session_id
                  FROM repo_watch_dispatch_action AS action
                 WHERE action.dispatch_id = OLD.blocking_dispatch_id
            )
            EXCEPT
            (
                SELECT NEW.external_blocking_session_id
                 WHERE NEW.parked_at IS NOT NULL
                   AND NEW.settled_kind IS NULL
                   AND NEW.external_blocking_session_id IS NOT NULL
                UNION
                SELECT action.session_id
                  FROM repo_watch_dispatch_action AS action
                 WHERE NEW.parked_at IS NOT NULL
                   AND NEW.settled_kind IS NULL
                   AND action.dispatch_id = NEW.blocking_dispatch_id
            )
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
                           WHEN EXISTS (
                               SELECT 1
                                 FROM turn_lifecycle
                                WHERE session_id = subject
                                  AND state_kind = 'queued'
                           ) AND NOT EXISTS (
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
    AFTER UPDATE OF parked_at, blocking_dispatch_id,
                    external_blocking_session_id, settled_kind
    ON repo_watch_dispatch_obligation
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    WHEN (OLD.parked_at IS DISTINCT FROM NEW.parked_at
          OR OLD.blocking_dispatch_id IS DISTINCT FROM NEW.blocking_dispatch_id
          OR OLD.external_blocking_session_id
                IS DISTINCT FROM NEW.external_blocking_session_id
          OR OLD.settled_kind IS DISTINCT FROM NEW.settled_kind)
    EXECUTE FUNCTION park_repo_watch_obligation_sessions();
