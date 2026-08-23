-- Keep current repository-watch operator reads bounded by their page sizes.

CREATE TABLE repo_watch_current_pull_request (
    repository text NOT NULL,
    pull_request_number numeric(20, 0) NOT NULL,
    cursor_generation bigint NOT NULL,
    lifecycle text NOT NULL,
    head_repository text NOT NULL,
    base_branch text NOT NULL,
    head_branch text NOT NULL,
    state_payload jsonb NOT NULL,

    PRIMARY KEY (repository, pull_request_number),
    FOREIGN KEY (repository, cursor_generation)
        REFERENCES repo_watch_cursor(repository, generation)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    CHECK (repo_watch_repository_is_valid(repository)),
    CHECK (pull_request_number > 0 AND pull_request_number <= 18446744073709551615),
    CHECK (cursor_generation > 0),
    CHECK (lifecycle IN ('open', 'closed', 'merged')),
    CHECK (repo_watch_repository_is_valid(head_repository)),
    CHECK (repo_watch_branch_is_valid(base_branch)),
    CHECK (repo_watch_branch_is_valid(head_branch)),
    CHECK (jsonb_typeof(state_payload) = 'object')
);

INSERT INTO repo_watch_current_pull_request (
    repository, pull_request_number, cursor_generation, lifecycle,
    head_repository, base_branch, head_branch, state_payload
)
SELECT latest.repository,
       (pull_request.value ->> 'number')::numeric,
       latest.generation,
       pull_request.value ->> 'lifecycle',
       pull_request.value ->> 'head_repository',
       pull_request.value ->> 'base_branch',
       pull_request.value ->> 'head_branch',
       pull_request.value
  FROM (
        SELECT DISTINCT ON (repository)
               repository, generation, cursor_payload
          FROM repo_watch_cursor
         ORDER BY repository, generation DESC
  ) AS latest
 CROSS JOIN LATERAL jsonb_array_elements(
       latest.cursor_payload -> 'state' -> 'pull_requests'
 ) AS pull_request(value);

ALTER TABLE repo_watch_rule_evaluation
    ADD COLUMN pull_request_number numeric(20, 0);

DROP TRIGGER repo_watch_rule_evaluation_is_append_only
    ON repo_watch_rule_evaluation;

UPDATE repo_watch_rule_evaluation AS evaluation
   SET pull_request_number = event.pull_request_number
  FROM repo_watch_event AS event
 WHERE event.event_id = evaluation.event_id;

CREATE TRIGGER repo_watch_rule_evaluation_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_rule_evaluation
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

ALTER TABLE repo_watch_rule_evaluation
    ADD CONSTRAINT repo_watch_rule_evaluation_pull_request_number_check
    CHECK (
        pull_request_number IS NULL
        OR (pull_request_number > 0 AND pull_request_number <= 18446744073709551615)
    );

CREATE INDEX repo_watch_current_pull_request_parent
    ON repo_watch_current_pull_request (
        repository, lifecycle, head_repository, head_branch, pull_request_number
    );

CREATE INDEX repo_watch_current_pull_request_children
    ON repo_watch_current_pull_request (
        repository, lifecycle, base_branch, pull_request_number
    );

CREATE INDEX repo_watch_rule_evaluation_actionable_event
    ON repo_watch_rule_evaluation (
        repository, pull_request_number, cursor_generation DESC,
        event_ordinal DESC, event_id
    )
    WHERE outcome_kind <> 'not_matched';

