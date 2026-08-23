-- Keep timeline detail tied to the immutable facts visible when its outbox
-- transition committed.
ALTER TABLE goal_turn_retired_outbox_event
    ADD COLUMN goal_event_ordinal numeric(20, 0);

DROP TRIGGER goal_turn_retired_outbox_event_is_append_only
    ON goal_turn_retired_outbox_event;

UPDATE goal_turn_retired_outbox_event AS retired
   SET goal_event_ordinal = (
       SELECT event.event_ordinal
         FROM goal_turn AS goal
         JOIN goal_event AS event
           ON event.session_id = goal.session_id
          AND event.generation = goal.goal_generation
        WHERE goal.session_id = retired.session_id
          AND goal.turn_id = retired.turn_id
        ORDER BY event.event_ordinal DESC
        LIMIT 1
   );

CREATE TRIGGER goal_turn_retired_outbox_event_is_append_only
BEFORE UPDATE OR DELETE ON goal_turn_retired_outbox_event
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

ALTER TABLE goal_turn_retired_outbox_event
    ALTER COLUMN goal_event_ordinal SET NOT NULL,
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
            )
            OR (
                member_kind = 'goal'
                AND request_id IS NULL
                AND attempt_id IS NULL
                AND goal_event_ordinal IS NOT NULL
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

INSERT INTO tool_batch_transition_detail_member
    (event_sequence, session_id, member_kind, member_index, request_id, attempt_id)
SELECT event.event_sequence, event.session_id, 'tool',
       row_number() OVER (
           PARTITION BY event.event_sequence
           ORDER BY request.request_ordinal,
                    generation.generation NULLS FIRST,
                    attempt.attempt_id NULLS FIRST
       ) - 1,
       request.request_id, attempt.attempt_id
  FROM tool_batch_transition_outbox_event AS event
  JOIN tool_request AS request
    ON request.producing_model_call_id = event.producing_model_call_id
  LEFT JOIN tool_attempt AS attempt
    ON attempt.request_id = request.request_id
  LEFT JOIN LATERAL (
      SELECT lease.generation
        FROM runner_physical_attempt_lease_binding AS binding
        JOIN runner_lease_generation AS lease
          ON lease.lease_id = binding.lease_id
         AND lease.attempt_id = binding.attempt_id
       WHERE binding.attempt_id = attempt.attempt_id
       ORDER BY lease.generation DESC
       LIMIT 1
  ) AS generation ON TRUE;

INSERT INTO tool_batch_transition_detail_member
    (event_sequence, session_id, member_kind, member_index, goal_event_ordinal)
SELECT event.event_sequence, event.session_id, 'goal',
       row_number() OVER (
           PARTITION BY event.event_sequence
           ORDER BY request.request_ordinal, goal.event_ordinal
       ) - 1,
       goal.event_ordinal
  FROM tool_batch_transition_outbox_event AS event
  JOIN tool_request AS request
    ON request.producing_model_call_id = event.producing_model_call_id
  JOIN goal_event AS goal
    ON goal.model_tool_request_id = request.request_id;

CREATE TRIGGER tool_batch_transition_detail_member_is_append_only
BEFORE UPDATE OR DELETE ON tool_batch_transition_detail_member
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER tool_batch_transition_detail_member_cannot_be_truncated
BEFORE TRUNCATE ON tool_batch_transition_detail_member
FOR EACH STATEMENT
EXECUTE FUNCTION reject_outbox_table_truncate();
