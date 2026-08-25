-- A held singleton slot names a pull request only when the dispatch that took
-- it originated in a pull-request fact. A rule matching
-- `branch_workflow_run_completed` under `Rule` or `Repository` singleton scope
-- takes the same slot from a branch fact, whose `repo_watch_event` row carries
-- `pull_request_number IS NULL` and `workflow_branch IS NOT NULL` by the target
-- shape check in 202608030002_repo_watch.sql. The projection already passed the
-- null through; it did not carry the branch that stands in the pull request's
-- place, leaving a reader unable to name the origin of such a hold.
--
-- Supersedes the projection from 202608140100_repo_watch_dispatch_release.sql,
-- appending `workflow_branch` so the origin of every hold is nameable. The
-- pair is exclusive: exactly one of `pull_request_number` and
-- `workflow_branch` is non-null on every row.
CREATE OR REPLACE VIEW repo_watch_held_dispatch_slot AS
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
       ], NULL) AS blockers,
       origin.workflow_branch
  FROM repo_watch_dispatch_batch AS batch
  JOIN repo_watch_event AS origin ON origin.event_id = batch.event_id
 WHERE NOT EXISTS (
       SELECT 1
         FROM repo_watch_dispatch_release AS released
        WHERE released.dispatch_id = batch.dispatch_id
 );