CREATE TABLE repo_watch_achieved_dispatch_settlement (
    dispatch_id uuid PRIMARY KEY,
    repository text NOT NULL,
    pull_request_number numeric(20, 0),
    event_id uuid NOT NULL,
    released_at timestamptz NOT NULL,

    FOREIGN KEY (dispatch_id)
        REFERENCES repo_watch_dispatch_release(dispatch_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    FOREIGN KEY (event_id)
        REFERENCES repo_watch_event(event_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    CHECK (repo_watch_repository_is_valid(repository)),
    CHECK (
        pull_request_number IS NULL
        OR (pull_request_number > 0 AND pull_request_number <= 18446744073709551615)
    )
);

CREATE INDEX repo_watch_achieved_dispatch_settlement_repository
    ON repo_watch_achieved_dispatch_settlement (
        repository, released_at DESC, dispatch_id DESC
    );

CREATE INDEX repo_watch_achieved_dispatch_settlement_pull_request
    ON repo_watch_achieved_dispatch_settlement (
        repository, pull_request_number, released_at DESC, dispatch_id DESC
    )
    WHERE pull_request_number IS NOT NULL;

CREATE FUNCTION project_repo_watch_achieved_dispatch_settlement()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO repo_watch_achieved_dispatch_settlement (
        dispatch_id, repository, pull_request_number, event_id, released_at
    )
    SELECT batch.dispatch_id, event.repository, event.pull_request_number,
           batch.event_id, NEW.released_at
      FROM repo_watch_dispatch_batch AS batch
      JOIN repo_watch_event AS event ON event.event_id = batch.event_id
     WHERE batch.dispatch_id = NEW.dispatch_id
       AND NOT EXISTS (
            SELECT 1
              FROM repo_watch_dispatch_action AS action
             WHERE action.dispatch_id = batch.dispatch_id
               AND NOT EXISTS (
                    SELECT 1
                      FROM repo_watch_dispatch_delivery AS delivery
                      JOIN goal_turn AS dispatched_turn
                        ON dispatched_turn.session_id = action.session_id
                       AND dispatched_turn.turn_id = delivery.turn_id
                      JOIN goal_event AS goal
                        ON goal.session_id = dispatched_turn.session_id
                       AND goal.generation = dispatched_turn.goal_generation
                       AND goal.event_ordinal = (
                            SELECT max(candidate.event_ordinal)
                              FROM goal_event AS candidate
                             WHERE candidate.session_id = dispatched_turn.session_id
                               AND candidate.generation = dispatched_turn.goal_generation
                       )
                       AND goal.event_kind = 'achieved'
                     WHERE delivery.dispatch_id = action.dispatch_id
                       AND delivery.action_ordinal = action.action_ordinal
               )
       )
    ON CONFLICT DO NOTHING;
    RETURN NULL;
END;
$$;

CREATE TRIGGER repo_watch_dispatch_release_projects_achievement
AFTER INSERT ON repo_watch_dispatch_release
FOR EACH ROW
EXECUTE FUNCTION project_repo_watch_achieved_dispatch_settlement();

INSERT INTO repo_watch_achieved_dispatch_settlement (
    dispatch_id, repository, pull_request_number, event_id, released_at
)
SELECT release.dispatch_id, event.repository, event.pull_request_number,
       batch.event_id, release.released_at
  FROM repo_watch_dispatch_release AS release
  JOIN repo_watch_dispatch_batch AS batch USING (dispatch_id)
  JOIN repo_watch_event AS event ON event.event_id = batch.event_id
 WHERE NOT EXISTS (
       SELECT 1
         FROM repo_watch_dispatch_action AS action
        WHERE action.dispatch_id = batch.dispatch_id
          AND NOT EXISTS (
               SELECT 1
                 FROM repo_watch_dispatch_delivery AS delivery
                 JOIN goal_turn AS dispatched_turn
                   ON dispatched_turn.session_id = action.session_id
                  AND dispatched_turn.turn_id = delivery.turn_id
                 JOIN goal_event AS goal
                   ON goal.session_id = dispatched_turn.session_id
                  AND goal.generation = dispatched_turn.goal_generation
                  AND goal.event_ordinal = (
                       SELECT max(candidate.event_ordinal)
                         FROM goal_event AS candidate
                        WHERE candidate.session_id = dispatched_turn.session_id
                          AND candidate.generation = dispatched_turn.goal_generation
                  )
                  AND goal.event_kind = 'achieved'
                WHERE delivery.dispatch_id = action.dispatch_id
                  AND delivery.action_ordinal = action.action_ordinal
          )
 );

CREATE TRIGGER repo_watch_achieved_dispatch_settlement_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_achieved_dispatch_settlement
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_achieved_dispatch_settlement_reject_truncate
BEFORE TRUNCATE ON repo_watch_achieved_dispatch_settlement
FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();
