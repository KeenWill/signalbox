DO $$
DECLARE definition text;
BEGIN
    SELECT pg_get_functiondef('guard_runner_state_transition_outbox_event()'::regprocedure) INTO definition;
    definition := replace(definition,
        'prior.requested_working_directory IS DISTINCT FROM
                    placement.requested_working_directory',
        'prior.pinned_working_directory IS DISTINCT FROM
                    placement.pinned_working_directory');
    definition := replace(definition,
        'prior.requested_working_directory IS NOT DISTINCT FROM
                placement.requested_working_directory',
        'prior.pinned_working_directory IS NOT DISTINCT FROM
                placement.pinned_working_directory');
    EXECUTE definition;
END;
$$;
