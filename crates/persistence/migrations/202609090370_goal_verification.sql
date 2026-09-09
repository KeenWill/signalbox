ALTER TABLE goal_event
    ADD COLUMN verified_turn_id uuid,
    ADD COLUMN verified_head_sha text,
    ADD COLUMN resolved_thread_ids text[],
    ADD CONSTRAINT goal_verified_turn FOREIGN KEY (session_id, verified_turn_id, generation)
        REFERENCES goal_turn(session_id, turn_id, goal_generation),
    ADD CONSTRAINT goal_verification_evidence CHECK (
        (verified_turn_id IS NULL AND verified_head_sha IS NULL AND resolved_thread_ids IS NULL)
        OR (event_kind = 'achieved' AND verified_turn_id IS NOT NULL
            AND verified_head_sha IS NOT NULL AND verified_head_sha ~ '^[0-9a-f]{40}$'
            AND resolved_thread_ids IS NOT NULL AND cardinality(resolved_thread_ids) > 0
            AND array_position(resolved_thread_ids, NULL) IS NULL
            AND array_position(resolved_thread_ids, '') IS NULL)
    );
ALTER TABLE goal_event DROP CONSTRAINT goal_event_shape;
ALTER TABLE goal_event
    ADD CONSTRAINT goal_event_shape CHECK (
        (((event_kind = 'commissioned'::text) AND (statement IS NOT NULL) AND (blocked_reason IS NULL) AND (need IS NULL) AND (guidance IS NULL) AND (report IS NULL) AND (user_command_id IS NOT NULL) AND (model_turn_id IS NULL) AND (model_tool_request_id IS NULL) AND (scheduler_turn_id IS NULL))
        OR ((event_kind = 'blocked'::text) AND (statement IS NULL) AND (blocked_reason IS NOT NULL) AND (need IS NOT NULL) AND (guidance IS NULL) AND (report IS NULL) AND (user_command_id IS NULL) AND (((blocked_reason = 'execution_failure'::text) AND (model_turn_id IS NULL) AND (model_tool_request_id IS NULL) AND (scheduler_turn_id IS NOT NULL)) OR ((blocked_reason <> 'execution_failure'::text) AND (model_turn_id IS NOT NULL) AND (model_tool_request_id IS NOT NULL) AND (scheduler_turn_id IS NULL))))
        OR ((event_kind = 'resumed'::text) AND (statement IS NULL) AND (blocked_reason IS NULL) AND (need IS NULL) AND (report IS NULL) AND (user_command_id IS NOT NULL) AND (model_turn_id IS NULL) AND (model_tool_request_id IS NULL) AND (scheduler_turn_id IS NULL))
        OR ((event_kind = 'achieved'::text) AND (statement IS NULL) AND (blocked_reason IS NULL) AND (need IS NULL) AND (guidance IS NULL) AND (report IS NOT NULL) AND (user_command_id IS NULL) AND (((model_turn_id IS NOT NULL) AND (model_tool_request_id IS NOT NULL) AND (verified_turn_id IS NULL)) OR ((model_turn_id IS NULL) AND (model_tool_request_id IS NULL) AND (verified_turn_id IS NOT NULL))) AND (scheduler_turn_id IS NULL))
        OR ((event_kind = 'user_stopped'::text) AND (statement IS NULL) AND (blocked_reason IS NULL) AND (need IS NULL) AND (guidance IS NULL) AND (report IS NULL) AND (user_command_id IS NOT NULL) AND (model_turn_id IS NULL) AND (model_tool_request_id IS NULL) AND (scheduler_turn_id IS NULL))
        OR ((event_kind = 'superseded'::text) AND (statement IS NOT NULL) AND (blocked_reason IS NULL) AND (need IS NULL) AND (guidance IS NULL) AND (report IS NULL) AND (user_command_id IS NOT NULL) AND (model_turn_id IS NULL) AND (model_tool_request_id IS NULL) AND (scheduler_turn_id IS NULL))
        OR ((event_kind = 'session_closed'::text) AND (statement IS NULL) AND (blocked_reason IS NULL) AND (need IS NULL) AND (guidance IS NULL) AND (report IS NULL) AND (user_command_id IS NULL) AND (scheduler_turn_id IS NULL) AND (model_turn_id IS NULL) AND (model_tool_request_id IS NULL)))
    );

CREATE FUNCTION enforce_goal_verified_turn() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.verified_turn_id IS NOT NULL AND NOT EXISTS (
        SELECT 1 FROM turn_lifecycle
        WHERE turn_id = NEW.verified_turn_id AND session_id = NEW.session_id
          AND state_kind = 'terminal' AND terminal_disposition_kind = 'completed'
          AND goal_event_names_current_goal_turn(NEW.session_id, NEW.generation, NEW.verified_turn_id)
    ) THEN
        RAISE EXCEPTION 'verified achievement requires the current completed goal turn'
            USING ERRCODE = '23514', CONSTRAINT = 'goal_verified_completed_turn';
    END IF;
    RETURN NEW;
