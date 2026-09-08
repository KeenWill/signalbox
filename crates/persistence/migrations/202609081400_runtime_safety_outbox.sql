CREATE OR REPLACE FUNCTION require_explicit_tool_approval_decided_outbox() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.recording_transaction_id <> pg_current_xact_id() THEN
        RAISE EXCEPTION 'tool approval decided event transaction is not current'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'tool_approval_decided_transaction_current';
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM tool_approval_decision
         WHERE request_id = NEW.request_id
           AND decision_source NOT IN ('policy_auto', 'session_blanket', 'runtime_safety')
    ) THEN
        RAISE EXCEPTION 'tool approval decided event requires explicit provenance'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'tool_approval_decided_requires_explicit_source';
    END IF;
    RETURN NULL;
END;
$$;
