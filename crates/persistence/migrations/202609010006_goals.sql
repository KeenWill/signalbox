-- Goals and scheduling: the goal command and event logs, goal turns and their
-- generation work accounting, execution-failure recovery, the session
-- scheduler, and the live queued-turn projection kept fresh from turn and
-- goal activity.

-- Function bodies may read tables a later section or file creates;
-- 202609010000_core.sql explains why validation is deferred.
SET check_function_bodies = false;

--
-- Functions.
--

--
-- Name: apply_goal_generation_queued_delta(uuid, numeric, integer); Type: FUNCTION; Schema: public
--

CREATE FUNCTION apply_goal_generation_queued_delta(changed_session uuid, changed_generation numeric, queued_delta integer) RETURNS void
    LANGUAGE plpgsql
    AS $$
BEGIN
    UPDATE session_goal_generation_work_fact
       SET queued_turn_count = queued_turn_count + queued_delta
     WHERE session_id = changed_session
       AND goal_generation = changed_generation;
    IF NOT FOUND THEN
        INSERT INTO session_goal_generation_work_fact (
            session_id, goal_generation, queued_turn_count
        ) VALUES (changed_session, changed_generation, queued_delta);
    END IF;
END
$$;


--
-- Name: credit_goal_turn_generation_work_fact(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION credit_goal_turn_generation_work_fact() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM 1 FROM outbox_sequence_state WHERE singleton FOR UPDATE;
    IF EXISTS (
        SELECT 1
          FROM turn_lifecycle AS turn
         WHERE turn.session_id = NEW.session_id
           AND turn.turn_id = NEW.turn_id
           AND turn.state_kind = 'queued'
           AND NOT turn.delegation_runtime_terminal
    ) THEN
        PERFORM apply_goal_generation_queued_delta(
            NEW.session_id, NEW.goal_generation, 1
        );
    END IF;
    RETURN NULL;
END
$$;


--
-- Name: enforce_goal_model_declaration_request(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION enforce_goal_model_declaration_request() RETURNS trigger
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

    expected_arguments := CASE NEW.event_kind
        WHEN 'achieved' THEN jsonb_build_object(
            'transition', 'achieved'
        )
        WHEN 'blocked' THEN jsonb_build_object(
            'transition', 'blocked',
            'reason', NEW.blocked_reason
        )
    END;

    IF stored_tool_name IS DISTINCT FROM 'goal_declare'
        OR stored_arguments_kind IS DISTINCT FROM 'json'
        OR stored_arguments IS DISTINCT FROM expected_arguments
        OR declared_text IS DISTINCT FROM COALESCE(NEW.report, NEW.need)
    THEN
        RAISE EXCEPTION 'goal model event lacks its exact declaration request'
            USING ERRCODE = '23514',
                CONSTRAINT = 'goal_event_model_declaration_request';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: enforce_goal_scheduler_failure_turn(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION enforce_goal_scheduler_failure_turn() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    failure_is_valid boolean;
BEGIN
    IF NEW.scheduler_turn_id IS NULL THEN
        RETURN NEW;
    END IF;

    SELECT goal_event_names_current_goal_turn(
               NEW.session_id, NEW.generation, NEW.scheduler_turn_id
           )
           AND lifecycle.state_kind = 'terminal'
           AND lifecycle.terminal_disposition_kind IN (
               'refused', 'failed', 'cancelled', 'reconciliation_required'
           )
      INTO failure_is_valid
      FROM turn_lifecycle AS lifecycle
     WHERE lifecycle.session_id = NEW.session_id
       AND lifecycle.turn_id = NEW.scheduler_turn_id
     FOR SHARE;

    IF failure_is_valid IS DISTINCT FROM true THEN
        RAISE EXCEPTION 'goal scheduler event requires the current unsuccessful goal turn'
            USING ERRCODE = '23514',
                CONSTRAINT = 'goal_event_scheduler_failure_turn';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: goal_event_names_current_goal_turn(uuid, numeric, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION goal_event_names_current_goal_turn(checked_session uuid, checked_generation numeric, checked_turn uuid) RETURNS boolean
    LANGUAGE plpgsql
    AS $$
DECLARE
    current_turn uuid;
BEGIN
    PERFORM 1 FROM session
     WHERE session_id = checked_session
     FOR NO KEY UPDATE;
    SELECT goal.turn_id
      INTO current_turn
      FROM goal_turn AS goal
      JOIN accepted_input AS accepted
        ON accepted.accepted_input_id = goal.accepted_input_id
       AND accepted.session_id = goal.session_id
       AND accepted.origin_turn_id = goal.turn_id
     WHERE goal.session_id = checked_session
       AND goal.goal_generation = checked_generation
     ORDER BY accepted.acceptance_position DESC
     LIMIT 1;
    RETURN current_turn IS NOT NULL AND checked_turn = current_turn;
END;
$$;


--
-- Name: goal_event_pursued_generation(text, numeric); Type: FUNCTION; Schema: public
--

CREATE FUNCTION goal_event_pursued_generation(checked_kind text, checked_generation numeric) RETURNS numeric
    LANGUAGE sql IMMUTABLE
    AS $$
    SELECT CASE
             WHEN checked_kind IN ('commissioned', 'resumed')
                 THEN checked_generation
             WHEN checked_kind = 'superseded'
                  AND checked_generation < 18446744073709551615
                 THEN checked_generation + 1
           END;
$$;


--
-- Name: goal_turn_generation_is_pursued(uuid, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION goal_turn_generation_is_pursued(checked_session uuid, checked_turn uuid) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    SELECT coalesce((
        SELECT (
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
          FROM goal_turn AS goal
         WHERE goal.session_id = checked_session
           AND goal.turn_id = checked_turn
    ), true);
$$;


--
-- Name: goal_turn_is_queue_order_relevant(uuid, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION goal_turn_is_queue_order_relevant(checked_session uuid, checked_turn uuid) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    SELECT COALESCE((
        SELECT (
            lifecycle.state_kind <> 'queued'
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
        )
          FROM turn_lifecycle AS lifecycle
          LEFT JOIN goal_turn AS goal
            ON goal.session_id = lifecycle.session_id
           AND goal.turn_id = lifecycle.turn_id
         WHERE lifecycle.session_id = checked_session
           AND lifecycle.turn_id = checked_turn
    ), true);
$$;


--
-- Name: goal_turn_is_runtime_relevant(uuid, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION goal_turn_is_runtime_relevant(checked_session uuid, checked_turn uuid) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    SELECT COALESCE((
        SELECT NOT lifecycle.delegation_runtime_terminal
               AND goal_turn_is_queue_order_relevant(
                    lifecycle.session_id, lifecycle.turn_id
               )
          FROM turn_lifecycle AS lifecycle
         WHERE lifecycle.session_id = checked_session
           AND lifecycle.turn_id = checked_turn
    ), true);
$$;


--
-- Name: record_operator_attention_goal_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION record_operator_attention_goal_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    INSERT INTO operator_attention_change (session_id, fact_kind)
    VALUES (NEW.session_id, 'goal');
    RETURN NULL;
END;
$$;


--
-- Name: record_operator_attention_rejected_goal_command_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION record_operator_attention_rejected_goal_command_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM session WHERE session_id = NEW.session_id) THEN
        INSERT INTO operator_attention_change (session_id, fact_kind)
        VALUES (NEW.session_id, 'goal');
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: refresh_session_live_goal_queue(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION refresh_session_live_goal_queue() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    prior_kind text;
    prior_generation numeric(20, 0);
    retired numeric(20, 0);
    pursued numeric(20, 0);
BEGIN
    IF NEW.event_ordinal = 1 THEN
        -- The session's first goal event commissions its own turn, whose
        -- lifecycle insert trigger adds the row; nothing changes hands.
        RETURN NULL;
    END IF;
    SELECT event_kind, generation INTO prior_kind, prior_generation
      FROM goal_event
     WHERE session_id = NEW.session_id
       AND event_ordinal = NEW.event_ordinal - 1;
    retired := goal_event_pursued_generation(prior_kind, prior_generation);
    pursued := goal_event_pursued_generation(NEW.event_kind, NEW.generation);
    IF retired IS NOT DISTINCT FROM pursued THEN
        RETURN NULL;
    END IF;
    -- Same allocator-then-rows lock order as refresh_session_live_queued_turn;
    -- the goal-event fact trigger that runs before this one already holds it.
    PERFORM 1 FROM outbox_sequence_state WHERE singleton FOR UPDATE;
    -- A NULL retired or pursued generation names no generation and moves no
    -- row, which is how the retiring kinds delete without inserting.
    DELETE FROM session_live_queued_turn AS queued
     USING goal_turn AS goal
     WHERE queued.session_id = NEW.session_id
       AND goal.session_id = queued.session_id
       AND goal.turn_id = queued.turn_id
       AND goal.goal_generation = retired;
    INSERT INTO session_live_queued_turn (
        session_id, turn_id, acceptance_position
    )
    SELECT lifecycle.session_id, lifecycle.turn_id,
           lifecycle.acceptance_position
      FROM goal_turn AS goal
      JOIN turn_lifecycle AS lifecycle
        ON lifecycle.session_id = goal.session_id
       AND lifecycle.turn_id = goal.turn_id
     WHERE goal.session_id = NEW.session_id
       AND goal.goal_generation = pursued
       AND lifecycle.state_kind = 'queued'
       AND NOT lifecycle.delegation_runtime_terminal
    ON CONFLICT (turn_id) DO NOTHING;
    RETURN NULL;
END
$$;


--
-- Name: refresh_session_live_queued_turn(uuid, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION refresh_session_live_queued_turn(checked_session uuid, checked_turn uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    lifecycle turn_lifecycle%ROWTYPE;
BEGIN
    -- Every timeline-fact trigger takes the outbox allocator lock before its
    -- fact row, and trigger-name ordering runs this function's lifecycle
    -- trigger before the fact trigger while the goal-event fact trigger runs
    -- before the goal-event queue trigger. Taking the allocator first here
    -- keeps one global allocator-then-rows order, so a queued-turn transition
    -- concurrent with a goal event cannot deadlock.
    PERFORM 1 FROM outbox_sequence_state WHERE singleton FOR UPDATE;
    DELETE FROM session_live_queued_turn
     WHERE session_id = checked_session AND turn_id = checked_turn;

    SELECT * INTO lifecycle
      FROM turn_lifecycle
     WHERE session_id = checked_session AND turn_id = checked_turn;

    IF lifecycle.turn_id IS NOT NULL
       AND lifecycle.state_kind = 'queued'
       AND NOT lifecycle.delegation_runtime_terminal
       AND goal_turn_is_runtime_relevant(checked_session, checked_turn) THEN
        INSERT INTO session_live_queued_turn (
            session_id, turn_id, acceptance_position
        ) VALUES (
            lifecycle.session_id, lifecycle.turn_id, lifecycle.acceptance_position
        );
    END IF;
END
$$;


--
-- Name: refresh_session_live_queued_turn_from_goal(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION refresh_session_live_queued_turn_from_goal() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM refresh_session_live_queued_turn(NEW.session_id, NEW.turn_id);
    RETURN NULL;
END
$$;


--
-- Name: refresh_session_live_queued_turn_from_lifecycle(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION refresh_session_live_queued_turn_from_lifecycle() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM refresh_session_live_queued_turn(NEW.session_id, NEW.turn_id);
    RETURN NULL;
END
$$;


--
-- Name: reject_goal_execution_failure_recovery_truncate(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_goal_execution_failure_recovery_truncate() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RAISE EXCEPTION '% cannot be truncated', TG_TABLE_NAME
        USING ERRCODE = '23514';
END;
$$;


--
-- Name: reject_goal_table_truncate(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_goal_table_truncate() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RAISE EXCEPTION 'goal history and command receipts are append-only'
        USING ERRCODE = '23514';
END;
$$;


--
-- Name: require_goal_command_applied_event_kind(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_goal_command_applied_event_kind() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_goal_event_continuity(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_goal_event_continuity() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    prior_ordinal numeric(20, 0);
    prior_generation numeric(20, 0);
    prior_kind text;
    current_generation numeric(20, 0);
BEGIN
    PERFORM 1 FROM session WHERE session_id = NEW.session_id FOR NO KEY UPDATE;
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


--
-- Name: require_goal_event_user_command_receipt(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_goal_event_user_command_receipt() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_goal_execution_failure_recovery_terminal(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_goal_execution_failure_recovery_terminal() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM turn_lifecycle AS lifecycle
         WHERE lifecycle.turn_id = NEW.turn_id
           AND lifecycle.session_id = NEW.session_id
           AND lifecycle.state_kind = 'terminal'
           AND lifecycle.terminal_disposition_kind = 'failed'
           AND lifecycle.terminal_model_call_id IS NULL
    ) THEN
        RAISE EXCEPTION 'goal execution-failure recovery requires its exact call-free failed turn'
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'goal_execution_failure_recovery_exact_terminal';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_goal_turn_retired_outbox_state(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_goal_turn_retired_outbox_state() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM goal_turn AS goal
          JOIN turn_lifecycle AS lifecycle
            ON lifecycle.session_id = goal.session_id
           AND lifecycle.turn_id = goal.turn_id
         WHERE goal.session_id = NEW.session_id
           AND goal.turn_id = NEW.turn_id
           AND lifecycle.state_kind = 'queued'
           AND NOT goal_turn_is_runtime_relevant(
               goal.session_id,
               goal.turn_id
           )
    ) THEN
        RAISE EXCEPTION 'goal turn retirement must name queued ineligible work'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'goal_turn_retired_outbox_state';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_goal_turn_shape(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_goal_turn_shape() RETURNS trigger
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
        IF predecessor.state_kind <> 'terminal'
            OR predecessor.terminal_disposition_kind <> 'completed' THEN
            RAISE EXCEPTION
                'goal continuation requires a successfully completed predecessor'
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
        SELECT statement INTO expected_content FROM goal_event
         WHERE session_id = NEW.session_id
           AND event_kind IN ('commissioned', 'superseded')
         ORDER BY event_ordinal DESC LIMIT 1;
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


--
-- Name: require_pursuing_goal_event_turn(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_pursuing_goal_event_turn() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Tables.
--

--
-- Name: goal_command; Type: TABLE; Schema: public
--

CREATE TABLE goal_command (
    command_id uuid NOT NULL,
    command_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    operation_kind text NOT NULL,
    statement text,
    guidance text,
    result_kind text NOT NULL,
    rejection_kind text,
    result_event_ordinal numeric(20,0),
    descendant_scope text,
    CONSTRAINT goal_command_command_kind_check CHECK ((command_kind = 'goal'::text)),
    CONSTRAINT goal_command_descendant_scope_shape CHECK ((((operation_kind = 'stop'::text) AND (descendant_scope IS NOT NULL) AND (descendant_scope = ANY (ARRAY['parent_alone'::text, 'parent_and_descendants'::text]))) OR ((operation_kind <> 'stop'::text) AND (descendant_scope IS NULL)))),
    CONSTRAINT goal_command_guidance_check CHECK (((guidance IS NULL) OR ((octet_length(guidance) >= 1) AND (octet_length(guidance) <= 1048576)))),
    CONSTRAINT goal_command_operation_kind_check CHECK ((operation_kind = ANY (ARRAY['attach'::text, 'resume'::text, 'stop'::text, 'supersede'::text]))),
    CONSTRAINT goal_command_operation_shape CHECK ((((operation_kind = ANY (ARRAY['attach'::text, 'supersede'::text])) AND (statement IS NOT NULL) AND (guidance IS NULL)) OR ((operation_kind = 'resume'::text) AND (statement IS NULL)) OR ((operation_kind = 'stop'::text) AND (statement IS NULL) AND (guidance IS NULL)))),
    CONSTRAINT goal_command_rejection_kind_check CHECK (((rejection_kind IS NULL) OR (rejection_kind = ANY (ARRAY['session_not_found'::text, 'goal_already_attached'::text, 'goal_not_attached'::text, 'unknown_model_alias'::text, 'requires_blocked'::text, 'requires_pursuing_or_blocked'::text, 'generation_exhausted'::text, 'event_ordinal_exhausted'::text, 'acceptance_position_exhausted'::text])))),
    CONSTRAINT goal_command_rejection_operation CHECK (((result_kind = 'applied'::text) OR (rejection_kind = 'session_not_found'::text) OR ((operation_kind = 'attach'::text) AND (rejection_kind = ANY (ARRAY['goal_already_attached'::text, 'unknown_model_alias'::text, 'generation_exhausted'::text, 'event_ordinal_exhausted'::text, 'acceptance_position_exhausted'::text]))) OR ((operation_kind = 'resume'::text) AND (rejection_kind = ANY (ARRAY['goal_not_attached'::text, 'unknown_model_alias'::text, 'requires_blocked'::text, 'event_ordinal_exhausted'::text, 'acceptance_position_exhausted'::text]))) OR ((operation_kind = 'stop'::text) AND (rejection_kind = ANY (ARRAY['goal_not_attached'::text, 'requires_pursuing_or_blocked'::text, 'event_ordinal_exhausted'::text]))) OR ((operation_kind = 'supersede'::text) AND (rejection_kind = ANY (ARRAY['goal_not_attached'::text, 'unknown_model_alias'::text, 'requires_pursuing_or_blocked'::text, 'generation_exhausted'::text, 'event_ordinal_exhausted'::text, 'acceptance_position_exhausted'::text]))))),
    CONSTRAINT goal_command_result_event_ordinal_check CHECK (((result_event_ordinal IS NULL) OR ((result_event_ordinal >= (1)::numeric) AND (result_event_ordinal <= '18446744073709551615'::numeric)))),
    CONSTRAINT goal_command_result_kind_check CHECK ((result_kind = ANY (ARRAY['applied'::text, 'rejected'::text]))),
    CONSTRAINT goal_command_result_shape CHECK ((((result_kind = 'applied'::text) AND (rejection_kind IS NULL) AND (result_event_ordinal IS NOT NULL)) OR ((result_kind = 'rejected'::text) AND (rejection_kind IS NOT NULL) AND (result_event_ordinal IS NULL)))),
    CONSTRAINT goal_command_statement_check CHECK (((statement IS NULL) OR ((octet_length(statement) >= 1) AND (octet_length(statement) <= 1048576)))),
    CONSTRAINT goal_command_storage_version_check CHECK ((storage_version = 1))
);


--
-- Name: goal_event; Type: TABLE; Schema: public
--

CREATE TABLE goal_event (
    session_id uuid NOT NULL,
    event_ordinal numeric(20,0) NOT NULL,
    generation numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    statement text,
    blocked_reason text,
    need text,
    guidance text,
    report text,
    user_command_id uuid,
    model_turn_id uuid,
    model_tool_request_id uuid,
    scheduler_turn_id uuid,
    CONSTRAINT goal_event_blocked_reason_check CHECK (((blocked_reason IS NULL) OR (blocked_reason = ANY (ARRAY['user_input_required'::text, 'external_change_required'::text, 'authorization_required'::text, 'execution_failure'::text])))),
    CONSTRAINT goal_event_event_kind_check CHECK ((event_kind = ANY (ARRAY['commissioned'::text, 'blocked'::text, 'resumed'::text, 'achieved'::text, 'user_stopped'::text, 'superseded'::text]))),
    CONSTRAINT goal_event_event_ordinal_check CHECK (((event_ordinal >= (1)::numeric) AND (event_ordinal <= '18446744073709551615'::numeric))),
    CONSTRAINT goal_event_generation_check CHECK (((generation >= (1)::numeric) AND (generation <= '18446744073709551615'::numeric))),
    CONSTRAINT goal_event_guidance_check CHECK (((guidance IS NULL) OR ((octet_length(guidance) >= 1) AND (octet_length(guidance) <= 1048576)))),
    CONSTRAINT goal_event_need_check CHECK (((need IS NULL) OR ((octet_length(need) >= 1) AND (octet_length(need) <= 1048576)))),
    CONSTRAINT goal_event_report_check CHECK (((report IS NULL) OR ((octet_length(report) >= 1) AND (octet_length(report) <= 1048576)))),
    CONSTRAINT goal_event_shape CHECK ((((event_kind = 'commissioned'::text) AND (statement IS NOT NULL) AND (blocked_reason IS NULL) AND (need IS NULL) AND (guidance IS NULL) AND (report IS NULL) AND (user_command_id IS NOT NULL) AND (model_turn_id IS NULL) AND (model_tool_request_id IS NULL) AND (scheduler_turn_id IS NULL)) OR ((event_kind = 'blocked'::text) AND (statement IS NULL) AND (blocked_reason IS NOT NULL) AND (need IS NOT NULL) AND (guidance IS NULL) AND (report IS NULL) AND (user_command_id IS NULL) AND (((blocked_reason = 'execution_failure'::text) AND (model_turn_id IS NULL) AND (model_tool_request_id IS NULL) AND (scheduler_turn_id IS NOT NULL)) OR ((blocked_reason <> 'execution_failure'::text) AND (model_turn_id IS NOT NULL) AND (model_tool_request_id IS NOT NULL) AND (scheduler_turn_id IS NULL)))) OR ((event_kind = 'resumed'::text) AND (statement IS NULL) AND (blocked_reason IS NULL) AND (need IS NULL) AND (report IS NULL) AND (user_command_id IS NOT NULL) AND (model_turn_id IS NULL) AND (model_tool_request_id IS NULL) AND (scheduler_turn_id IS NULL)) OR ((event_kind = 'achieved'::text) AND (statement IS NULL) AND (blocked_reason IS NULL) AND (need IS NULL) AND (guidance IS NULL) AND (report IS NOT NULL) AND (user_command_id IS NULL) AND (model_turn_id IS NOT NULL) AND (model_tool_request_id IS NOT NULL) AND (scheduler_turn_id IS NULL)) OR ((event_kind = 'user_stopped'::text) AND (statement IS NULL) AND (blocked_reason IS NULL) AND (need IS NULL) AND (guidance IS NULL) AND (report IS NULL) AND (user_command_id IS NOT NULL) AND (model_turn_id IS NULL) AND (model_tool_request_id IS NULL) AND (scheduler_turn_id IS NULL)) OR ((event_kind = 'superseded'::text) AND (statement IS NOT NULL) AND (blocked_reason IS NULL) AND (need IS NULL) AND (guidance IS NULL) AND (report IS NULL) AND (user_command_id IS NOT NULL) AND (model_turn_id IS NULL) AND (model_tool_request_id IS NULL) AND (scheduler_turn_id IS NULL)))),
    CONSTRAINT goal_event_statement_check CHECK (((statement IS NULL) OR ((octet_length(statement) >= 1) AND (octet_length(statement) <= 1048576))))
);


--
-- Name: goal_execution_failure_recovery; Type: TABLE; Schema: public
--

CREATE TABLE goal_execution_failure_recovery (
    turn_id uuid NOT NULL,
    session_id uuid NOT NULL,
    cause_kind text NOT NULL,
    recorded_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    CONSTRAINT goal_execution_failure_recovery_cause_kind_closed CHECK ((cause_kind = 'context_compaction_input_does_not_fit'::text))
);


--
-- Name: goal_turn; Type: TABLE; Schema: public
--

CREATE TABLE goal_turn (
    session_id uuid NOT NULL,
    goal_generation numeric(20,0) NOT NULL,
    turn_id uuid NOT NULL,
    accepted_input_id uuid NOT NULL,
    source_event_ordinal numeric(20,0),
    predecessor_turn_id uuid,
    CONSTRAINT goal_turn_goal_generation_check CHECK (((goal_generation >= (1)::numeric) AND (goal_generation <= '18446744073709551615'::numeric))),
    CONSTRAINT goal_turn_source_event_ordinal_check CHECK (((source_event_ordinal IS NULL) OR ((source_event_ordinal >= (1)::numeric) AND (source_event_ordinal <= '18446744073709551615'::numeric)))),
    CONSTRAINT goal_turn_source_shape CHECK ((((source_event_ordinal IS NOT NULL) AND (predecessor_turn_id IS NULL)) OR ((source_event_ordinal IS NULL) AND (predecessor_turn_id IS NOT NULL))))
);


--
-- Name: goal_turn_retired_outbox_event; Type: TABLE; Schema: public
--

CREATE TABLE goal_turn_retired_outbox_event (
    event_sequence numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    CONSTRAINT goal_turn_retired_outbox_kind_closed CHECK ((event_kind = 'goal_turn_retired'::text)),
    CONSTRAINT goal_turn_retired_outbox_version_supported CHECK ((storage_version = 1))
);


--
-- Name: session_goal_generation_work_fact; Type: TABLE; Schema: public
--

CREATE TABLE session_goal_generation_work_fact (
    session_id uuid NOT NULL,
    goal_generation numeric(20,0) NOT NULL,
    queued_turn_count numeric(20,0) NOT NULL,
    CONSTRAINT session_goal_generation_work_fact_goal_generation_check CHECK (((goal_generation >= (1)::numeric) AND (goal_generation <= '18446744073709551615'::numeric))),
    CONSTRAINT session_goal_generation_work_fact_queued_turn_count_check CHECK (((queued_turn_count >= (0)::numeric) AND (queued_turn_count <= '18446744073709551615'::numeric)))
);


--
-- Name: session_live_queued_turn; Type: TABLE; Schema: public
--

CREATE TABLE session_live_queued_turn (
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    acceptance_position numeric(20,0) NOT NULL
);


--
-- Name: session_scheduler; Type: TABLE; Schema: public
--

CREATE TABLE session_scheduler (
    session_id uuid NOT NULL
);


--
-- Constraints.
--

--
-- Name: goal_command goal_command_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_command
    ADD CONSTRAINT goal_command_pkey PRIMARY KEY (command_id);


--
-- Name: goal_command goal_command_session_correlation_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_command
    ADD CONSTRAINT goal_command_session_correlation_key UNIQUE (command_id, session_id);


--
-- Name: goal_event goal_event_generation_correlation_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_event
    ADD CONSTRAINT goal_event_generation_correlation_key UNIQUE (session_id, event_ordinal, generation);


--
-- Name: goal_event goal_event_model_tool_request_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_event
    ADD CONSTRAINT goal_event_model_tool_request_id_key UNIQUE (model_tool_request_id);


--
-- Name: goal_event goal_event_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_event
    ADD CONSTRAINT goal_event_pkey PRIMARY KEY (session_id, event_ordinal);


--
-- Name: goal_event goal_event_scheduler_turn_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_event
    ADD CONSTRAINT goal_event_scheduler_turn_id_key UNIQUE (scheduler_turn_id);


--
-- Name: goal_event goal_event_user_command_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_event
    ADD CONSTRAINT goal_event_user_command_id_key UNIQUE (user_command_id);


--
-- Name: goal_event goal_event_user_command_result_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_event
    ADD CONSTRAINT goal_event_user_command_result_key UNIQUE (user_command_id, session_id, event_ordinal);


--
-- Name: goal_execution_failure_recovery goal_execution_failure_recovery_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_execution_failure_recovery
    ADD CONSTRAINT goal_execution_failure_recovery_pkey PRIMARY KEY (turn_id);


--
-- Name: goal_execution_failure_recovery goal_execution_failure_recovery_turn_id_session_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_execution_failure_recovery
    ADD CONSTRAINT goal_execution_failure_recovery_turn_id_session_id_key UNIQUE (turn_id, session_id);


--
-- Name: goal_turn goal_turn_accepted_input_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_turn
    ADD CONSTRAINT goal_turn_accepted_input_id_key UNIQUE (accepted_input_id);


--
-- Name: goal_turn goal_turn_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_turn
    ADD CONSTRAINT goal_turn_pkey PRIMARY KEY (session_id, turn_id);


--
-- Name: goal_turn_retired_outbox_event goal_turn_retired_outbox_event_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_turn_retired_outbox_event
    ADD CONSTRAINT goal_turn_retired_outbox_event_pkey PRIMARY KEY (event_sequence);


--
-- Name: goal_turn_retired_outbox_event goal_turn_retired_outbox_event_turn_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_turn_retired_outbox_event
    ADD CONSTRAINT goal_turn_retired_outbox_event_turn_id_key UNIQUE (turn_id);


--
-- Name: goal_turn goal_turn_session_id_goal_generation_source_event_ordinal_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_turn
    ADD CONSTRAINT goal_turn_session_id_goal_generation_source_event_ordinal_key UNIQUE (session_id, goal_generation, source_event_ordinal);


--
-- Name: goal_turn goal_turn_session_id_predecessor_turn_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_turn
    ADD CONSTRAINT goal_turn_session_id_predecessor_turn_id_key UNIQUE (session_id, predecessor_turn_id);


--
-- Name: goal_turn goal_turn_session_id_turn_id_goal_generation_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_turn
    ADD CONSTRAINT goal_turn_session_id_turn_id_goal_generation_key UNIQUE (session_id, turn_id, goal_generation);


--
-- Name: session_goal_generation_work_fact session_goal_generation_work_fact_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_goal_generation_work_fact
    ADD CONSTRAINT session_goal_generation_work_fact_pkey PRIMARY KEY (session_id, goal_generation);


--
-- Name: session_live_queued_turn session_live_queued_turn_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_live_queued_turn
    ADD CONSTRAINT session_live_queued_turn_pkey PRIMARY KEY (session_id, acceptance_position);


--
-- Name: session_live_queued_turn session_live_queued_turn_turn_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_live_queued_turn
    ADD CONSTRAINT session_live_queued_turn_turn_id_key UNIQUE (turn_id);


--
-- Name: session_scheduler session_scheduler_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_scheduler
    ADD CONSTRAINT session_scheduler_pkey PRIMARY KEY (session_id);


--
-- Triggers.
--

--
-- Name: goal_command applied_goal_command_requires_delegation_cascade; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER applied_goal_command_requires_delegation_cascade AFTER INSERT OR UPDATE ON goal_command DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_applied_goal_command_delegation_cascade();


--
-- Name: goal_command goal_command_applied_event_kind; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER goal_command_applied_event_kind AFTER INSERT ON goal_command DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_goal_command_applied_event_kind();


--
-- Name: goal_command goal_command_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER goal_command_is_append_only BEFORE DELETE OR UPDATE ON goal_command FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: goal_command goal_command_locks_delegation_frontier; Type: TRIGGER; Schema: public
--

CREATE TRIGGER goal_command_locks_delegation_frontier BEFORE INSERT ON goal_command FOR EACH ROW EXECUTE FUNCTION lock_delegation_frontier_before_goal_stop();


--
-- Name: goal_command goal_command_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER goal_command_reject_truncate BEFORE TRUNCATE ON goal_command FOR EACH STATEMENT EXECUTE FUNCTION reject_goal_table_truncate();


--
-- Name: goal_event goal_event_applied_command_receipt; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER goal_event_applied_command_receipt AFTER INSERT ON goal_event DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_goal_event_user_command_receipt();


--
-- Name: goal_event goal_event_continuity; Type: TRIGGER; Schema: public
--

CREATE TRIGGER goal_event_continuity BEFORE INSERT ON goal_event FOR EACH ROW EXECUTE FUNCTION require_goal_event_continuity();


--
-- Name: goal_event goal_event_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER goal_event_is_append_only BEFORE DELETE OR UPDATE ON goal_event FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: goal_event goal_event_model_declaration_request; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER goal_event_model_declaration_request AFTER INSERT OR UPDATE ON goal_event DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION enforce_goal_model_declaration_request();


--
-- Name: goal_event goal_event_pursuing_turn; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER goal_event_pursuing_turn AFTER INSERT ON goal_event DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_pursuing_goal_event_turn();


--
-- Name: goal_event goal_event_reconciles_timeline_work_fact; Type: TRIGGER; Schema: public
--

CREATE TRIGGER goal_event_reconciles_timeline_work_fact AFTER INSERT ON goal_event FOR EACH ROW EXECUTE FUNCTION reconcile_session_timeline_goal_work_fact();


--
-- Name: goal_event goal_event_records_operator_attention_change; Type: TRIGGER; Schema: public
--

CREATE TRIGGER goal_event_records_operator_attention_change AFTER INSERT ON goal_event FOR EACH ROW EXECUTE FUNCTION record_operator_attention_goal_change();


--
-- Name: goal_event goal_event_refreshes_session_live_queue; Type: TRIGGER; Schema: public
--

CREATE TRIGGER goal_event_refreshes_session_live_queue AFTER INSERT ON goal_event FOR EACH ROW EXECUTE FUNCTION refresh_session_live_goal_queue();


--
-- Name: goal_event goal_event_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER goal_event_reject_truncate BEFORE TRUNCATE ON goal_event FOR EACH STATEMENT EXECUTE FUNCTION reject_goal_table_truncate();


--
-- Name: goal_event goal_event_scheduler_failure_turn; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER goal_event_scheduler_failure_turn AFTER INSERT OR UPDATE ON goal_event DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION enforce_goal_scheduler_failure_turn();


--
-- Name: goal_execution_failure_recovery goal_execution_failure_recovery_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER goal_execution_failure_recovery_is_append_only BEFORE DELETE OR UPDATE ON goal_execution_failure_recovery FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: goal_execution_failure_recovery goal_execution_failure_recovery_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER goal_execution_failure_recovery_reject_truncate BEFORE TRUNCATE ON goal_execution_failure_recovery FOR EACH STATEMENT EXECUTE FUNCTION reject_goal_execution_failure_recovery_truncate();


--
-- Name: goal_execution_failure_recovery goal_execution_failure_recovery_requires_terminal; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER goal_execution_failure_recovery_requires_terminal AFTER INSERT ON goal_execution_failure_recovery DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_goal_execution_failure_recovery_terminal();


--
-- Name: goal_turn goal_turn_credits_generation_work_fact; Type: TRIGGER; Schema: public
--

CREATE TRIGGER goal_turn_credits_generation_work_fact AFTER INSERT ON goal_turn FOR EACH ROW EXECUTE FUNCTION credit_goal_turn_generation_work_fact();


--
-- Name: goal_turn goal_turn_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER goal_turn_is_append_only BEFORE DELETE OR UPDATE ON goal_turn FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: goal_turn goal_turn_refreshes_session_live_queue; Type: TRIGGER; Schema: public
--

CREATE TRIGGER goal_turn_refreshes_session_live_queue AFTER INSERT ON goal_turn FOR EACH ROW EXECUTE FUNCTION refresh_session_live_queued_turn_from_goal();


--
-- Name: goal_turn goal_turn_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER goal_turn_reject_truncate BEFORE TRUNCATE ON goal_turn FOR EACH STATEMENT EXECUTE FUNCTION reject_goal_table_truncate();


--
-- Name: goal_turn_retired_outbox_event goal_turn_retired_outbox_event_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER goal_turn_retired_outbox_event_cannot_be_truncated BEFORE TRUNCATE ON goal_turn_retired_outbox_event FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();


--
-- Name: goal_turn_retired_outbox_event goal_turn_retired_outbox_event_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER goal_turn_retired_outbox_event_is_append_only BEFORE DELETE OR UPDATE ON goal_turn_retired_outbox_event FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: goal_turn_retired_outbox_event goal_turn_retired_outbox_state; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER goal_turn_retired_outbox_state AFTER INSERT OR UPDATE ON goal_turn_retired_outbox_event DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_goal_turn_retired_outbox_state();


--
-- Name: goal_turn goal_turn_shape; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER goal_turn_shape AFTER INSERT ON goal_turn DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_goal_turn_shape();


--
-- Name: goal_command rejected_goal_command_records_operator_attention_change; Type: TRIGGER; Schema: public
--

CREATE TRIGGER rejected_goal_command_records_operator_attention_change AFTER INSERT ON goal_command FOR EACH ROW WHEN ((new.result_kind = 'rejected'::text)) EXECUTE FUNCTION record_operator_attention_rejected_goal_command_change();


--
-- Name: session_scheduler session_scheduler_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_scheduler_is_append_only BEFORE DELETE OR UPDATE ON session_scheduler FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: turn_lifecycle turn_lifecycle_refreshes_session_live_queue; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_lifecycle_refreshes_session_live_queue AFTER INSERT OR UPDATE OF state_kind, delegation_runtime_terminal ON turn_lifecycle FOR EACH ROW EXECUTE FUNCTION refresh_session_live_queued_turn_from_lifecycle();


--
-- Foreign keys.
--

--
-- Name: goal_command goal_command_applied_event_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_command
    ADD CONSTRAINT goal_command_applied_event_fk FOREIGN KEY (command_id, session_id, result_event_ordinal) REFERENCES goal_event(user_command_id, session_id, event_ordinal) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: goal_command goal_command_command_id_command_kind_storage_version_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_command
    ADD CONSTRAINT goal_command_command_id_command_kind_storage_version_fkey FOREIGN KEY (command_id, command_kind, storage_version) REFERENCES durable_command(command_id, command_kind, storage_version) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: goal_event goal_event_model_goal_turn_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_event
    ADD CONSTRAINT goal_event_model_goal_turn_fk FOREIGN KEY (session_id, model_turn_id, generation) REFERENCES goal_turn(session_id, turn_id, goal_generation) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: goal_event goal_event_model_tool_request_id_model_turn_id_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_event
    ADD CONSTRAINT goal_event_model_tool_request_id_model_turn_id_session_id_fkey FOREIGN KEY (model_tool_request_id, model_turn_id, session_id) REFERENCES tool_request(request_id, turn_id, session_id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: goal_event goal_event_scheduler_goal_turn_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_event
    ADD CONSTRAINT goal_event_scheduler_goal_turn_fk FOREIGN KEY (session_id, scheduler_turn_id, generation) REFERENCES goal_turn(session_id, turn_id, goal_generation) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: goal_event goal_event_scheduler_turn_id_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_event
    ADD CONSTRAINT goal_event_scheduler_turn_id_session_id_fkey FOREIGN KEY (scheduler_turn_id, session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: goal_event goal_event_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_event
    ADD CONSTRAINT goal_event_session_id_fkey FOREIGN KEY (session_id) REFERENCES session(session_id) ON DELETE RESTRICT;


--
-- Name: goal_event goal_event_user_command_id_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_event
    ADD CONSTRAINT goal_event_user_command_id_session_id_fkey FOREIGN KEY (user_command_id, session_id) REFERENCES goal_command(command_id, session_id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: goal_execution_failure_recovery goal_execution_failure_recovery_turn_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_execution_failure_recovery
    ADD CONSTRAINT goal_execution_failure_recovery_turn_fk FOREIGN KEY (turn_id, session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: goal_turn goal_turn_accepted_input_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_turn
    ADD CONSTRAINT goal_turn_accepted_input_fk FOREIGN KEY (accepted_input_id, session_id, turn_id) REFERENCES accepted_input(accepted_input_id, session_id, origin_turn_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: goal_turn goal_turn_event_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_turn
    ADD CONSTRAINT goal_turn_event_fk FOREIGN KEY (session_id, source_event_ordinal) REFERENCES goal_event(session_id, event_ordinal) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: goal_turn goal_turn_lifecycle_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_turn
    ADD CONSTRAINT goal_turn_lifecycle_fk FOREIGN KEY (turn_id, session_id, accepted_input_id) REFERENCES turn_lifecycle(turn_id, session_id, origin_accepted_input_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: goal_turn goal_turn_predecessor_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_turn
    ADD CONSTRAINT goal_turn_predecessor_fk FOREIGN KEY (session_id, predecessor_turn_id, goal_generation) REFERENCES goal_turn(session_id, turn_id, goal_generation) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: goal_turn_retired_outbox_event goal_turn_retired_outbox_goal_turn_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_turn_retired_outbox_event
    ADD CONSTRAINT goal_turn_retired_outbox_goal_turn_fk FOREIGN KEY (session_id, turn_id) REFERENCES goal_turn(session_id, turn_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: goal_turn_retired_outbox_event goal_turn_retired_outbox_header_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY goal_turn_retired_outbox_event
    ADD CONSTRAINT goal_turn_retired_outbox_header_fk FOREIGN KEY (event_sequence, event_kind, storage_version, session_id) REFERENCES outbox_event(event_sequence, event_kind, storage_version, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_goal_generation_work_fact session_goal_generation_work_fact_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_goal_generation_work_fact
    ADD CONSTRAINT session_goal_generation_work_fact_session_id_fkey FOREIGN KEY (session_id) REFERENCES session(session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: session_live_queued_turn session_live_queued_turn_turn_id_session_id_acceptance_pos_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_live_queued_turn
    ADD CONSTRAINT session_live_queued_turn_turn_id_session_id_acceptance_pos_fkey FOREIGN KEY (turn_id, session_id, acceptance_position) REFERENCES turn_lifecycle(turn_id, session_id, acceptance_position) ON UPDATE CASCADE ON DELETE CASCADE;


--
-- Name: session_scheduler session_scheduler_session_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_scheduler
    ADD CONSTRAINT session_scheduler_session_fk FOREIGN KEY (session_id) REFERENCES session(session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Search-path pins for this file's constraint-reachable functions.
--
-- The pin has to name the schema the migration selected rather than a
-- literal, so it is applied here through current_schema instead of inline
-- in each CREATE FUNCTION (the full rationale is in 202609010000_core.sql;
-- crates/persistence/tests/search_path_postgres.rs is the guard).
--

DO $search_path_pins$
DECLARE
    signature text;
BEGIN
    -- the server default captured at creation time by SET search_path FROM CURRENT
    FOREACH signature IN ARRAY ARRAY[
        'apply_goal_generation_queued_delta(uuid, numeric, integer)',
        'credit_goal_turn_generation_work_fact()',
        'goal_event_pursued_generation(text, numeric)',
        'goal_turn_generation_is_pursued(uuid, uuid)',
        'refresh_session_live_goal_queue()',
        'refresh_session_live_queued_turn(uuid, uuid)',
        'refresh_session_live_queued_turn_from_goal()',
        'refresh_session_live_queued_turn_from_lifecycle()'
    ] LOOP
        EXECUTE format('ALTER FUNCTION %s SET search_path TO "$user", %I',
                   signature, current_schema);
    END LOOP;
END
$search_path_pins$;


--
-- Restore body validation for anything applied after this file.
--

RESET check_function_bodies;
