-- A direct Prepared-to-terminal transition is provably unsent, so it cannot
-- carry a definitive provider-failure classification.
CREATE FUNCTION reject_model_call_unsent_provider_failure_cause()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF OLD.state_kind = 'prepared'
       AND NEW.state_kind = 'terminal'
       AND NEW.terminal_provider_failure_cause IS NOT NULL
    THEN
        RAISE EXCEPTION 'an unsent call cannot carry a provider-failure cause'
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'model_call_unsent_provider_failure_cause_absent';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER model_call_unsent_provider_failure_cause_is_absent
BEFORE UPDATE ON model_call
FOR EACH ROW
EXECUTE FUNCTION reject_model_call_unsent_provider_failure_cause();
