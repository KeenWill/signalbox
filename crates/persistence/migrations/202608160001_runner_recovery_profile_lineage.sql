-- Authenticate runner-recovery leases through uninterrupted profile-only
-- placement successors without rewriting the already-applied recovery schema.

CREATE FUNCTION runner_lease_placement_reaches_loss_revision(
    checked_session_id uuid,
    leased_event_ordinal numeric,
    checked_loss_revision numeric,
    checked_runner_id uuid
)
RETURNS boolean
LANGUAGE sql
STABLE
AS $$
    SELECT EXISTS (
        SELECT 1
          FROM runner_session_placement_record AS leased_placement
          JOIN runner_session_placement_record AS loss_placement
            ON loss_placement.session_id = leased_placement.session_id
           AND loss_placement.event_kind = 'runner_lost'
           AND loss_placement.state_kind = 'runner_lost'
           AND loss_placement.lost_runner_id = checked_runner_id
           AND loss_placement.placement_revision = checked_loss_revision
           AND loss_placement.event_ordinal > leased_placement.event_ordinal
         WHERE leased_placement.session_id = checked_session_id
           AND leased_placement.event_ordinal = leased_event_ordinal
           AND leased_placement.state_kind = 'pinned'
           AND leased_placement.pinned_runner_id = checked_runner_id
           AND (
                leased_placement.placement_revision = checked_loss_revision
                OR (
                    leased_placement.placement_revision < checked_loss_revision
                    AND EXISTS (
                        SELECT 1
                          FROM runner_session_placement_record AS successor
                         WHERE successor.session_id = checked_session_id
                           AND successor.event_ordinal > leased_event_ordinal
                           AND successor.event_ordinal < loss_placement.event_ordinal
                    )
                    AND NOT EXISTS (
                        SELECT 1
                          FROM runner_session_placement_record AS successor
                         WHERE successor.session_id = checked_session_id
                           AND successor.event_ordinal > leased_event_ordinal
                           AND successor.event_ordinal < loss_placement.event_ordinal
                           AND successor.event_kind <> 'profile_replaced'
                    )
                )
           )
    );
$$;

DO $migration$
DECLARE
    current_definition text;
    replacement_definition text;
BEGIN
    SELECT pg_get_functiondef(
        'assert_runner_placement_interrupted_attempt_complete(uuid,numeric)'::regprocedure
    ) INTO current_definition;
    replacement_definition := replace(
        current_definition,
        E'               AND leased_placement.placement_revision =\n                    placement.placement_revision',
        E'               AND runner_lease_placement_reaches_loss_revision(\n                    lease.session_id,\n                    lease.placement_event_ordinal,\n                    placement.placement_revision,\n                    placement.lost_runner_id\n               )'
    );
    IF replacement_definition = current_definition THEN
        RAISE EXCEPTION
            'runner placement interrupted-attempt lineage predicate is missing';
    END IF;
    EXECUTE replacement_definition;

    SELECT pg_get_functiondef(
        'assert_turn_runner_recovery_complete(uuid,uuid)'::regprocedure
    ) INTO current_definition;
    replacement_definition := replace(
        current_definition,
        E'               AND leased_placement.placement_revision =\n                    lifecycle.runner_recovery_placement_revision',
        E'               AND runner_lease_placement_reaches_loss_revision(\n                    lease.session_id,\n                    lease.placement_event_ordinal,\n                    lifecycle.runner_recovery_placement_revision,\n                    lifecycle.runner_recovery_runner_id\n               )'
    );
    IF replacement_definition = current_definition THEN
        RAISE EXCEPTION 'turn runner-recovery lineage predicate is missing';
    END IF;
    EXECUTE replacement_definition;

    SELECT pg_get_functiondef(
        'require_interrupt_submit_input_effect_correlation()'::regprocedure
    ) INTO current_definition;
    replacement_definition := replace(
        current_definition,
        E'                           AND leased_placement.placement_revision =\n                                effect.placement_revision',
        E'                           AND runner_lease_placement_reaches_loss_revision(\n                                lease.session_id,\n                                lease.placement_event_ordinal,\n                                effect.placement_revision,\n                                effect.runner_id\n                           )'
    );
    IF replacement_definition = current_definition THEN
        RAISE EXCEPTION 'runner-recovery interrupt lineage predicate is missing';
    END IF;
    EXECUTE replacement_definition;
END;
$migration$;
