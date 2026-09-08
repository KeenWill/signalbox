DROP INDEX compact_session_command_automatic_turn_once;

CREATE UNIQUE INDEX compact_session_command_automatic_turn_boundary
    ON compact_session_command (session_id, automatic_for_turn_id, requested_through_position)
    WHERE automatic_for_turn_id IS NOT NULL;
