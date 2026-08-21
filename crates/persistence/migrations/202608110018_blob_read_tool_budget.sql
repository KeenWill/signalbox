-- Durable request-identity charges for the generic blob-read tool.

CREATE TABLE blob_read_tool_charge (
    request_id uuid PRIMARY KEY,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    blob_digest bytea NOT NULL,
    decoded_byte_count numeric(20, 0) NOT NULL,
    admitted boolean NOT NULL,

    CONSTRAINT blob_read_tool_charge_bytes_positive_u64
        CHECK (decoded_byte_count BETWEEN 1 AND 18446744073709551615),
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

ALTER TABLE tool_attempt
    DROP CONSTRAINT tool_attempt_error_kind_closed;

ALTER TABLE tool_attempt
    ADD CONSTRAINT tool_attempt_error_kind_closed CHECK (
        error_kind IS NULL OR error_kind IN (
            'unknown_tool', 'invalid_arguments', 'preauthorization_rejected',
            'execution_failed', 'result_too_large', 'crash_lost'
        )
    );

CREATE TRIGGER blob_read_tool_charge_is_append_only
BEFORE UPDATE OR DELETE ON blob_read_tool_charge
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER blob_read_tool_charge_cannot_be_truncated
BEFORE TRUNCATE ON blob_read_tool_charge
FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();
