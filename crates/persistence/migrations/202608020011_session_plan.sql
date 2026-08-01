-- Session plans are append-only event histories. Entry identity is the ordinal of
-- its creation event; revisions, status changes, and dependency edges retain
-- their exact trusted tool-dispatch provenance.

CREATE TABLE session_plan_event (
    session_id uuid NOT NULL REFERENCES session(session_id) ON DELETE RESTRICT,
    event_ordinal numeric(20, 0) NOT NULL
        CHECK (event_ordinal BETWEEN 1 AND 18446744073709551615),
    prior_event_ordinal numeric(20, 0)
        CHECK (
            prior_event_ordinal IS NULL
            OR prior_event_ordinal BETWEEN 1 AND 18446744073709551615
        ),
    event_kind text NOT NULL
        CONSTRAINT session_plan_event_kind_closed
        CHECK (event_kind IN ('created', 'text_revised', 'status_changed', 'depends_on')),
    entry_ordinal numeric(20, 0) NOT NULL
        CHECK (entry_ordinal BETWEEN 1 AND 18446744073709551615),
    dependency_ordinal numeric(20, 0)
        CHECK (
            dependency_ordinal IS NULL
            OR dependency_ordinal BETWEEN 1 AND 18446744073709551615
        ),
    entry_text text,
    entry_status text
        CONSTRAINT session_plan_event_status_closed
        CHECK (
            entry_status IS NULL
            OR entry_status IN ('pending', 'in_progress', 'completed', 'abandoned')
        ),
    provenance_turn_id uuid NOT NULL,
    provenance_issuing_turn_attempt_id uuid NOT NULL,
    provenance_request_id uuid NOT NULL,
    provenance_attempt_id uuid NOT NULL UNIQUE,
    provenance_dispatch_generation numeric(20, 0) NOT NULL
        CHECK (
            provenance_dispatch_generation
                BETWEEN 1 AND 18446744073709551615
        ),

    PRIMARY KEY (session_id, event_ordinal),
    FOREIGN KEY (
        provenance_attempt_id,
        provenance_request_id,
        provenance_issuing_turn_attempt_id,
        provenance_dispatch_generation
    )
        REFERENCES tool_attempt (
            attempt_id,
            request_id,
            issuing_turn_attempt_id,
            dispatch_generation
        )
        ON DELETE RESTRICT,
    FOREIGN KEY (provenance_attempt_id, provenance_turn_id, session_id)
        REFERENCES tool_attempt (attempt_id, turn_id, session_id)
        ON DELETE RESTRICT,
    FOREIGN KEY (session_id, prior_event_ordinal)
        REFERENCES session_plan_event (session_id, event_ordinal)
        ON DELETE RESTRICT,
    FOREIGN KEY (session_id, dependency_ordinal)
        REFERENCES session_plan_event (session_id, event_ordinal)
        ON DELETE RESTRICT,
    CONSTRAINT session_plan_event_predecessor_shape CHECK (
        (event_ordinal = 1 AND prior_event_ordinal IS NULL)
        OR (
            event_ordinal > 1
            AND prior_event_ordinal = event_ordinal - 1
        )
    ),
    CONSTRAINT session_plan_event_shape CHECK (
        (
            event_kind = 'created'
            AND entry_ordinal = event_ordinal
            AND dependency_ordinal IS NULL
            AND entry_text IS NOT NULL
            AND char_length(entry_text) BETWEEN 1 AND 4096
            AND entry_status IS NULL
        )
        OR
        (
            event_kind = 'text_revised'
            AND entry_ordinal < event_ordinal
            AND dependency_ordinal IS NULL
            AND entry_text IS NOT NULL
            AND char_length(entry_text) BETWEEN 1 AND 4096
            AND entry_status IS NULL
        )
        OR
        (
            event_kind = 'status_changed'
            AND entry_ordinal < event_ordinal
            AND dependency_ordinal IS NULL
            AND entry_text IS NULL
            AND entry_status IS NOT NULL
        )
        OR
        (
            event_kind = 'depends_on'
            AND entry_ordinal < event_ordinal
            AND dependency_ordinal IS NOT NULL
            AND dependency_ordinal < event_ordinal
            AND dependency_ordinal <> entry_ordinal
            AND entry_text IS NULL
            AND entry_status IS NULL
        )
    )
);