END;
$$;
CREATE CONSTRAINT TRIGGER goal_verified_completed_turn
    AFTER INSERT ON goal_event DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION enforce_goal_verified_turn();

ALTER TABLE goal_turn ADD COLUMN verification_need text
    CHECK (verification_need IS NULL OR
        (predecessor_turn_id IS NOT NULL AND octet_length(verification_need) BETWEEN 1 AND 1048576));

CREATE OR REPLACE FUNCTION require_goal_turn_shape() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    accepted accepted_input%ROWTYPE;
    queued queued_input_origin%ROWTYPE;
    defaults session_defaults_version%ROWTYPE;
    lifecycle turn_lifecycle%ROWTYPE;
    latest_event goal_event%ROWTYPE;
    source_event goal_event%ROWTYPE;
    predecessor turn_lifecycle%ROWTYPE;
    expected_content text;
    compaction_continuation boolean := false;
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
        OR accepted.session_id <> NEW.session_id
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
                AND queued.frozen_direct_model_selection_id =
                    queued.requested_direct_model_selection_id)
            OR (queued.requested_model_kind = 'alias'
                AND queued.frozen_model_kind = 'frozen_alias'
                AND queued.frozen_model_alias_id = queued.requested_model_alias_id)
        )
        OR queued.model_parameters <> 'provider_defaults'
        OR queued.known_provider_failure_retry <> 'disabled'
        OR queued.model_fallback <> 'disabled'
        OR queued.dangerous_tool_auto_approval <>
            defaults.dangerous_tool_auto_approval
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
            RAISE EXCEPTION
                'first goal turn generation disagrees with its user event'
                USING ERRCODE = '23514',
                    CONSTRAINT = 'goal_turn_source_generation';
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
        SELECT EXISTS (
            SELECT 1 FROM session AS owner
            JOIN tool_continuation_context_headroom AS headroom
              ON headroom.session_id = owner.session_id
             AND headroom.turn_id = predecessor.turn_id
             AND headroom.terminal_attempt_id = predecessor.terminal_attempt_id
            WHERE owner.session_id = NEW.session_id
              AND owner.creation_cause = 'module_dispatched'
              AND owner.dispatching_module = 'repo_watch'
        ) INTO compaction_continuation;
        IF predecessor.state_kind <> 'terminal'
            OR (predecessor.terminal_disposition_kind <> 'completed'
                AND NOT compaction_continuation) THEN
            RAISE EXCEPTION
                'goal continuation requires completion or repository-watch context compaction'
                USING ERRCODE = '23514',
                    CONSTRAINT = 'goal_turn_completed_predecessor';
        END IF;
        IF EXISTS (
            SELECT 1
              FROM goal_turn AS later_goal
              JOIN turn_lifecycle AS later
                ON later.session_id = later_goal.session_id
               AND later.turn_id = later_goal.turn_id
             WHERE later_goal.session_id = NEW.session_id
               AND later_goal.goal_generation = NEW.goal_generation
               AND later_goal.turn_id <> NEW.turn_id
               AND later.acceptance_position > predecessor.acceptance_position
        ) THEN
            RAISE EXCEPTION
                'goal continuation requires the latest accepted goal turn'
                USING ERRCODE = '23514',
                    CONSTRAINT = 'goal_turn_latest_predecessor';
        END IF;
        IF compaction_continuation THEN
            -- Fixed continuation input in docs/spec/model-call-execution.md.
            expected_content := 'Continue the unfinished repository-watch task from the compacted context.';
        ELSIF NEW.verification_need IS NOT NULL THEN
            expected_content := NEW.verification_need;
        ELSE
        SELECT statement INTO expected_content FROM goal_event
         WHERE session_id = NEW.session_id
           AND event_kind IN ('commissioned', 'superseded')
         ORDER BY event_ordinal DESC LIMIT 1;
        END IF;
    END IF;

    IF expected_content IS NULL
        OR (
            accepted.accepting_command_id IS NULL
            AND NOT EXISTS (
                SELECT 1
                  FROM accepted_input_content_part AS part
                 WHERE part.accepted_input_id = accepted.accepted_input_id
                   AND part.position = 0
                   AND part.part_kind = 'text'
                   AND part.text_value = expected_content
                   AND NOT EXISTS (
                        SELECT 1
                          FROM accepted_input_content_part AS extra
                         WHERE extra.accepted_input_id = accepted.accepted_input_id
                           AND extra.position <> 0
                   )
            )
        )
    THEN
        RAISE EXCEPTION 'goal turn input does not match its immutable source'
            USING ERRCODE = '23514', CONSTRAINT = 'goal_turn_input_content';
    END IF;
    RETURN NULL;
END;
$$;
