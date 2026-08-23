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

CREATE TABLE tool_batch_transition_detail_member (
    event_sequence numeric(20, 0) NOT NULL,
    session_id uuid NOT NULL,
    member_kind text NOT NULL,
    member_index numeric(20, 0) NOT NULL,
    request_id uuid,
    attempt_id uuid,
    goal_event_ordinal numeric(20, 0),
    approval_judge_escalated boolean,

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
            )
            OR (
                member_kind = 'goal'
                AND request_id IS NULL
                AND attempt_id IS NULL
                AND goal_event_ordinal IS NOT NULL
                AND approval_judge_escalated IS NULL
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
        FOREIGN KEY (request_id)
        REFERENCES tool_request (request_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT tool_batch_transition_detail_member_attempt_fk
        FOREIGN KEY (attempt_id)
        REFERENCES tool_attempt (attempt_id)
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