-- This bounded projection keeps one row for each distinct current edge while
-- the event table retains every duplicate append as history.
CREATE TABLE session_plan_current_dependency (
    session_id uuid NOT NULL,
    entry_ordinal numeric(20, 0) NOT NULL
        CHECK (entry_ordinal BETWEEN 1 AND 18446744073709551615),
    dependency_ordinal numeric(20, 0) NOT NULL
        CHECK (dependency_ordinal BETWEEN 1 AND 18446744073709551615),
    first_event_ordinal numeric(20, 0) NOT NULL
        CHECK (first_event_ordinal BETWEEN 1 AND 18446744073709551615),

    PRIMARY KEY (session_id, entry_ordinal, dependency_ordinal),
    UNIQUE (session_id, first_event_ordinal),
    FOREIGN KEY (session_id, entry_ordinal)
        REFERENCES session_plan_event (session_id, event_ordinal)
        ON DELETE RESTRICT,
    FOREIGN KEY (session_id, dependency_ordinal)
        REFERENCES session_plan_event (session_id, event_ordinal)
        ON DELETE RESTRICT,
    FOREIGN KEY (session_id, first_event_ordinal)
        REFERENCES session_plan_event (session_id, event_ordinal)
        ON DELETE RESTRICT,
    CHECK (entry_ordinal <> dependency_ordinal),
    CHECK (first_event_ordinal > entry_ordinal),
    CHECK (first_event_ordinal > dependency_ordinal)
);

-- This mutable head certifies the complete contiguous prefix admitted by the
-- append guard. Reads compare it with the indexed latest event instead of
-- replaying an unbounded history to rediscover sequence integrity.
CREATE TABLE session_plan_head (
    session_id uuid PRIMARY KEY
        REFERENCES session(session_id) ON DELETE RESTRICT,
    event_ordinal numeric(20, 0) NOT NULL
        CHECK (event_ordinal BETWEEN 1 AND 18446744073709551615),
    FOREIGN KEY (session_id, event_ordinal)
        REFERENCES session_plan_event (session_id, event_ordinal)
        ON DELETE RESTRICT
);

CREATE FUNCTION session_plan_request_arguments_json(
    arguments_kind text,
    arguments_text text
)
RETURNS jsonb
LANGUAGE plpgsql
IMMUTABLE
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

CREATE FUNCTION session_plan_event_has_authority(candidate session_plan_event)
RETURNS boolean
LANGUAGE sql
STABLE
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

CREATE FUNCTION next_session_plan_event_ordinal(target_session_id uuid)
RETURNS numeric(20, 0)
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

CREATE FUNCTION session_plan_dependency_cycle_exists(
    target_session_id uuid,
    root_ordinals numeric[]
)
RETURNS boolean
LANGUAGE plpgsql
STABLE
AS $$
DECLARE
    root_ordinal numeric;
    stack_nodes numeric[] := ARRAY[]::numeric[];
    stack_positions integer[] := ARRAY[]::integer[];
    visited_nodes numeric[] := ARRAY[]::numeric[];
    active_nodes numeric[] := ARRAY[]::numeric[];
    stack_size integer;
    current_node numeric;
    current_position integer;
    next_node numeric;
