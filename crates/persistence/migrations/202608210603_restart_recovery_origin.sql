-- Record the exact turns whose ambiguous operation wait was created by the
-- daemon startup scan. Runtime liveness recovery deliberately does not write
-- this record, even though it shares the lower-level crash classifier.

CREATE TABLE turn_restart_recovery_origin (
    turn_id uuid PRIMARY KEY,
    session_id uuid NOT NULL,
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    UNIQUE (turn_id, session_id),
    FOREIGN KEY (turn_id, session_id)
        REFERENCES turn_lifecycle (turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

CREATE FUNCTION reject_turn_restart_recovery_origin_truncate()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION '% cannot be truncated', TG_TABLE_NAME
        USING ERRCODE = '23514';
END;
$$;

CREATE TRIGGER turn_restart_recovery_origin_is_append_only
BEFORE UPDATE OR DELETE ON turn_restart_recovery_origin
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER turn_restart_recovery_origin_reject_truncate
BEFORE TRUNCATE ON turn_restart_recovery_origin
FOR EACH STATEMENT
EXECUTE FUNCTION reject_turn_restart_recovery_origin_truncate();
