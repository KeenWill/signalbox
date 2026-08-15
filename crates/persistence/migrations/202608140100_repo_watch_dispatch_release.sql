-- Release repository-watch singleton slots from work that can no longer run,
-- stop commissions whose pull request is terminal, and expose every hold.

-- Supersedes the outcome vocabulary from
-- 202608140003_repo_watch_dispatch_obligation.sql.
ALTER TABLE repo_watch_rule_evaluation
    DROP CONSTRAINT repo_watch_rule_evaluation_outcome_kind_check;

ALTER TABLE repo_watch_rule_evaluation
    ADD CONSTRAINT repo_watch_rule_evaluation_outcome_kind_check
    CHECK (outcome_kind IN (
        'not_matched', 'target_closed', 'occupied', 'coalesced',
        'cooldown', 'dispatched'
    ));

-- Supersedes the settlement vocabulary and shape from
-- 202608140003_repo_watch_dispatch_obligation.sql.
ALTER TABLE repo_watch_dispatch_obligation
    DROP CONSTRAINT repo_watch_dispatch_obligation_settled_kind_check,
    DROP CONSTRAINT repo_watch_dispatch_obligation_check1;

ALTER TABLE repo_watch_dispatch_obligation
    ADD CONSTRAINT repo_watch_dispatch_obligation_settled_kind_check
    CHECK (
        settled_kind IS NULL
        OR settled_kind IN ('dispatched', 'deactivated', 'target_closed')
    ),
    ADD CONSTRAINT repo_watch_dispatch_obligation_settlement_shape_check
    CHECK (
        (settled_kind IS NULL
            AND settled_dispatch_id IS NULL
            AND settled_at IS NULL)
        OR (settled_kind = 'dispatched'
            AND settled_dispatch_id IS NOT NULL
            AND settled_at IS NOT NULL)
        OR (settled_kind IN ('deactivated', 'target_closed')
            AND settled_dispatch_id IS NULL
            AND settled_at IS NOT NULL)
    );

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
                        OR NOT goal_turn_is_runtime_relevant(
                            turn.session_id,
                            turn.turn_id
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

-- The original immediate trigger observes a stop before its termination
-- cascade has made an active turn runtime-terminal. Recheck at transaction end.
DROP TRIGGER repo_watch_dispatch_release_on_terminal_goal ON goal_event;

CREATE CONSTRAINT TRIGGER repo_watch_dispatch_release_on_terminal_goal
AFTER INSERT ON goal_event
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION repo_watch_release_completed_dispatch_batches_for_goal();

CREATE VIEW repo_watch_held_dispatch_slot AS
SELECT batch.dispatch_id,
       origin.repository,
       origin.pull_request_number,
       batch.rule_id,
       batch.rule_version,
       batch.singleton_scope,
       batch.singleton_repository,
       batch.singleton_pull_request_number,
       batch.singleton_stack_root_pull_request_number,
       batch.admitted_at AS held_since,
       ARRAY(
           SELECT action.session_id
             FROM repo_watch_dispatch_action AS action
            WHERE action.dispatch_id = batch.dispatch_id
            ORDER BY action.action_ordinal
       ) AS session_ids,
       batch.action_count = (
           SELECT count(*)
             FROM repo_watch_dispatch_action AS action
             JOIN repo_watch_dispatch_delivery AS delivery
               ON delivery.dispatch_id = action.dispatch_id
              AND delivery.action_ordinal = action.action_ordinal
            WHERE action.dispatch_id = batch.dispatch_id
       ) AS every_action_delivered,
       batch.action_count = (
           SELECT count(*)
             FROM repo_watch_dispatch_action AS action
             JOIN repo_watch_dispatch_delivery AS delivery
               ON delivery.dispatch_id = action.dispatch_id
              AND delivery.action_ordinal = action.action_ordinal
             JOIN turn_lifecycle AS turn ON turn.turn_id = delivery.turn_id
              AND (
                  turn.state_kind = 'terminal'
                  OR NOT goal_turn_is_runtime_relevant(turn.session_id, turn.turn_id)
              )
            WHERE action.dispatch_id = batch.dispatch_id
       ) AS every_delivery_turn_releasable,
       NOT EXISTS (
           SELECT 1
             FROM repo_watch_dispatch_action AS action
             JOIN turn_lifecycle AS live_turn ON live_turn.session_id = action.session_id
            WHERE action.dispatch_id = batch.dispatch_id
              AND live_turn.state_kind <> 'terminal'
              AND goal_turn_is_runtime_relevant(live_turn.session_id, live_turn.turn_id)
       ) AS no_live_runtime_turn,
       NOT EXISTS (
           SELECT 1
             FROM repo_watch_dispatch_action AS action
             JOIN goal_event AS current_goal ON current_goal.session_id = action.session_id
            WHERE action.dispatch_id = batch.dispatch_id
              AND current_goal.event_ordinal = (
                  SELECT max(candidate.event_ordinal)
                    FROM goal_event AS candidate
                   WHERE candidate.session_id = action.session_id
              )
              AND current_goal.event_kind IN ('commissioned', 'resumed', 'superseded')
       ) AS every_goal_nonpursuing,
       ARRAY_REMOVE(ARRAY[
           CASE WHEN batch.action_count <> (
               SELECT count(*)
                 FROM repo_watch_dispatch_action AS action
                 JOIN repo_watch_dispatch_delivery AS delivery
                   ON delivery.dispatch_id = action.dispatch_id
                  AND delivery.action_ordinal = action.action_ordinal
                WHERE action.dispatch_id = batch.dispatch_id
           ) THEN 'undelivered_action'::text END,
           CASE WHEN batch.action_count <> (
               SELECT count(*)
                 FROM repo_watch_dispatch_action AS action
                 JOIN repo_watch_dispatch_delivery AS delivery
                   ON delivery.dispatch_id = action.dispatch_id
                  AND delivery.action_ordinal = action.action_ordinal
                 JOIN turn_lifecycle AS turn ON turn.turn_id = delivery.turn_id
                  AND (
                      turn.state_kind = 'terminal'
                      OR NOT goal_turn_is_runtime_relevant(turn.session_id, turn.turn_id)
                  )
                WHERE action.dispatch_id = batch.dispatch_id
           ) THEN 'delivery_turn_runtime_relevant'::text END,
           CASE WHEN EXISTS (
               SELECT 1
                 FROM repo_watch_dispatch_action AS action
                 JOIN turn_lifecycle AS live_turn ON live_turn.session_id = action.session_id
                WHERE action.dispatch_id = batch.dispatch_id
                  AND live_turn.state_kind <> 'terminal'
                  AND goal_turn_is_runtime_relevant(live_turn.session_id, live_turn.turn_id)
           ) THEN 'live_runtime_turn'::text END,
           CASE WHEN EXISTS (
               SELECT 1
                 FROM repo_watch_dispatch_action AS action
                 JOIN goal_event AS current_goal ON current_goal.session_id = action.session_id
                WHERE action.dispatch_id = batch.dispatch_id
                  AND current_goal.event_ordinal = (
                      SELECT max(candidate.event_ordinal)
                        FROM goal_event AS candidate
                       WHERE candidate.session_id = action.session_id
                  )
                  AND current_goal.event_kind IN ('commissioned', 'resumed', 'superseded')
           ) THEN 'pursuing_goal'::text END
       ], NULL) AS blockers
  FROM repo_watch_dispatch_batch AS batch
  JOIN repo_watch_event AS origin ON origin.event_id = batch.event_id
 WHERE NOT EXISTS (
       SELECT 1
         FROM repo_watch_dispatch_release AS released
        WHERE released.dispatch_id = batch.dispatch_id
 );

-- Re-evaluate existing holds through the corrected predicate. This writes only
-- releases whose complete safety predicate now holds.
DO $$
DECLARE
    held_session_id uuid;
BEGIN
    FOR held_session_id IN
        SELECT DISTINCT action.session_id
          FROM repo_watch_dispatch_action AS action
          JOIN repo_watch_dispatch_batch AS batch
            ON batch.dispatch_id = action.dispatch_id
         WHERE NOT EXISTS (
               SELECT 1
                 FROM repo_watch_dispatch_release AS released
                WHERE released.dispatch_id = batch.dispatch_id
         )
         ORDER BY action.session_id
    LOOP
        PERFORM repo_watch_release_completed_dispatch_batches_for_turn(
            NULL::uuid,
            held_session_id
        );
    END LOOP;
END;
$$;