BEGIN
    FOREACH root_ordinal IN ARRAY root_ordinals LOOP
        IF root_ordinal = ANY(visited_nodes) THEN
            CONTINUE;
        END IF;
        stack_nodes := array_append(stack_nodes, root_ordinal);
        stack_positions := array_append(stack_positions, 0);
        active_nodes := array_append(active_nodes, root_ordinal);

        WHILE cardinality(stack_nodes) > 0 LOOP
            stack_size := cardinality(stack_nodes);
            current_node := stack_nodes[stack_size];
            current_position := stack_positions[stack_size];

            SELECT edge.dependency_ordinal
              INTO next_node
              FROM session_plan_current_dependency AS edge
             WHERE edge.session_id = target_session_id
               AND edge.entry_ordinal = current_node
             ORDER BY edge.first_event_ordinal
             OFFSET current_position
             LIMIT 1;
            IF NOT FOUND THEN
                visited_nodes := array_append(visited_nodes, current_node);
                active_nodes := array_remove(active_nodes, current_node);
                stack_nodes := trim_array(stack_nodes, 1);
                stack_positions := trim_array(stack_positions, 1);
            ELSE
                stack_positions[stack_size] := current_position + 1;
                IF next_node = ANY(active_nodes) THEN
                    RETURN TRUE;
                END IF;
                IF NOT (next_node = ANY(visited_nodes)) THEN
                    stack_nodes := array_append(stack_nodes, next_node);
                    stack_positions := array_append(stack_positions, 0);
                    active_nodes := array_append(active_nodes, next_node);
                END IF;
            END IF;
        END LOOP;
    END LOOP;
    RETURN FALSE;
END;
$$;

CREATE FUNCTION guard_session_plan_event_append()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    latest_ordinal numeric(20, 0);
    target_kind text;
    closes_cycle boolean;
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
        SELECT event_kind
          INTO target_kind
          FROM session_plan_event
         WHERE session_id = NEW.session_id
           AND event_ordinal = NEW.entry_ordinal;
        IF target_kind IS DISTINCT FROM 'created' THEN
            RAISE EXCEPTION 'session plan mutation must name a creation event';
        END IF;
    END IF;

    IF NEW.event_kind = 'depends_on' THEN
        SELECT event_kind
          INTO target_kind
          FROM session_plan_event
         WHERE session_id = NEW.session_id
           AND event_ordinal = NEW.dependency_ordinal;
        IF target_kind IS DISTINCT FROM 'created' THEN
            RAISE EXCEPTION
                'session plan dependency must name a creation event';
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

        IF session_plan_dependency_cycle_exists(
            NEW.session_id,
            ARRAY[NEW.entry_ordinal, NEW.dependency_ordinal]
        ) THEN
            RAISE EXCEPTION 'session plan dependency graph is already cyclic'
                USING ERRCODE = '23514',
                    CONSTRAINT = 'session_plan_dependency_graph_cycle';
        END IF;

        WITH RECURSIVE dependency_node(node) AS (
            SELECT NEW.dependency_ordinal
            UNION
            SELECT edge.dependency_ordinal
              FROM dependency_node
              JOIN session_plan_current_dependency AS edge
                ON edge.session_id = NEW.session_id
               AND edge.entry_ordinal = dependency_node.node
        )
        SELECT EXISTS (
            SELECT 1
              FROM dependency_node
             WHERE node = NEW.entry_ordinal
        )
          INTO closes_cycle;
        IF closes_cycle THEN
            RAISE EXCEPTION 'session plan dependency would create a cycle'
                USING ERRCODE = '23514',
                    CONSTRAINT = 'session_plan_dependency_cycle';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER session_plan_event_append_guard
BEFORE INSERT ON session_plan_event
FOR EACH ROW EXECUTE FUNCTION guard_session_plan_event_append();

CREATE FUNCTION project_session_plan_current_dependency()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.event_kind = 'depends_on' THEN
        INSERT INTO session_plan_current_dependency (
            session_id,
            entry_ordinal,
            dependency_ordinal,
            first_event_ordinal
        )
        VALUES (
            NEW.session_id,
            NEW.entry_ordinal,
            NEW.dependency_ordinal,
            NEW.event_ordinal
        )
        ON CONFLICT (session_id, entry_ordinal, dependency_ordinal)
        DO NOTHING;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER session_plan_event_projects_dependency
AFTER INSERT ON session_plan_event
FOR EACH ROW EXECUTE FUNCTION project_session_plan_current_dependency();

CREATE FUNCTION guard_session_plan_current_dependency_maintenance()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF pg_trigger_depth() < 2 THEN
        RAISE EXCEPTION 'session plan current dependencies are trigger-maintained';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER session_plan_current_dependency_maintenance_guard
BEFORE INSERT ON session_plan_current_dependency
FOR EACH ROW EXECUTE FUNCTION guard_session_plan_current_dependency_maintenance();

