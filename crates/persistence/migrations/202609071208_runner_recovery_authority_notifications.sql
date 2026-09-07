CREATE FUNCTION notify_runner_recovery_authority_change() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify('runner_recovery', '');
    RETURN NULL;
END;
$$;

CREATE TRIGGER runner_recovery_observes_connection_authority
    AFTER INSERT OR UPDATE ON runner_connection_authority_head
    FOR EACH ROW EXECUTE FUNCTION notify_runner_recovery_authority_change();
CREATE TRIGGER runner_recovery_observes_registration_authority
    AFTER INSERT OR UPDATE ON runner_current_registration
    FOR EACH ROW EXECUTE FUNCTION notify_runner_recovery_authority_change();
CREATE TRIGGER runner_recovery_observes_enrollment_authority
    AFTER UPDATE ON runner_enrollment
    FOR EACH ROW EXECUTE FUNCTION notify_runner_recovery_authority_change();
