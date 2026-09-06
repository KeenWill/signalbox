-- Record the exact serving target selected after fast-mode mapping.

ALTER TABLE model_call
    ADD COLUMN effective_provider_model_identity_id uuid NOT NULL;

CREATE FUNCTION reject_model_call_effective_target_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.effective_provider_model_identity_id IS DISTINCT FROM
       OLD.effective_provider_model_identity_id THEN
        RAISE EXCEPTION 'model call effective target is immutable'
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'model_call_effective_target_immutable';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER model_call_effective_target_is_immutable
    BEFORE UPDATE OF effective_provider_model_identity_id ON model_call
    FOR EACH ROW EXECUTE FUNCTION reject_model_call_effective_target_change();
