-- The placement-policy trigger already derives the exact effective runner
-- approval from override, sandbox, and effect, and binds it to the durable
-- decision source for every lease insert. Remove the older profileless check
-- that instead consulted the catalog default: WorkspaceRestricted deliberately
-- resolves a Confirm declaration to policy-auto, while Ambient still resolves
-- it to session policy and remains user-confirmation-only.
DO $migration$
DECLARE
    definition text;
    revised text;
BEGIN
    SELECT pg_get_functiondef(
        'guard_runner_lease_generation()'::regprocedure
    ) INTO definition;
    revised := replace(
        definition,
        $old$    -- A profileless Confirm declaration accepts only a user-command
    -- decision or the frozen session blanket. Policy-auto provenance would
    -- bypass the confirmation the daemon-authoritative declaration records.
    IF NEW.credential_profile_name IS NULL
       AND registered_permission = 'confirm'
       AND NOT EXISTS (
            SELECT 1
              FROM tool_approval_decision AS approval
             WHERE approval.request_id = attempted_request
               AND approval.decision_kind = 'approve'
               AND approval.decision_source
                    IN ('user_command', 'session_blanket')
       )
    THEN
        RAISE EXCEPTION
            'profileless confirm lease admission requires confirmed approval provenance'
            USING ERRCODE = '23514';
    END IF;
$old$,
        ''
    );
    IF revised = definition THEN
        RAISE EXCEPTION
            'profileless runner approval correction did not find legacy guard';
    END IF;
    EXECUTE revised;
END;
$migration$;
