-- Durable request-identity charges for the generic blob-read tool.

CREATE TABLE blob_read_tool_charge (
    request_id uuid PRIMARY KEY,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    blob_digest bytea NOT NULL,
    decoded_byte_count numeric(20, 0) NOT NULL,
    admitted boolean NOT NULL,
    rejection_reason text,

    CONSTRAINT blob_read_tool_charge_bytes_positive_u64
        CHECK (decoded_byte_count BETWEEN 1 AND 18446744073709551615),
    CONSTRAINT blob_read_tool_charge_rejection_shape CHECK (
        (admitted AND rejection_reason IS NULL)
        OR (
            NOT admitted
            AND rejection_reason IN (
                'blob_turn_byte_budget_exceeded',
                'blob_turn_read_count_exceeded'
            )
        )
    ),
    CONSTRAINT blob_read_tool_charge_blob_fk
        FOREIGN KEY (blob_digest) REFERENCES blob(digest)
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT blob_read_tool_charge_request_fk
        FOREIGN KEY (request_id, turn_id, session_id)
        REFERENCES tool_request (request_id, turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE INDEX blob_read_tool_charge_admitted_turn_idx
    ON blob_read_tool_charge (turn_id)
    WHERE admitted;

ALTER TABLE tool_attempt
    DROP CONSTRAINT tool_attempt_error_kind_closed;

ALTER TABLE tool_attempt
    ADD CONSTRAINT tool_attempt_error_kind_closed CHECK (
        error_kind IS NULL OR error_kind IN (
            'unknown_tool', 'invalid_arguments', 'preauthorization_rejected',
            'execution_failed', 'result_too_large', 'crash_lost'
        )
    );

CREATE OR REPLACE FUNCTION reject_tool_attempt_invalid_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.state_kind <> 'prepared' THEN
            RAISE EXCEPTION 'tool attempt must be inserted as Prepared'
                USING
                    ERRCODE = '23514',
                    CONSTRAINT = 'tool_attempt_inserted_prepared';
        END IF;
        RETURN NEW;
    END IF;

    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'tool_attempt is not deletable'
            USING ERRCODE = '23514';
    END IF;

    IF ROW(
        OLD.attempt_id,
        OLD.request_id,
        OLD.session_id,
        OLD.turn_id,
        OLD.issuing_turn_attempt_id,
        OLD.effect_class,
        OLD.dispatch_generation
    ) IS DISTINCT FROM ROW(
        NEW.attempt_id,
        NEW.request_id,
        NEW.session_id,
        NEW.turn_id,
        NEW.issuing_turn_attempt_id,
        NEW.effect_class,
        NEW.dispatch_generation
    ) THEN
        RAISE EXCEPTION 'tool attempt authorization facts are immutable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'terminal' THEN
        RAISE EXCEPTION 'terminal tool attempt is immutable'
            USING ERRCODE = '23514';
    END IF;

    IF NOT (
        OLD.state_kind = NEW.state_kind
        OR (
            OLD.state_kind = 'prepared'
            AND NEW.state_kind IN ('in_flight', 'terminal')
        )
        OR (
            OLD.state_kind = 'in_flight'
            AND NEW.state_kind = 'terminal'
        )
    ) THEN
        RAISE EXCEPTION 'tool attempt transition is not monotonic'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'prepared'
       AND NEW.state_kind = 'terminal'
       AND (
            NEW.terminal_disposition_kind <> 'known_failed'
            OR NEW.error_kind NOT IN (
                'unknown_tool',
                'invalid_arguments',
                'preauthorization_rejected',
                'crash_lost'
            )
       )
    THEN
        RAISE EXCEPTION 'unsent tool attempt has impossible terminal evidence'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'in_flight'
       AND NEW.state_kind = 'terminal'
       AND (
            NEW.error_kind IN (
                'unknown_tool',
                'invalid_arguments',
                'preauthorization_rejected'
            )
            OR (
                OLD.effect_class = 'external_effect'
                AND NEW.error_kind = 'crash_lost'
                AND NOT EXISTS (
                    SELECT 1
                      FROM runner_lease_no_execution_proof AS proof
                     WHERE proof.attempt_id = NEW.attempt_id
                )
            )
       )
    THEN
        RAISE EXCEPTION 'dispatched tool attempt has impossible terminal evidence'
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER blob_read_tool_charge_is_append_only
BEFORE UPDATE OR DELETE ON blob_read_tool_charge
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER blob_read_tool_charge_cannot_be_truncated
BEFORE TRUNCATE ON blob_read_tool_charge
FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();
