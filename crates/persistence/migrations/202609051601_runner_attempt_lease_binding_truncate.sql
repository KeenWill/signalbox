CREATE TRIGGER runner_physical_attempt_lease_binding_rejects_truncate
    BEFORE TRUNCATE ON runner_physical_attempt_lease_binding
    FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();
