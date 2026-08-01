-- Complete per-session credential snapshots form an append-only event history.
-- Event 1 is the creation pin. A later explicit update will append a complete
-- snapshot and advance the guarded head; no present command performs that append.

CREATE TABLE session_model_credential_record (
    session_id uuid NOT NULL REFERENCES session(session_id) ON DELETE RESTRICT,
    event_ordinal numeric(20, 0) NOT NULL
        CHECK (event_ordinal BETWEEN 1 AND 18446744073709551615),
    event_kind text NOT NULL CHECK (event_kind IN ('created', 'updated')),
    provenance_kind text NOT NULL
        CHECK (provenance_kind IN (
            'create_session', 'imported_session', 'migration_backfill',
            'credential_update'
        )),
    provenance_command_id uuid NOT NULL REFERENCES durable_command(command_id) ON DELETE RESTRICT,
    recorded_at timestamptz NOT NULL,
    PRIMARY KEY (session_id, event_ordinal),
    CHECK (
        (event_ordinal = 1 AND event_kind = 'created'
            AND provenance_kind IN (
                'create_session', 'imported_session', 'migration_backfill'
            ))
        OR
        (event_ordinal > 1 AND event_kind = 'updated'
            AND provenance_kind = 'credential_update')
    )
);

CREATE TABLE session_model_credential_entry (
    session_id uuid NOT NULL,
    event_ordinal numeric(20, 0) NOT NULL,
    model_family text NOT NULL CHECK (model_family <> ''),
    credential_reference text NOT NULL CHECK (credential_reference <> ''),
    PRIMARY KEY (session_id, event_ordinal, model_family),
    FOREIGN KEY (session_id, event_ordinal)
        REFERENCES session_model_credential_record(session_id, event_ordinal)
        ON DELETE RESTRICT
);

CREATE TABLE session_current_model_credentials (
    session_id uuid PRIMARY KEY REFERENCES session(session_id) ON DELETE RESTRICT,
    current_event_ordinal numeric(20, 0) NOT NULL
        CHECK (current_event_ordinal BETWEEN 1 AND 18446744073709551615),
    FOREIGN KEY (session_id, current_event_ordinal)
        REFERENCES session_model_credential_record(session_id, event_ordinal)
        ON DELETE RESTRICT
);

CREATE FUNCTION guard_session_model_credential_record_append()
RETURNS trigger
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

CREATE TRIGGER session_model_credential_record_append_guard
BEFORE INSERT ON session_model_credential_record
FOR EACH ROW EXECUTE FUNCTION guard_session_model_credential_record_append();

CREATE FUNCTION guard_session_model_credential_head()
RETURNS trigger
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

CREATE TRIGGER session_model_credential_head_guard
BEFORE INSERT OR UPDATE OR DELETE ON session_current_model_credentials
FOR EACH ROW EXECUTE FUNCTION guard_session_model_credential_head();

CREATE FUNCTION reject_session_model_credential_rewrite()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'session model credential history is append-only';
END;
$$;

CREATE TRIGGER session_model_credential_record_immutable
BEFORE UPDATE OR DELETE ON session_model_credential_record
FOR EACH ROW EXECUTE FUNCTION reject_session_model_credential_rewrite();

CREATE TRIGGER session_model_credential_entry_immutable
BEFORE UPDATE OR DELETE ON session_model_credential_entry
FOR EACH ROW EXECUTE FUNCTION reject_session_model_credential_rewrite();

CREATE FUNCTION reject_session_model_credential_entry_after_publication()
RETURNS trigger
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

CREATE TRIGGER session_model_credential_entry_sealed_after_publication
BEFORE INSERT ON session_model_credential_entry
FOR EACH ROW EXECUTE FUNCTION reject_session_model_credential_entry_after_publication();

CREATE TRIGGER session_model_credential_record_rejects_truncate
BEFORE TRUNCATE ON session_model_credential_record
FOR EACH STATEMENT EXECUTE FUNCTION reject_session_model_credential_rewrite();

CREATE TRIGGER session_model_credential_entry_rejects_truncate
BEFORE TRUNCATE ON session_model_credential_entry
FOR EACH STATEMENT EXECUTE FUNCTION reject_session_model_credential_rewrite();

CREATE TRIGGER session_model_credential_head_rejects_truncate
BEFORE TRUNCATE ON session_current_model_credentials
FOR EACH STATEMENT EXECUTE FUNCTION reject_session_model_credential_rewrite();

WITH creation AS (
    SELECT created_session_id AS session_id,
           command_id
      FROM create_session_command
    UNION ALL
    SELECT created_session_id,
           command_id
      FROM create_session_from_imported_frontier_command
)
INSERT INTO session_model_credential_record
    (session_id, event_ordinal, event_kind, provenance_kind,
     provenance_command_id, recorded_at)
SELECT session_id, 1, 'created', 'migration_backfill', command_id, transaction_timestamp()
  FROM creation;

INSERT INTO session_model_credential_entry
    (session_id, event_ordinal, model_family, credential_reference)
SELECT session_id, 1, 'anthropic', 'anthropic-primary'
  FROM session_model_credential_record;

INSERT INTO session_current_model_credentials
    (session_id, current_event_ordinal)
SELECT session_id, 1
  FROM session_model_credential_record;
