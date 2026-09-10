CREATE TABLE outbox_event_quarantine (
    event_sequence numeric(20,0) PRIMARY KEY,
    decode_error text NOT NULL,
    CHECK (event_sequence BETWEEN 1 AND 18446744073709551615),
    CHECK (octet_length(decode_error) > 0)
);

CREATE TRIGGER outbox_event_quarantine_is_append_only
    BEFORE DELETE OR UPDATE ON outbox_event_quarantine
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER outbox_event_quarantine_rejects_truncate
    BEFORE TRUNCATE ON outbox_event_quarantine
    FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();
