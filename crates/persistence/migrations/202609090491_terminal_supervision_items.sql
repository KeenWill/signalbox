-- Terminal lifecycle facts remain immutable while supervision records operator evidence.
CREATE OR REPLACE FUNCTION guard_session_lifecycle_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'session lifecycle rows are never deleted'
            USING ERRCODE = '23514';
    END IF;
    IF OLD.state_kind = 'terminal' AND
        (to_jsonb(NEW) - ARRAY['supervision_failure_class', 'supervision_cause_code', 'supervision_pending'])
        IS DISTINCT FROM
        (to_jsonb(OLD) - ARRAY['supervision_failure_class', 'supervision_cause_code', 'supervision_pending']) THEN
        RAISE EXCEPTION 'session lifecycle is terminal and cannot change'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.session_id IS DISTINCT FROM OLD.session_id THEN
        RAISE EXCEPTION 'session lifecycle identity is immutable'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;
