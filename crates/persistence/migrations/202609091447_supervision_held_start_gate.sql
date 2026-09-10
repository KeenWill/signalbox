CREATE OR REPLACE FUNCTION hold_session_start_gate() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.start_gate_held
       AND NEW.state_kind NOT IN ('created', 'terminal')
       AND NOT (
            NEW.state_kind = 'parked'
            AND NEW.parked_cause IN ('module_park', 'unknown_failure')
       )
    THEN
        RETURN OLD;
    END IF;
    RETURN NEW;
END;
$$;
