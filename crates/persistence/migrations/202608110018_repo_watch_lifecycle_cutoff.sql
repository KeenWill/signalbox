-- Stop repository-watch commissions when their pull request closes.

ALTER TABLE repo_watch_rule_evaluation
    DROP CONSTRAINT repo_watch_rule_evaluation_outcome_kind_check,
    DROP CONSTRAINT repo_watch_rule_evaluation_check,
    ADD CHECK (outcome_kind IN (
        'not_matched', 'self_caused', 'target_closed', 'occupied',
        'cooldown', 'dispatched'
    )),
    ADD CHECK ((dispatch_id IS NOT NULL) = (outcome_kind = 'dispatched'));

CREATE TABLE repo_watch_lifecycle_cutoff (
    event_id uuid PRIMARY KEY,
    disposition_kind text NOT NULL CHECK (
        disposition_kind IN ('terminal', 'reopened')
    ),
    processed_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    FOREIGN KEY (event_id)
        REFERENCES repo_watch_event(event_id) ON UPDATE RESTRICT ON DELETE RESTRICT
);

CREATE TABLE repo_watch_lifecycle_cutoff_goal (
    event_id uuid NOT NULL,
    session_id uuid NOT NULL,
    goal_command_id uuid NOT NULL,
    PRIMARY KEY (event_id, session_id),
    UNIQUE (goal_command_id),
    FOREIGN KEY (event_id)
        REFERENCES repo_watch_lifecycle_cutoff(event_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    FOREIGN KEY (goal_command_id, session_id)
        REFERENCES goal_command(command_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
);

CREATE TRIGGER repo_watch_lifecycle_cutoff_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_lifecycle_cutoff
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_lifecycle_cutoff_goal_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_lifecycle_cutoff_goal
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_lifecycle_cutoff_reject_truncate
BEFORE TRUNCATE ON repo_watch_lifecycle_cutoff
FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();

CREATE TRIGGER repo_watch_lifecycle_cutoff_goal_reject_truncate
BEFORE TRUNCATE ON repo_watch_lifecycle_cutoff_goal
FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();

CREATE OR REPLACE FUNCTION repo_watch_release_completed_dispatch_batches_for_turn(
    completed_turn_id uuid,
    completed_session_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    candidate_dispatch_id uuid;
BEGIN
    FOR candidate_dispatch_id IN
        SELECT DISTINCT action.dispatch_id
          FROM repo_watch_dispatch_action AS action
         WHERE action.session_id = completed_session_id
         ORDER BY action.dispatch_id
    LOOP
        PERFORM 1
          FROM repo_watch_dispatch_batch AS locked_batch
         WHERE locked_batch.dispatch_id = candidate_dispatch_id
           FOR UPDATE;

        INSERT INTO repo_watch_dispatch_release (dispatch_id, released_at)
        SELECT batch.dispatch_id, clock_timestamp()
          FROM repo_watch_dispatch_batch AS batch
         WHERE batch.dispatch_id = candidate_dispatch_id
           AND NOT EXISTS (
                SELECT 1
                  FROM repo_watch_dispatch_release AS released
                 WHERE released.dispatch_id = batch.dispatch_id
           )
           AND batch.action_count = (
                SELECT count(*)
                  FROM repo_watch_dispatch_action AS action
                  JOIN repo_watch_dispatch_delivery AS delivery
                    ON delivery.dispatch_id = action.dispatch_id
                   AND delivery.action_ordinal = action.action_ordinal
                  JOIN turn_lifecycle AS turn
                    ON turn.turn_id = delivery.turn_id
                   AND (
                        turn.state_kind = 'terminal'
                        OR turn.turn_id = completed_turn_id
                        OR (
                            NOT goal_turn_is_runtime_relevant(
                                turn.session_id,
                                turn.turn_id
                            )
                            AND EXISTS (
                                SELECT 1
                                  FROM repo_watch_lifecycle_cutoff_goal AS cutoff_goal
                                 WHERE cutoff_goal.session_id = turn.session_id
                            )
                        )
                   )
                 WHERE action.dispatch_id = batch.dispatch_id
                   AND NOT EXISTS (
                        SELECT 1
                         FROM turn_lifecycle AS live_turn
                         WHERE live_turn.session_id = action.session_id
                           AND live_turn.state_kind <> 'terminal'
                           AND goal_turn_is_runtime_relevant(
                                live_turn.session_id,
                                live_turn.turn_id
                           )
                           AND (
                                completed_turn_id IS NULL
                                OR live_turn.turn_id <> completed_turn_id
                           )
                   )
                   AND NOT EXISTS (
                        SELECT 1
                          FROM goal_event AS current_goal
                         WHERE current_goal.session_id = action.session_id
                           AND current_goal.event_ordinal = (
                                SELECT max(candidate.event_ordinal)
                                  FROM goal_event AS candidate
                                 WHERE candidate.session_id = action.session_id
                           )
                           AND current_goal.event_kind IN (
                                'commissioned', 'resumed', 'superseded'
                           )
                   )
           )
        ON CONFLICT DO NOTHING;
    END LOOP;
END;
$$;
