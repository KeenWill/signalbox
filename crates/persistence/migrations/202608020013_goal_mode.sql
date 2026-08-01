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
            'requires_blocked', 'requires_pursuing_or_blocked',
            'generation_exhausted', 'event_ordinal_exhausted'
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

ALTER TABLE goal_command
    ADD CONSTRAINT goal_command_applied_event_fk
    FOREIGN KEY (session_id, result_event_ordinal)
        REFERENCES goal_event(session_id, event_ordinal)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;

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
