-- Preserve the model-identity entry when deriving the predecessor frontier for
-- a turn whose runner relocation advanced the session placement frontier.

DO $migration$
DECLARE
    definition text;
    revised text;
BEGIN
    definition := pg_get_functiondef(
        'assert_turn_lifecycle_final_state_without_steering(uuid)'::regprocedure
    );
    revised := replace(
        definition,
        'turn_lifecycle_origin_member_span(
                       checked_turn_id,
                       checked_session_id
                   )
               ) AS effective',
        'turn_lifecycle_origin_member_span(
                       checked_turn_id,
                       checked_session_id
                   )
                   + turn_start_model_identity_entry_count(
                       checked_turn_id,
                       checked_starting_frontier
                   )
               ) AS effective'
    );
    IF revised = definition THEN
        RAISE EXCEPTION
            'active-turn model-boundary insertion point is missing';
    END IF;
    EXECUTE revised;

    definition := pg_get_functiondef(
        'assert_terminal_started_turn_common_final_state(uuid)'::regprocedure
    );
    revised := replace(
        definition,
        'turn_lifecycle_origin_member_span(
                       checked_turn_id,
                       checked_session
                   )
               ) AS effective',
        'turn_lifecycle_origin_member_span(
                       checked_turn_id,
                       checked_session
                   )
                   + turn_start_model_identity_entry_count(
                       checked_turn_id,
                       checked_starting_frontier
                   )
               ) AS effective'
    );
    IF revised = definition THEN
        RAISE EXCEPTION
            'terminal-turn model-boundary insertion point is missing';
    END IF;
    EXECUTE revised;
END;
$migration$;
