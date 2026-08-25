-- Keep timeline detail tied to the immutable facts visible when its outbox
-- transition committed.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM goal_turn_retired_outbox_event) THEN
        RAISE EXCEPTION
            'cannot reconstruct exact historical goal retirement correlations'
            USING ERRCODE = '23514',
                  CONSTRAINT =
                      'timeline_detail_exact_goal_retirement_required';
    END IF;
    IF EXISTS (SELECT 1 FROM tool_batch_transition_outbox_event) THEN
        RAISE EXCEPTION
            'cannot reconstruct exact historical tool transition members'
            USING ERRCODE = '23514',
                  CONSTRAINT =
                      'timeline_detail_exact_tool_members_required';
    END IF;
END;
$$;

ALTER TABLE goal_turn_retired_outbox_event
    ADD COLUMN goal_event_ordinal numeric(20, 0) NOT NULL,
    ADD CONSTRAINT goal_turn_retired_outbox_goal_event_fk
        FOREIGN KEY (session_id, goal_event_ordinal)
        REFERENCES goal_event (session_id, event_ordinal)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE tool_batch_transition_outbox_event
    ADD CONSTRAINT tool_batch_transition_outbox_event_session_key
        UNIQUE (event_sequence, session_id);

-- A frozen member row correlates its attempt to its exact request, so a
-- mixed-provenance member fails at insertion instead of projecting one
-- request's text with another attempt's evidence.
ALTER TABLE tool_attempt
    ADD CONSTRAINT tool_attempt_attempt_request_key
        UNIQUE (attempt_id, request_id);

CREATE TABLE tool_batch_transition_detail_member (
    event_sequence numeric(20, 0) NOT NULL,
    session_id uuid NOT NULL,
    member_kind text NOT NULL,
    member_index numeric(20, 0) NOT NULL,
    request_id uuid,
    attempt_id uuid,
    goal_event_ordinal numeric(20, 0),
    approval_judge_escalated boolean,
    attempt_state_kind text,
    attempt_terminal_disposition_kind text,
    attempt_error_kind text,
    attempt_has_result boolean,
    attempt_has_failure boolean,
    attempt_sandbox_posture text,
    attempt_result_text text,
    attempt_error_detail text,

    CONSTRAINT tool_batch_transition_detail_member_pk
        PRIMARY KEY (event_sequence, member_kind, member_index),
    CONSTRAINT tool_batch_transition_detail_member_kind_closed
        CHECK (member_kind IN ('tool', 'goal')),
    CONSTRAINT tool_batch_transition_detail_member_index_u32
        CHECK (member_index BETWEEN 0 AND 4294967295),
    CONSTRAINT tool_batch_transition_detail_member_shape
        CHECK (
            (
                member_kind = 'tool'
                AND request_id IS NOT NULL
                AND goal_event_ordinal IS NULL
                AND approval_judge_escalated IS NOT NULL
                AND (
                    (
                        attempt_id IS NULL
                        AND attempt_state_kind IS NULL
                        AND attempt_terminal_disposition_kind IS NULL
                        AND attempt_error_kind IS NULL
                        AND attempt_has_result IS NULL
                        AND attempt_has_failure IS NULL
                        AND attempt_sandbox_posture IS NULL
                        AND attempt_result_text IS NULL
                        AND attempt_error_detail IS NULL
                    )
                    OR (
                        attempt_id IS NOT NULL
                        AND attempt_state_kind IS NOT NULL
                        AND attempt_has_result IS NOT NULL
                        AND attempt_has_failure IS NOT NULL
                        AND attempt_has_result =
                            (attempt_result_text IS NOT NULL)
                        AND attempt_has_failure =
                            (attempt_error_detail IS NOT NULL)
                    )
                )
            )
            OR (
                member_kind = 'goal'
                AND request_id IS NULL
                AND attempt_id IS NULL
                AND goal_event_ordinal IS NOT NULL
                AND approval_judge_escalated IS NULL
                AND attempt_state_kind IS NULL
                AND attempt_terminal_disposition_kind IS NULL
                AND attempt_error_kind IS NULL
                AND attempt_has_result IS NULL
                AND attempt_has_failure IS NULL
                AND attempt_sandbox_posture IS NULL
                AND attempt_result_text IS NULL
                AND attempt_error_detail IS NULL
            )
        ),
    CONSTRAINT tool_batch_transition_detail_member_sandbox_closed
        CHECK (
            attempt_sandbox_posture IS NULL
            OR attempt_sandbox_posture IN ('unsandboxed', 'sandboxed')
        ),
    CONSTRAINT tool_batch_transition_detail_member_attempt_state_closed
        CHECK (
            attempt_state_kind IS NULL
            OR (
                attempt_state_kind IN ('prepared', 'in_flight')
                AND attempt_terminal_disposition_kind IS NULL
            )
            OR (
                attempt_state_kind = 'terminal'
                AND attempt_terminal_disposition_kind IN (
                    'completed',
                    'known_failed',
                    'awaiting_child',
                    'ambiguous'
                )
            )
        ),
    CONSTRAINT tool_batch_transition_detail_member_attempt_error_closed
        CHECK (
            attempt_error_kind IS NULL
            OR attempt_error_kind IN (
                'unknown_tool',
                'invalid_arguments',
                'execution_failed',
                'result_too_large',
                'crash_lost'
            )
        ),
    CONSTRAINT tool_batch_transition_detail_member_event_fk
        FOREIGN KEY (event_sequence, session_id)
        REFERENCES tool_batch_transition_outbox_event
            (event_sequence, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT tool_batch_transition_detail_member_request_fk
        FOREIGN KEY (request_id, session_id)
        REFERENCES tool_request (request_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT tool_batch_transition_detail_member_attempt_fk
        FOREIGN KEY (attempt_id, request_id)
        REFERENCES tool_attempt (attempt_id, request_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT tool_batch_transition_detail_member_goal_event_fk
        FOREIGN KEY (session_id, goal_event_ordinal)
        REFERENCES goal_event (session_id, event_ordinal)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER tool_batch_transition_detail_member_is_append_only
BEFORE UPDATE OR DELETE ON tool_batch_transition_detail_member
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER tool_batch_transition_detail_member_cannot_be_truncated
BEFORE TRUNCATE ON tool_batch_transition_detail_member
FOR EACH STATEMENT
EXECUTE FUNCTION reject_outbox_table_truncate();
