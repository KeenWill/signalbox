-- Preserve how each new dedicated compaction call's provider counted cached
-- input so its reported usage can remain an exact lower bound for later
-- activation. Calls prepared before this migration may already carry usage
-- whose input/cache relationship cannot be reconstructed from durable facts;
-- keep that historical meaning unknown and default only new calls.

ALTER TABLE context_compaction_model_call
    ADD COLUMN usage_input_includes_cache_tokens boolean;

ALTER TABLE context_compaction_model_call
    ALTER COLUMN usage_input_includes_cache_tokens SET DEFAULT false;

CREATE FUNCTION reject_context_compaction_input_semantics_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF OLD.usage_input_includes_cache_tokens IS DISTINCT FROM
       NEW.usage_input_includes_cache_tokens
    THEN
        RAISE EXCEPTION 'compaction model call input semantics are immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'context_compaction_input_semantics_immutable';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER context_compaction_input_semantics_are_immutable
BEFORE UPDATE ON context_compaction_model_call
FOR EACH ROW
EXECUTE FUNCTION reject_context_compaction_input_semantics_change();

CREATE INDEX context_compaction_reported_usage_by_session_target
    ON context_compaction_model_call
        (session_id, resolved_provider_model_identity_id, model_call_id DESC)
    WHERE state_kind = 'terminal' AND input_tokens IS NOT NULL;
