DO $$ DECLARE definition text; BEGIN
    SELECT pg_get_constraintdef(oid) INTO definition FROM pg_constraint WHERE conrelid = 'outbox_event'::regclass AND conname = 'outbox_event_kind_closed';
    ALTER TABLE outbox_event DROP CONSTRAINT outbox_event_kind_closed;
    EXECUTE 'ALTER TABLE outbox_event ADD CONSTRAINT outbox_event_kind_closed CHECK ((' || substr(definition, 8, length(definition) - 8) || ') OR event_kind = ''automatic_reconciliation_exhausted'')';
END $$;

CREATE TABLE automatic_reconciliation_exhausted_outbox_event (
    event_sequence numeric(20,0) PRIMARY KEY,
    event_kind text NOT NULL CHECK (event_kind = 'automatic_reconciliation_exhausted'),
    storage_version smallint NOT NULL CHECK (storage_version = 1),
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL UNIQUE,
    model_call_id uuid,
    tool_attempt_id uuid,
    FOREIGN KEY (tool_attempt_id, turn_id, session_id) REFERENCES tool_attempt(attempt_id, turn_id, session_id),
    CHECK (num_nonnulls(model_call_id, tool_attempt_id) = 1),
    FOREIGN KEY (session_id, turn_id) REFERENCES turn_lifecycle(session_id, turn_id),
    FOREIGN KEY (session_id, turn_id, model_call_id) REFERENCES model_call(session_id, turn_id, model_call_id),
    FOREIGN KEY (event_sequence, event_kind, storage_version, session_id) REFERENCES outbox_event(event_sequence, event_kind, storage_version, session_id) DEFERRABLE INITIALLY DEFERRED
);
CREATE TRIGGER automatic_reconciliation_exhausted_outbox_immutable
    BEFORE UPDATE OR DELETE ON automatic_reconciliation_exhausted_outbox_event
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER automatic_reconciliation_exhausted_outbox_cannot_be_truncated
    BEFORE TRUNCATE ON automatic_reconciliation_exhausted_outbox_event
    FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();

CREATE OR REPLACE FUNCTION require_outbox_event_typed_record() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    matching_records bigint;
BEGIN
    CASE NEW.event_kind
        WHEN 'session_created' THEN
            SELECT count(*) INTO matching_records
              FROM session_created_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'session_state_changed' THEN
            SELECT count(*) INTO matching_records
              FROM session_state_changed_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'session_terminal' THEN
            SELECT count(*) INTO matching_records
              FROM session_terminal_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'automatic_reconciliation_exhausted' THEN
            SELECT count(*) INTO matching_records FROM automatic_reconciliation_exhausted_outbox_event WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_credential_pool_exhausted' THEN
            SELECT count(*) INTO matching_records FROM credential_pool_exhaustion_outbox_event WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_terminal' THEN
            SELECT count(*) INTO matching_records
              FROM turn_terminal_outbox_event
             WHERE event_sequence = NEW.event_sequence
               AND disposition_kind = NEW.turn_disposition;
        WHEN 'goal_changed' THEN
            SELECT count(*) INTO matching_records
              FROM goal_changed_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'command_settled' THEN
            SELECT count(*) INTO matching_records
              FROM command_settled_outbox_event
             WHERE event_sequence = NEW.event_sequence
               AND session_id IS NOT DISTINCT FROM NEW.session_id;
        WHEN 'injection_settled' THEN
            SELECT count(*) INTO matching_records
              FROM injection_settled_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'session_ownership_changed' THEN
            SELECT count(*) INTO matching_records
              FROM session_ownership_changed_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'session_model_settings_changed' THEN
            SELECT count(*) INTO matching_records
              FROM session_model_settings_changed_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_model_settings_resolved' THEN
            SELECT count(*) INTO matching_records
              FROM turn_model_settings_resolved_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'input_accepted' THEN
            SELECT count(*) INTO matching_records
              FROM input_accepted_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_activated' THEN
            SELECT count(*) INTO matching_records
              FROM turn_activated_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'model_call_transition' THEN
            SELECT count(*) INTO matching_records
              FROM model_call_transition_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'tool_batch_transition' THEN
            SELECT count(*) INTO matching_records
              FROM tool_batch_transition_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'tool_approval_decided' THEN
            SELECT count(*) INTO matching_records
              FROM tool_approval_decided_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'context_compacted' THEN
            SELECT count(*) INTO matching_records
              FROM context_compacted_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'runner_state_transition' THEN
            SELECT count(*) INTO matching_records
              FROM runner_state_transition_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        ELSE
            RAISE EXCEPTION 'unsupported outbox event kind %', NEW.event_kind
                USING ERRCODE = '23514';
    END CASE;

    IF matching_records <> 1 THEN
        RAISE EXCEPTION 'outbox event % requires exactly one % typed record',
            NEW.event_sequence,
            NEW.event_kind
            USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;


CREATE OR REPLACE FUNCTION record_operator_attention_outbox_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.event_kind IN (
        'session_state_changed', 'session_terminal', 'session_ownership_changed',
        'goal_changed', 'command_settled', 'injection_settled',
        'turn_credential_pool_exhausted'
    ) THEN
        RETURN NULL;
    END IF;
    INSERT INTO operator_attention_change (session_id, fact_kind)
    VALUES (
        NEW.session_id,
        CASE NEW.event_kind
            WHEN 'session_created' THEN 'session'
            WHEN 'session_model_settings_changed' THEN 'session'
            WHEN 'turn_terminal' THEN CASE NEW.turn_disposition
                WHEN 'retired' THEN 'goal'
                ELSE 'turn'
            END
            WHEN 'runner_state_transition' THEN 'runner'
            WHEN 'turn_model_settings_resolved' THEN 'turn'
            WHEN 'input_accepted' THEN 'turn'
            WHEN 'automatic_reconciliation_exhausted' THEN 'turn'
            WHEN 'turn_activated' THEN 'turn'
            WHEN 'model_call_transition' THEN 'turn'
            WHEN 'tool_batch_transition' THEN 'turn'
            WHEN 'tool_approval_decided' THEN 'turn'
            WHEN 'context_compacted' THEN 'turn'
            ELSE NULL
        END
    );
    RETURN NULL;
END;
$$;
