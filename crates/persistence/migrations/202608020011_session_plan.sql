-- Session plans are append-only event histories. Entry identity is the ordinal of
-- its creation event; revisions and status changes retain their exact trusted
-- tool-dispatch provenance.

CREATE TABLE session_plan_event (
    session_id uuid NOT NULL REFERENCES session(session_id) ON DELETE RESTRICT,
    event_ordinal numeric(20, 0) NOT NULL
        CHECK (event_ordinal BETWEEN 1 AND 18446744073709551615),
    event_kind text NOT NULL
        CONSTRAINT session_plan_event_kind_closed
        CHECK (event_kind IN ('created', 'text_revised', 'status_changed')),
    entry_ordinal numeric(20, 0) NOT NULL
        CHECK (entry_ordinal BETWEEN 1 AND 18446744073709551615),
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
    CONSTRAINT session_plan_event_shape CHECK (
        (
            event_kind = 'created'
            AND entry_ordinal = event_ordinal
            AND entry_text IS NOT NULL
            AND char_length(entry_text) BETWEEN 1 AND 4096
            AND entry_status IS NULL
        )
        OR
        (
            event_kind = 'text_revised'
            AND entry_ordinal < event_ordinal
            AND entry_text IS NOT NULL
            AND char_length(entry_text) BETWEEN 1 AND 4096
            AND entry_status IS NULL
        )
        OR
        (
            event_kind = 'status_changed'
            AND entry_ordinal < event_ordinal
            AND entry_text IS NULL
            AND entry_status IS NOT NULL
        )
    )
);

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
        RAISE EXCEPTION 'session plan event requires its owning session';
    END IF;

    SELECT max(event_ordinal)
      INTO latest_ordinal
      FROM session_plan_event
     WHERE session_id = target_session_id;
    RETURN coalesce(latest_ordinal + 1, 1);
END;
$$;

CREATE FUNCTION guard_session_plan_event_append()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    latest_ordinal numeric(20, 0);
    target_kind text;
BEGIN
    PERFORM 1
      FROM session
     WHERE session_id = NEW.session_id
       FOR NO KEY UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'session plan event requires its owning session';
    END IF;

    SELECT max(event_ordinal)
      INTO latest_ordinal
      FROM session_plan_event
     WHERE session_id = NEW.session_id;
    IF (latest_ordinal IS NULL AND NEW.event_ordinal <> 1)
        OR (
            latest_ordinal IS NOT NULL
            AND NEW.event_ordinal <> latest_ordinal + 1
        )
    THEN
        RAISE EXCEPTION 'session plan events must append by one ordinal';
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
    RETURN NEW;
END;
$$;

CREATE TRIGGER session_plan_event_append_guard
BEFORE INSERT ON session_plan_event
FOR EACH ROW EXECUTE FUNCTION guard_session_plan_event_append();

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

CREATE INDEX session_plan_event_entry_history
    ON session_plan_event (session_id, entry_ordinal, event_ordinal);
