ALTER TABLE outbox_event
    DROP CONSTRAINT outbox_event_kind_closed,
    ADD CONSTRAINT outbox_event_kind_closed
        CHECK (
            event_kind IN (
                'session_created', 'input_accepted', 'goal_turn_retired',
                'turn_activated', 'turn_failed', 'model_call_transition',
                'tool_batch_transition', 'tool_approval_decided',
                'context_compacted', 'turn_completed', 'turn_refused',
                'turn_cancelled', 'turn_reconciliation_required'
            )
        );

CREATE TABLE tool_approval_decided_outbox_event (
    event_sequence numeric(20, 0) PRIMARY KEY,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    request_id uuid NOT NULL UNIQUE,

    CONSTRAINT tool_approval_decided_outbox_kind_closed
        CHECK (event_kind = 'tool_approval_decided'),
    CONSTRAINT tool_approval_decided_outbox_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT tool_approval_decided_outbox_header_fk
        FOREIGN KEY (event_sequence, event_kind, storage_version, session_id)
        REFERENCES outbox_event (
            event_sequence, event_kind, storage_version, session_id
        )
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT tool_approval_decided_outbox_request_fk
        FOREIGN KEY (request_id, turn_id, session_id)
        REFERENCES tool_request (request_id, turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    CONSTRAINT tool_approval_decided_outbox_decision_fk
        FOREIGN KEY (request_id)
        REFERENCES tool_approval_decision (request_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE FUNCTION require_explicit_tool_approval_decided_outbox()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM tool_approval_decision
         WHERE request_id = NEW.request_id
           AND decision_source NOT IN ('policy_auto', 'session_blanket')
    ) THEN
        RAISE EXCEPTION 'tool approval decided event requires explicit provenance'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'tool_approval_decided_requires_explicit_source';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER tool_approval_decided_requires_explicit_source
AFTER INSERT ON tool_approval_decided_outbox_event
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_explicit_tool_approval_decided_outbox();

CREATE TRIGGER tool_approval_decided_outbox_event_is_append_only
BEFORE UPDATE OR DELETE ON tool_approval_decided_outbox_event
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER tool_approval_decided_outbox_event_cannot_be_truncated
BEFORE TRUNCATE ON tool_approval_decided_outbox_event
FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();

CREATE FUNCTION require_explicit_tool_approval_effect()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    matching_effects bigint;
BEGIN
    IF NEW.decision_source IN ('policy_auto', 'session_blanket') THEN
        RETURN NULL;
    END IF;

    SELECT count(*)
      INTO matching_effects
      FROM tool_approval_decided_outbox_event AS dispatched
      JOIN tool_request AS request
        ON request.request_id = dispatched.request_id
      JOIN turn_lifecycle AS lifecycle
        ON lifecycle.turn_id = request.turn_id
       AND lifecycle.session_id = request.session_id
     WHERE dispatched.request_id = NEW.request_id
       AND lifecycle.active_tool_round_call_id =
           request.producing_model_call_id
       AND NOT EXISTS (
            SELECT 1
              FROM tool_request AS earlier
              LEFT JOIN tool_approval_decision AS earlier_decision
                ON earlier_decision.request_id = earlier.request_id
             WHERE earlier.producing_model_call_id =
                   request.producing_model_call_id
               AND earlier.request_ordinal < request.request_ordinal
               AND earlier_decision.request_id IS NULL
       )
       AND NOT (
            lifecycle.state_kind = 'active'
            AND lifecycle.active_phase_kind = 'awaiting_tool_approval'
            AND lifecycle.approval_tool_request_id = NEW.request_id
       );
    IF matching_effects <> 1 THEN
        RAISE EXCEPTION
            'explicit decision lacks its outbox and lifecycle effect'
            USING
                ERRCODE = '23514',
                CONSTRAINT =
                    'tool_approval_explicit_requires_atomic_effect';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER tool_approval_explicit_requires_atomic_effect
AFTER INSERT OR UPDATE ON tool_approval_decision
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_explicit_tool_approval_effect();

CREATE OR REPLACE FUNCTION require_outbox_event_typed_record()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    matching_records bigint;
BEGIN
    CASE NEW.event_kind
        WHEN 'session_created' THEN SELECT count(*) INTO matching_records FROM session_created_outbox_event WHERE event_sequence = NEW.event_sequence;
        WHEN 'input_accepted' THEN SELECT count(*) INTO matching_records FROM input_accepted_outbox_event WHERE event_sequence = NEW.event_sequence;
        WHEN 'goal_turn_retired' THEN SELECT count(*) INTO matching_records FROM goal_turn_retired_outbox_event WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_activated' THEN SELECT count(*) INTO matching_records FROM turn_activated_outbox_event WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_failed' THEN SELECT count(*) INTO matching_records FROM turn_failed_outbox_event WHERE event_sequence = NEW.event_sequence;
        WHEN 'model_call_transition' THEN SELECT count(*) INTO matching_records FROM model_call_transition_outbox_event WHERE event_sequence = NEW.event_sequence;
        WHEN 'tool_batch_transition' THEN SELECT count(*) INTO matching_records FROM tool_batch_transition_outbox_event WHERE event_sequence = NEW.event_sequence;
        WHEN 'tool_approval_decided' THEN SELECT count(*) INTO matching_records FROM tool_approval_decided_outbox_event WHERE event_sequence = NEW.event_sequence;
        WHEN 'context_compacted' THEN SELECT count(*) INTO matching_records FROM context_compacted_outbox_event WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_completed' THEN SELECT count(*) INTO matching_records FROM turn_completed_outbox_event WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_refused' THEN SELECT count(*) INTO matching_records FROM turn_refused_outbox_event WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_cancelled' THEN SELECT count(*) INTO matching_records FROM turn_cancelled_outbox_event WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_reconciliation_required' THEN SELECT count(*) INTO matching_records FROM turn_reconciliation_required_outbox_event WHERE event_sequence = NEW.event_sequence;
        ELSE
            RAISE EXCEPTION 'unsupported outbox event kind %', NEW.event_kind
                USING ERRCODE = '23514';
    END CASE;
    IF matching_records <> 1 THEN
        RAISE EXCEPTION 'outbox event % requires exactly one % typed record',
            NEW.event_sequence, NEW.event_kind USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;

-- Decisions committed before this event family existed already installed their
-- lifecycle effect. Append their typed history at the migration boundary so
-- every explicit decision remains visible to event consumers after upgrade.
DO $$
DECLARE
    prior_decision record;
BEGIN
    FOR prior_decision IN
        SELECT request.request_id, request.turn_id, request.session_id
          FROM tool_approval_decision AS decision
          JOIN tool_request AS request
            ON request.request_id = decision.request_id
         WHERE decision.decision_source NOT IN (
                   'policy_auto', 'session_blanket'
               )
         ORDER BY request.request_id
    LOOP
        WITH header AS (
            INSERT INTO outbox_event
                (event_kind, storage_version, session_id)
            VALUES (
                'tool_approval_decided', 1, prior_decision.session_id
            )
            RETURNING event_sequence, event_kind, storage_version, session_id
        )
        INSERT INTO tool_approval_decided_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             turn_id, request_id)
        SELECT event_sequence, event_kind, storage_version, session_id,
               prior_decision.turn_id, prior_decision.request_id
          FROM header;
    END LOOP;
END;
$$;
