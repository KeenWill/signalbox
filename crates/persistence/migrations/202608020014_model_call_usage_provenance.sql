-- Every model call carries one closed provenance for its token-usage fields.
-- Existing and newly committed evidence is provider-reported; a later
-- estimator may write `estimated` only as part of the call's terminal commit.

ALTER TABLE model_call
    ADD COLUMN usage_provenance_kind text NOT NULL DEFAULT 'reported',
    ADD CONSTRAINT model_call_usage_provenance_kind_closed
        CHECK (usage_provenance_kind IN ('reported', 'estimated'));

-- The original model-call guard already makes authorization facts immutable
-- and every terminal row wholly immutable. Add provenance to the authorization
-- guard so a nonterminal call cannot be relabeled between preparation and its
-- terminal evidence commit.
CREATE FUNCTION reject_model_call_usage_provenance_rewrite()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.usage_provenance_kind IS DISTINCT FROM OLD.usage_provenance_kind
       AND NOT (OLD.state_kind <> 'terminal' AND NEW.state_kind = 'terminal') THEN
        RAISE EXCEPTION 'model-call usage provenance is immutable'
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'model_call_usage_provenance_immutable';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER model_call_usage_provenance_is_immutable
BEFORE UPDATE ON model_call
FOR EACH ROW
EXECUTE FUNCTION reject_model_call_usage_provenance_rewrite();