CREATE FUNCTION advance_session_plan_head()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.event_ordinal = 1 THEN
        INSERT INTO session_plan_head (session_id, event_ordinal)
        VALUES (NEW.session_id, NEW.event_ordinal);
    ELSE
        UPDATE session_plan_head
           SET event_ordinal = NEW.event_ordinal
         WHERE session_id = NEW.session_id
           AND event_ordinal = NEW.prior_event_ordinal;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'session plan head must advance by one ordinal';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER session_plan_event_advances_head
AFTER INSERT ON session_plan_event
FOR EACH ROW EXECUTE FUNCTION advance_session_plan_head();

CREATE FUNCTION guard_session_plan_head_maintenance()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF pg_trigger_depth() < 2 THEN
        RAISE EXCEPTION 'session plan head is trigger-maintained';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER session_plan_head_maintenance_guard
BEFORE INSERT OR UPDATE ON session_plan_head
FOR EACH ROW EXECUTE FUNCTION guard_session_plan_head_maintenance();

CREATE FUNCTION reject_session_plan_event_rewrite()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'session plan history is append-only';
END;
$$;

CREATE TRIGGER session_plan_event_immutable
BEFORE UPDATE OR DELETE ON session_plan_event
FOR EACH ROW EXECUTE FUNCTION reject_session_plan_event_rewrite();

CREATE TRIGGER session_plan_event_rejects_truncate
BEFORE TRUNCATE ON session_plan_event
FOR EACH STATEMENT EXECUTE FUNCTION reject_session_plan_event_rewrite();

CREATE FUNCTION reject_session_plan_head_rewrite()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'session plan head is trigger-maintained';
END;
$$;

CREATE TRIGGER session_plan_head_immutable_identity
BEFORE DELETE ON session_plan_head
FOR EACH ROW EXECUTE FUNCTION reject_session_plan_head_rewrite();

CREATE TRIGGER session_plan_head_rejects_truncate
BEFORE TRUNCATE ON session_plan_head
FOR EACH STATEMENT EXECUTE FUNCTION reject_session_plan_head_rewrite();

CREATE FUNCTION reject_session_plan_current_dependency_rewrite()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'session plan current dependencies are append projections';
END;
$$;

CREATE TRIGGER session_plan_current_dependency_immutable
BEFORE UPDATE OR DELETE ON session_plan_current_dependency
FOR EACH ROW EXECUTE FUNCTION reject_session_plan_current_dependency_rewrite();

CREATE TRIGGER session_plan_current_dependency_rejects_truncate
BEFORE TRUNCATE ON session_plan_current_dependency
FOR EACH STATEMENT EXECUTE FUNCTION reject_session_plan_current_dependency_rewrite();

CREATE INDEX session_plan_current_dependency_first_append
    ON session_plan_current_dependency (
        session_id,
        entry_ordinal,
        first_event_ordinal
    )
    INCLUDE (dependency_ordinal);

CREATE INDEX session_plan_event_entry_history
    ON session_plan_event (session_id, entry_ordinal, event_ordinal);

CREATE INDEX session_plan_event_dependencies
    ON session_plan_event (session_id, entry_ordinal, event_ordinal)
    INCLUDE (dependency_ordinal)
    WHERE event_kind = 'depends_on';

CREATE INDEX session_plan_event_unsupported_kind
    ON session_plan_event (session_id, event_kind)
    WHERE (
        event_kind IS NULL
        OR event_kind NOT IN ('created', 'text_revised', 'status_changed', 'depends_on')
    );

CREATE INDEX session_plan_event_created_page
    ON session_plan_event (session_id, event_ordinal)
    WHERE event_kind = 'created';

CREATE INDEX session_plan_event_latest_text_revision
    ON session_plan_event (session_id, entry_ordinal, event_ordinal DESC)
    INCLUDE (entry_text, entry_status)
    WHERE event_kind = 'text_revised';

CREATE INDEX session_plan_event_latest_status_change
    ON session_plan_event (session_id, entry_ordinal, event_ordinal DESC)
    INCLUDE (entry_text, entry_status)
    WHERE event_kind = 'status_changed';
