CREATE FUNCTION credential_wait_is_eligible(checked_wait uuid) RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT consumed_by_attempt_id IS NULL AND (eligible OR COALESCE(deadline <= statement_timestamp(), false))
       AND NOT EXISTS (SELECT 1 FROM credential_pool_availability_successor successor
           WHERE successor.successor_turn_attempt_id = checked_wait
             AND successor.retry_not_before > statement_timestamp())
      FROM credential_availability_wait WHERE wait_attempt_id = checked_wait
$$;

CREATE FUNCTION wake_credential_member(checked_profile text) RETURNS void LANGUAGE sql AS $$
    UPDATE credential_availability_wait waiting SET eligible = true
     WHERE waiting.consumed_by_attempt_id IS NULL AND NOT waiting.eligible
       AND EXISTS (SELECT 1 FROM credential_availability_wait_member member
           WHERE member.wait_attempt_id = waiting.wait_attempt_id AND member.profile = checked_profile)
$$;

CREATE FUNCTION wake_cleared_credential_waits() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.outcome = 'cleared' THEN
        PERFORM wake_credential_member(NEW.target->>'profile');
    END IF;
    RETURN NULL;
END;
$$;
CREATE TRIGGER credential_wait_operator_clear AFTER INSERT ON clear_credential_exclusion_command
    FOR EACH ROW EXECUTE FUNCTION wake_cleared_credential_waits();

CREATE FUNCTION wake_updated_credential_waits() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM wake_credential_member(NEW.credential_reference);
    RETURN NULL;
END;
$$;
CREATE TRIGGER credential_wait_capacity_update AFTER INSERT OR UPDATE ON credential_rate_limit_snapshot
    FOR EACH ROW EXECUTE FUNCTION wake_updated_credential_waits();
CREATE TRIGGER credential_wait_transient_update AFTER INSERT OR UPDATE ON credential_pool_transient_exclusion
    FOR EACH ROW EXECUTE FUNCTION wake_updated_credential_waits();
CREATE TRIGGER credential_wait_displacement_consumed AFTER UPDATE OF consumed_turn_id ON credential_pool_member_action
    FOR EACH ROW WHEN (OLD.consumed_turn_id IS NULL AND NEW.consumed_turn_id IS NOT NULL)
    EXECUTE FUNCTION wake_updated_credential_waits();

CREATE FUNCTION wake_replaced_oauth_credential_waits() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM wake_credential_member(NEW.profile);
    RETURN NULL;
END;
$$;
CREATE TRIGGER credential_wait_oauth_replaced AFTER INSERT OR UPDATE OF generation ON oauth_credential_authorization
    FOR EACH ROW EXECUTE FUNCTION wake_replaced_oauth_credential_waits();
