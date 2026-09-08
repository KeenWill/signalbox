DO $$
DECLARE definition text;
BEGIN
    SELECT pg_get_functiondef('require_runner_placement_boundary()'::regprocedure) INTO definition;
    definition := replace(definition,
        'AND state_kind IN (''in_flight'', ''cancellation_requested'')) THEN',
        'AND state_kind IN (''in_flight'', ''cancellation_requested''))
        OR EXISTS (SELECT 1 FROM context_compaction_model_call
            WHERE session_id = NEW.session_id AND state_kind <> ''terminal'') THEN');
    EXECUTE definition;
END;
$$;

CREATE TRIGGER runner_recovery_after_compaction_observation
    AFTER UPDATE OF state_kind ON context_compaction_model_call
    FOR EACH ROW WHEN (OLD.state_kind IS DISTINCT FROM NEW.state_kind AND NEW.state_kind = 'terminal')
    EXECUTE FUNCTION notify_runner_recovery_authority_change();
