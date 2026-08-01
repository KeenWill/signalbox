-- Session-scoped commissioned-goal commands and append-only event histories.

ALTER TABLE durable_command
    DROP CONSTRAINT durable_command_kind_closed,
    DROP CONSTRAINT durable_command_storage_version_supported;

ALTER TABLE durable_command
    ADD CONSTRAINT durable_command_kind_closed
        CHECK (
            command_kind IN (
                'create_session',
                'create_session_from_imported_frontier',
                'replace_session_defaults',
                'replace_session_metadata',
                'submit_input',
                'decide_tool_request',
                'review_workflow',
                'review_orchestration',
                'compact_session',
                'goal'
            )
        ),
    ADD CONSTRAINT durable_command_storage_version_supported
        CHECK (
            (command_kind = 'create_session' AND storage_version IN (1, 2, 3, 4))
            OR (
                command_kind IN (
                    'create_session_from_imported_frontier',
                    'replace_session_defaults'
                )
                AND storage_version IN (1, 2, 3)
            )
            OR (
                command_kind IN (
                    'replace_session_metadata',
                    'submit_input',
                    'decide_tool_request',
                    'review_workflow',
                    'review_orchestration',
                    'compact_session',
                    'goal'
                )
                AND storage_version = 1
            )
        );

CREATE TABLE goal_command (
    command_id uuid PRIMARY KEY,
    command_kind text NOT NULL CHECK (command_kind = 'goal'),
    storage_version smallint NOT NULL CHECK (storage_version = 1),
    session_id uuid NOT NULL,
    operation_kind text NOT NULL CHECK (
        operation_kind IN ('attach', 'resume', 'stop', 'supersede')
    ),
    statement text CHECK (
        statement IS NULL OR octet_length(statement) BETWEEN 1 AND 1048576
    ),
    guidance text CHECK (
        guidance IS NULL OR octet_length(guidance) BETWEEN 1 AND 1048576
    ),
    result_kind text NOT NULL CHECK (result_kind IN ('applied', 'rejected')),
    rejection_kind text CHECK (
        rejection_kind IS NULL OR rejection_kind IN (
            'session_not_found', 'goal_already_attached', 'goal_not_attached',
            'unknown_model_alias', 'requires_blocked', 'requires_pursuing_or_blocked',
            'generation_exhausted', 'event_ordinal_exhausted',
            'acceptance_position_exhausted'
        )
    ),
    result_event_ordinal numeric(20, 0) CHECK (
        result_event_ordinal IS NULL
        OR result_event_ordinal BETWEEN 1 AND 18446744073709551615
    ),
    CONSTRAINT goal_command_operation_shape CHECK (
        (operation_kind IN ('attach', 'supersede') AND statement IS NOT NULL AND guidance IS NULL)
        OR (operation_kind = 'resume' AND statement IS NULL)
        OR (operation_kind = 'stop' AND statement IS NULL AND guidance IS NULL)
    ),
    CONSTRAINT goal_command_result_shape CHECK (
        (result_kind = 'applied' AND rejection_kind IS NULL AND result_event_ordinal IS NOT NULL)
        OR (result_kind = 'rejected' AND rejection_kind IS NOT NULL AND result_event_ordinal IS NULL)
    ),
    CONSTRAINT goal_command_rejection_operation CHECK (
        result_kind = 'applied'
        OR rejection_kind = 'session_not_found'
        OR (operation_kind = 'attach' AND rejection_kind IN (
            'goal_already_attached', 'unknown_model_alias',
            'generation_exhausted', 'event_ordinal_exhausted',
            'acceptance_position_exhausted'
        ))
        OR (operation_kind = 'resume' AND rejection_kind IN (
            'goal_not_attached', 'unknown_model_alias',
            'requires_blocked', 'event_ordinal_exhausted',
            'acceptance_position_exhausted'
        ))
        OR (operation_kind = 'stop' AND rejection_kind IN (
            'goal_not_attached', 'requires_pursuing_or_blocked',
            'event_ordinal_exhausted'
        ))
        OR (operation_kind = 'supersede' AND rejection_kind IN (
            'goal_not_attached', 'unknown_model_alias',
            'requires_pursuing_or_blocked', 'generation_exhausted',
            'event_ordinal_exhausted', 'acceptance_position_exhausted'
        ))
    ),
    CONSTRAINT goal_command_session_correlation_key UNIQUE (command_id, session_id),
    FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command(command_id, command_kind, storage_version)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE goal_event (
    session_id uuid NOT NULL REFERENCES session(session_id) ON DELETE RESTRICT,
    event_ordinal numeric(20, 0) NOT NULL CHECK (
        event_ordinal BETWEEN 1 AND 18446744073709551615
    ),
    generation numeric(20, 0) NOT NULL CHECK (
        generation BETWEEN 1 AND 18446744073709551615
    ),
    event_kind text NOT NULL CHECK (
        event_kind IN (
            'commissioned', 'blocked', 'resumed', 'achieved',
            'user_stopped', 'superseded'
        )
    ),
    statement text CHECK (
        statement IS NULL OR octet_length(statement) BETWEEN 1 AND 1048576
    ),
    blocked_reason text CHECK (
        blocked_reason IS NULL OR blocked_reason IN (
            'user_input_required', 'external_change_required',
            'authorization_required', 'execution_failure'
        )
    ),
    need text CHECK (
        need IS NULL OR octet_length(need) BETWEEN 1 AND 1048576
    ),
    guidance text CHECK (
        guidance IS NULL OR octet_length(guidance) BETWEEN 1 AND 1048576
    ),
    report text CHECK (
        report IS NULL OR octet_length(report) BETWEEN 1 AND 1048576
    ),
    user_command_id uuid,
    model_turn_id uuid,
    model_tool_request_id uuid,
    scheduler_turn_id uuid,
    PRIMARY KEY (session_id, event_ordinal),
    UNIQUE (user_command_id),
    UNIQUE (model_tool_request_id),
    UNIQUE (scheduler_turn_id),
    CONSTRAINT goal_event_user_command_result_key
        UNIQUE (user_command_id, session_id, event_ordinal),
    CONSTRAINT goal_event_shape CHECK (
        (event_kind = 'commissioned'
            AND statement IS NOT NULL AND blocked_reason IS NULL AND need IS NULL
            AND guidance IS NULL AND report IS NULL AND user_command_id IS NOT NULL
            AND model_turn_id IS NULL AND model_tool_request_id IS NULL
            AND scheduler_turn_id IS NULL)
        OR (event_kind = 'blocked'
            AND statement IS NULL AND blocked_reason IS NOT NULL AND need IS NOT NULL
            AND guidance IS NULL AND report IS NULL AND user_command_id IS NULL
            AND (
                (blocked_reason = 'execution_failure'
                    AND model_turn_id IS NULL AND model_tool_request_id IS NULL
                    AND scheduler_turn_id IS NOT NULL)
                OR (blocked_reason <> 'execution_failure'
                    AND model_turn_id IS NOT NULL AND model_tool_request_id IS NOT NULL
                    AND scheduler_turn_id IS NULL)
            ))
        OR (event_kind = 'resumed'
            AND statement IS NULL AND blocked_reason IS NULL AND need IS NULL
            AND report IS NULL AND user_command_id IS NOT NULL
            AND model_turn_id IS NULL AND model_tool_request_id IS NULL
            AND scheduler_turn_id IS NULL)
        OR (event_kind = 'achieved'
            AND statement IS NULL AND blocked_reason IS NULL AND need IS NULL
            AND guidance IS NULL AND report IS NOT NULL AND user_command_id IS NULL
            AND model_turn_id IS NOT NULL AND model_tool_request_id IS NOT NULL
            AND scheduler_turn_id IS NULL)
        OR (event_kind = 'user_stopped'
            AND statement IS NULL AND blocked_reason IS NULL AND need IS NULL
            AND guidance IS NULL AND report IS NULL AND user_command_id IS NOT NULL
            AND model_turn_id IS NULL AND model_tool_request_id IS NULL
            AND scheduler_turn_id IS NULL)
        OR (event_kind = 'superseded'
            AND statement IS NOT NULL AND blocked_reason IS NULL AND need IS NULL
            AND guidance IS NULL AND report IS NULL AND user_command_id IS NOT NULL
            AND model_turn_id IS NULL AND model_tool_request_id IS NULL
            AND scheduler_turn_id IS NULL)
    ),
    FOREIGN KEY (user_command_id, session_id)
        REFERENCES goal_command(command_id, session_id)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (model_tool_request_id, model_turn_id, session_id)
        REFERENCES tool_request(request_id, turn_id, session_id)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (scheduler_turn_id, session_id)
        REFERENCES turn_lifecycle(turn_id, session_id)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

-- Goal turns reuse the accepted-input turn engine without inventing user
-- commands. A null accepting command is admitted only when this migration's
-- deferred correlation proves an exact goal_turn source.
ALTER TABLE accepted_input
    ALTER COLUMN accepting_command_id DROP NOT NULL;

ALTER TABLE goal_event
    ADD CONSTRAINT goal_event_generation_correlation_key
        UNIQUE (session_id, event_ordinal, generation);

CREATE TABLE goal_turn (
    session_id uuid NOT NULL,
    goal_generation numeric(20, 0) NOT NULL CHECK (
        goal_generation BETWEEN 1 AND 18446744073709551615
    ),
    turn_id uuid NOT NULL,
    accepted_input_id uuid NOT NULL UNIQUE,
    source_event_ordinal numeric(20, 0) CHECK (
        source_event_ordinal IS NULL
        OR source_event_ordinal BETWEEN 1 AND 18446744073709551615
    ),
    predecessor_turn_id uuid,
    PRIMARY KEY (session_id, turn_id),
    UNIQUE (session_id, turn_id, goal_generation),
    UNIQUE (session_id, goal_generation, source_event_ordinal),
    UNIQUE (session_id, predecessor_turn_id),
    CONSTRAINT goal_turn_source_shape CHECK (
        (source_event_ordinal IS NOT NULL AND predecessor_turn_id IS NULL)
        OR (source_event_ordinal IS NULL AND predecessor_turn_id IS NOT NULL)
    ),
    CONSTRAINT goal_turn_event_fk
        FOREIGN KEY (session_id, source_event_ordinal)
        REFERENCES goal_event(session_id, event_ordinal)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT goal_turn_predecessor_fk
        FOREIGN KEY (session_id, predecessor_turn_id, goal_generation)
        REFERENCES goal_turn(session_id, turn_id, goal_generation)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT goal_turn_accepted_input_fk
        FOREIGN KEY (accepted_input_id, session_id, turn_id)
        REFERENCES accepted_input(accepted_input_id, session_id, origin_turn_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT goal_turn_lifecycle_fk
        FOREIGN KEY (turn_id, session_id, accepted_input_id)
        REFERENCES turn_lifecycle(turn_id, session_id, origin_accepted_input_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

ALTER TABLE goal_event
    ADD CONSTRAINT goal_event_model_goal_turn_fk
        FOREIGN KEY (session_id, model_turn_id, generation)
        REFERENCES goal_turn(session_id, turn_id, goal_generation)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    ADD CONSTRAINT goal_event_scheduler_goal_turn_fk
        FOREIGN KEY (session_id, scheduler_turn_id, generation)
        REFERENCES goal_turn(session_id, turn_id, goal_generation)

        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;

CREATE FUNCTION enforce_goal_model_declaration_request()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    stored_tool_name text;
    stored_arguments_kind text;
    stored_arguments jsonb;
    expected_arguments jsonb;
BEGIN
    IF NEW.model_tool_request_id IS NULL THEN
        RETURN NEW;
    END IF;

    SELECT
        request.tool_name,
        request.arguments_kind,
        CASE
            WHEN request.arguments_kind = 'json'
                THEN request.arguments_text::jsonb
        END
      INTO stored_tool_name, stored_arguments_kind, stored_arguments
      FROM tool_request AS request
     WHERE request.request_id = NEW.model_tool_request_id;

    expected_arguments := CASE NEW.event_kind
        WHEN 'achieved' THEN jsonb_build_object(
            'transition', 'achieved',
            'report', NEW.report
        )
        WHEN 'blocked' THEN jsonb_build_object(
            'transition', 'blocked',
            'reason', NEW.blocked_reason,
            'need', NEW.need
        )
    END;

    IF stored_tool_name IS DISTINCT FROM 'goal_declare'
        OR stored_arguments_kind IS DISTINCT FROM 'json'
        OR stored_arguments IS DISTINCT FROM expected_arguments
    THEN
        RAISE EXCEPTION 'goal model event lacks its exact declaration request'
            USING ERRCODE = '23514',
                CONSTRAINT = 'goal_event_model_declaration_request';
    END IF;
    RETURN NEW;
END;
$$;

CREATE CONSTRAINT TRIGGER goal_event_model_declaration_request
AFTER INSERT OR UPDATE ON goal_event
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION enforce_goal_model_declaration_request();

-- Queued goal turns remain immutable history after their generation ends, but
-- only the queued turn of the current pursuing generation participates in
-- runtime scheduling or queue predecessor selection.
CREATE FUNCTION goal_turn_is_runtime_relevant(
    checked_session uuid,
    checked_turn uuid
)
RETURNS boolean
LANGUAGE sql
STABLE
AS $$
    SELECT COALESCE((
        SELECT lifecycle.state_kind <> 'queued'
            OR goal.turn_id IS NULL
            OR (
                SELECT (
                    event.event_kind IN ('commissioned', 'resumed')
                    AND event.generation = goal.goal_generation
                ) OR (
                    event.event_kind = 'superseded'
                    AND event.generation < 18446744073709551615
                    AND event.generation + 1 = goal.goal_generation
                )
                  FROM goal_event AS event
                 WHERE event.session_id = checked_session
                 ORDER BY event.event_ordinal DESC
                 LIMIT 1
            )
          FROM turn_lifecycle AS lifecycle
          LEFT JOIN goal_turn AS goal
            ON goal.session_id = lifecycle.session_id
           AND goal.turn_id = lifecycle.turn_id
         WHERE lifecycle.session_id = checked_session
           AND lifecycle.turn_id = checked_turn
    ), true);
$$;

CREATE OR REPLACE FUNCTION accepted_input_turn_queue_predecessor(
    checked_session uuid,
    checked_turn uuid
)
RETURNS uuid
LANGUAGE sql
STABLE
AS $$
    WITH RECURSIVE derived_order (
        turn_id,
        root_position,
        interrupt_depth
    ) AS (
        SELECT
            lifecycle.turn_id,
            lifecycle.acceptance_position,
            0::bigint
          FROM turn_lifecycle AS lifecycle
          JOIN queued_input_origin AS origin
            ON origin.turn_id = lifecycle.turn_id
           AND origin.session_id = lifecycle.session_id
         WHERE lifecycle.session_id = checked_session
           AND origin.priority_kind = 'ordinary'
           AND goal_turn_is_runtime_relevant(
                lifecycle.session_id,
                lifecycle.turn_id
           )
        UNION ALL
        SELECT
            successor.turn_id,
            predecessor.root_position,
            predecessor.interrupt_depth + 1
          FROM derived_order AS predecessor
          JOIN queued_input_origin AS successor
            ON successor.session_id = checked_session
           AND successor.priority_kind = 'interrupt_immediately_after'
           AND successor.interrupt_predecessor_turn_id = predecessor.turn_id
          JOIN turn_lifecycle AS successor_lifecycle
            ON successor_lifecycle.turn_id = successor.turn_id
           AND successor_lifecycle.session_id = successor.session_id
         WHERE goal_turn_is_runtime_relevant(
            successor_lifecycle.session_id,
            successor_lifecycle.turn_id
         )
    ),
    ranked AS (
        SELECT
            turn_id,
            lag(turn_id) OVER (
                ORDER BY root_position, interrupt_depth
            ) AS predecessor_turn
          FROM derived_order
    )
    SELECT predecessor_turn
      FROM ranked
     WHERE turn_id = checked_turn;
$$;

CREATE OR REPLACE FUNCTION accepted_input_turn_is_first_nonterminal(
    checked_session uuid,
    checked_turn uuid
)
RETURNS boolean
LANGUAGE sql
STABLE
AS $$
    WITH RECURSIVE derived_order (
        turn_id,
        root_position,
        interrupt_depth
    ) AS (
        SELECT
            lifecycle.turn_id,
            lifecycle.acceptance_position,
            0::bigint
          FROM turn_lifecycle AS lifecycle
          JOIN queued_input_origin AS origin
            ON origin.turn_id = lifecycle.turn_id
           AND origin.session_id = lifecycle.session_id
         WHERE lifecycle.session_id = checked_session
           AND origin.priority_kind = 'ordinary'
           AND goal_turn_is_runtime_relevant(
                lifecycle.session_id,
                lifecycle.turn_id
           )
        UNION ALL
        SELECT
            successor.turn_id,
            predecessor.root_position,
            predecessor.interrupt_depth + 1
          FROM derived_order AS predecessor
          JOIN queued_input_origin AS successor
            ON successor.session_id = checked_session
           AND successor.priority_kind = 'interrupt_immediately_after'
           AND successor.interrupt_predecessor_turn_id = predecessor.turn_id
          JOIN turn_lifecycle AS successor_lifecycle
            ON successor_lifecycle.turn_id = successor.turn_id
           AND successor_lifecycle.session_id = successor.session_id
         WHERE goal_turn_is_runtime_relevant(
            successor_lifecycle.session_id,
            successor_lifecycle.turn_id
         )
    ),
    ranked AS (
        SELECT
            turn_id,
            row_number() OVER (
                ORDER BY root_position, interrupt_depth
            ) AS queue_rank
          FROM derived_order
    ),
    candidate AS (
        SELECT queue_rank
          FROM ranked
         WHERE turn_id = checked_turn
    )
    SELECT EXISTS (SELECT 1 FROM candidate)
       AND NOT EXISTS (
            SELECT 1
              FROM ranked AS earlier
              JOIN turn_lifecycle AS lifecycle
                ON lifecycle.turn_id = earlier.turn_id
               AND lifecycle.session_id = checked_session
              JOIN candidate
                ON earlier.queue_rank < candidate.queue_rank
             WHERE lifecycle.state_kind <> 'terminal'
       );
$$;

-- The lifecycle assertion predates goal generations and otherwise counts a
-- queued turn whose goal is no longer pursuing as earlier accepted work. Keep
-- that history immutable while excluding it from the runtime lineage proof.
DO $migration$
DECLARE
    lifecycle_definition text;
    updated_definition text;
    accepted_predecessor_selection CONSTANT text := $old$
           OR EXISTS (
            SELECT 1
              FROM turn_lifecycle AS earlier
             WHERE earlier.session_id = checked_session_id
               AND earlier.turn_id <> checked_turn_id
               AND earlier.acceptance_position < checked_position
        ) THEN
$old$;
    runtime_predecessor_selection CONSTANT text := $new$
           OR EXISTS (
            SELECT 1
              FROM turn_lifecycle AS earlier
             WHERE earlier.session_id = checked_session_id
               AND earlier.turn_id <> checked_turn_id
               AND earlier.acceptance_position < checked_position
               AND goal_turn_is_runtime_relevant(
                    earlier.session_id,
                    earlier.turn_id
               )
        ) THEN
$new$;
BEGIN
    SELECT pg_get_functiondef(
        'assert_turn_lifecycle_final_state_without_steering(uuid)'::regprocedure
    )
      INTO lifecycle_definition;
    updated_definition := replace(
        lifecycle_definition,
        accepted_predecessor_selection,
        runtime_predecessor_selection
    );
    IF updated_definition = lifecycle_definition THEN
        RAISE EXCEPTION
            'goal mode could not update lifecycle first-lineage assertion';
    END IF;
    EXECUTE updated_definition;
END;
$migration$;

CREATE FUNCTION require_goal_turn_shape()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    accepted accepted_input%ROWTYPE;
    queued queued_input_origin%ROWTYPE;
    defaults session_defaults_version%ROWTYPE;
    lifecycle turn_lifecycle%ROWTYPE;
    latest_event goal_event%ROWTYPE;
    source_event goal_event%ROWTYPE;
    predecessor turn_lifecycle%ROWTYPE;
    expected_content text;
BEGIN
    SELECT * INTO accepted FROM accepted_input
     WHERE accepted_input_id = NEW.accepted_input_id;
    SELECT * INTO queued FROM queued_input_origin
     WHERE turn_id = NEW.turn_id;
    SELECT * INTO defaults FROM session_defaults_version
     WHERE session_id = NEW.session_id
       AND version = queued.defaults_version;
    SELECT * INTO lifecycle FROM turn_lifecycle
     WHERE turn_id = NEW.turn_id;
    SELECT * INTO latest_event FROM goal_event
     WHERE session_id = NEW.session_id
     ORDER BY event_ordinal DESC LIMIT 1;

    IF accepted.accepted_input_id IS NULL
        OR accepted.accepting_command_id IS NOT NULL
        OR accepted.session_id <> NEW.session_id
        OR accepted.content_kind <> 'text'
        OR accepted.delivery_kind <> 'start_when_no_active_turn'
        OR accepted.expected_active_turn_id IS NOT NULL
        OR accepted.expected_defaults_version IS NULL
        OR accepted.model_override_kind <> 'use_session_default'
        OR accepted.replacement_model_kind IS NOT NULL
        OR accepted.replacement_direct_model_selection_id IS NOT NULL
        OR accepted.replacement_model_alias_id IS NOT NULL
        OR accepted.disposition_kind <> 'origin_of'
        OR accepted.origin_turn_id <> NEW.turn_id
        OR queued.turn_id IS NULL
        OR queued.accepted_input_id <> NEW.accepted_input_id
        OR queued.session_id <> NEW.session_id
        OR queued.acceptance_position <> accepted.acceptance_position
        OR queued.priority_kind <> 'ordinary'
        OR queued.interrupt_predecessor_turn_id IS NOT NULL
        OR queued.source_configuration_turn_id IS NOT NULL
        OR defaults.session_id IS NULL
        OR accepted.expected_defaults_version <> queued.defaults_version
        OR queued.requested_model_kind <> defaults.model_selection_kind
        OR queued.requested_direct_model_selection_id
            IS DISTINCT FROM defaults.direct_model_selection_id
        OR queued.requested_model_alias_id
            IS DISTINCT FROM defaults.model_alias_id
        OR NOT (
            (queued.requested_model_kind = 'direct'
                AND queued.frozen_model_kind = 'direct'
                AND queued.frozen_direct_model_selection_id
                    = queued.requested_direct_model_selection_id)
            OR (queued.requested_model_kind = 'alias'
                AND queued.frozen_model_kind = 'frozen_alias'
                AND queued.frozen_model_alias_id = queued.requested_model_alias_id)
        )
        OR queued.model_parameters <> 'provider_defaults'
        OR queued.known_provider_failure_retry <> 'disabled'
        OR queued.model_fallback <> 'disabled'
        OR queued.dangerous_tool_auto_approval
            <> defaults.dangerous_tool_auto_approval
        OR lifecycle.turn_id IS NULL
        OR lifecycle.session_id <> NEW.session_id
        OR lifecycle.origin_accepted_input_id <> NEW.accepted_input_id
        OR lifecycle.acceptance_position <> accepted.acceptance_position
        OR lifecycle.state_kind <> 'queued'
    THEN
        RAISE EXCEPTION 'goal turn lacks its exact queued accepted-input shape'
            USING ERRCODE = '23514', CONSTRAINT = 'goal_turn_runtime_shape';
    END IF;

    IF latest_event.event_ordinal IS NULL
        OR (
            latest_event.event_kind = 'superseded'
            AND latest_event.generation + 1 <> NEW.goal_generation
        )
        OR (
            latest_event.event_kind <> 'superseded'
            AND latest_event.generation <> NEW.goal_generation
        )
        OR latest_event.event_kind NOT IN ('commissioned', 'resumed', 'superseded')
    THEN
        RAISE EXCEPTION 'goal turn requires the current pursuing generation'
            USING ERRCODE = '23514', CONSTRAINT = 'goal_turn_current_pursuit';
    END IF;

    IF NEW.source_event_ordinal IS NOT NULL THEN
        SELECT * INTO source_event FROM goal_event
         WHERE session_id = NEW.session_id
           AND event_ordinal = NEW.source_event_ordinal;
        IF source_event.event_kind NOT IN ('commissioned', 'resumed', 'superseded') THEN
            RAISE EXCEPTION 'first goal turn requires a pursuing user event'
                USING ERRCODE = '23514', CONSTRAINT = 'goal_turn_source_event';
        END IF;
        IF (
            source_event.event_kind = 'superseded'
            AND source_event.generation + 1 <> NEW.goal_generation
        ) OR (
            source_event.event_kind <> 'superseded'
            AND source_event.generation <> NEW.goal_generation
        ) THEN
            RAISE EXCEPTION 'first goal turn generation disagrees with its user event'
                USING ERRCODE = '23514', CONSTRAINT = 'goal_turn_source_generation';
        END IF;
        IF source_event.event_kind = 'resumed' THEN
            IF source_event.guidance IS NOT NULL THEN
                expected_content := source_event.guidance;
            ELSE
                SELECT statement INTO expected_content FROM goal_event
                 WHERE session_id = NEW.session_id
                   AND event_ordinal <= NEW.source_event_ordinal
                   AND event_kind IN ('commissioned', 'superseded')
                 ORDER BY event_ordinal DESC LIMIT 1;
            END IF;
        ELSE
            expected_content := source_event.statement;
        END IF;
    ELSE
        SELECT * INTO predecessor FROM turn_lifecycle
         WHERE session_id = NEW.session_id
           AND turn_id = NEW.predecessor_turn_id;
        IF predecessor.state_kind <> 'terminal'
            OR predecessor.terminal_disposition_kind <> 'completed' THEN
            RAISE EXCEPTION 'goal continuation requires a successfully completed predecessor'
                USING ERRCODE = '23514', CONSTRAINT = 'goal_turn_completed_predecessor';
        END IF;
        SELECT statement INTO expected_content FROM goal_event
         WHERE session_id = NEW.session_id
           AND event_kind IN ('commissioned', 'superseded')
         ORDER BY event_ordinal DESC LIMIT 1;
    END IF;

    IF expected_content IS NULL OR accepted.content_text <> expected_content THEN
        RAISE EXCEPTION 'goal turn input does not match its immutable source'
            USING ERRCODE = '23514', CONSTRAINT = 'goal_turn_input_content';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER goal_turn_shape
    AFTER INSERT ON goal_turn
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION require_goal_turn_shape();

CREATE FUNCTION require_accepted_input_source()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE goal_sources bigint;
BEGIN
    SELECT count(*) INTO goal_sources FROM goal_turn
     WHERE accepted_input_id = NEW.accepted_input_id;
    IF (NEW.accepting_command_id IS NULL AND goal_sources <> 1)
        OR (NEW.accepting_command_id IS NOT NULL AND goal_sources <> 0) THEN
        RAISE EXCEPTION 'accepted input requires exactly one command or goal source'
            USING ERRCODE = '23514', CONSTRAINT = 'accepted_input_source_closed';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER accepted_input_source_closed
    AFTER INSERT OR UPDATE ON accepted_input
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION require_accepted_input_source();

ALTER TABLE goal_command
    ADD CONSTRAINT goal_command_applied_event_fk
    FOREIGN KEY (command_id, session_id, result_event_ordinal)
        REFERENCES goal_event(user_command_id, session_id, event_ordinal)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;

CREATE FUNCTION require_goal_command_applied_event_kind()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE applied_event goal_event%ROWTYPE;
BEGIN
    IF NEW.result_kind <> 'applied' THEN
        RETURN NULL;
    END IF;
    SELECT * INTO applied_event
      FROM goal_event
     WHERE session_id = NEW.session_id
       AND event_ordinal = NEW.result_event_ordinal;
    IF NOT FOUND THEN
        RETURN NULL;
    END IF;
    IF NOT (
        (NEW.operation_kind = 'attach'
            AND applied_event.event_kind = 'commissioned'
            AND applied_event.statement = NEW.statement)
        OR (NEW.operation_kind = 'resume'
            AND applied_event.event_kind = 'resumed'
            AND applied_event.guidance IS NOT DISTINCT FROM NEW.guidance)
        OR (NEW.operation_kind = 'stop'
            AND applied_event.event_kind = 'user_stopped')
        OR (NEW.operation_kind = 'supersede'
            AND applied_event.event_kind = 'superseded'
            AND applied_event.statement = NEW.statement)
    ) THEN
        RAISE EXCEPTION 'goal command operation disagrees with applied event kind'
            USING ERRCODE = '23514',
                CONSTRAINT = 'goal_command_applied_event_kind';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER goal_command_applied_event_kind
    AFTER INSERT ON goal_command
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION require_goal_command_applied_event_kind();

CREATE FUNCTION require_goal_event_user_command_receipt()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE receipt goal_command%ROWTYPE;
BEGIN
    IF NEW.user_command_id IS NULL THEN
        RETURN NULL;
    END IF;
    SELECT * INTO receipt FROM goal_command
     WHERE command_id = NEW.user_command_id
       AND session_id = NEW.session_id;
    IF receipt.command_id IS NULL
        OR receipt.result_kind <> 'applied'
        OR receipt.result_event_ordinal <> NEW.event_ordinal THEN
        RAISE EXCEPTION 'goal user event lacks its exact applied command receipt'
            USING ERRCODE = '23514',
                CONSTRAINT = 'goal_event_applied_command_receipt';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER goal_event_applied_command_receipt
    AFTER INSERT ON goal_event
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION require_goal_event_user_command_receipt();

CREATE FUNCTION require_pursuing_goal_event_turn()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE matching_turns bigint;
BEGIN
    IF NEW.event_kind NOT IN ('commissioned', 'resumed', 'superseded') THEN
        RETURN NULL;
    END IF;
    SELECT count(*) INTO matching_turns FROM goal_turn
     WHERE session_id = NEW.session_id
       AND source_event_ordinal = NEW.event_ordinal;
    IF matching_turns <> 1 THEN
        RAISE EXCEPTION 'pursuing goal event requires exactly one source turn'
            USING ERRCODE = '23514',
                CONSTRAINT = 'goal_event_pursuing_turn';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER goal_event_pursuing_turn
    AFTER INSERT ON goal_event
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION require_pursuing_goal_event_turn();

CREATE FUNCTION require_goal_event_continuity()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    prior_ordinal numeric(20, 0);
    prior_generation numeric(20, 0);
    prior_kind text;
    current_generation numeric(20, 0);
BEGIN
    PERFORM 1 FROM session WHERE session_id = NEW.session_id FOR UPDATE;
    SELECT event_ordinal, generation, event_kind
      INTO prior_ordinal, prior_generation, prior_kind
      FROM goal_event
     WHERE session_id = NEW.session_id
     ORDER BY event_ordinal DESC
     LIMIT 1;

    IF NOT FOUND THEN
        IF NEW.event_ordinal <> 1 OR NEW.generation <> 1
            OR NEW.event_kind <> 'commissioned' THEN
            RAISE EXCEPTION 'first goal event must commission generation one at ordinal one'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;

    IF NEW.event_ordinal <> prior_ordinal + 1 THEN
        RAISE EXCEPTION 'goal event ordinal must be contiguous'
            USING ERRCODE = '23514';
    END IF;
    current_generation := prior_generation
        + CASE
            WHEN prior_kind = 'superseded' THEN 1
            WHEN prior_kind IN ('achieved', 'user_stopped')
                AND NEW.event_kind = 'commissioned' THEN 1
            ELSE 0
          END;
    IF NEW.generation <> current_generation THEN
        RAISE EXCEPTION 'goal event generation does not name the current statement'
            USING ERRCODE = '23514';
    END IF;
    IF prior_kind IN ('achieved', 'user_stopped') AND NEW.event_kind <> 'commissioned' THEN
        RAISE EXCEPTION 'terminal goal generation admits only a later commission'
            USING ERRCODE = '23514';
    END IF;
    IF prior_kind = 'blocked' AND NEW.event_kind NOT IN ('resumed', 'user_stopped', 'superseded') THEN
        RAISE EXCEPTION 'blocked goal admits only resume, stop, or supersede'
            USING ERRCODE = '23514';
    END IF;
    IF prior_kind IN ('commissioned', 'resumed', 'superseded')
        AND NEW.event_kind NOT IN ('blocked', 'achieved', 'user_stopped', 'superseded') THEN
        RAISE EXCEPTION 'pursuing goal transition is invalid'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.event_kind = 'superseded'
        AND NEW.generation = 18446744073709551615 THEN
        RAISE EXCEPTION 'goal generation exhausted'
            USING ERRCODE = '22003';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER goal_event_continuity
    BEFORE INSERT ON goal_event
    FOR EACH ROW EXECUTE FUNCTION require_goal_event_continuity();

CREATE FUNCTION reject_goal_table_truncate()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'goal history and command receipts are append-only'
        USING ERRCODE = '23514';
END;
$$;

CREATE TRIGGER goal_command_is_append_only
    BEFORE UPDATE OR DELETE ON goal_command
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER goal_command_reject_truncate
    BEFORE TRUNCATE ON goal_command
    FOR EACH STATEMENT EXECUTE FUNCTION reject_goal_table_truncate();

CREATE TRIGGER goal_event_is_append_only
    BEFORE UPDATE OR DELETE ON goal_event
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER goal_event_reject_truncate
    BEFORE TRUNCATE ON goal_event
    FOR EACH STATEMENT EXECUTE FUNCTION reject_goal_table_truncate();

CREATE TRIGGER goal_turn_is_append_only
    BEFORE UPDATE OR DELETE ON goal_turn
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER goal_turn_reject_truncate
    BEFORE TRUNCATE ON goal_turn
    FOR EACH STATEMENT EXECUTE FUNCTION reject_goal_table_truncate();

CREATE OR REPLACE FUNCTION require_durable_command_typed_record()
RETURNS trigger LANGUAGE plpgsql AS $$
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
        WHEN 'review_workflow' THEN SELECT count(*) INTO matching_records FROM review_workflow_command WHERE command_id = NEW.command_id;
        WHEN 'review_orchestration' THEN
            SELECT
                (SELECT count(*) FROM review_orchestration_command WHERE command_id = NEW.command_id)
                + (SELECT count(*) FROM review_orchestration_command_intent WHERE command_id = NEW.command_id)
            INTO matching_records;
        WHEN 'compact_session' THEN SELECT count(*) INTO matching_records FROM compact_session_command WHERE command_id = NEW.command_id;
        WHEN 'goal' THEN SELECT count(*) INTO matching_records FROM goal_command WHERE command_id = NEW.command_id;
        ELSE RAISE EXCEPTION 'unsupported durable command kind %', NEW.command_kind USING ERRCODE = '23514';
    END CASE;
    IF matching_records <> 1 THEN
        RAISE EXCEPTION 'durable command % requires exactly one % typed record', NEW.command_id, NEW.command_kind USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;
