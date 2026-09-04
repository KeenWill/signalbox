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
DO $rewrite_retirement_cause$
DECLARE
    constraint_name text;
    definition text;
    rewritten text;
BEGIN
    FOREACH constraint_name IN ARRAY ARRAY[
        'session_lifecycle_terminal_cause_closed',
        'session_lifecycle_terminal_shape',
        'session_lifecycle_pending_terminal_shape'
    ]
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
        EXECUTE format(
            'ALTER TABLE session_lifecycle DROP CONSTRAINT %I, ADD CONSTRAINT %I %s',
            constraint_name,
            constraint_name,
            rewritten
        );
    END LOOP;
END
$rewrite_retirement_cause$;

-- A module obligation that directly names or wraps a live session projects
-- that park into the core lifecycle queue.
CREATE FUNCTION park_repo_watch_obligation_sessions() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    subject uuid;
BEGIN
    IF NEW.parked_at IS NULL OR OLD.parked_at IS NOT NULL THEN
        RETURN NULL;
    END IF;

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
    RETURN NULL;
END;
$$;

CREATE TRIGGER repo_watch_obligation_parks_core_session
    AFTER UPDATE OF parked_at ON repo_watch_dispatch_obligation
    FOR EACH ROW EXECUTE FUNCTION park_repo_watch_obligation_sessions();
