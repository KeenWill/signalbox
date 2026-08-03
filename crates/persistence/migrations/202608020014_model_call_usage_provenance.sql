-- Every model call carries one closed provenance for its token-usage fields.
-- Existing and newly committed evidence is provider-reported; a later
-- estimator may write `estimated` only as part of the call's terminal commit.

ALTER TABLE model_call
    ADD COLUMN usage_input_includes_cache_tokens boolean,
    ADD COLUMN usage_provenance_kind text NOT NULL DEFAULT 'reported',
    ADD CONSTRAINT model_call_usage_provenance_kind_closed
        CHECK (usage_provenance_kind IN ('reported', 'estimated'));

-- Calls prepared before this migration may already carry Codex usage whose
-- input/cache relationship cannot be reconstructed from durable facts. Keep
-- that meaning unknown, while calls prepared after migration default to the
-- cache-exclusive meaning unless their writer explicitly pins otherwise.
ALTER TABLE model_call
    ALTER COLUMN usage_input_includes_cache_tokens SET DEFAULT false;

-- The original model-call guard already makes authorization facts immutable
-- and every terminal row wholly immutable. Add provenance to the authorization
-- guard, and keep the adapter-specific meaning of input tokens immutable from
-- the call's prepared checkpoint onward.
CREATE FUNCTION reject_model_call_usage_metadata_rewrite()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.usage_input_includes_cache_tokens
           IS DISTINCT FROM OLD.usage_input_includes_cache_tokens
       OR (
           NEW.usage_provenance_kind IS DISTINCT FROM OLD.usage_provenance_kind
           AND NOT (
               OLD.state_kind <> 'terminal'
               AND NEW.state_kind = 'terminal'
           )
       ) THEN
        RAISE EXCEPTION 'model-call usage metadata is immutable'
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'model_call_usage_metadata_immutable';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER model_call_usage_metadata_is_immutable
BEFORE UPDATE ON model_call
FOR EACH ROW
EXECUTE FUNCTION reject_model_call_usage_metadata_rewrite();
