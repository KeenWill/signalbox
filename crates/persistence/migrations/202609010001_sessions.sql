-- Sessions: the session record itself, its metadata and tags with their
-- installation receipts, current defaults and default versions, per-session
-- model credentials and settings, the session-scoped commands that create and
-- reshape sessions, the session timeline fact, and the session plan event log
-- with its projected dependency graph.

-- Function bodies may read tables a later section or file creates;
-- 202609010000_core.sql explains why validation is deferred.
SET check_function_bodies = false;

--
-- Functions.
--

--
-- Name: advance_session_plan_projection(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION advance_session_plan_projection() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    prior_dependency_event_ordinal numeric(20, 0);
    dependency_count bigint;
    target_kind text;
    target_shape_valid boolean;
    target_authorized boolean;
    closes_cycle boolean;
    graph_cyclic boolean;
    graph_shape_valid boolean;
    graph_authorized boolean;
    graph_within_limit boolean;
    projected boolean := FALSE;
BEGIN
    SELECT dependency_event_ordinal
      INTO prior_dependency_event_ordinal
      FROM session_plan_head
     WHERE session_id = NEW.session_id;

    IF NEW.event_kind = 'depends_on' THEN
        IF NOT session_plan_event_has_valid_shape(NEW)
            OR NEW.entry_ordinal >= NEW.event_ordinal
            OR NEW.dependency_ordinal IS NULL
            OR NEW.dependency_ordinal >= NEW.event_ordinal
            OR NEW.dependency_ordinal = NEW.entry_ordinal
            OR NEW.entry_text IS NOT NULL
            OR NEW.entry_status IS NOT NULL
            OR NOT session_plan_event_has_authority(NEW)
        THEN
            RAISE EXCEPTION 'session plan dependency has invalid certified shape'
                USING ERRCODE = '23514',
                    CONSTRAINT = 'session_plan_dependency_shape';
        END IF;

        SELECT target.event_kind,
               session_plan_event_has_valid_shape(target)
               AND session_plan_creation_has_valid_shape(target),
               session_plan_event_has_authority(target)
          INTO target_kind, target_shape_valid, target_authorized
          FROM session_plan_event AS target
         WHERE target.session_id = NEW.session_id
           AND target.event_ordinal = NEW.entry_ordinal;
        IF target_kind IS DISTINCT FROM 'created'
            OR target_shape_valid IS DISTINCT FROM TRUE
            OR target_authorized IS DISTINCT FROM TRUE
        THEN
            RAISE EXCEPTION 'session plan dependency entry must be a creation'
                USING ERRCODE = '23514',
                    CONSTRAINT = 'session_plan_dependency_entry';
        END IF;
        SELECT target.event_kind,
               session_plan_event_has_valid_shape(target)
               AND session_plan_creation_has_valid_shape(target),
               session_plan_event_has_authority(target)
          INTO target_kind, target_shape_valid, target_authorized
          FROM session_plan_event AS target
         WHERE target.session_id = NEW.session_id
           AND target.event_ordinal = NEW.dependency_ordinal;
        IF target_kind IS DISTINCT FROM 'created'
            OR target_shape_valid IS DISTINCT FROM TRUE
            OR target_authorized IS DISTINCT FROM TRUE
        THEN
            RAISE EXCEPTION 'session plan dependency target must be a creation'
                USING ERRCODE = '23514',
                    CONSTRAINT = 'session_plan_dependency_target';
        END IF;

        SELECT inspection.graph_cyclic,
               inspection.closes_cycle,
               inspection.graph_shape_valid,
               inspection.graph_authorized,
               inspection.graph_within_limit
          INTO graph_cyclic,
               closes_cycle,
               graph_shape_valid,
               graph_authorized,
               graph_within_limit
          FROM inspect_session_plan_dependency_graph(
              NEW.session_id,
              NEW.entry_ordinal,
              NEW.dependency_ordinal
          ) AS inspection;
        IF NOT graph_shape_valid THEN
            RAISE EXCEPTION
                'session plan dependency graph has invalid event shape'
                USING ERRCODE = '23514',
                    CONSTRAINT = 'session_plan_dependency_graph_shape';
        END IF;
        IF NOT graph_authorized THEN
            RAISE EXCEPTION
                'session plan dependency graph lacks certified authority'
                USING ERRCODE = '23514',
                    CONSTRAINT = 'session_plan_dependency_graph_authority';
        END IF;
        IF NOT graph_within_limit THEN
            RAISE EXCEPTION 'session plan dependency graph exceeds its limit'
                USING ERRCODE = '23514',
                    CONSTRAINT = 'session_plan_dependency_limit';
        END IF;
        IF graph_cyclic THEN
            RAISE EXCEPTION 'session plan dependency graph is already cyclic'
                USING ERRCODE = '23514',
                    CONSTRAINT = 'session_plan_dependency_graph_cycle';
        END IF;
        IF closes_cycle THEN
            RAISE EXCEPTION 'session plan dependency would create a cycle'
                USING ERRCODE = '23514',
                    CONSTRAINT = 'session_plan_dependency_cycle';
        END IF;
        IF NOT EXISTS (
            SELECT 1
              FROM session_plan_current_dependency AS edge
             WHERE edge.session_id = NEW.session_id
               AND edge.entry_ordinal = NEW.entry_ordinal
               AND edge.dependency_ordinal = NEW.dependency_ordinal
        ) THEN
            SELECT count(*)
              INTO dependency_count
              FROM session_plan_current_dependency AS edge
             WHERE edge.session_id = NEW.session_id
               AND edge.entry_ordinal = NEW.entry_ordinal;
            -- Checked mechanically against MAX_PLAN_DEPENDENCIES_PER_ENTRY.
            IF dependency_count >= 32 THEN
                RAISE EXCEPTION 'session plan entry dependency limit reached'
                    USING ERRCODE = '23514',
                        CONSTRAINT = 'session_plan_dependency_limit';
            END IF;

            INSERT INTO session_plan_current_dependency (
                session_id,
                entry_ordinal,
                dependency_ordinal,
                first_event_ordinal,
                prior_first_event_ordinal
            )
            VALUES (
                NEW.session_id,
                NEW.entry_ordinal,
                NEW.dependency_ordinal,
                NEW.event_ordinal,
                prior_dependency_event_ordinal
            );
            projected := TRUE;
        END IF;
    END IF;

    IF NEW.event_ordinal = 1 THEN
        INSERT INTO session_plan_head (
            session_id, event_ordinal, dependency_event_ordinal
        )
        VALUES (
            NEW.session_id,
            NEW.event_ordinal,
            CASE WHEN projected THEN NEW.event_ordinal ELSE NULL END
        );
    ELSE
        UPDATE session_plan_head
           SET event_ordinal = NEW.event_ordinal,
               dependency_event_ordinal = CASE
                   WHEN projected THEN NEW.event_ordinal
                   ELSE prior_dependency_event_ordinal
               END
         WHERE session_id = NEW.session_id
           AND event_ordinal = NEW.prior_event_ordinal
           AND dependency_event_ordinal IS NOT DISTINCT FROM
               prior_dependency_event_ordinal;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'session plan head must advance by one ordinal';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: append_session_timeline_event_fact(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION append_session_timeline_event_fact() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    UPDATE session_timeline_fact
       SET item_count = item_count + 1,
           first_sequence = coalesce(first_sequence, NEW.event_sequence),
           latest_sequence = NEW.event_sequence,
           event_kind_bytes = event_kind_bytes + octet_length(NEW.event_kind)
     WHERE session_id = NEW.session_id;
    RETURN NULL;
END
$$;


--
-- Name: append_session_timeline_input_bytes(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION append_session_timeline_input_bytes() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    accepted_text_bytes bigint;
BEGIN
    SELECT COALESCE(sum(octet_length(convert_to(text_value, 'UTF8'))), 0)
      INTO accepted_text_bytes
      FROM accepted_input_content_part
     WHERE accepted_input_id = NEW.accepted_input_id
       AND part_kind = 'text';
    -- Submission later acquires the allocator through lifecycle/outbox work.
    -- Preserve the global allocator-then-session-fact lock order here too.
    PERFORM 1 FROM outbox_sequence_state WHERE singleton FOR UPDATE;
    UPDATE session_timeline_fact
       SET projected_text_bytes = projected_text_bytes + accepted_text_bytes
     WHERE session_id = NEW.session_id;
    RETURN NULL;
END
$$;


--
-- Name: append_session_timeline_transcript_bytes(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION append_session_timeline_transcript_bytes() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.assistant_text_value IS NULL AND NEW.context_summary_value IS NULL THEN
        RETURN NULL;
    END IF;
    -- Transcript persistence can share a transaction with later outbox work.
    -- Preserve the global allocator-then-session-fact lock order here too.
    PERFORM 1 FROM outbox_sequence_state WHERE singleton FOR UPDATE;
    UPDATE session_timeline_fact
       SET projected_text_bytes = projected_text_bytes
           + coalesce(octet_length(convert_to(NEW.assistant_text_value, 'UTF8')), 0)
           + coalesce(octet_length(convert_to(NEW.context_summary_value, 'UTF8')), 0)
     WHERE session_id = NEW.source_session_id;
    RETURN NULL;
END
$$;


--
-- Name: guard_session_model_credential_head(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_session_model_credential_head() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    entry_count bigint;
    latest_ordinal numeric(20, 0);
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'session model credential head is not deletable';
    END IF;
    PERFORM 1
      FROM session
     WHERE session_id = NEW.session_id
       FOR UPDATE;
    IF TG_OP = 'INSERT' AND NEW.current_event_ordinal <> 1 THEN
        RAISE EXCEPTION 'first session model credential head must name event 1';
    END IF;
    IF TG_OP = 'UPDATE'
        AND (NEW.session_id <> OLD.session_id
            OR NEW.current_event_ordinal <> OLD.current_event_ordinal + 1) THEN
        RAISE EXCEPTION 'session model credential head must advance by one ordinal';
    END IF;
    SELECT max(event_ordinal)
      INTO latest_ordinal
      FROM session_model_credential_record
     WHERE session_id = NEW.session_id;
    IF NEW.current_event_ordinal IS DISTINCT FROM latest_ordinal THEN
        RAISE EXCEPTION 'session model credential head must name the latest event';
    END IF;
    SELECT count(*)
      INTO entry_count
      FROM session_model_credential_entry
     WHERE session_id = NEW.session_id
       AND event_ordinal = NEW.current_event_ordinal;
    IF entry_count = 0 THEN
        RAISE EXCEPTION 'session model credential snapshot must be nonempty';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: guard_session_model_credential_record_append(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_session_model_credential_record_append() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    latest numeric(20, 0);
BEGIN
    SELECT max(event_ordinal)
      INTO latest
      FROM session_model_credential_record
     WHERE session_id = NEW.session_id;
    IF (latest IS NULL AND NEW.event_ordinal <> 1)
        OR (latest IS NOT NULL AND NEW.event_ordinal <> latest + 1) THEN
        RAISE EXCEPTION 'session model credential events must append by one ordinal';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: guard_session_plan_dependency_predecessor(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_session_plan_dependency_predecessor() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    expected_predecessor numeric(20, 0);
    successor_first_event_ordinal numeric(20, 0);
    successor_predecessor numeric(20, 0);
BEGIN
    IF TG_OP = 'UPDATE' THEN
        SELECT max(edge.first_event_ordinal)
          INTO expected_predecessor
          FROM session_plan_current_dependency AS edge
         WHERE edge.session_id = NEW.session_id
           AND edge.first_event_ordinal < NEW.first_event_ordinal
           AND NOT (
               edge.session_id = OLD.session_id
               AND edge.entry_ordinal = OLD.entry_ordinal
               AND edge.dependency_ordinal = OLD.dependency_ordinal
           );
        SELECT edge.first_event_ordinal, edge.prior_first_event_ordinal
          INTO successor_first_event_ordinal, successor_predecessor
          FROM session_plan_current_dependency AS edge
         WHERE edge.session_id = NEW.session_id
           AND edge.first_event_ordinal > NEW.first_event_ordinal
           AND NOT (
               edge.session_id = OLD.session_id
               AND edge.entry_ordinal = OLD.entry_ordinal
               AND edge.dependency_ordinal = OLD.dependency_ordinal
           )
         ORDER BY edge.first_event_ordinal
         LIMIT 1;
    ELSE
        SELECT max(edge.first_event_ordinal)
          INTO expected_predecessor
          FROM session_plan_current_dependency AS edge
         WHERE edge.session_id = NEW.session_id
           AND edge.first_event_ordinal < NEW.first_event_ordinal;
        SELECT edge.first_event_ordinal, edge.prior_first_event_ordinal
          INTO successor_first_event_ordinal, successor_predecessor
          FROM session_plan_current_dependency AS edge
         WHERE edge.session_id = NEW.session_id
           AND edge.first_event_ordinal > NEW.first_event_ordinal
         ORDER BY edge.first_event_ordinal
         LIMIT 1;
    END IF;
    IF NEW.prior_first_event_ordinal IS DISTINCT FROM expected_predecessor THEN
        RAISE EXCEPTION
            'session plan dependency predecessor must be immediate'
            USING ERRCODE = '23514',
                CONSTRAINT = 'session_plan_dependency_predecessor';
    END IF;
    IF successor_first_event_ordinal IS NOT NULL
       AND successor_predecessor IS DISTINCT FROM NEW.first_event_ordinal THEN
        RAISE EXCEPTION
            'session plan dependency successor must be immediate'
            USING ERRCODE = '23514',
                CONSTRAINT = 'session_plan_dependency_successor';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: guard_session_plan_event_append(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_session_plan_event_append() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    latest_ordinal numeric(20, 0);
    target_kind text;
    target_shape_valid boolean;
    target_authorized boolean;
    closes_cycle boolean;
    graph_cyclic boolean;
    graph_shape_valid boolean;
    graph_authorized boolean;
    graph_within_limit boolean;
    dependency_count bigint;
BEGIN
    PERFORM 1
      FROM session
     WHERE session_id = NEW.session_id
       FOR NO KEY UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'session plan event requires its owning session';
    END IF;

    PERFORM 1
      FROM tool_attempt AS attempt
      JOIN tool_request AS request
        ON request.request_id = attempt.request_id
     WHERE attempt.attempt_id = NEW.provenance_attempt_id
       AND attempt.request_id = NEW.provenance_request_id
       AND attempt.issuing_turn_attempt_id =
            NEW.provenance_issuing_turn_attempt_id
       AND attempt.dispatch_generation =
            NEW.provenance_dispatch_generation
       AND attempt.turn_id = NEW.provenance_turn_id
       AND attempt.session_id = NEW.session_id
       AND request.tool_name = 'plan_write'
       AND request.arguments_kind = 'json'
       AND session_plan_request_arguments_json(
               request.arguments_kind, request.arguments_text
           ) =
            CASE NEW.event_kind
                WHEN 'created' THEN jsonb_build_object(
                    'kind', 'create',
                    'text', NEW.entry_text
                )
                WHEN 'text_revised' THEN jsonb_build_object(
                    'kind', 'revise',
                    'entry_id', NEW.entry_ordinal,
                    'text', NEW.entry_text
                )
                WHEN 'status_changed' THEN jsonb_build_object(
                    'kind', 'set_status',
                    'entry_id', NEW.entry_ordinal,
                    'status', NEW.entry_status
                )
                WHEN 'depends_on' THEN jsonb_build_object(
                    'kind', 'depends_on',
                    'entry_id', NEW.entry_ordinal,
                    'dependency_id', NEW.dependency_ordinal
                )
            END
       AND attempt.effect_class = 'external_effect'
       AND attempt.state_kind = 'in_flight'
       FOR SHARE OF attempt;
    IF NOT FOUND THEN
        RAISE EXCEPTION
            'session plan event requires an active plan_write attempt'
            USING
                ERRCODE = '23514',
                CONSTRAINT =
                    'session_plan_event_requires_active_plan_write_attempt';
    END IF;

    SELECT event_ordinal
      INTO latest_ordinal
      FROM session_plan_head
     WHERE session_id = NEW.session_id;
    IF (latest_ordinal IS NULL AND NEW.event_ordinal <> 1)
        OR (
            latest_ordinal IS NOT NULL
            AND NEW.event_ordinal <> latest_ordinal + 1
        )
    THEN
        RAISE EXCEPTION 'session plan events must append by one ordinal';
    END IF;

    IF (NEW.event_ordinal = 1 AND NEW.prior_event_ordinal IS NOT NULL)
        OR (
            NEW.event_ordinal > 1
            AND NEW.prior_event_ordinal IS DISTINCT FROM NEW.event_ordinal - 1
        )
        OR (
            NEW.event_kind = 'created'
            AND (
                NEW.entry_ordinal IS DISTINCT FROM NEW.event_ordinal
                OR NEW.dependency_ordinal IS NOT NULL
                OR NEW.entry_text IS NULL
                OR char_length(NEW.entry_text) NOT BETWEEN 1 AND 4096
                OR NEW.entry_status IS NOT NULL
            )
        )
        OR (
            NEW.event_kind = 'text_revised'
            AND (
                NEW.entry_ordinal >= NEW.event_ordinal
                OR NEW.dependency_ordinal IS NOT NULL
                OR NEW.entry_text IS NULL
                OR char_length(NEW.entry_text) NOT BETWEEN 1 AND 4096
                OR NEW.entry_status IS NOT NULL
            )
        )
        OR (
            NEW.event_kind = 'status_changed'
            AND (
                NEW.entry_ordinal >= NEW.event_ordinal
                OR NEW.dependency_ordinal IS NOT NULL
                OR NEW.entry_text IS NOT NULL
                OR NEW.entry_status IS NULL
                OR NEW.entry_status NOT IN (
                    'pending', 'in_progress', 'completed', 'abandoned'
                )
            )
        )
        OR (
            NEW.event_kind = 'depends_on'
            AND (
                NEW.entry_ordinal >= NEW.event_ordinal
                OR NEW.dependency_ordinal IS NULL
                OR NEW.dependency_ordinal >= NEW.event_ordinal
                OR NEW.dependency_ordinal = NEW.entry_ordinal
                OR NEW.entry_text IS NOT NULL
                OR NEW.entry_status IS NOT NULL
            )
        )
        OR NEW.event_kind NOT IN ('created', 'text_revised', 'status_changed', 'depends_on')
    THEN
        RAISE EXCEPTION 'session plan event has invalid certified shape';
    END IF;

    IF NEW.event_kind <> 'created' THEN
        SELECT target.event_kind,
               session_plan_event_has_valid_shape(target)
               AND session_plan_creation_has_valid_shape(target),
               session_plan_event_has_authority(target)
          INTO target_kind, target_shape_valid, target_authorized
          FROM session_plan_event AS target
         WHERE target.session_id = NEW.session_id
           AND target.event_ordinal = NEW.entry_ordinal;
        IF target_kind IS DISTINCT FROM 'created' THEN
            RAISE EXCEPTION 'session plan mutation must name a creation event';
        END IF;
        IF target_shape_valid IS DISTINCT FROM TRUE
            OR target_authorized IS DISTINCT FROM TRUE
        THEN
            RAISE EXCEPTION 'session plan mutation entry lacks certified authority'
                USING ERRCODE = '23514',
                    CONSTRAINT = 'session_plan_mutation_entry_authority';
        END IF;
    END IF;

    IF NEW.event_kind = 'depends_on' THEN
        SELECT target.event_kind,
               session_plan_event_has_valid_shape(target)
               AND session_plan_creation_has_valid_shape(target),
               session_plan_event_has_authority(target)
          INTO target_kind, target_shape_valid, target_authorized
          FROM session_plan_event AS target
         WHERE target.session_id = NEW.session_id
           AND target.event_ordinal = NEW.dependency_ordinal;
        IF target_kind IS DISTINCT FROM 'created' THEN
            RAISE EXCEPTION
                'session plan dependency must name a creation event';
        END IF;
        IF target_shape_valid IS DISTINCT FROM TRUE
            OR target_authorized IS DISTINCT FROM TRUE
        THEN
            RAISE EXCEPTION 'session plan dependency lacks certified authority'
                USING ERRCODE = '23514',
                    CONSTRAINT = 'session_plan_dependency_authority';
        END IF;

        IF NOT EXISTS (
            SELECT 1
              FROM session_plan_current_dependency AS edge
             WHERE edge.session_id = NEW.session_id
               AND edge.entry_ordinal = NEW.entry_ordinal
               AND edge.dependency_ordinal = NEW.dependency_ordinal
        ) THEN
            SELECT count(*)
              INTO dependency_count
              FROM session_plan_current_dependency AS edge
             WHERE edge.session_id = NEW.session_id
               AND edge.entry_ordinal = NEW.entry_ordinal;
            -- Checked mechanically against MAX_PLAN_DEPENDENCIES_PER_ENTRY.
            IF dependency_count >= 32 THEN
                RAISE EXCEPTION
                    'session plan entry dependency limit reached'
                    USING ERRCODE = '23514',
                        CONSTRAINT = 'session_plan_dependency_limit';
            END IF;
        END IF;

        SELECT inspection.graph_cyclic,
               inspection.closes_cycle,
               inspection.graph_shape_valid,
               inspection.graph_authorized,
               inspection.graph_within_limit
          INTO graph_cyclic,
               closes_cycle,
               graph_shape_valid,
               graph_authorized,
               graph_within_limit
          FROM inspect_session_plan_dependency_graph(
              NEW.session_id,
              NEW.entry_ordinal,
              NEW.dependency_ordinal
          ) AS inspection;
        IF NOT graph_shape_valid THEN
            RAISE EXCEPTION
                'session plan dependency graph has invalid event shape'
                USING ERRCODE = '23514',
                    CONSTRAINT = 'session_plan_dependency_graph_shape';
        END IF;
        IF NOT graph_authorized THEN
            RAISE EXCEPTION
                'session plan dependency graph lacks certified authority'
                USING ERRCODE = '23514',
                    CONSTRAINT = 'session_plan_dependency_graph_authority';
        END IF;
        IF NOT graph_within_limit THEN
            RAISE EXCEPTION 'session plan dependency graph exceeds its limit'
                USING ERRCODE = '23514',
                    CONSTRAINT = 'session_plan_dependency_limit';
        END IF;
        IF graph_cyclic THEN
            RAISE EXCEPTION 'session plan dependency graph is already cyclic'
                USING ERRCODE = '23514',
                    CONSTRAINT = 'session_plan_dependency_graph_cycle';
        END IF;
        IF closes_cycle THEN
            RAISE EXCEPTION 'session plan dependency would create a cycle'
                USING ERRCODE = '23514',
                    CONSTRAINT = 'session_plan_dependency_cycle';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: guard_session_plan_head_maintenance(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_session_plan_head_maintenance() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF pg_trigger_depth() < 2 THEN
        RAISE EXCEPTION 'session plan head is trigger-maintained';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: initialize_session_timeline_fact(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION initialize_session_timeline_fact() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    INSERT INTO session_timeline_fact (
        session_id, item_count, event_kind_bytes,
        projected_text_bytes, active_turn_count, queued_turn_count
    ) VALUES (NEW.session_id, 0, 0, 0, 0, 0);
    RETURN NULL;
END
$$;


--
-- Name: inspect_session_plan_dependency_graph(uuid, numeric, numeric); Type: FUNCTION; Schema: public
--

CREATE FUNCTION inspect_session_plan_dependency_graph(target_session_id uuid, target_entry_ordinal numeric, target_dependency_ordinal numeric) RETURNS TABLE(graph_cyclic boolean, closes_cycle boolean, graph_shape_valid boolean, graph_authorized boolean, graph_within_limit boolean)
    LANGUAGE plpgsql
    AS $$
DECLARE
    root_node numeric(20, 0);
    detects_proposed_cycle boolean;
    stack_depth bigint;
    current_node numeric(20, 0);
    after_dependency numeric(20, 0);
    next_dependency numeric(20, 0);
    edge_shape_valid boolean;
    edge_authorized boolean;
    edge_found boolean;
    node_shape_valid boolean;
    node_authorized boolean;
    child_finished boolean;
    child_seen boolean;
    dependency_count bigint;
    distinct_dependency_count bigint;
BEGIN
    CREATE TEMP TABLE IF NOT EXISTS pg_temp.session_plan_dependency_visit (
        node numeric(20, 0) PRIMARY KEY,
        finished boolean NOT NULL
    ) ON COMMIT DELETE ROWS;
    CREATE TEMP TABLE IF NOT EXISTS pg_temp.session_plan_dependency_stack (
        depth bigint PRIMARY KEY,
        node numeric(20, 0) NOT NULL,
        next_dependency_ordinal numeric(20, 0)
    ) ON COMMIT DELETE ROWS;
    DELETE FROM pg_temp.session_plan_dependency_stack;
    DELETE FROM pg_temp.session_plan_dependency_visit;

    graph_cyclic := FALSE;
    closes_cycle := FALSE;
    graph_shape_valid := TRUE;
    graph_authorized := TRUE;
    graph_within_limit := TRUE;
    <<root_scan>>
    FOR root_node, detects_proposed_cycle IN
        SELECT root.node, root.detects_cycle
          FROM (
              VALUES
                  (target_dependency_ordinal, TRUE),
                  (target_entry_ordinal, FALSE)
          ) AS root(node, detects_cycle)
    LOOP
        IF EXISTS (
            SELECT 1
              FROM pg_temp.session_plan_dependency_visit AS visit
             WHERE visit.node = root_node
        ) THEN
            CONTINUE;
        END IF;

        INSERT INTO pg_temp.session_plan_dependency_visit (node, finished)
        VALUES (root_node, FALSE);
        stack_depth := 1;
        INSERT INTO pg_temp.session_plan_dependency_stack (
            depth, node, next_dependency_ordinal
        )
        VALUES (stack_depth, root_node, NULL);

        WHILE stack_depth > 0 LOOP
            SELECT stack.node, stack.next_dependency_ordinal
              INTO current_node, after_dependency
              FROM pg_temp.session_plan_dependency_stack AS stack
             WHERE stack.depth = stack_depth;

            IF after_dependency IS NULL THEN
                SELECT count(*) = 1
                           AND coalesce(
                               bool_and(
                                   session_plan_event_has_valid_shape(creation)
                                   AND session_plan_creation_has_valid_shape(
                                       creation
                                   )
                               ),
                               FALSE
                           ),
                       count(*) = 1
                           AND coalesce(
                               bool_and(
                                   session_plan_event_has_authority(creation)
                               ),
                               FALSE
                           )
                  INTO node_shape_valid, node_authorized
                  FROM session_plan_event AS creation
                 WHERE creation.session_id = target_session_id
                   AND creation.event_ordinal = current_node;
                IF NOT node_shape_valid THEN
                    graph_shape_valid := FALSE;
                    EXIT root_scan;
                END IF;
                IF NOT node_authorized THEN
                    graph_authorized := FALSE;
                    EXIT root_scan;
                END IF;

                SELECT count(*), count(DISTINCT edge.dependency_ordinal)
                  INTO dependency_count, distinct_dependency_count
                  FROM session_plan_current_dependency AS edge
                 WHERE edge.session_id = target_session_id
                   AND edge.entry_ordinal = current_node;
                IF dependency_count <> distinct_dependency_count THEN
                    graph_shape_valid := FALSE;
                    EXIT root_scan;
                END IF;
                -- Checked mechanically against MAX_PLAN_DEPENDENCIES_PER_ENTRY.
                IF dependency_count > 32 THEN
                    graph_within_limit := FALSE;
                    EXIT root_scan;
                END IF;
            END IF;

            SELECT edge.dependency_ordinal,
                   coalesce(
                       session_plan_event_has_valid_shape(first_event)
                       AND first_event.event_kind = 'depends_on'
                       AND first_event.entry_ordinal = edge.entry_ordinal
                       AND first_event.dependency_ordinal =
                           edge.dependency_ordinal
                       AND first_event.event_ordinal =
                           edge.first_event_ordinal,
                       FALSE
                   ),
                   coalesce(
                       session_plan_event_has_authority(first_event),
                       FALSE
                   )
              INTO next_dependency, edge_shape_valid, edge_authorized
              FROM session_plan_current_dependency AS edge
              LEFT JOIN session_plan_event AS first_event
                ON first_event.session_id = edge.session_id
               AND first_event.event_ordinal = edge.first_event_ordinal
             WHERE edge.session_id = target_session_id
               AND edge.entry_ordinal = current_node
               AND (
                   after_dependency IS NULL
                   OR edge.dependency_ordinal > after_dependency
               )
             ORDER BY edge.dependency_ordinal
             LIMIT 1;
            edge_found := FOUND;

            IF NOT edge_found THEN
                UPDATE pg_temp.session_plan_dependency_visit AS visit
                   SET finished = TRUE
                 WHERE visit.node = current_node;
                DELETE FROM pg_temp.session_plan_dependency_stack AS stack
                 WHERE stack.depth = stack_depth;
                stack_depth := stack_depth - 1;
                CONTINUE;
            END IF;

            IF NOT edge_shape_valid THEN
                graph_shape_valid := FALSE;
                EXIT root_scan;
            END IF;
            IF NOT edge_authorized THEN
                graph_authorized := FALSE;
                EXIT root_scan;
            END IF;

            UPDATE pg_temp.session_plan_dependency_stack AS stack
               SET next_dependency_ordinal = next_dependency
             WHERE stack.depth = stack_depth;
            IF detects_proposed_cycle
                AND next_dependency = target_entry_ordinal
            THEN
                closes_cycle := TRUE;
            END IF;

            SELECT visit.finished
              INTO child_finished
              FROM pg_temp.session_plan_dependency_visit AS visit
             WHERE visit.node = next_dependency;
            child_seen := FOUND;
            IF child_seen THEN
                IF NOT child_finished THEN
                    graph_cyclic := TRUE;
                    EXIT root_scan;
                END IF;
                CONTINUE;
            END IF;

            INSERT INTO pg_temp.session_plan_dependency_visit (node, finished)
            VALUES (next_dependency, FALSE);
            stack_depth := stack_depth + 1;
            INSERT INTO pg_temp.session_plan_dependency_stack (
                depth, node, next_dependency_ordinal
            )
            VALUES (stack_depth, next_dependency, NULL);
        END LOOP;
    END LOOP root_scan;
    RETURN NEXT;
END;
$$;


--
-- Name: maintain_session_catalog_last_activity(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION maintain_session_catalog_last_activity() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    UPDATE session_timeline_fact
       SET attention_activity_recorded_at = NEW.recorded_at
     WHERE session_id = NEW.session_id;
    RETURN NULL;
END;
$$;


--
-- Name: next_session_plan_event_ordinal(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION next_session_plan_event_ordinal(target_session_id uuid) RETURNS numeric
    LANGUAGE plpgsql
    AS $$
DECLARE
    latest_ordinal numeric(20, 0);
BEGIN
    PERFORM 1
      FROM session
     WHERE session_id = target_session_id
       FOR NO KEY UPDATE;
    IF NOT FOUND THEN
        RETURN NULL;
    END IF;

    SELECT event_ordinal
      INTO latest_ordinal
      FROM session_plan_head
     WHERE session_id = target_session_id;
    RETURN coalesce(latest_ordinal + 1, 1);
END;
$$;


--
-- Name: project_web_search_session_metadata(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION project_web_search_session_metadata() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    projected_text text;
    anchor_sequence numeric(20, 0);
BEGIN
    SELECT concat_ws(
               E'\n', metadata.title,
               (SELECT string_agg(tag.tag, E'\n' ORDER BY tag.tag)
                  FROM session_metadata_tag AS tag
                 WHERE tag.session_id = metadata.session_id),
               (SELECT string_agg(
                           attribute.attribute_key || E'\n' || attribute.attribute_value,
                           E'\n' ORDER BY attribute.attribute_key
                       )
                  FROM session_metadata_attribute AS attribute
                 WHERE attribute.session_id = metadata.session_id)
           ), created.event_sequence
      INTO projected_text, anchor_sequence
      FROM session_metadata AS metadata
      JOIN session_created_outbox_event AS created
        ON created.session_id = metadata.session_id
     WHERE metadata.session_id = NEW.session_id;
    IF projected_text IS NULL OR projected_text = '' THEN
        DELETE FROM web_search_projection
         WHERE source_kind = 'session_metadata'
           AND source_id = NEW.session_id
           AND content_class = 'session_metadata';
        RETURN NULL;
    END IF;
    INSERT INTO web_search_projection (
        source_kind, source_id, session_id, event_sequence,
        item_kind, item_id, turn_id, content_class,
        projection_ordinal, content_text
    ) SELECT
        'session_metadata', NEW.session_id, NEW.session_id, anchor_sequence,
        'session', NEW.session_id, NULL, 'session_metadata',
        chunk.ordinal, chunk.content_text
      FROM web_search_projection_chunks(projected_text) AS chunk
    ON CONFLICT (
        source_kind, source_id, content_class, projection_ordinal
    ) DO UPDATE
       SET content_text = EXCLUDED.content_text;
    DELETE FROM web_search_projection
     WHERE source_kind = 'session_metadata'
       AND source_id = NEW.session_id
       AND content_class = 'session_metadata'
       AND projection_ordinal >= (
           SELECT count(*)
             FROM web_search_projection_chunks(projected_text)
       );
    RETURN NULL;
END
$$;


--
-- Name: reconcile_session_timeline_goal_work_fact(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reconcile_session_timeline_goal_work_fact() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    prior_kind text;
    prior_generation numeric(20, 0);
    retired numeric(20, 0);
    pursued numeric(20, 0);
    gained numeric(20, 0);
    lost numeric(20, 0);
BEGIN
    IF NEW.event_ordinal = 1 THEN
        -- The session's first goal event. No committed goal turn can precede it
        -- because a goal turn requires a goal event, and any goal turn inserted
        -- alongside it must belong to the generation it commissions, since
        -- `goal_turn_current_pursuit` is checked against the latest event at
        -- commit. Such a turn was admitted before this event by the same "no
        -- goal event speaks for it" fallback that admits it after, so nothing
        -- changes hands and the allocator is not worth touching.
        RETURN NULL;
    END IF;
    -- `goal_event` is append-only with contiguous ordinals, serialised per
    -- session by the row lock `require_goal_event_continuity` already holds, so
    -- the event this one displaces as latest is a primary-key lookup.
    SELECT event_kind, generation INTO prior_kind, prior_generation
      FROM goal_event
     WHERE session_id = NEW.session_id
       AND event_ordinal = NEW.event_ordinal - 1;
    retired := goal_event_pursued_generation(prior_kind, prior_generation);
    pursued := goal_event_pursued_generation(NEW.event_kind, NEW.generation);
    IF retired IS NOT DISTINCT FROM pursued THEN
        -- The session pursues what it already pursued, so no queued turn
        -- changes hands. This decision reads only `goal_event`, which the
        -- session row lock above already serialises, so returning here without
        -- the allocator keeps a restated retirement off the global lock.
        RETURN NULL;
    END IF;

    -- Everything below runs under the allocator lock, in the same
    -- allocator-then-fact order every other fact update takes. The counts have
    -- to be read under it rather than before it: that lock is what serialises
    -- this against a lifecycle transition moving the same turns concurrently,
    -- and a delta computed from an earlier read could subtract a turn the other
    -- transaction has already accounted for.
    PERFORM 1 FROM outbox_sequence_state WHERE singleton FOR UPDATE;
    -- Each count is a primary-key read of the per-generation fact, so this
    -- transition costs two row lookups whatever the retired generation's
    -- history or the session's queue holds. Counting the same turns by joining
    -- `goal_turn` to `turn_lifecycle` would instead scan one whole side of the
    -- intersection -- the generation's turns or the session's queued turns --
    -- because neither table indexes generation and queued-ness together. A NULL
    -- generation names no generation and matches no row, which is how the
    -- retiring kinds count zero on the pursued side.
    SELECT coalesce((
        SELECT fact.queued_turn_count
          FROM session_goal_generation_work_fact AS fact
         WHERE fact.session_id = NEW.session_id
           AND fact.goal_generation = pursued
    ), 0) INTO gained;
    SELECT coalesce((
        SELECT fact.queued_turn_count
          FROM session_goal_generation_work_fact AS fact
         WHERE fact.session_id = NEW.session_id
           AND fact.goal_generation = retired
    ), 0) INTO lost;
    IF gained = lost THEN
        RETURN NULL;
    END IF;
    UPDATE session_timeline_fact
       SET queued_turn_count = queued_turn_count + gained - lost
     WHERE session_id = NEW.session_id;
    RETURN NULL;
END
$$;


--
-- Name: record_session_metadata_installation(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION record_session_metadata_installation() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.result_kind <> 'applied' THEN
        RETURN NEW;
    END IF;

    IF EXISTS (
        SELECT 1
          FROM session_metadata_installation
         WHERE session_id = NEW.result_applied_session_id
           AND source_command_id = NEW.command_id
    ) THEN
        RAISE EXCEPTION
            'session metadata receipt % was already installed for session %',
            NEW.command_id,
            NEW.result_applied_session_id
            USING ERRCODE = '23514';
    END IF;

    INSERT INTO session_metadata_installation (
        session_id,
        source_command_id
    )
    VALUES (
        NEW.result_applied_session_id,
        NEW.command_id
    );

    RETURN NEW;
END;
$$;


--
-- Name: reject_compact_session_command_invalid_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_compact_session_command_invalid_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.result_kind <> 'pending' THEN
            RAISE EXCEPTION 'compaction command must begin pending'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;

    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'compaction command is not deletable'
            USING ERRCODE = '23514';
    END IF;

    IF ROW(
        OLD.command_id,
        OLD.command_kind,
        OLD.storage_version,
        OLD.session_id,
        OLD.requested_through_position,
        OLD.automatic_for_turn_id,
        OLD.model_call_id
    ) IS DISTINCT FROM ROW(
        NEW.command_id,
        NEW.command_kind,
        NEW.storage_version,
        NEW.session_id,
        NEW.requested_through_position,
        NEW.automatic_for_turn_id,
        NEW.model_call_id
    ) THEN
        RAISE EXCEPTION 'compaction command request is immutable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.result_kind <> 'pending'
       OR NEW.result_kind NOT IN ('applied', 'failed')
    THEN
        RAISE EXCEPTION 'invalid compaction command result transition'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: reject_sealed_session_metadata_receipt_satellite_insert(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_sealed_session_metadata_receipt_satellite_insert() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM command_id
      FROM durable_command
     WHERE command_id = NEW.command_id
       FOR UPDATE;

    IF NOT FOUND THEN
        RAISE EXCEPTION
            'session metadata receipt satellite % has no command claim',
            NEW.command_id
            USING ERRCODE = '23503';
    END IF;

    IF EXISTS (
        SELECT 1
          FROM replace_session_metadata_command
         WHERE command_id = NEW.command_id
    ) THEN
        RAISE EXCEPTION
            'session metadata receipt % is already sealed',
            NEW.command_id
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;


--
-- Name: reject_session_metadata_identity_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_session_metadata_identity_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.session_id IS DISTINCT FROM OLD.session_id THEN
        RAISE EXCEPTION
            'session metadata identity is immutable'
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;


--
-- Name: reject_session_metadata_receipt_reinstallation(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_session_metadata_receipt_reinstallation() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.source_command_id IS NOT DISTINCT FROM OLD.source_command_id THEN
        RETURN NEW;
    END IF;

    IF EXISTS (
        SELECT 1
          FROM session_metadata_installation
         WHERE session_id = NEW.session_id
           AND source_command_id = NEW.source_command_id
    ) THEN
        RAISE EXCEPTION
            'session metadata receipt % was already installed for session %',
            NEW.source_command_id,
            NEW.session_id
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;


--
-- Name: reject_session_metadata_table_truncate(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_session_metadata_table_truncate() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RAISE EXCEPTION
        'session metadata table % is not truncatable',
        TG_TABLE_NAME
        USING ERRCODE = '23514';
END;
$$;


--
-- Name: reject_session_model_credential_entry_after_publication(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_session_model_credential_entry_after_publication() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM 1
      FROM session
     WHERE session_id = NEW.session_id
       FOR UPDATE;
    IF EXISTS (
        SELECT 1
          FROM session_current_model_credentials
         WHERE session_id = NEW.session_id
           AND current_event_ordinal >= NEW.event_ordinal
    ) THEN
        RAISE EXCEPTION 'published session model credential snapshots are immutable';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: reject_session_model_credential_rewrite(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_session_model_credential_rewrite() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RAISE EXCEPTION 'session model credential history is append-only';
END;
$$;


--
-- Name: reject_session_plan_current_dependency_rewrite(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_session_plan_current_dependency_rewrite() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP = 'INSERT' AND pg_trigger_depth() >= 2 THEN
        RETURN NEW;
    END IF;
    RAISE EXCEPTION 'session plan current dependencies are append projections'
        USING ERRCODE = '23514',
            CONSTRAINT = 'session_plan_current_dependency_maintenance';
END;
$$;


--
-- Name: reject_session_plan_event_rewrite(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_session_plan_event_rewrite() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RAISE EXCEPTION 'session plan history is append-only';
END;
$$;


--
-- Name: reject_session_plan_head_rewrite(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_session_plan_head_rewrite() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RAISE EXCEPTION 'session plan head is trigger-maintained';
END;
$$;


--
-- Name: require_session_creation_command(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_session_creation_command() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE native_count bigint; imported_count bigint; delegated_count bigint;
BEGIN
    SELECT count(*) INTO native_count FROM create_session_command
     WHERE created_session_id = NEW.session_id;
    SELECT count(*) INTO imported_count FROM create_session_from_imported_frontier_command
     WHERE created_session_id = NEW.session_id;
    SELECT count(*) INTO delegated_count FROM session_delegation
     WHERE child_session_id = NEW.session_id;
    IF (NEW.creation_cause = 'user_initiated' AND NEW.ancestry_kind = 'none'
            AND (native_count, imported_count, delegated_count) <> (1, 0, 0))
        OR (NEW.creation_cause = 'user_initiated' AND NEW.ancestry_kind = 'imported_conversation'
            AND (native_count, imported_count, delegated_count) <> (0, 1, 0))
        OR (NEW.creation_cause = 'delegated'
            AND (native_count, imported_count, delegated_count) <> (0, 0, 1)) THEN
        RAISE EXCEPTION 'session % requires exactly one matching creation family', NEW.session_id
            USING ERRCODE = '23503', CONSTRAINT = 'session_requires_creation_command';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_session_metadata_installation_is_current(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_session_metadata_installation_is_current() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM session_metadata
         WHERE session_id = NEW.session_id
           AND source_command_id = NEW.source_command_id
    ) THEN
        RAISE EXCEPTION
            'session metadata installation is not current for session %',
            NEW.session_id
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;


--
-- Name: require_session_metadata_matches_receipt(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_session_metadata_matches_receipt() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    checked_session_id uuid;
BEGIN
    IF TG_OP = 'DELETE' THEN
        checked_session_id := OLD.session_id;
    ELSE
        checked_session_id := NEW.session_id;
    END IF;

    IF EXISTS (
        SELECT 1
          FROM session_metadata AS current
          LEFT JOIN replace_session_metadata_command AS receipt
            ON receipt.command_id = current.source_command_id
           AND receipt.result_applied_session_id = current.session_id
         WHERE current.session_id = checked_session_id
           AND (
                receipt.command_id IS NULL
                OR receipt.result_kind <> 'applied'
                OR current.title IS DISTINCT FROM receipt.replacement_title
                OR current.archived IS DISTINCT FROM receipt.replacement_archived
                OR current.updated_at IS DISTINCT FROM receipt.result_updated_at
                OR current.actor_kind IS DISTINCT FROM receipt.result_actor_kind
                OR current.actor_turn_id
                    IS DISTINCT FROM receipt.result_actor_turn_id
                OR current.actor_tool_request_id
                    IS DISTINCT FROM receipt.result_actor_tool_request_id
                OR EXISTS (
                    SELECT current_tag.tag
                      FROM session_metadata_tag AS current_tag
                     WHERE current_tag.session_id = current.session_id
                    EXCEPT
                    SELECT receipt_tag.tag
                      FROM replace_session_metadata_command_tag AS receipt_tag
                     WHERE receipt_tag.command_id = receipt.command_id
                )
                OR EXISTS (
                    SELECT receipt_tag.tag
                      FROM replace_session_metadata_command_tag AS receipt_tag
                     WHERE receipt_tag.command_id = receipt.command_id
                    EXCEPT
                    SELECT current_tag.tag
                      FROM session_metadata_tag AS current_tag
                     WHERE current_tag.session_id = current.session_id
                )
                OR EXISTS (
                    SELECT
                        current_attribute.attribute_key,
                        current_attribute.attribute_value
                      FROM session_metadata_attribute AS current_attribute
                     WHERE current_attribute.session_id = current.session_id
                    EXCEPT
                    SELECT
                        receipt_attribute.attribute_key,
                        receipt_attribute.attribute_value
                      FROM replace_session_metadata_command_attribute
                        AS receipt_attribute
                     WHERE receipt_attribute.command_id = receipt.command_id
                )
                OR EXISTS (
                    SELECT
                        receipt_attribute.attribute_key,
                        receipt_attribute.attribute_value
                      FROM replace_session_metadata_command_attribute
                        AS receipt_attribute
                     WHERE receipt_attribute.command_id = receipt.command_id
                    EXCEPT
                    SELECT
                        current_attribute.attribute_key,
                        current_attribute.attribute_value
                      FROM session_metadata_attribute AS current_attribute
                     WHERE current_attribute.session_id = current.session_id
                )
           )
    ) THEN
        RAISE EXCEPTION
            'current session metadata does not match its source receipt'
            USING ERRCODE = '23514';
    END IF;

    IF TG_OP = 'DELETE' THEN
        RETURN OLD;
    END IF;

    RETURN NEW;
END;
$$;


--
-- Name: session_plan_request_arguments_json(text, text); Type: FUNCTION; Schema: public
--

CREATE FUNCTION session_plan_request_arguments_json(arguments_kind text, arguments_text text) RETURNS jsonb
    LANGUAGE plpgsql IMMUTABLE
    AS $$
BEGIN
    IF arguments_kind IS DISTINCT FROM 'json' OR arguments_text IS NULL THEN
        RETURN NULL;
    END IF;
    BEGIN
        RETURN arguments_text::jsonb;
    EXCEPTION
        -- Every JSONB conversion failure makes the request unsuitable as
        -- append authority, including escaped NUL and numeric overflow.
        WHEN data_exception THEN
            RETURN NULL;
    END;
END;
$$;


--
-- Name: session_system_prompt_digest(text); Type: FUNCTION; Schema: public
--

CREATE FUNCTION session_system_prompt_digest(prompt text) RETURNS bytea
    LANGUAGE sql IMMUTABLE PARALLEL SAFE
    RETURN COALESCE(sha256(convert_to(prompt, 'UTF8'::name)), '\x'::bytea);


--
-- Name: update_session_timeline_work_fact(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION update_session_timeline_work_fact() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    pursued boolean;
    turn_generation numeric(20, 0);
    generation_delta integer;
BEGIN
    -- Outbox appends acquire the allocator before this session fact. Taking the
    -- same locks in the same order prevents lifecycle updates from inverting it.
    PERFORM 1 FROM outbox_sequence_state WHERE singleton FOR UPDATE;
    -- The generation this turn belongs to is a primary-key read of `goal_turn`,
    -- and a turn with no goal turn belongs to no generation: it is credited
    -- unconditionally and no goal transition ever moves it, so it contributes
    -- to no per-generation row. `goal_turn` is append-only, so the generation
    -- this reads cannot change under a later transition of the same turn.
    SELECT goal.goal_generation INTO turn_generation
      FROM goal_turn AS goal
     WHERE goal.session_id = NEW.session_id
       AND goal.turn_id = NEW.turn_id;
    IF turn_generation IS NOT NULL THEN
        generation_delta :=
            (NEW.state_kind = 'queued'
             AND NOT NEW.delegation_runtime_terminal)::integer
            - CASE
                WHEN TG_OP = 'UPDATE'
                    THEN (OLD.state_kind = 'queued'
                          AND NOT OLD.delegation_runtime_terminal)::integer
                ELSE 0
              END;
        IF generation_delta <> 0 THEN
            PERFORM apply_goal_generation_queued_delta(
                NEW.session_id, turn_generation, generation_delta
            );
        END IF;
    END IF;
    -- A queued turn is credited only while its goal generation is still
    -- pursued, so the subtraction has to carry the same guard the credit did.
    -- Without it a turn already retired by a goal event -- whose credit the
    -- reconciliation below has removed -- is subtracted a second time when it
    -- later leaves the queue or releases its delegated runtime slot, driving
    -- the count negative and aborting the writing transaction on the fact
    -- table's nonnegative check.
    --
    -- This trigger fires only on `state_kind` and `delegation_runtime_terminal`
    -- and neither changes which generation the session pursues, so a single
    -- evaluation is correct for the old and the new state alike. It has to
    -- happen under the allocator lock: that lock is what serialises this
    -- against a goal event retiring the same generation concurrently, and a
    -- read taken before it could credit a generation the committed goal event
    -- has already retired.
    pursued := goal_turn_generation_is_pursued(NEW.session_id, NEW.turn_id);
    IF TG_OP = 'UPDATE' THEN
        UPDATE session_timeline_fact
           SET active_turn_count = active_turn_count
                   - (OLD.state_kind = 'active' AND NOT OLD.delegation_runtime_terminal)::integer
                   + (NEW.state_kind = 'active' AND NOT NEW.delegation_runtime_terminal)::integer,
               queued_turn_count = queued_turn_count
                   - (OLD.state_kind = 'queued'
                      AND NOT OLD.delegation_runtime_terminal
                      AND pursued)::integer
                   + (NEW.state_kind = 'queued'
                      AND NOT NEW.delegation_runtime_terminal
                      AND pursued)::integer
         WHERE session_id = NEW.session_id;
    ELSE
        UPDATE session_timeline_fact
           SET active_turn_count = active_turn_count
                   + (NEW.state_kind = 'active' AND NOT NEW.delegation_runtime_terminal)::integer,
               queued_turn_count = queued_turn_count
                   + (NEW.state_kind = 'queued'
                      AND NOT NEW.delegation_runtime_terminal
                      AND pursued)::integer
         WHERE session_id = NEW.session_id;
    END IF;
    RETURN NULL;
END
$$;


--
-- Tables.
--

--
-- Name: session_plan_event; Type: TABLE; Schema: public
--

CREATE TABLE session_plan_event (
    session_id uuid NOT NULL,
    event_ordinal numeric(20,0) NOT NULL,
    prior_event_ordinal numeric(20,0),
    event_kind text NOT NULL,
    entry_ordinal numeric(20,0) NOT NULL,
    dependency_ordinal numeric(20,0),
    entry_text text,
    entry_status text,
    provenance_turn_id uuid NOT NULL,
    provenance_issuing_turn_attempt_id uuid NOT NULL,
    provenance_request_id uuid NOT NULL,
    provenance_attempt_id uuid NOT NULL,
    provenance_dispatch_generation numeric(20,0) NOT NULL,
    CONSTRAINT session_plan_event_dependency_ordinal_check CHECK (((dependency_ordinal IS NULL) OR ((dependency_ordinal >= (1)::numeric) AND (dependency_ordinal <= '18446744073709551615'::numeric)))),
    CONSTRAINT session_plan_event_entry_ordinal_check CHECK (((entry_ordinal >= (1)::numeric) AND (entry_ordinal <= '18446744073709551615'::numeric))),
    CONSTRAINT session_plan_event_event_ordinal_check CHECK (((event_ordinal >= (1)::numeric) AND (event_ordinal <= '18446744073709551615'::numeric))),
    CONSTRAINT session_plan_event_kind_closed CHECK ((event_kind = ANY (ARRAY['created'::text, 'text_revised'::text, 'status_changed'::text, 'depends_on'::text]))),
    CONSTRAINT session_plan_event_predecessor_shape CHECK ((((event_ordinal = (1)::numeric) AND (prior_event_ordinal IS NULL)) OR ((event_ordinal > (1)::numeric) AND (prior_event_ordinal = (event_ordinal - (1)::numeric))))),
    CONSTRAINT session_plan_event_prior_event_ordinal_check CHECK (((prior_event_ordinal IS NULL) OR ((prior_event_ordinal >= (1)::numeric) AND (prior_event_ordinal <= '18446744073709551615'::numeric)))),
    CONSTRAINT session_plan_event_provenance_dispatch_generation_check CHECK (((provenance_dispatch_generation >= (1)::numeric) AND (provenance_dispatch_generation <= '18446744073709551615'::numeric))),
    CONSTRAINT session_plan_event_shape CHECK ((((event_kind = 'created'::text) AND (entry_ordinal = event_ordinal) AND (dependency_ordinal IS NULL) AND (entry_text IS NOT NULL) AND ((char_length(entry_text) >= 1) AND (char_length(entry_text) <= 4096)) AND (entry_status IS NULL)) OR ((event_kind = 'text_revised'::text) AND (entry_ordinal < event_ordinal) AND (dependency_ordinal IS NULL) AND (entry_text IS NOT NULL) AND ((char_length(entry_text) >= 1) AND (char_length(entry_text) <= 4096)) AND (entry_status IS NULL)) OR ((event_kind = 'status_changed'::text) AND (entry_ordinal < event_ordinal) AND (dependency_ordinal IS NULL) AND (entry_text IS NULL) AND (entry_status IS NOT NULL)) OR ((event_kind = 'depends_on'::text) AND (entry_ordinal < event_ordinal) AND (dependency_ordinal IS NOT NULL) AND (dependency_ordinal < event_ordinal) AND (dependency_ordinal <> entry_ordinal) AND (entry_text IS NULL) AND (entry_status IS NULL)))),
    CONSTRAINT session_plan_event_status_closed CHECK (((entry_status IS NULL) OR (entry_status = ANY (ARRAY['pending'::text, 'in_progress'::text, 'completed'::text, 'abandoned'::text]))))
);


--
-- Name: session_plan_creation_has_valid_shape(session_plan_event); Type: FUNCTION; Schema: public
--

CREATE FUNCTION session_plan_creation_has_valid_shape(candidate session_plan_event) RETURNS boolean
    LANGUAGE sql IMMUTABLE
    AS $$
    SELECT candidate.event_kind = 'created'
       AND candidate.entry_ordinal = candidate.event_ordinal
       AND candidate.dependency_ordinal IS NULL
       AND candidate.entry_text IS NOT NULL
       AND char_length(candidate.entry_text) BETWEEN 1 AND 4096
       AND candidate.entry_status IS NULL;
$$;


--
-- Name: session_plan_event_has_authority(session_plan_event); Type: FUNCTION; Schema: public
--

CREATE FUNCTION session_plan_event_has_authority(candidate session_plan_event) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    SELECT EXISTS (
        SELECT 1
          FROM tool_attempt AS attempt
          JOIN tool_request AS request
            ON request.request_id = attempt.request_id
         WHERE attempt.attempt_id = candidate.provenance_attempt_id
           AND attempt.request_id = candidate.provenance_request_id
           AND attempt.issuing_turn_attempt_id =
                candidate.provenance_issuing_turn_attempt_id
           AND attempt.dispatch_generation =
                candidate.provenance_dispatch_generation
           AND attempt.turn_id = candidate.provenance_turn_id
           AND attempt.session_id = candidate.session_id
           AND attempt.effect_class = 'external_effect'
           AND (
                attempt.state_kind = 'in_flight'
                OR (
                    attempt.state_kind = 'terminal'
                    AND (
                        attempt.terminal_disposition_kind IN (
                            'completed', 'ambiguous'
                        )
                        OR (
                            attempt.terminal_disposition_kind = 'known_failed'
                            AND attempt.error_kind IN (
                                'execution_failed', 'result_too_large'
                            )
                        )
                    )
                )
           )
           AND request.request_id = candidate.provenance_request_id
           AND request.session_id = candidate.session_id
           AND request.turn_id = candidate.provenance_turn_id
           AND request.tool_name = 'plan_write'
           AND request.arguments_kind = 'json'
           AND session_plan_request_arguments_json(
                   request.arguments_kind, request.arguments_text
               ) =
                CASE candidate.event_kind
                    WHEN 'created' THEN jsonb_build_object(
                        'kind', 'create',
                        'text', candidate.entry_text
                    )
                    WHEN 'text_revised' THEN jsonb_build_object(
                        'kind', 'revise',
                        'entry_id', candidate.entry_ordinal,
                        'text', candidate.entry_text
                    )
                    WHEN 'status_changed' THEN jsonb_build_object(
                        'kind', 'set_status',
                        'entry_id', candidate.entry_ordinal,
                        'status', candidate.entry_status
                    )
                    WHEN 'depends_on' THEN jsonb_build_object(
                        'kind', 'depends_on',
                        'entry_id', candidate.entry_ordinal,
                        'dependency_id', candidate.dependency_ordinal
                    )
                END
    );
$$;


--
-- Name: session_plan_event_has_valid_shape(session_plan_event); Type: FUNCTION; Schema: public
--

CREATE FUNCTION session_plan_event_has_valid_shape(candidate session_plan_event) RETURNS boolean
    LANGUAGE sql IMMUTABLE
    AS $$
    SELECT coalesce(
        (
            (
                candidate.event_ordinal = 1
                AND candidate.prior_event_ordinal IS NULL
            )
            OR (
                candidate.event_ordinal > 1
                AND candidate.prior_event_ordinal =
                    candidate.event_ordinal - 1
            )
        )
        AND CASE candidate.event_kind
            WHEN 'created' THEN
                candidate.entry_ordinal = candidate.event_ordinal
                AND candidate.dependency_ordinal IS NULL
                AND candidate.entry_text IS NOT NULL
                AND char_length(candidate.entry_text) BETWEEN 1 AND 4096
                AND candidate.entry_status IS NULL
            WHEN 'text_revised' THEN
                candidate.entry_ordinal < candidate.event_ordinal
                AND candidate.dependency_ordinal IS NULL
                AND candidate.entry_text IS NOT NULL
                AND char_length(candidate.entry_text) BETWEEN 1 AND 4096
                AND candidate.entry_status IS NULL
            WHEN 'status_changed' THEN
                candidate.entry_ordinal < candidate.event_ordinal
                AND candidate.dependency_ordinal IS NULL
                AND candidate.entry_text IS NULL
                AND candidate.entry_status IN (
                    'pending', 'in_progress', 'completed', 'abandoned'
                )
            WHEN 'depends_on' THEN
                candidate.entry_ordinal < candidate.event_ordinal
                AND candidate.dependency_ordinal IS NOT NULL
                AND candidate.dependency_ordinal < candidate.event_ordinal
                AND candidate.dependency_ordinal <>
                    candidate.entry_ordinal
                AND candidate.entry_text IS NULL
                AND candidate.entry_status IS NULL
            ELSE FALSE
        END,
        FALSE
    );
$$;


--
-- Name: compact_session_command; Type: TABLE; Schema: public
--

CREATE TABLE compact_session_command (
    command_id uuid NOT NULL,
    command_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    requested_through_position numeric(20,0),
    automatic_for_turn_id uuid,
    result_kind text NOT NULL,
    result_context_compaction_id uuid,
    model_call_id uuid NOT NULL,
    result_through_position numeric(20,0),
    result_summary_entry_id uuid,
    result_frontier_id uuid,
    CONSTRAINT compact_session_command_kind_closed CHECK ((command_kind = 'compact_session'::text)),
    CONSTRAINT compact_session_command_requested_position_u64 CHECK (((requested_through_position IS NULL) OR ((requested_through_position >= (1)::numeric) AND (requested_through_position <= '18446744073709551615'::numeric)))),
    CONSTRAINT compact_session_command_result_kind_closed CHECK ((result_kind = ANY (ARRAY['pending'::text, 'applied'::text, 'failed'::text]))),
    CONSTRAINT compact_session_command_result_position_u64 CHECK (((result_through_position IS NULL) OR ((result_through_position >= (1)::numeric) AND (result_through_position <= '18446744073709551615'::numeric)))),
    CONSTRAINT compact_session_command_result_shape CHECK ((((result_kind = ANY (ARRAY['pending'::text, 'failed'::text])) AND (result_context_compaction_id IS NULL) AND (result_through_position IS NULL) AND (result_summary_entry_id IS NULL) AND (result_frontier_id IS NULL)) OR ((result_kind = 'applied'::text) AND (result_context_compaction_id IS NOT NULL) AND (result_through_position IS NOT NULL) AND (result_summary_entry_id IS NOT NULL) AND (result_frontier_id IS NOT NULL)))),
    CONSTRAINT compact_session_command_storage_version_supported CHECK ((storage_version = 1))
);


--
-- Name: create_session_command; Type: TABLE; Schema: public
--

CREATE TABLE create_session_command (
    command_id uuid NOT NULL,
    command_kind text NOT NULL,
    storage_version smallint NOT NULL,
    creation_cause text NOT NULL,
    ancestry_kind text NOT NULL,
    initial_defaults_version numeric(20,0) NOT NULL,
    model_selection_kind text NOT NULL,
    direct_model_selection_id uuid,
    model_alias_id uuid,
    model_selection_reference uuid GENERATED ALWAYS AS (COALESCE(direct_model_selection_id, model_alias_id)) STORED,
    result_kind text NOT NULL,
    created_session_id uuid NOT NULL,
    dangerous_tool_auto_approval text DEFAULT 'disabled'::text NOT NULL,
    system_prompt text,
    system_prompt_digest bytea GENERATED ALWAYS AS (session_system_prompt_digest(system_prompt)) STORED NOT NULL,
    template_name text,
    template_content_digest bytea,
    placement_path text,
    root_global_read_intent boolean DEFAULT false NOT NULL,
    model_settings jsonb DEFAULT '{"effective": {"fast_mode": "disabled", "service_tier": null, "reasoning_level": null}, "precedence": {"profile": {"fast_mode": {"kind": "inherit"}, "service_tier": {"kind": "inherit"}, "reasoning_level": {"kind": "inherit"}}, "session": {"fast_mode": {"kind": "inherit"}, "service_tier": {"kind": "inherit"}, "reasoning_level": {"kind": "inherit"}}, "per_call": {"fast_mode": {"kind": "inherit"}, "service_tier": {"kind": "inherit"}, "reasoning_level": {"kind": "inherit"}}, "global_default": {"fast_mode": {"kind": "inherit"}, "service_tier": {"kind": "inherit"}, "reasoning_level": {"kind": "inherit"}}}, "fast_mode_source": null, "reasoning_source": null, "service_tier_source": null, "validated_for_selection_id": null}'::jsonb NOT NULL,
    CONSTRAINT create_session_command_ancestry_kind_closed CHECK ((ancestry_kind = 'none'::text)),
    CONSTRAINT create_session_command_creation_cause_closed CHECK ((creation_cause = 'user_initiated'::text)),
    CONSTRAINT create_session_command_initial_defaults_version CHECK ((initial_defaults_version = (1)::numeric)),
    CONSTRAINT create_session_command_kind_closed CHECK ((command_kind = 'create_session'::text)),
    CONSTRAINT create_session_command_model_selection_kind_closed CHECK ((model_selection_kind = ANY (ARRAY['direct'::text, 'alias'::text]))),
    CONSTRAINT create_session_command_model_selection_shape CHECK ((((model_selection_kind = 'direct'::text) AND (direct_model_selection_id IS NOT NULL) AND (model_alias_id IS NULL)) OR ((model_selection_kind = 'alias'::text) AND (direct_model_selection_id IS NULL) AND (model_alias_id IS NOT NULL)))),
    CONSTRAINT create_session_command_model_settings_object CHECK ((jsonb_typeof(model_settings) = 'object'::text)),
    CONSTRAINT create_session_command_placement_path_shape CHECK (((placement_path IS NULL) OR ((octet_length(placement_path) BETWEEN 1 AND 4159) AND (placement_path ~ '^[A-Za-z0-9_-]{1,64}(\.[A-Za-z0-9_-]{1,64}){0,63}$'::text)))),
    CONSTRAINT create_session_command_placement_versioned CHECK (((storage_version >= 6) OR ((placement_path IS NULL) AND (NOT root_global_read_intent)))),
    CONSTRAINT create_session_command_result_kind_closed CHECK ((result_kind = 'applied'::text)),
    CONSTRAINT create_session_command_root_intent_shape CHECK ((root_global_read_intent = ((placement_path IS NOT NULL) AND (POSITION(('.'::text) IN (placement_path)) = 0)))),
    CONSTRAINT create_session_command_storage_version_supported CHECK ((storage_version = ANY (ARRAY[1, 2, 3, 4, 5, 6, 7]))),
    CONSTRAINT create_session_command_system_prompt_bounded CHECK (((system_prompt IS NULL) OR ((octet_length(convert_to(system_prompt, 'UTF8'::name)) >= 1) AND (octet_length(convert_to(system_prompt, 'UTF8'::name)) <= 1048576)))),
    CONSTRAINT create_session_command_system_prompt_versioned CHECK (((system_prompt IS NULL) OR (storage_version >= 3))),
    CONSTRAINT create_session_command_template_prompt_required CHECK (((template_name IS NULL) OR (system_prompt IS NOT NULL))),
    CONSTRAINT create_session_command_template_provenance_shape CHECK ((((template_name IS NULL) AND (template_content_digest IS NULL)) OR ((template_name IS NOT NULL) AND (template_content_digest IS NOT NULL) AND ((octet_length(convert_to(template_name, 'UTF8'::name)) >= 1) AND (octet_length(convert_to(template_name, 'UTF8'::name)) <= 128)) AND (template_name ~ '^[a-z0-9][a-z0-9._-]*$'::text) AND (octet_length(template_content_digest) = 32)))),
    CONSTRAINT create_session_command_template_provenance_versioned CHECK (((storage_version >= 4) OR ((template_name IS NULL) AND (template_content_digest IS NULL)))),
    CONSTRAINT create_session_command_tool_approval_closed CHECK ((dangerous_tool_auto_approval = ANY (ARRAY['disabled'::text, 'approve_all'::text])))
);


--
-- Name: create_session_from_imported_frontier_command; Type: TABLE; Schema: public
--

CREATE TABLE create_session_from_imported_frontier_command (
    command_id uuid CONSTRAINT create_session_from_imported_frontier_comma_command_id_not_null NOT NULL,
    command_kind text CONSTRAINT create_session_from_imported_frontier_com_command_kind_not_null NOT NULL,
    storage_version smallint CONSTRAINT create_session_from_imported_frontier__storage_version_not_null NOT NULL,
    imported_conversation_id uuid CONSTRAINT create_session_from_imported__imported_conversation_id_not_null NOT NULL,
    imported_frontier_entry_id uuid CONSTRAINT create_session_from_importe_imported_frontier_entry_id_not_null NOT NULL,
    imported_frontier_position numeric(20,0) CONSTRAINT create_session_from_importe_imported_frontier_position_not_null NOT NULL,
    imported_relationship_kind text CONSTRAINT create_session_from_importe_imported_relationship_kind_not_null NOT NULL,
    creation_cause text CONSTRAINT create_session_from_imported_frontier_c_creation_cause_not_null NOT NULL,
    ancestry_kind text CONSTRAINT create_session_from_imported_frontier_co_ancestry_kind_not_null NOT NULL,
    initial_defaults_version numeric(20,0) CONSTRAINT create_session_from_imported__initial_defaults_version_not_null NOT NULL,
    model_selection_kind text CONSTRAINT create_session_from_imported_fron_model_selection_kind_not_null NOT NULL,
    direct_model_selection_id uuid,
    model_alias_id uuid,
    model_selection_reference uuid GENERATED ALWAYS AS (COALESCE(direct_model_selection_id, model_alias_id)) STORED,
    result_kind text CONSTRAINT create_session_from_imported_frontier_comm_result_kind_not_null NOT NULL,
    created_session_id uuid CONSTRAINT create_session_from_imported_fronti_created_session_id_not_null NOT NULL,
    dangerous_tool_auto_approval text DEFAULT 'disabled'::text CONSTRAINT create_session_from_importe_dangerous_tool_auto_approv_not_null NOT NULL,
    system_prompt text,
    system_prompt_digest bytea GENERATED ALWAYS AS (session_system_prompt_digest(system_prompt)) STORED CONSTRAINT create_session_from_imported_fron_system_prompt_digest_not_null NOT NULL,
    model_settings jsonb DEFAULT '{"effective": {"fast_mode": "disabled", "service_tier": null, "reasoning_level": null}, "precedence": {"profile": {"fast_mode": {"kind": "inherit"}, "service_tier": {"kind": "inherit"}, "reasoning_level": {"kind": "inherit"}}, "session": {"fast_mode": {"kind": "inherit"}, "service_tier": {"kind": "inherit"}, "reasoning_level": {"kind": "inherit"}}, "per_call": {"fast_mode": {"kind": "inherit"}, "service_tier": {"kind": "inherit"}, "reasoning_level": {"kind": "inherit"}}, "global_default": {"fast_mode": {"kind": "inherit"}, "service_tier": {"kind": "inherit"}, "reasoning_level": {"kind": "inherit"}}}, "fast_mode_source": null, "reasoning_source": null, "service_tier_source": null, "validated_for_selection_id": null}'::jsonb CONSTRAINT create_session_from_imported_frontier_c_model_settings_not_null NOT NULL,
    CONSTRAINT create_session_from_imported_frontier_command_ancestry_closed CHECK ((ancestry_kind = 'imported_conversation'::text)),
    CONSTRAINT create_session_from_imported_frontier_command_cause_closed CHECK ((creation_cause = 'user_initiated'::text)),
    CONSTRAINT create_session_from_imported_frontier_command_initial_defaults CHECK ((initial_defaults_version = (1)::numeric)),
    CONSTRAINT create_session_from_imported_frontier_command_kind_closed CHECK ((command_kind = 'create_session_from_imported_frontier'::text)),
    CONSTRAINT create_session_from_imported_frontier_command_model_kind_closed CHECK ((model_selection_kind = ANY (ARRAY['direct'::text, 'alias'::text]))),
    CONSTRAINT create_session_from_imported_frontier_command_model_shape CHECK ((((model_selection_kind = 'direct'::text) AND (direct_model_selection_id IS NOT NULL) AND (model_alias_id IS NULL)) OR ((model_selection_kind = 'alias'::text) AND (direct_model_selection_id IS NULL) AND (model_alias_id IS NOT NULL)))),
    CONSTRAINT create_session_from_imported_frontier_command_result_closed CHECK ((result_kind = 'applied'::text)),
    CONSTRAINT create_session_from_imported_frontier_command_version_supported CHECK ((storage_version = ANY (ARRAY[1, 2, 3, 5]))),
    CONSTRAINT imported_frontier_command_position_positive_u64 CHECK (((imported_frontier_position >= (1)::numeric) AND (imported_frontier_position <= '18446744073709551615'::numeric))),
    CONSTRAINT imported_frontier_command_relationship_closed CHECK ((imported_relationship_kind = ANY (ARRAY['resume'::text, 'fork'::text]))),
    CONSTRAINT imported_frontier_command_system_prompt_bounded CHECK (((system_prompt IS NULL) OR ((octet_length(convert_to(system_prompt, 'UTF8'::name)) >= 1) AND (octet_length(convert_to(system_prompt, 'UTF8'::name)) <= 1048576)))),
    CONSTRAINT imported_frontier_command_system_prompt_versioned CHECK (((system_prompt IS NULL) OR (storage_version >= 3))),
    CONSTRAINT imported_frontier_command_tool_approval_closed CHECK ((dangerous_tool_auto_approval = ANY (ARRAY['disabled'::text, 'approve_all'::text]))),
    CONSTRAINT imported_session_command_model_settings_object CHECK ((jsonb_typeof(model_settings) = 'object'::text))
);


--
-- Name: replace_session_defaults_command; Type: TABLE; Schema: public
--

CREATE TABLE replace_session_defaults_command (
    command_id uuid NOT NULL,
    command_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    expected_current_version numeric(20,0) CONSTRAINT replace_session_defaults_comm_expected_current_version_not_null NOT NULL,
    model_selection_kind text NOT NULL,
    direct_model_selection_id uuid,
    model_alias_id uuid,
    model_selection_reference uuid GENERATED ALWAYS AS (COALESCE(direct_model_selection_id, model_alias_id)) STORED,
    result_kind text NOT NULL,
    rejection_kind text,
    result_session_id uuid NOT NULL,
    result_installed_version numeric(20,0),
    result_expected_version numeric(20,0),
    result_current_version numeric(20,0),
    dangerous_tool_auto_approval text DEFAULT 'disabled'::text CONSTRAINT replace_session_defaults_co_dangerous_tool_auto_approv_not_null NOT NULL,
    system_prompt text,
    system_prompt_digest bytea GENERATED ALWAYS AS (session_system_prompt_digest(system_prompt)) STORED NOT NULL,
    replacement_model_settings jsonb DEFAULT '{"effective": {"fast_mode": "disabled", "service_tier": null, "reasoning_level": null}, "precedence": {"profile": {"fast_mode": {"kind": "inherit"}, "service_tier": {"kind": "inherit"}, "reasoning_level": {"kind": "inherit"}}, "session": {"fast_mode": {"kind": "inherit"}, "service_tier": {"kind": "inherit"}, "reasoning_level": {"kind": "inherit"}}, "per_call": {"fast_mode": {"kind": "inherit"}, "service_tier": {"kind": "inherit"}, "reasoning_level": {"kind": "inherit"}}, "global_default": {"fast_mode": {"kind": "inherit"}, "service_tier": {"kind": "inherit"}, "reasoning_level": {"kind": "inherit"}}}, "fast_mode_source": null, "reasoning_source": null, "service_tier_source": null, "validated_for_selection_id": null}'::jsonb CONSTRAINT replace_session_defaults_co_replacement_model_settings_not_null NOT NULL,
    caller_model_settings jsonb DEFAULT '{"fast_mode": {"kind": "inherit"}, "service_tier": {"kind": "inherit"}, "reasoning_level": {"kind": "inherit"}}'::jsonb NOT NULL,
    CONSTRAINT replace_session_defaults_caller_model_settings_object CHECK ((jsonb_typeof(caller_model_settings) = 'object'::text)),
    CONSTRAINT replace_session_defaults_command_expected_version_positive_u64 CHECK (((expected_current_version >= (1)::numeric) AND (expected_current_version <= '18446744073709551615'::numeric))),
    CONSTRAINT replace_session_defaults_command_installed_version_positive_u64 CHECK (((result_installed_version IS NULL) OR ((result_installed_version >= (1)::numeric) AND (result_installed_version <= '18446744073709551615'::numeric)))),
    CONSTRAINT replace_session_defaults_command_kind_closed CHECK ((command_kind = 'replace_session_defaults'::text)),
    CONSTRAINT replace_session_defaults_command_model_selection_kind_closed CHECK ((model_selection_kind = ANY (ARRAY['direct'::text, 'alias'::text]))),
    CONSTRAINT replace_session_defaults_command_model_selection_shape CHECK ((((model_selection_kind = 'direct'::text) AND (direct_model_selection_id IS NOT NULL) AND (model_alias_id IS NULL)) OR ((model_selection_kind = 'alias'::text) AND (direct_model_selection_id IS NULL) AND (model_alias_id IS NOT NULL)))),
    CONSTRAINT replace_session_defaults_command_rejection_kind_closed CHECK (((rejection_kind IS NULL) OR (rejection_kind = ANY (ARRAY['session_not_found'::text, 'current_version_mismatch'::text, 'version_exhausted'::text])))),
    CONSTRAINT replace_session_defaults_command_result_current_positive_u64 CHECK (((result_current_version IS NULL) OR ((result_current_version >= (1)::numeric) AND (result_current_version <= '18446744073709551615'::numeric)))),
    CONSTRAINT replace_session_defaults_command_result_expected_positive_u64 CHECK (((result_expected_version IS NULL) OR ((result_expected_version >= (1)::numeric) AND (result_expected_version <= '18446744073709551615'::numeric)))),
    CONSTRAINT replace_session_defaults_command_result_kind_closed CHECK ((result_kind = ANY (ARRAY['applied'::text, 'rejected'::text]))),
    CONSTRAINT replace_session_defaults_command_result_session_matches CHECK ((result_session_id = session_id)),
    CONSTRAINT replace_session_defaults_command_result_shape CHECK ((((result_kind = 'applied'::text) AND (rejection_kind IS NULL) AND (result_installed_version IS NOT NULL) AND (result_expected_version IS NULL) AND (result_current_version IS NULL) AND (result_installed_version = (expected_current_version + (1)::numeric))) OR ((result_kind = 'rejected'::text) AND (rejection_kind = 'session_not_found'::text) AND (result_installed_version IS NULL) AND (result_expected_version IS NULL) AND (result_current_version IS NULL)) OR ((result_kind = 'rejected'::text) AND (rejection_kind = 'current_version_mismatch'::text) AND (result_installed_version IS NULL) AND (result_expected_version = expected_current_version) AND (result_current_version IS NOT NULL) AND (result_current_version <> result_expected_version)) OR ((result_kind = 'rejected'::text) AND (rejection_kind = 'version_exhausted'::text) AND (result_installed_version IS NULL) AND (result_expected_version IS NULL) AND (result_current_version = expected_current_version) AND (result_current_version = '18446744073709551615'::numeric)))),
    CONSTRAINT replace_session_defaults_command_storage_version_supported CHECK ((storage_version = ANY (ARRAY[1, 2, 3, 4]))),
    CONSTRAINT replace_session_defaults_command_system_prompt_bounded CHECK (((system_prompt IS NULL) OR ((octet_length(convert_to(system_prompt, 'UTF8'::name)) >= 1) AND (octet_length(convert_to(system_prompt, 'UTF8'::name)) <= 1048576)))),
    CONSTRAINT replace_session_defaults_command_system_prompt_versioned CHECK (((system_prompt IS NULL) OR (storage_version >= 3))),
    CONSTRAINT replace_session_defaults_command_tool_approval_closed CHECK ((dangerous_tool_auto_approval = ANY (ARRAY['disabled'::text, 'approve_all'::text]))),
    CONSTRAINT replace_session_defaults_replacement_model_settings_object CHECK ((jsonb_typeof(replacement_model_settings) = 'object'::text))
);


--
-- Name: replace_session_metadata_command; Type: TABLE; Schema: public
--

CREATE TABLE replace_session_metadata_command (
    command_id uuid NOT NULL,
    command_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    actor_kind text NOT NULL,
    actor_turn_id uuid,
    actor_tool_request_id uuid,
    replacement_title text,
    replacement_archived boolean NOT NULL,
    result_kind text NOT NULL,
    rejection_kind text,
    result_session_id uuid NOT NULL,
    result_applied_session_id uuid,
    result_updated_at timestamp with time zone,
    result_actor_kind text,
    result_actor_turn_id uuid,
    result_actor_tool_request_id uuid,
    issuer_kind text NOT NULL,
    issuer_tool_request_id uuid,
    CONSTRAINT replace_session_metadata_command_actor_kind_closed CHECK ((actor_kind = ANY (ARRAY['user'::text, 'model'::text, 'recovery'::text, 'tool'::text]))),
    CONSTRAINT replace_session_metadata_command_actor_shape CHECK ((((actor_kind = ANY (ARRAY['user'::text, 'recovery'::text])) AND (actor_turn_id IS NULL) AND (actor_tool_request_id IS NULL)) OR ((actor_kind = 'model'::text) AND (actor_turn_id IS NOT NULL) AND (actor_tool_request_id IS NULL)) OR ((actor_kind = 'tool'::text) AND (actor_turn_id IS NULL) AND (actor_tool_request_id IS NOT NULL)))),
    CONSTRAINT replace_session_metadata_command_issuer_shape CHECK ((((issuer_kind = 'user'::text) AND (issuer_tool_request_id IS NULL)) OR ((issuer_kind = 'tool'::text) AND (issuer_tool_request_id IS NOT NULL)))),
    CONSTRAINT replace_session_metadata_command_kind_closed CHECK ((command_kind = 'replace_session_metadata'::text)),
    CONSTRAINT replace_session_metadata_command_rejection_kind_closed CHECK (((rejection_kind IS NULL) OR (rejection_kind = 'session_not_found'::text))),
    CONSTRAINT replace_session_metadata_command_result_actor_kind_closed CHECK (((result_actor_kind IS NULL) OR (result_actor_kind = ANY (ARRAY['user'::text, 'model'::text, 'recovery'::text, 'tool'::text])))),
    CONSTRAINT replace_session_metadata_command_result_actor_shape CHECK ((((result_actor_kind IS NULL) AND (result_actor_turn_id IS NULL) AND (result_actor_tool_request_id IS NULL)) OR ((result_actor_kind = ANY (ARRAY['user'::text, 'recovery'::text])) AND (result_actor_turn_id IS NULL) AND (result_actor_tool_request_id IS NULL)) OR ((result_actor_kind = 'model'::text) AND (result_actor_turn_id IS NOT NULL) AND (result_actor_tool_request_id IS NULL)) OR ((result_actor_kind = 'tool'::text) AND (result_actor_turn_id IS NULL) AND (result_actor_tool_request_id IS NOT NULL)))),
    CONSTRAINT replace_session_metadata_command_result_kind_closed CHECK ((result_kind = ANY (ARRAY['applied'::text, 'rejected'::text]))),
    CONSTRAINT replace_session_metadata_command_result_session_matches CHECK ((result_session_id = session_id)),
    CONSTRAINT replace_session_metadata_command_result_shape CHECK ((((result_kind = 'applied'::text) AND (rejection_kind IS NULL) AND (result_applied_session_id IS NOT NULL) AND (result_applied_session_id = session_id) AND (result_updated_at IS NOT NULL) AND (result_actor_kind IS NOT NULL) AND (result_actor_kind = actor_kind) AND (NOT (result_actor_turn_id IS DISTINCT FROM actor_turn_id)) AND (NOT (result_actor_tool_request_id IS DISTINCT FROM actor_tool_request_id))) OR ((result_kind = 'rejected'::text) AND (rejection_kind = 'session_not_found'::text) AND (result_applied_session_id IS NULL) AND (result_updated_at IS NULL) AND (result_actor_kind IS NULL) AND (result_actor_turn_id IS NULL) AND (result_actor_tool_request_id IS NULL)))),
    CONSTRAINT replace_session_metadata_command_result_updated_at_finite CHECK (((result_updated_at IS NULL) OR ((result_updated_at > '-infinity'::timestamp with time zone) AND (result_updated_at < 'infinity'::timestamp with time zone)))),
    CONSTRAINT replace_session_metadata_command_storage_version_supported CHECK ((storage_version = 1)),
    CONSTRAINT replace_session_metadata_command_title_nonempty CHECK (((replacement_title IS NULL) OR (octet_length(replacement_title) > 0)))
);


--
-- Name: replace_session_metadata_command_attribute; Type: TABLE; Schema: public
--

CREATE TABLE replace_session_metadata_command_attribute (
    command_id uuid NOT NULL,
    attribute_key text CONSTRAINT replace_session_metadata_command_attribu_attribute_key_not_null NOT NULL,
    attribute_value text CONSTRAINT replace_session_metadata_command_attri_attribute_value_not_null NOT NULL,
    CONSTRAINT replace_session_metadata_command_attribute_key_indexed_bound CHECK ((octet_length(convert_to(attribute_key, 'UTF8'::name)) <= 1024)),
    CONSTRAINT replace_session_metadata_command_attribute_key_nonempty CHECK ((octet_length(attribute_key) > 0))
);


--
-- Name: replace_session_metadata_command_tag; Type: TABLE; Schema: public
--

CREATE TABLE replace_session_metadata_command_tag (
    command_id uuid NOT NULL,
    tag text NOT NULL,
    CONSTRAINT replace_session_metadata_command_tag_indexed_bound CHECK ((octet_length(convert_to(tag, 'UTF8'::name)) <= 1024)),
    CONSTRAINT replace_session_metadata_command_tag_nonempty CHECK ((octet_length(tag) > 0))
);


--
-- Name: session; Type: TABLE; Schema: public
--

CREATE TABLE session (
    session_id uuid NOT NULL,
    creation_cause text NOT NULL,
    ancestry_kind text NOT NULL,
    imported_conversation_id uuid,
    imported_frontier_entry_id uuid,
    imported_frontier_position numeric(20,0),
    imported_relationship_kind text,
    template_name text,
    template_content_digest bytea,
    spawning_tool_request_id uuid,
    CONSTRAINT session_ancestry_kind_closed CHECK ((ancestry_kind = ANY (ARRAY['none'::text, 'imported_conversation'::text]))),
    CONSTRAINT session_ancestry_shape CHECK ((((ancestry_kind = 'none'::text) AND (imported_conversation_id IS NULL) AND (imported_frontier_entry_id IS NULL) AND (imported_frontier_position IS NULL) AND (imported_relationship_kind IS NULL)) OR ((ancestry_kind = 'imported_conversation'::text) AND (imported_conversation_id IS NOT NULL) AND (imported_frontier_entry_id IS NOT NULL) AND (imported_frontier_position IS NOT NULL) AND (imported_relationship_kind IS NOT NULL)))),
    CONSTRAINT session_creation_cause_closed CHECK ((creation_cause = ANY (ARRAY['user_initiated'::text, 'delegated'::text]))),
    CONSTRAINT session_delegated_cause_shape CHECK ((((creation_cause = 'user_initiated'::text) AND (spawning_tool_request_id IS NULL)) OR ((creation_cause = 'delegated'::text) AND (ancestry_kind = 'none'::text) AND (spawning_tool_request_id IS NOT NULL)))),
    CONSTRAINT session_imported_frontier_position_positive_u64 CHECK (((imported_frontier_position IS NULL) OR ((imported_frontier_position >= (1)::numeric) AND (imported_frontier_position <= '18446744073709551615'::numeric)))),
    CONSTRAINT session_imported_relationship_closed CHECK (((imported_relationship_kind IS NULL) OR (imported_relationship_kind = ANY (ARRAY['resume'::text, 'fork'::text])))),
    CONSTRAINT session_template_provenance_shape CHECK ((((template_name IS NULL) AND (template_content_digest IS NULL)) OR ((template_name IS NOT NULL) AND (template_content_digest IS NOT NULL) AND (ancestry_kind = 'none'::text) AND ((octet_length(convert_to(template_name, 'UTF8'::name)) >= 1) AND (octet_length(convert_to(template_name, 'UTF8'::name)) <= 128)) AND (template_name ~ '^[a-z0-9][a-z0-9._-]*$'::text) AND (octet_length(template_content_digest) = 32))))
);


--
-- Name: session_created_outbox_event; Type: TABLE; Schema: public
--

CREATE TABLE session_created_outbox_event (
    event_sequence numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    CONSTRAINT session_created_outbox_event_kind_closed CHECK ((event_kind = 'session_created'::text)),
    CONSTRAINT session_created_outbox_event_storage_version_supported CHECK ((storage_version = 1))
);


--
-- Name: session_current_defaults; Type: TABLE; Schema: public
--

CREATE TABLE session_current_defaults (
    session_id uuid NOT NULL,
    current_version numeric(20,0) NOT NULL,
    CONSTRAINT session_current_defaults_version_positive_u64 CHECK (((current_version >= (1)::numeric) AND (current_version <= '18446744073709551615'::numeric)))
);


--
-- Name: session_current_model_credentials; Type: TABLE; Schema: public
--

CREATE TABLE session_current_model_credentials (
    session_id uuid NOT NULL,
    current_event_ordinal numeric(20,0) CONSTRAINT session_current_model_credential_current_event_ordinal_not_null NOT NULL,
    CONSTRAINT session_current_model_credentials_current_event_ordinal_check CHECK (((current_event_ordinal >= (1)::numeric) AND (current_event_ordinal <= '18446744073709551615'::numeric)))
);


--
-- Name: session_defaults_version; Type: TABLE; Schema: public
--

CREATE TABLE session_defaults_version (
    session_id uuid NOT NULL,
    version numeric(20,0) NOT NULL,
    model_selection_kind text NOT NULL,
    direct_model_selection_id uuid,
    model_alias_id uuid,
    model_selection_reference uuid GENERATED ALWAYS AS (COALESCE(direct_model_selection_id, model_alias_id)) STORED,
    dangerous_tool_auto_approval text DEFAULT 'disabled'::text NOT NULL,
    system_prompt text,
    system_prompt_digest bytea GENERATED ALWAYS AS (session_system_prompt_digest(system_prompt)) STORED NOT NULL,
    model_settings jsonb DEFAULT '{"effective": {"fast_mode": "disabled", "service_tier": null, "reasoning_level": null}, "precedence": {"profile": {"fast_mode": {"kind": "inherit"}, "service_tier": {"kind": "inherit"}, "reasoning_level": {"kind": "inherit"}}, "session": {"fast_mode": {"kind": "inherit"}, "service_tier": {"kind": "inherit"}, "reasoning_level": {"kind": "inherit"}}, "per_call": {"fast_mode": {"kind": "inherit"}, "service_tier": {"kind": "inherit"}, "reasoning_level": {"kind": "inherit"}}, "global_default": {"fast_mode": {"kind": "inherit"}, "service_tier": {"kind": "inherit"}, "reasoning_level": {"kind": "inherit"}}}, "fast_mode_source": null, "reasoning_source": null, "service_tier_source": null, "validated_for_selection_id": null}'::jsonb NOT NULL,
    CONSTRAINT session_defaults_version_model_selection_kind_closed CHECK ((model_selection_kind = ANY (ARRAY['direct'::text, 'alias'::text]))),
    CONSTRAINT session_defaults_version_model_selection_shape CHECK ((((model_selection_kind = 'direct'::text) AND (direct_model_selection_id IS NOT NULL) AND (model_alias_id IS NULL)) OR ((model_selection_kind = 'alias'::text) AND (direct_model_selection_id IS NULL) AND (model_alias_id IS NOT NULL)))),
    CONSTRAINT session_defaults_version_model_settings_object CHECK ((jsonb_typeof(model_settings) = 'object'::text)),
    CONSTRAINT session_defaults_version_positive_u64 CHECK (((version >= (1)::numeric) AND (version <= '18446744073709551615'::numeric))),
    CONSTRAINT session_defaults_version_system_prompt_bounded CHECK (((system_prompt IS NULL) OR ((octet_length(convert_to(system_prompt, 'UTF8'::name)) >= 1) AND (octet_length(convert_to(system_prompt, 'UTF8'::name)) <= 1048576)))),
    CONSTRAINT session_defaults_version_tool_approval_closed CHECK ((dangerous_tool_auto_approval = ANY (ARRAY['disabled'::text, 'approve_all'::text])))
);


--
-- Name: session_metadata; Type: TABLE; Schema: public
--

CREATE TABLE session_metadata (
    session_id uuid NOT NULL,
    source_command_id uuid NOT NULL,
    title text,
    archived boolean NOT NULL,
    updated_at timestamp with time zone NOT NULL,
    actor_kind text NOT NULL,
    actor_turn_id uuid,
    actor_tool_request_id uuid,
    CONSTRAINT session_metadata_actor_kind_closed CHECK ((actor_kind = ANY (ARRAY['user'::text, 'model'::text, 'recovery'::text, 'tool'::text]))),
    CONSTRAINT session_metadata_actor_shape CHECK ((((actor_kind = ANY (ARRAY['user'::text, 'recovery'::text])) AND (actor_turn_id IS NULL) AND (actor_tool_request_id IS NULL)) OR ((actor_kind = 'model'::text) AND (actor_turn_id IS NOT NULL) AND (actor_tool_request_id IS NULL)) OR ((actor_kind = 'tool'::text) AND (actor_turn_id IS NULL) AND (actor_tool_request_id IS NOT NULL)))),
    CONSTRAINT session_metadata_title_nonempty CHECK (((title IS NULL) OR (octet_length(title) > 0))),
    CONSTRAINT session_metadata_updated_at_finite CHECK (((updated_at > '-infinity'::timestamp with time zone) AND (updated_at < 'infinity'::timestamp with time zone)))
);


--
-- Name: session_metadata_attribute; Type: TABLE; Schema: public
--

CREATE TABLE session_metadata_attribute (
    session_id uuid NOT NULL,
    attribute_key text NOT NULL,
    attribute_value text NOT NULL,
    CONSTRAINT session_metadata_attribute_key_indexed_bound CHECK ((octet_length(convert_to(attribute_key, 'UTF8'::name)) <= 1024)),
    CONSTRAINT session_metadata_attribute_key_nonempty CHECK ((octet_length(attribute_key) > 0))
);


--
-- Name: session_metadata_installation; Type: TABLE; Schema: public
--

CREATE TABLE session_metadata_installation (
    session_id uuid NOT NULL,
    source_command_id uuid NOT NULL
);


--
-- Name: session_metadata_tag; Type: TABLE; Schema: public
--

CREATE TABLE session_metadata_tag (
    session_id uuid NOT NULL,
    tag text NOT NULL,
    CONSTRAINT session_metadata_tag_indexed_bound CHECK ((octet_length(convert_to(tag, 'UTF8'::name)) <= 1024)),
    CONSTRAINT session_metadata_tag_nonempty CHECK ((octet_length(tag) > 0))
);


--
-- Name: session_model_credential_entry; Type: TABLE; Schema: public
--

CREATE TABLE session_model_credential_entry (
    session_id uuid NOT NULL,
    event_ordinal numeric(20,0) NOT NULL,
    model_family text NOT NULL,
    credential_reference text NOT NULL,
    CONSTRAINT session_model_credential_entry_credential_reference_check CHECK ((credential_reference <> ''::text)),
    CONSTRAINT session_model_credential_entry_model_family_check CHECK ((model_family <> ''::text))
);


--
-- Name: session_model_credential_record; Type: TABLE; Schema: public
--

CREATE TABLE session_model_credential_record (
    session_id uuid NOT NULL,
    event_ordinal numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    provenance_kind text NOT NULL,
    provenance_command_id uuid,
    recorded_at timestamp with time zone NOT NULL,
    provenance_tool_request_id uuid,
    CONSTRAINT session_model_credential_record_check CHECK ((((event_ordinal = (1)::numeric) AND (event_kind = 'created'::text) AND (((provenance_kind = ANY (ARRAY['create_session'::text, 'imported_session'::text, 'migration_backfill'::text])) AND (provenance_command_id IS NOT NULL) AND (provenance_tool_request_id IS NULL)) OR ((provenance_kind = 'delegated_session'::text) AND (provenance_command_id IS NULL) AND (provenance_tool_request_id IS NOT NULL)))) OR ((event_ordinal > (1)::numeric) AND (event_kind = 'updated'::text) AND (provenance_kind = 'credential_update'::text) AND (provenance_command_id IS NOT NULL) AND (provenance_tool_request_id IS NULL)))),
    CONSTRAINT session_model_credential_record_event_kind_check CHECK ((event_kind = ANY (ARRAY['created'::text, 'updated'::text]))),
    CONSTRAINT session_model_credential_record_event_ordinal_check CHECK (((event_ordinal >= (1)::numeric) AND (event_ordinal <= '18446744073709551615'::numeric))),
    CONSTRAINT session_model_credential_record_provenance_kind_check CHECK ((provenance_kind = ANY (ARRAY['create_session'::text, 'imported_session'::text, 'migration_backfill'::text, 'credential_update'::text, 'delegated_session'::text])))
);


--
-- Name: session_model_settings_changed; Type: TABLE; Schema: public
--

CREATE TABLE session_model_settings_changed (
    session_id uuid NOT NULL,
    command_id uuid NOT NULL,
    prior_defaults_version numeric(20,0) NOT NULL,
    installed_defaults_version numeric(20,0) CONSTRAINT session_model_settings_chan_installed_defaults_version_not_null NOT NULL,
    prior_model_settings jsonb NOT NULL,
    installed_model_settings jsonb CONSTRAINT session_model_settings_change_installed_model_settings_not_null NOT NULL,
    caller_model_settings jsonb NOT NULL,
    adjustments jsonb NOT NULL,
    CONSTRAINT session_model_settings_changed_documents CHECK (((jsonb_typeof(prior_model_settings) = 'object'::text) AND (jsonb_typeof(installed_model_settings) = 'object'::text) AND (jsonb_typeof(caller_model_settings) = 'object'::text) AND (jsonb_typeof(adjustments) = 'array'::text))),
    CONSTRAINT session_model_settings_changed_successor CHECK ((installed_defaults_version = (prior_defaults_version + (1)::numeric)))
);


--
-- Name: session_model_settings_changed_outbox_event; Type: TABLE; Schema: public
--

CREATE TABLE session_model_settings_changed_outbox_event (
    event_sequence numeric(20,0) CONSTRAINT session_model_settings_changed_outbox_e_event_sequence_not_null NOT NULL,
    event_kind text NOT NULL,
    storage_version smallint CONSTRAINT session_model_settings_changed_outbox__storage_version_not_null NOT NULL,
    session_id uuid NOT NULL,
    installed_defaults_version numeric(20,0) CONSTRAINT session_model_settings_cha_installed_defaults_version_not_null1 NOT NULL,
    CONSTRAINT session_model_settings_changed_outbox_kind_closed CHECK ((event_kind = 'session_model_settings_changed'::text)),
    CONSTRAINT session_model_settings_changed_outbox_version_supported CHECK ((storage_version = 1))
);


--
-- Name: session_plan_current_dependency; Type: TABLE; Schema: public
--

CREATE TABLE session_plan_current_dependency (
    session_id uuid NOT NULL,
    entry_ordinal numeric(20,0) NOT NULL,
    dependency_ordinal numeric(20,0) NOT NULL,
    first_event_ordinal numeric(20,0) NOT NULL,
    prior_first_event_ordinal numeric(20,0),
    CONSTRAINT session_plan_current_dependency_check CHECK ((entry_ordinal <> dependency_ordinal)),
    CONSTRAINT session_plan_current_dependency_check1 CHECK ((first_event_ordinal > entry_ordinal)),
    CONSTRAINT session_plan_current_dependency_check2 CHECK ((first_event_ordinal > dependency_ordinal)),
    CONSTRAINT session_plan_current_dependency_check3 CHECK (((prior_first_event_ordinal IS NULL) OR (prior_first_event_ordinal < first_event_ordinal))),
    CONSTRAINT session_plan_current_dependency_dependency_ordinal_check CHECK (((dependency_ordinal >= (1)::numeric) AND (dependency_ordinal <= '18446744073709551615'::numeric))),
    CONSTRAINT session_plan_current_dependency_entry_ordinal_check CHECK (((entry_ordinal >= (1)::numeric) AND (entry_ordinal <= '18446744073709551615'::numeric))),
    CONSTRAINT session_plan_current_dependency_first_event_ordinal_check CHECK (((first_event_ordinal >= (1)::numeric) AND (first_event_ordinal <= '18446744073709551615'::numeric))),
    CONSTRAINT session_plan_current_dependency_prior_first_event_ordinal_check CHECK (((prior_first_event_ordinal >= (1)::numeric) AND (prior_first_event_ordinal <= '18446744073709551615'::numeric)))
);


--
-- Name: session_plan_head; Type: TABLE; Schema: public
--

CREATE TABLE session_plan_head (
    session_id uuid NOT NULL,
    event_ordinal numeric(20,0) NOT NULL,
    dependency_event_ordinal numeric(20,0),
    CONSTRAINT session_plan_head_check CHECK (((dependency_event_ordinal IS NULL) OR (dependency_event_ordinal <= event_ordinal))),
    CONSTRAINT session_plan_head_dependency_event_ordinal_check CHECK (((dependency_event_ordinal >= (1)::numeric) AND (dependency_event_ordinal <= '18446744073709551615'::numeric))),
    CONSTRAINT session_plan_head_event_ordinal_check CHECK (((event_ordinal >= (1)::numeric) AND (event_ordinal <= '18446744073709551615'::numeric)))
);


--
-- Name: session_timeline_fact; Type: TABLE; Schema: public
--

CREATE TABLE session_timeline_fact (
    session_id uuid NOT NULL,
    item_count numeric(20,0) NOT NULL,
    first_sequence numeric(20,0),
    latest_sequence numeric(20,0),
    event_kind_bytes numeric(20,0) NOT NULL,
    projected_text_bytes numeric(20,0) NOT NULL,
    active_turn_count numeric(20,0) NOT NULL,
    queued_turn_count numeric(20,0) NOT NULL,
    attention_activity_recorded_at timestamp with time zone,
    CONSTRAINT session_timeline_fact_active_turn_count_check CHECK (((active_turn_count >= (0)::numeric) AND (active_turn_count <= '18446744073709551615'::numeric))),
    CONSTRAINT session_timeline_fact_check CHECK (((item_count = (0)::numeric) = ((first_sequence IS NULL) AND (latest_sequence IS NULL)))),
    CONSTRAINT session_timeline_fact_check1 CHECK (((first_sequence IS NULL) OR ((first_sequence >= (1)::numeric) AND (first_sequence <= latest_sequence)))),
    CONSTRAINT session_timeline_fact_event_kind_bytes_check CHECK (((event_kind_bytes >= (0)::numeric) AND (event_kind_bytes <= '18446744073709551615'::numeric))),
    CONSTRAINT session_timeline_fact_item_count_check CHECK (((item_count >= (0)::numeric) AND (item_count <= '18446744073709551615'::numeric))),
    CONSTRAINT session_timeline_fact_latest_sequence_check CHECK (((latest_sequence IS NULL) OR (latest_sequence <= '18446744073709551615'::numeric))),
    CONSTRAINT session_timeline_fact_projected_text_bytes_check CHECK (((projected_text_bytes >= (0)::numeric) AND (projected_text_bytes <= '18446744073709551615'::numeric))),
    CONSTRAINT session_timeline_fact_queued_turn_count_check CHECK (((queued_turn_count >= (0)::numeric) AND (queued_turn_count <= '18446744073709551615'::numeric)))
);


--
-- Constraints.
--

--
-- Name: compact_session_command compact_session_command_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY compact_session_command
    ADD CONSTRAINT compact_session_command_pkey PRIMARY KEY (command_id);


--
-- Name: create_session_command create_session_command_created_session_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY create_session_command
    ADD CONSTRAINT create_session_command_created_session_id_key UNIQUE (created_session_id);


--
-- Name: create_session_command create_session_command_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY create_session_command
    ADD CONSTRAINT create_session_command_pkey PRIMARY KEY (command_id);


--
-- Name: create_session_command create_session_command_template_provenance_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY create_session_command
    ADD CONSTRAINT create_session_command_template_provenance_key UNIQUE (created_session_id, template_name, template_content_digest);


--
-- Name: create_session_from_imported_frontier_command create_session_from_imported_frontier_co_created_session_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY create_session_from_imported_frontier_command
    ADD CONSTRAINT create_session_from_imported_frontier_co_created_session_id_key UNIQUE (created_session_id);


--
-- Name: create_session_from_imported_frontier_command create_session_from_imported_frontier_command_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY create_session_from_imported_frontier_command
    ADD CONSTRAINT create_session_from_imported_frontier_command_pkey PRIMARY KEY (command_id);


--
-- Name: replace_session_defaults_command replace_session_defaults_command_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY replace_session_defaults_command
    ADD CONSTRAINT replace_session_defaults_command_pkey PRIMARY KEY (command_id);


--
-- Name: replace_session_defaults_command replace_session_defaults_command_settings_event_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY replace_session_defaults_command
    ADD CONSTRAINT replace_session_defaults_command_settings_event_key UNIQUE (command_id, result_session_id, result_installed_version);


--
-- Name: replace_session_metadata_command replace_session_metadata_command_applied_session_unique; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY replace_session_metadata_command
    ADD CONSTRAINT replace_session_metadata_command_applied_session_unique UNIQUE (command_id, result_applied_session_id);


--
-- Name: replace_session_metadata_command_attribute replace_session_metadata_command_attribute_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY replace_session_metadata_command_attribute
    ADD CONSTRAINT replace_session_metadata_command_attribute_pk PRIMARY KEY (command_id, attribute_key);


--
-- Name: replace_session_metadata_command replace_session_metadata_command_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY replace_session_metadata_command
    ADD CONSTRAINT replace_session_metadata_command_pkey PRIMARY KEY (command_id);


--
-- Name: replace_session_metadata_command_tag replace_session_metadata_command_tag_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY replace_session_metadata_command_tag
    ADD CONSTRAINT replace_session_metadata_command_tag_pk PRIMARY KEY (command_id, tag);


--
-- Name: session_created_outbox_event session_created_outbox_event_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_created_outbox_event
    ADD CONSTRAINT session_created_outbox_event_pkey PRIMARY KEY (event_sequence);


--
-- Name: session_created_outbox_event session_created_outbox_event_session_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_created_outbox_event
    ADD CONSTRAINT session_created_outbox_event_session_id_key UNIQUE (session_id);


--
-- Name: session_current_defaults session_current_defaults_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_current_defaults
    ADD CONSTRAINT session_current_defaults_pkey PRIMARY KEY (session_id);


--
-- Name: session_current_model_credentials session_current_model_credentials_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_current_model_credentials
    ADD CONSTRAINT session_current_model_credentials_pkey PRIMARY KEY (session_id);


--
-- Name: session_defaults_version session_defaults_version_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_defaults_version
    ADD CONSTRAINT session_defaults_version_pk PRIMARY KEY (session_id, version);


--
-- Name: session_defaults_version session_defaults_version_selection_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_defaults_version
    ADD CONSTRAINT session_defaults_version_selection_key UNIQUE (session_id, version, model_selection_kind, model_selection_reference, dangerous_tool_auto_approval, system_prompt_digest);


--
-- Name: session session_delegated_provenance_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session
    ADD CONSTRAINT session_delegated_provenance_key UNIQUE (spawning_tool_request_id, session_id);


--
-- Name: session session_imported_provenance_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session
    ADD CONSTRAINT session_imported_provenance_key UNIQUE (session_id, creation_cause, ancestry_kind, imported_conversation_id, imported_frontier_entry_id, imported_frontier_position, imported_relationship_kind);


--
-- Name: session_metadata_attribute session_metadata_attribute_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_metadata_attribute
    ADD CONSTRAINT session_metadata_attribute_pk PRIMARY KEY (session_id, attribute_key);


--
-- Name: session_metadata_installation session_metadata_installation_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_metadata_installation
    ADD CONSTRAINT session_metadata_installation_pk PRIMARY KEY (session_id, source_command_id);


--
-- Name: session_metadata session_metadata_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_metadata
    ADD CONSTRAINT session_metadata_pkey PRIMARY KEY (session_id);


--
-- Name: session_metadata_tag session_metadata_tag_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_metadata_tag
    ADD CONSTRAINT session_metadata_tag_pk PRIMARY KEY (session_id, tag);


--
-- Name: session_model_credential_entry session_model_credential_entry_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_model_credential_entry
    ADD CONSTRAINT session_model_credential_entry_pkey PRIMARY KEY (session_id, event_ordinal, model_family);


--
-- Name: session_model_credential_record session_model_credential_record_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_model_credential_record
    ADD CONSTRAINT session_model_credential_record_pkey PRIMARY KEY (session_id, event_ordinal);


--
-- Name: session_model_settings_changed session_model_settings_changed_command_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_model_settings_changed
    ADD CONSTRAINT session_model_settings_changed_command_id_key UNIQUE (command_id);


--
-- Name: session_model_settings_changed_outbox_event session_model_settings_changed_outbox_event_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_model_settings_changed_outbox_event
    ADD CONSTRAINT session_model_settings_changed_outbox_event_pkey PRIMARY KEY (event_sequence);


--
-- Name: session_model_settings_changed_outbox_event session_model_settings_changed_outbox_source_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_model_settings_changed_outbox_event
    ADD CONSTRAINT session_model_settings_changed_outbox_source_key UNIQUE (session_id, installed_defaults_version);


--
-- Name: session_model_settings_changed session_model_settings_changed_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_model_settings_changed
    ADD CONSTRAINT session_model_settings_changed_pk PRIMARY KEY (session_id, installed_defaults_version);


--
-- Name: session session_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session
    ADD CONSTRAINT session_pkey PRIMARY KEY (session_id);


--
-- Name: session_plan_current_dependency session_plan_current_dependen_session_id_first_event_ordina_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_plan_current_dependency
    ADD CONSTRAINT session_plan_current_dependen_session_id_first_event_ordina_key UNIQUE (session_id, first_event_ordinal);


--
-- Name: session_plan_current_dependency session_plan_current_dependency_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_plan_current_dependency
    ADD CONSTRAINT session_plan_current_dependency_pkey PRIMARY KEY (session_id, entry_ordinal, dependency_ordinal);


--
-- Name: session_plan_event session_plan_event_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_plan_event
    ADD CONSTRAINT session_plan_event_pkey PRIMARY KEY (session_id, event_ordinal);


--
-- Name: session_plan_event session_plan_event_provenance_attempt_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_plan_event
    ADD CONSTRAINT session_plan_event_provenance_attempt_id_key UNIQUE (provenance_attempt_id);


--
-- Name: session_plan_head session_plan_head_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_plan_head
    ADD CONSTRAINT session_plan_head_pkey PRIMARY KEY (session_id);


--
-- Name: session session_provenance_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session
    ADD CONSTRAINT session_provenance_key UNIQUE (session_id, creation_cause, ancestry_kind);


--
-- Name: session session_template_provenance_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session
    ADD CONSTRAINT session_template_provenance_key UNIQUE (session_id, template_name, template_content_digest);


--
-- Name: session_timeline_fact session_timeline_fact_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_timeline_fact
    ADD CONSTRAINT session_timeline_fact_pkey PRIMARY KEY (session_id);


--
-- Indexes.
--

--
-- Name: compact_session_command_automatic_turn_once; Type: INDEX; Schema: public
--

CREATE UNIQUE INDEX compact_session_command_automatic_turn_once ON compact_session_command USING btree (session_id, automatic_for_turn_id) WHERE (automatic_for_turn_id IS NOT NULL);


--
-- Name: session_metadata_tag_lookup; Type: INDEX; Schema: public
--

CREATE INDEX session_metadata_tag_lookup ON session_metadata_tag USING btree (tag, session_id);


--
-- Name: session_plan_current_dependency_first_append; Type: INDEX; Schema: public
--

CREATE INDEX session_plan_current_dependency_first_append ON session_plan_current_dependency USING btree (session_id, entry_ordinal, first_event_ordinal) INCLUDE (dependency_ordinal);


--
-- Name: session_plan_event_created_page; Type: INDEX; Schema: public
--

CREATE INDEX session_plan_event_created_page ON session_plan_event USING btree (session_id, event_ordinal) WHERE (event_kind = 'created'::text);


--
-- Name: session_plan_event_dependencies; Type: INDEX; Schema: public
--

CREATE INDEX session_plan_event_dependencies ON session_plan_event USING btree (session_id, entry_ordinal, event_ordinal) INCLUDE (dependency_ordinal) WHERE (event_kind = 'depends_on'::text);


--
-- Name: session_plan_event_entry_history; Type: INDEX; Schema: public
--

CREATE INDEX session_plan_event_entry_history ON session_plan_event USING btree (session_id, entry_ordinal, event_ordinal);


--
-- Name: session_plan_event_latest_status_change; Type: INDEX; Schema: public
--

CREATE INDEX session_plan_event_latest_status_change ON session_plan_event USING btree (session_id, entry_ordinal, event_ordinal DESC) INCLUDE (entry_text, entry_status) WHERE (event_kind = 'status_changed'::text);


--
-- Name: session_plan_event_latest_text_revision; Type: INDEX; Schema: public
--

CREATE INDEX session_plan_event_latest_text_revision ON session_plan_event USING btree (session_id, entry_ordinal, event_ordinal DESC) INCLUDE (entry_text, entry_status) WHERE (event_kind = 'text_revised'::text);


--
-- Name: session_plan_event_unsupported_kind; Type: INDEX; Schema: public
--

CREATE INDEX session_plan_event_unsupported_kind ON session_plan_event USING btree (session_id, event_kind) WHERE ((event_kind IS NULL) OR (event_kind <> ALL (ARRAY['created'::text, 'text_revised'::text, 'status_changed'::text, 'depends_on'::text])));


--
-- Name: session_timeline_fact_by_attention_activity; Type: INDEX; Schema: public
--

CREATE INDEX session_timeline_fact_by_attention_activity ON session_timeline_fact USING btree (attention_activity_recorded_at DESC, session_id);


--
-- Triggers.
--

--
-- Name: compact_session_command compact_session_command_changes_are_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER compact_session_command_changes_are_guarded BEFORE INSERT OR DELETE OR UPDATE ON compact_session_command FOR EACH ROW EXECUTE FUNCTION reject_compact_session_command_invalid_change();


--
-- Name: create_session_command create_session_command_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER create_session_command_is_append_only BEFORE DELETE OR UPDATE ON create_session_command FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: create_session_from_imported_frontier_command create_session_from_imported_frontier_command_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER create_session_from_imported_frontier_command_is_append_only BEFORE DELETE OR UPDATE ON create_session_from_imported_frontier_command FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: outbox_event outbox_event_updates_timeline_fact; Type: TRIGGER; Schema: public
--

CREATE TRIGGER outbox_event_updates_timeline_fact AFTER INSERT ON outbox_event FOR EACH ROW EXECUTE FUNCTION append_session_timeline_event_fact();


--
-- Name: replace_session_defaults_command replace_session_defaults_command_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER replace_session_defaults_command_is_append_only BEFORE DELETE OR UPDATE ON replace_session_defaults_command FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: replace_session_metadata_command_attribute replace_session_metadata_command_attribute_insert_before_seal; Type: TRIGGER; Schema: public
--

CREATE TRIGGER replace_session_metadata_command_attribute_insert_before_seal BEFORE INSERT ON replace_session_metadata_command_attribute FOR EACH ROW EXECUTE FUNCTION reject_sealed_session_metadata_receipt_satellite_insert();


--
-- Name: replace_session_metadata_command_attribute replace_session_metadata_command_attribute_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER replace_session_metadata_command_attribute_is_append_only BEFORE DELETE OR UPDATE ON replace_session_metadata_command_attribute FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: replace_session_metadata_command_attribute replace_session_metadata_command_attribute_truncate_is_rejected; Type: TRIGGER; Schema: public
--

CREATE TRIGGER replace_session_metadata_command_attribute_truncate_is_rejected BEFORE TRUNCATE ON replace_session_metadata_command_attribute FOR EACH STATEMENT EXECUTE FUNCTION reject_session_metadata_table_truncate();


--
-- Name: replace_session_metadata_command replace_session_metadata_command_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER replace_session_metadata_command_is_append_only BEFORE DELETE OR UPDATE ON replace_session_metadata_command FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: replace_session_metadata_command replace_session_metadata_command_records_installation; Type: TRIGGER; Schema: public
--

CREATE TRIGGER replace_session_metadata_command_records_installation AFTER INSERT ON replace_session_metadata_command FOR EACH ROW EXECUTE FUNCTION record_session_metadata_installation();


--
-- Name: replace_session_metadata_command_tag replace_session_metadata_command_tag_insert_before_seal; Type: TRIGGER; Schema: public
--

CREATE TRIGGER replace_session_metadata_command_tag_insert_before_seal BEFORE INSERT ON replace_session_metadata_command_tag FOR EACH ROW EXECUTE FUNCTION reject_sealed_session_metadata_receipt_satellite_insert();


--
-- Name: replace_session_metadata_command_tag replace_session_metadata_command_tag_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER replace_session_metadata_command_tag_is_append_only BEFORE DELETE OR UPDATE ON replace_session_metadata_command_tag FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: replace_session_metadata_command_tag replace_session_metadata_command_tag_truncate_is_rejected; Type: TRIGGER; Schema: public
--

CREATE TRIGGER replace_session_metadata_command_tag_truncate_is_rejected BEFORE TRUNCATE ON replace_session_metadata_command_tag FOR EACH STATEMENT EXECUTE FUNCTION reject_session_metadata_table_truncate();


--
-- Name: replace_session_metadata_command replace_session_metadata_command_truncate_is_rejected; Type: TRIGGER; Schema: public
--

CREATE TRIGGER replace_session_metadata_command_truncate_is_rejected BEFORE TRUNCATE ON replace_session_metadata_command FOR EACH STATEMENT EXECUTE FUNCTION reject_session_metadata_table_truncate();


--
-- Name: session_created_outbox_event session_created_outbox_event_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_created_outbox_event_cannot_be_truncated BEFORE TRUNCATE ON session_created_outbox_event FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();


--
-- Name: session_created_outbox_event session_created_outbox_event_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_created_outbox_event_is_append_only BEFORE DELETE OR UPDATE ON session_created_outbox_event FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: session_defaults_version session_defaults_version_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_defaults_version_is_append_only BEFORE DELETE OR UPDATE ON session_defaults_version FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: session session_initializes_timeline_fact; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_initializes_timeline_fact AFTER INSERT ON session FOR EACH ROW EXECUTE FUNCTION initialize_session_timeline_fact();


--
-- Name: session session_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_is_append_only BEFORE DELETE OR UPDATE ON session FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: session_metadata_attribute session_metadata_attribute_matches_receipt; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_metadata_attribute_matches_receipt AFTER INSERT OR DELETE OR UPDATE ON session_metadata_attribute DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_session_metadata_matches_receipt();


--
-- Name: session_metadata_attribute session_metadata_attribute_truncate_is_rejected; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_metadata_attribute_truncate_is_rejected BEFORE TRUNCATE ON session_metadata_attribute FOR EACH STATEMENT EXECUTE FUNCTION reject_session_metadata_table_truncate();


--
-- Name: session_metadata_attribute session_metadata_attribute_update_is_rejected; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_metadata_attribute_update_is_rejected BEFORE UPDATE ON session_metadata_attribute FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: session_metadata session_metadata_delete_is_rejected; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_metadata_delete_is_rejected BEFORE DELETE ON session_metadata FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: session_metadata session_metadata_identity_is_immutable; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_metadata_identity_is_immutable BEFORE UPDATE ON session_metadata FOR EACH ROW EXECUTE FUNCTION reject_session_metadata_identity_change();


--
-- Name: session_metadata_installation session_metadata_installation_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_metadata_installation_is_append_only BEFORE DELETE OR UPDATE ON session_metadata_installation FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: session_metadata_installation session_metadata_installation_matches_receipt; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_metadata_installation_matches_receipt BEFORE INSERT ON session_metadata_installation FOR EACH ROW EXECUTE FUNCTION require_session_metadata_matches_receipt();


--
-- Name: session_metadata_installation session_metadata_installation_projects_web_search; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_metadata_installation_projects_web_search AFTER INSERT ON session_metadata_installation DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION project_web_search_session_metadata();


--
-- Name: session_metadata_installation session_metadata_installation_requires_current; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_metadata_installation_requires_current BEFORE INSERT ON session_metadata_installation FOR EACH ROW EXECUTE FUNCTION require_session_metadata_installation_is_current();


--
-- Name: session_metadata_installation session_metadata_installation_truncate_is_rejected; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_metadata_installation_truncate_is_rejected BEFORE TRUNCATE ON session_metadata_installation FOR EACH STATEMENT EXECUTE FUNCTION reject_session_metadata_table_truncate();


--
-- Name: session_metadata session_metadata_matches_receipt; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_metadata_matches_receipt AFTER INSERT OR UPDATE ON session_metadata DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_session_metadata_matches_receipt();


--
-- Name: session_metadata session_metadata_receipt_reinstallation_is_rejected; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_metadata_receipt_reinstallation_is_rejected BEFORE UPDATE OF source_command_id ON session_metadata FOR EACH ROW EXECUTE FUNCTION reject_session_metadata_receipt_reinstallation();


--
-- Name: session_metadata_tag session_metadata_tag_matches_receipt; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_metadata_tag_matches_receipt AFTER INSERT OR DELETE OR UPDATE ON session_metadata_tag DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_session_metadata_matches_receipt();


--
-- Name: session_metadata_tag session_metadata_tag_truncate_is_rejected; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_metadata_tag_truncate_is_rejected BEFORE TRUNCATE ON session_metadata_tag FOR EACH STATEMENT EXECUTE FUNCTION reject_session_metadata_table_truncate();


--
-- Name: session_metadata_tag session_metadata_tag_update_is_rejected; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_metadata_tag_update_is_rejected BEFORE UPDATE ON session_metadata_tag FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: session_metadata session_metadata_truncate_is_rejected; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_metadata_truncate_is_rejected BEFORE TRUNCATE ON session_metadata FOR EACH STATEMENT EXECUTE FUNCTION reject_session_metadata_table_truncate();


--
-- Name: session_model_credential_entry session_model_credential_entry_immutable; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_model_credential_entry_immutable BEFORE DELETE OR UPDATE ON session_model_credential_entry FOR EACH ROW EXECUTE FUNCTION reject_session_model_credential_rewrite();


--
-- Name: session_model_credential_entry session_model_credential_entry_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_model_credential_entry_rejects_truncate BEFORE TRUNCATE ON session_model_credential_entry FOR EACH STATEMENT EXECUTE FUNCTION reject_session_model_credential_rewrite();


--
-- Name: session_model_credential_entry session_model_credential_entry_sealed_after_publication; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_model_credential_entry_sealed_after_publication BEFORE INSERT ON session_model_credential_entry FOR EACH ROW EXECUTE FUNCTION reject_session_model_credential_entry_after_publication();


--
-- Name: session_current_model_credentials session_model_credential_head_guard; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_model_credential_head_guard BEFORE INSERT OR DELETE OR UPDATE ON session_current_model_credentials FOR EACH ROW EXECUTE FUNCTION guard_session_model_credential_head();


--
-- Name: session_current_model_credentials session_model_credential_head_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_model_credential_head_rejects_truncate BEFORE TRUNCATE ON session_current_model_credentials FOR EACH STATEMENT EXECUTE FUNCTION reject_session_model_credential_rewrite();


--
-- Name: session_model_credential_record session_model_credential_record_append_guard; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_model_credential_record_append_guard BEFORE INSERT ON session_model_credential_record FOR EACH ROW EXECUTE FUNCTION guard_session_model_credential_record_append();


--
-- Name: session_model_credential_record session_model_credential_record_immutable; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_model_credential_record_immutable BEFORE DELETE OR UPDATE ON session_model_credential_record FOR EACH ROW EXECUTE FUNCTION reject_session_model_credential_rewrite();


--
-- Name: session_model_credential_record session_model_credential_record_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_model_credential_record_rejects_truncate BEFORE TRUNCATE ON session_model_credential_record FOR EACH STATEMENT EXECUTE FUNCTION reject_session_model_credential_rewrite();


--
-- Name: session_model_settings_changed session_model_settings_changed_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_model_settings_changed_is_append_only BEFORE DELETE OR UPDATE ON session_model_settings_changed FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: session_model_settings_changed_outbox_event session_model_settings_changed_outbox_event_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_model_settings_changed_outbox_event_cannot_be_truncated BEFORE TRUNCATE ON session_model_settings_changed_outbox_event FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();


--
-- Name: session_model_settings_changed_outbox_event session_model_settings_changed_outbox_event_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_model_settings_changed_outbox_event_is_append_only BEFORE DELETE OR UPDATE ON session_model_settings_changed_outbox_event FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: session_plan_current_dependency session_plan_current_dependency_immutable; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_plan_current_dependency_immutable BEFORE INSERT OR DELETE OR UPDATE ON session_plan_current_dependency FOR EACH ROW EXECUTE FUNCTION reject_session_plan_current_dependency_rewrite();


--
-- Name: session_plan_current_dependency session_plan_current_dependency_predecessor_guard; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_plan_current_dependency_predecessor_guard BEFORE INSERT OR UPDATE ON session_plan_current_dependency FOR EACH ROW EXECUTE FUNCTION guard_session_plan_dependency_predecessor();


--
-- Name: session_plan_current_dependency session_plan_current_dependency_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_plan_current_dependency_rejects_truncate BEFORE TRUNCATE ON session_plan_current_dependency FOR EACH STATEMENT EXECUTE FUNCTION reject_session_plan_current_dependency_rewrite();


--
-- Name: session_plan_event session_plan_event_advances_projection; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_plan_event_advances_projection AFTER INSERT ON session_plan_event FOR EACH ROW EXECUTE FUNCTION advance_session_plan_projection();


--
-- Name: session_plan_event session_plan_event_append_guard; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_plan_event_append_guard BEFORE INSERT ON session_plan_event FOR EACH ROW EXECUTE FUNCTION guard_session_plan_event_append();


--
-- Name: session_plan_event session_plan_event_immutable; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_plan_event_immutable BEFORE DELETE OR UPDATE ON session_plan_event FOR EACH ROW EXECUTE FUNCTION reject_session_plan_event_rewrite();


--
-- Name: session_plan_event session_plan_event_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_plan_event_rejects_truncate BEFORE TRUNCATE ON session_plan_event FOR EACH STATEMENT EXECUTE FUNCTION reject_session_plan_event_rewrite();


--
-- Name: session_plan_head session_plan_head_immutable_identity; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_plan_head_immutable_identity BEFORE DELETE ON session_plan_head FOR EACH ROW EXECUTE FUNCTION reject_session_plan_head_rewrite();


--
-- Name: session_plan_head session_plan_head_maintenance_guard; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_plan_head_maintenance_guard BEFORE INSERT OR UPDATE ON session_plan_head FOR EACH ROW EXECUTE FUNCTION guard_session_plan_head_maintenance();


--
-- Name: session_plan_head session_plan_head_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_plan_head_rejects_truncate BEFORE TRUNCATE ON session_plan_head FOR EACH STATEMENT EXECUTE FUNCTION reject_session_plan_head_rewrite();


--
-- Name: session session_requires_creation_command; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_requires_creation_command AFTER INSERT ON session DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_session_creation_command();


--
-- Foreign keys.
--

--
-- Name: compact_session_command compact_session_command_durable_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY compact_session_command
    ADD CONSTRAINT compact_session_command_durable_fk FOREIGN KEY (command_id, command_kind, storage_version) REFERENCES durable_command(command_id, command_kind, storage_version) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: compact_session_command compact_session_command_session_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY compact_session_command
    ADD CONSTRAINT compact_session_command_session_fk FOREIGN KEY (session_id) REFERENCES session(session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: create_session_command create_session_command_initial_defaults_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY create_session_command
    ADD CONSTRAINT create_session_command_initial_defaults_fk FOREIGN KEY (created_session_id, initial_defaults_version, model_selection_kind, model_selection_reference, dangerous_tool_auto_approval, system_prompt_digest) REFERENCES session_defaults_version(session_id, version, model_selection_kind, model_selection_reference, dangerous_tool_auto_approval, system_prompt_digest) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: create_session_command create_session_command_provenance_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY create_session_command
    ADD CONSTRAINT create_session_command_provenance_fk FOREIGN KEY (created_session_id, creation_cause, ancestry_kind) REFERENCES session(session_id, creation_cause, ancestry_kind) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: create_session_command create_session_command_registry_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY create_session_command
    ADD CONSTRAINT create_session_command_registry_fk FOREIGN KEY (command_id, command_kind, storage_version) REFERENCES durable_command(command_id, command_kind, storage_version) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: create_session_command create_session_command_template_provenance_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY create_session_command
    ADD CONSTRAINT create_session_command_template_provenance_fk FOREIGN KEY (created_session_id, template_name, template_content_digest) REFERENCES session(session_id, template_name, template_content_digest) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: create_session_from_imported_frontier_command create_session_from_imported_frontier_command_defaults_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY create_session_from_imported_frontier_command
    ADD CONSTRAINT create_session_from_imported_frontier_command_defaults_fk FOREIGN KEY (created_session_id, initial_defaults_version, model_selection_kind, model_selection_reference, dangerous_tool_auto_approval, system_prompt_digest) REFERENCES session_defaults_version(session_id, version, model_selection_kind, model_selection_reference, dangerous_tool_auto_approval, system_prompt_digest) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: create_session_from_imported_frontier_command create_session_from_imported_frontier_command_provenance_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY create_session_from_imported_frontier_command
    ADD CONSTRAINT create_session_from_imported_frontier_command_provenance_fk FOREIGN KEY (created_session_id, creation_cause, ancestry_kind, imported_conversation_id, imported_frontier_entry_id, imported_frontier_position, imported_relationship_kind) REFERENCES session(session_id, creation_cause, ancestry_kind, imported_conversation_id, imported_frontier_entry_id, imported_frontier_position, imported_relationship_kind) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: create_session_from_imported_frontier_command create_session_from_imported_frontier_command_registry_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY create_session_from_imported_frontier_command
    ADD CONSTRAINT create_session_from_imported_frontier_command_registry_fk FOREIGN KEY (command_id, command_kind, storage_version) REFERENCES durable_command(command_id, command_kind, storage_version) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: replace_session_defaults_command replace_session_defaults_command_applied_defaults_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY replace_session_defaults_command
    ADD CONSTRAINT replace_session_defaults_command_applied_defaults_fk FOREIGN KEY (result_session_id, result_installed_version, model_selection_kind, model_selection_reference, dangerous_tool_auto_approval, system_prompt_digest) REFERENCES session_defaults_version(session_id, version, model_selection_kind, model_selection_reference, dangerous_tool_auto_approval, system_prompt_digest) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: replace_session_defaults_command replace_session_defaults_command_registry_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY replace_session_defaults_command
    ADD CONSTRAINT replace_session_defaults_command_registry_fk FOREIGN KEY (command_id, command_kind, storage_version) REFERENCES durable_command(command_id, command_kind, storage_version) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: replace_session_metadata_command replace_session_metadata_command_applied_session_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY replace_session_metadata_command
    ADD CONSTRAINT replace_session_metadata_command_applied_session_fk FOREIGN KEY (result_applied_session_id) REFERENCES session_metadata(session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: replace_session_metadata_command_attribute replace_session_metadata_command_attribute_command_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY replace_session_metadata_command_attribute
    ADD CONSTRAINT replace_session_metadata_command_attribute_command_fk FOREIGN KEY (command_id) REFERENCES replace_session_metadata_command(command_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: replace_session_metadata_command replace_session_metadata_command_installation_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY replace_session_metadata_command
    ADD CONSTRAINT replace_session_metadata_command_installation_fk FOREIGN KEY (result_applied_session_id, command_id) REFERENCES session_metadata_installation(session_id, source_command_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: replace_session_metadata_command replace_session_metadata_command_registry_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY replace_session_metadata_command
    ADD CONSTRAINT replace_session_metadata_command_registry_fk FOREIGN KEY (command_id, command_kind, storage_version) REFERENCES durable_command(command_id, command_kind, storage_version) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: replace_session_metadata_command_tag replace_session_metadata_command_tag_command_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY replace_session_metadata_command_tag
    ADD CONSTRAINT replace_session_metadata_command_tag_command_fk FOREIGN KEY (command_id) REFERENCES replace_session_metadata_command(command_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_created_outbox_event session_created_outbox_event_header_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_created_outbox_event
    ADD CONSTRAINT session_created_outbox_event_header_fk FOREIGN KEY (event_sequence, event_kind, storage_version, session_id) REFERENCES outbox_event(event_sequence, event_kind, storage_version, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session session_current_defaults_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session
    ADD CONSTRAINT session_current_defaults_fk FOREIGN KEY (session_id) REFERENCES session_current_defaults(session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_current_defaults session_current_defaults_version_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_current_defaults
    ADD CONSTRAINT session_current_defaults_version_fk FOREIGN KEY (session_id, current_version) REFERENCES session_defaults_version(session_id, version) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_current_model_credentials session_current_model_credent_session_id_current_event_ord_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_current_model_credentials
    ADD CONSTRAINT session_current_model_credent_session_id_current_event_ord_fkey FOREIGN KEY (session_id, current_event_ordinal) REFERENCES session_model_credential_record(session_id, event_ordinal) ON DELETE RESTRICT;


--
-- Name: session_current_model_credentials session_current_model_credentials_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_current_model_credentials
    ADD CONSTRAINT session_current_model_credentials_session_id_fkey FOREIGN KEY (session_id) REFERENCES session(session_id) ON DELETE RESTRICT;


--
-- Name: session_defaults_version session_defaults_version_session_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_defaults_version
    ADD CONSTRAINT session_defaults_version_session_fk FOREIGN KEY (session_id) REFERENCES session(session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: session_metadata_attribute session_metadata_attribute_session_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_metadata_attribute
    ADD CONSTRAINT session_metadata_attribute_session_fk FOREIGN KEY (session_id) REFERENCES session_metadata(session_id) ON UPDATE RESTRICT ON DELETE CASCADE;


--
-- Name: session_metadata session_metadata_current_installation_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_metadata
    ADD CONSTRAINT session_metadata_current_installation_fk FOREIGN KEY (session_id, source_command_id) REFERENCES session_metadata_installation(session_id, source_command_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_metadata_installation session_metadata_installation_receipt_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_metadata_installation
    ADD CONSTRAINT session_metadata_installation_receipt_fk FOREIGN KEY (source_command_id, session_id) REFERENCES replace_session_metadata_command(command_id, result_applied_session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_metadata session_metadata_session_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_metadata
    ADD CONSTRAINT session_metadata_session_fk FOREIGN KEY (session_id) REFERENCES session(session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: session_metadata session_metadata_source_command_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_metadata
    ADD CONSTRAINT session_metadata_source_command_fk FOREIGN KEY (source_command_id, session_id) REFERENCES replace_session_metadata_command(command_id, result_applied_session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_metadata_tag session_metadata_tag_session_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_metadata_tag
    ADD CONSTRAINT session_metadata_tag_session_fk FOREIGN KEY (session_id) REFERENCES session_metadata(session_id) ON UPDATE RESTRICT ON DELETE CASCADE;


--
-- Name: session_model_credential_entry session_model_credential_entry_session_id_event_ordinal_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_model_credential_entry
    ADD CONSTRAINT session_model_credential_entry_session_id_event_ordinal_fkey FOREIGN KEY (session_id, event_ordinal) REFERENCES session_model_credential_record(session_id, event_ordinal) ON DELETE RESTRICT;


--
-- Name: session_model_credential_record session_model_credential_record_provenance_command_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_model_credential_record
    ADD CONSTRAINT session_model_credential_record_provenance_command_id_fkey FOREIGN KEY (provenance_command_id) REFERENCES durable_command(command_id) ON DELETE RESTRICT;


--
-- Name: session_model_credential_record session_model_credential_record_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_model_credential_record
    ADD CONSTRAINT session_model_credential_record_session_id_fkey FOREIGN KEY (session_id) REFERENCES session(session_id) ON DELETE RESTRICT;


--
-- Name: session_model_settings_changed session_model_settings_changed_command_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_model_settings_changed
    ADD CONSTRAINT session_model_settings_changed_command_fk FOREIGN KEY (command_id, session_id, installed_defaults_version) REFERENCES replace_session_defaults_command(command_id, result_session_id, result_installed_version) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_model_settings_changed session_model_settings_changed_installed_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_model_settings_changed
    ADD CONSTRAINT session_model_settings_changed_installed_fk FOREIGN KEY (session_id, installed_defaults_version) REFERENCES session_defaults_version(session_id, version) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: session_model_settings_changed_outbox_event session_model_settings_changed_outbox_event_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_model_settings_changed_outbox_event
    ADD CONSTRAINT session_model_settings_changed_outbox_event_fk FOREIGN KEY (session_id, installed_defaults_version) REFERENCES session_model_settings_changed(session_id, installed_defaults_version) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_model_settings_changed_outbox_event session_model_settings_changed_outbox_header_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_model_settings_changed_outbox_event
    ADD CONSTRAINT session_model_settings_changed_outbox_header_fk FOREIGN KEY (event_sequence, event_kind, storage_version, session_id) REFERENCES outbox_event(event_sequence, event_kind, storage_version, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_model_settings_changed session_model_settings_changed_prior_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_model_settings_changed
    ADD CONSTRAINT session_model_settings_changed_prior_fk FOREIGN KEY (session_id, prior_defaults_version) REFERENCES session_defaults_version(session_id, version) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: session_plan_current_dependency session_plan_current_dependen_session_id_dependency_ordina_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_plan_current_dependency
    ADD CONSTRAINT session_plan_current_dependen_session_id_dependency_ordina_fkey FOREIGN KEY (session_id, dependency_ordinal) REFERENCES session_plan_event(session_id, event_ordinal) ON DELETE RESTRICT;


--
-- Name: session_plan_current_dependency session_plan_current_dependen_session_id_first_event_ordin_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_plan_current_dependency
    ADD CONSTRAINT session_plan_current_dependen_session_id_first_event_ordin_fkey FOREIGN KEY (session_id, first_event_ordinal) REFERENCES session_plan_event(session_id, event_ordinal) ON DELETE RESTRICT;


--
-- Name: session_plan_current_dependency session_plan_current_dependen_session_id_prior_first_event_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_plan_current_dependency
    ADD CONSTRAINT session_plan_current_dependen_session_id_prior_first_event_fkey FOREIGN KEY (session_id, prior_first_event_ordinal) REFERENCES session_plan_current_dependency(session_id, first_event_ordinal) ON DELETE RESTRICT;


--
-- Name: session_plan_current_dependency session_plan_current_dependency_session_id_entry_ordinal_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_plan_current_dependency
    ADD CONSTRAINT session_plan_current_dependency_session_id_entry_ordinal_fkey FOREIGN KEY (session_id, entry_ordinal) REFERENCES session_plan_event(session_id, event_ordinal) ON DELETE RESTRICT;


--
-- Name: session_plan_event session_plan_event_session_id_dependency_ordinal_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_plan_event
    ADD CONSTRAINT session_plan_event_session_id_dependency_ordinal_fkey FOREIGN KEY (session_id, dependency_ordinal) REFERENCES session_plan_event(session_id, event_ordinal) ON DELETE RESTRICT;


--
-- Name: session_plan_event session_plan_event_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_plan_event
    ADD CONSTRAINT session_plan_event_session_id_fkey FOREIGN KEY (session_id) REFERENCES session(session_id) ON DELETE RESTRICT;


--
-- Name: session_plan_event session_plan_event_session_id_prior_event_ordinal_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_plan_event
    ADD CONSTRAINT session_plan_event_session_id_prior_event_ordinal_fkey FOREIGN KEY (session_id, prior_event_ordinal) REFERENCES session_plan_event(session_id, event_ordinal) ON DELETE RESTRICT;


--
-- Name: session_plan_head session_plan_head_session_id_dependency_event_ordinal_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_plan_head
    ADD CONSTRAINT session_plan_head_session_id_dependency_event_ordinal_fkey FOREIGN KEY (session_id, dependency_event_ordinal) REFERENCES session_plan_current_dependency(session_id, first_event_ordinal) ON DELETE RESTRICT;


--
-- Name: session_plan_head session_plan_head_session_id_event_ordinal_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_plan_head
    ADD CONSTRAINT session_plan_head_session_id_event_ordinal_fkey FOREIGN KEY (session_id, event_ordinal) REFERENCES session_plan_event(session_id, event_ordinal) ON DELETE RESTRICT;


--
-- Name: session_plan_head session_plan_head_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_plan_head
    ADD CONSTRAINT session_plan_head_session_id_fkey FOREIGN KEY (session_id) REFERENCES session(session_id) ON DELETE RESTRICT;


--
-- Name: session session_template_provenance_creation_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session
    ADD CONSTRAINT session_template_provenance_creation_fk FOREIGN KEY (session_id, template_name, template_content_digest) REFERENCES create_session_command(created_session_id, template_name, template_content_digest) DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_timeline_fact session_timeline_fact_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_timeline_fact
    ADD CONSTRAINT session_timeline_fact_session_id_fkey FOREIGN KEY (session_id) REFERENCES session(session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


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
        'append_session_timeline_event_fact()',
        'append_session_timeline_input_bytes()',
        'append_session_timeline_transcript_bytes()',
        'initialize_session_timeline_fact()',
        'maintain_session_catalog_last_activity()',
        'project_web_search_session_metadata()',
        'reconcile_session_timeline_goal_work_fact()',
        'update_session_timeline_work_fact()'
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
