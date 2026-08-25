-- Requeue every repository-watch dispatch that releases without converging.
-- Supersedes the release function from
-- 202608140100_repo_watch_dispatch_release.sql.

-- The superseded event/rule batch uniqueness is defined by
-- 202608030004_repo_watch_dispatch.sql. A termination-created obligation may owe
-- a second batch for the same event when no later matching event exists. Fresh
-- evaluation replay is serialized by its evaluation row, while obligation replay
-- is serialized by atomic settlement.
--
-- IF EXISTS because 202608150005_repo_watch_continuous_dispatch.sql drops the
-- same constraint for the same reason, and is already applied to the deployed
-- database this migration was renumbered behind. Whichever of the two applies
-- first supersedes the definition; the second must not fail the daemon's
-- startup migration over a constraint that is already gone.
ALTER TABLE repo_watch_dispatch_batch
    DROP CONSTRAINT IF EXISTS repo_watch_dispatch_batch_event_id_rule_id_rule_version_key;

-- A fresh batch delivers its originating event, but an obligation successor
-- replays a still-matching earlier event over the target's collapsed current
-- state. Comparing achievement against the originating event would therefore
-- requeue a successor that already carried the newest head, forever.
--
-- Every batch admitted from here on records this, so NULL means exactly one
-- thing: the batch was admitted before this column existed and its delivered
-- state is unknown. Unknown never seals. Reading NULL as the originating event
-- instead would silently skip work whenever a head returns to an earlier value
-- -- a successor given B, originating from A, achieving after the head reverts
-- to A, would compare A with A and seal without ever delivering it. The cost of
-- refusing to seal is one redelivery per batch still in flight at the upgrade,
-- which is bounded and self-correcting; the cost of guessing is silent loss.
-- Reconstructing the value is not available: the newest event recorded before
-- the batch was admitted is transaction-time state, not the accepted context.
-- This migration therefore writes no data.
ALTER TABLE repo_watch_dispatch_batch
    ADD COLUMN delivered_state_event_id uuid;

ALTER TABLE repo_watch_dispatch_batch
    ADD CONSTRAINT repo_watch_dispatch_batch_delivered_state_event_id_fkey
    FOREIGN KEY (delivered_state_event_id)
        REFERENCES repo_watch_event(event_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT;

CREATE FUNCTION repo_watch_stamp_dispatch_batch_delivered_state()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    -- Admission coalesces a fresh match into an outstanding obligation instead
    -- of dispatching it, and settles an obligation only after inserting its
    -- successor batch. An unsettled obligation on this singleton therefore
    -- identifies the successor exactly. The batch table is append-only, so the
    -- delivered state is recorded here rather than stamped after settlement.
    IF NOT EXISTS (
        SELECT 1
          FROM repo_watch_dispatch_obligation AS obligation
         WHERE obligation.rule_id = NEW.rule_id
           AND obligation.rule_version = NEW.rule_version
           AND obligation.singleton_scope = NEW.singleton_scope
           AND obligation.singleton_repository
                IS NOT DISTINCT FROM NEW.singleton_repository
           AND obligation.singleton_pull_request_number
                IS NOT DISTINCT FROM NEW.singleton_pull_request_number
           AND obligation.singleton_stack_root_pull_request_number
                IS NOT DISTINCT FROM NEW.singleton_stack_root_pull_request_number
           AND obligation.settled_kind IS NULL
    ) THEN
        NEW.delivered_state_event_id := NEW.event_id;
        RETURN NEW;
    END IF;
    SELECT (
            SELECT state.event_id
              FROM repo_watch_event AS state
             WHERE state.repository = origin.repository
               AND state.pull_request_number = origin.pull_request_number
             ORDER BY state.cursor_generation DESC,
                      state.event_ordinal DESC
             LIMIT 1
    )
      INTO NEW.delivered_state_event_id
      FROM repo_watch_event AS origin
     WHERE origin.event_id = NEW.event_id;
    RETURN NEW;
END;
$$;

CREATE TRIGGER repo_watch_dispatch_batch_records_delivered_state
BEFORE INSERT ON repo_watch_dispatch_batch
FOR EACH ROW
EXECUTE FUNCTION repo_watch_stamp_dispatch_batch_delivered_state();

-- The singleton advisory key admission takes, reproduced so that release
-- serializes against evaluation on the identical key.
CREATE FUNCTION repo_watch_dispatch_singleton_lock_key(
    rule_id text,
    rule_version bigint,
    singleton_scope text,
    singleton_repository text,
    singleton_pull_request_number numeric,
    singleton_stack_root_pull_request_number numeric
)
RETURNS text
LANGUAGE sql
IMMUTABLE
AS $$
    SELECT concat_ws(
        E'\x1f',
        'repo-watch',
        rule_id,
        rule_version::text,
        singleton_scope,
        coalesce(singleton_repository, ''),
        coalesce(singleton_pull_request_number::text, ''),
        coalesce(singleton_stack_root_pull_request_number::text, '')
    );
$$;

-- Owes one requeue for a terminating dispatch, or nothing when its delivered
-- state already converged. Separate from release so that the decision to owe and
-- the decision to release are each readable on their own; the release loop below
-- decides only which batches reach this.
CREATE FUNCTION repo_watch_owe_dispatch_requeue(
    candidate_dispatch_id uuid,
    completed_session_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    candidate_repository text;
    candidate_rule_id text;
    candidate_rule_version bigint;
    candidate_singleton_key text;
    terminal_goal_kind text;
BEGIN
    -- The terminal event that matters is the one ending the generation this
    -- dispatch commissioned, not whatever the session is doing now. A session
    -- whose sibling action still holds the batch unreleased may legally accept
    -- an unrelated successor goal, and reading that successor's termination
    -- would owe a requeue for work the dispatched generation already converged.
    SELECT current_goal.event_kind
      INTO terminal_goal_kind
      FROM goal_event AS current_goal
     WHERE current_goal.session_id = completed_session_id
       AND current_goal.generation = (
            SELECT dispatched_turn.goal_generation
              FROM repo_watch_dispatch_action AS action
              JOIN repo_watch_dispatch_delivery AS delivery
                ON delivery.dispatch_id = action.dispatch_id
               AND delivery.action_ordinal = action.action_ordinal
              JOIN goal_turn AS dispatched_turn
                ON dispatched_turn.session_id = action.session_id
               AND dispatched_turn.turn_id = delivery.turn_id
             WHERE action.dispatch_id = candidate_dispatch_id
               AND action.session_id = completed_session_id
       )
     ORDER BY current_goal.event_ordinal DESC
     LIMIT 1;

    SELECT origin.repository, batch.rule_id, batch.rule_version,
           repo_watch_dispatch_singleton_lock_key(
                batch.rule_id,
                batch.rule_version,
                batch.singleton_scope,
                batch.singleton_repository,
                batch.singleton_pull_request_number,
                batch.singleton_stack_root_pull_request_number
           )
      INTO candidate_repository, candidate_rule_id, candidate_rule_version,
           candidate_singleton_key
      FROM repo_watch_dispatch_batch AS batch
      JOIN repo_watch_event AS origin ON origin.event_id = batch.event_id
     WHERE batch.dispatch_id = candidate_dispatch_id;

    -- Termination runs inside the transaction that ends the goal, which
    -- already holds that session's row. Lifecycle-cutoff processing takes
    -- the repository advisory key and then waits for the same session row,
    -- so taking the repository key here would invert that order and deadlock
    -- a goal pass against a cutoff attempt. Termination therefore takes only
    -- keys no repository-key holder waits behind a session row for.
    --
    -- Fresh evaluation and obligation admission take the singleton key, so a
    -- match racing this termination waits and then joins the obligation
    -- through its settle-guarded update, rather than aborting on the
    -- active-singleton index. Deactivation is serialized by row: inserting
    -- into repo_watch_rule_deactivation takes a key-share lock on the
    -- activation row this locks exclusively, so the two cannot both pass
    -- their checks against a snapshot predating the other's row.
    PERFORM pg_advisory_xact_lock(hashtextextended(candidate_singleton_key, 0));

    PERFORM 1
      FROM repo_watch_rule_activation AS activation
     WHERE activation.repository = candidate_repository
       AND activation.rule_id = candidate_rule_id
       AND activation.rule_version = candidate_rule_version
       FOR UPDATE;

    -- A terminal session leaves the current dispatch state owed unless it
    -- achieved against state that is still current. The active-singleton
    -- index collapses sibling terminations and preserves any later matching
    -- event already recorded by ordinary evaluation.
    INSERT INTO repo_watch_dispatch_obligation
        (obligation_id, repository, rule_id, rule_version,
         singleton_scope, singleton_repository, singleton_pull_request_number,
         singleton_stack_root_pull_request_number, first_repository,
         first_event_id, latest_event_id, matched_event_count,
         blocking_dispatch_id)
    SELECT gen_random_uuid(), origin.repository, batch.rule_id,
           batch.rule_version, batch.singleton_scope,
           batch.singleton_repository, batch.singleton_pull_request_number,
           batch.singleton_stack_root_pull_request_number, origin.repository,
           origin.event_id, origin.event_id, 1, batch.dispatch_id
      FROM repo_watch_dispatch_batch AS batch
      JOIN repo_watch_event AS origin ON origin.event_id = batch.event_id
      LEFT JOIN repo_watch_event AS delivered
        ON delivered.event_id = batch.delivered_state_event_id
     WHERE batch.dispatch_id = candidate_dispatch_id
       AND EXISTS (
            SELECT 1
              FROM repo_watch_dispatch_action AS action
             WHERE action.dispatch_id = batch.dispatch_id
               AND action.session_id = completed_session_id
       )
       AND terminal_goal_kind IN ('blocked', 'achieved', 'user_stopped')
       -- A branch target carries no durable revision at all: its only event
       -- kind records a workflow conclusion, so achievement is its own seal.
       -- A pull-request target seals only when the state this batch delivered is
       -- known and is still the pull request's latest durable head.
       AND NOT (
            terminal_goal_kind = 'achieved'
            AND (
                origin.target_kind <> 'pull_request'
                OR (
                    batch.delivered_state_event_id IS NOT NULL
                    AND delivered.head_sha = (
                        SELECT current_state.head_sha
                          FROM repo_watch_event AS current_state
                         WHERE current_state.repository = origin.repository
                           AND current_state.pull_request_number
                                = origin.pull_request_number
                         ORDER BY current_state.cursor_generation DESC,
                                  current_state.event_ordinal DESC
                         LIMIT 1
                    )
                )
            )
       )
       AND NOT EXISTS (
            SELECT 1
              FROM repo_watch_rule_deactivation AS deactivation
             WHERE deactivation.repository = origin.repository
               AND deactivation.rule_id = batch.rule_id
               AND deactivation.rule_version = batch.rule_version
       )
       -- A later close or merge makes outstanding work stale. The cutoff event
       -- itself is the fact a rule may match, not work invalidated by that
       -- fact, so a dispatch of the latest cutoff keeps its requeue -- but only
       -- while that cutoff is still latest. A reopen makes the close obsolete,
       -- and requeueing it would run close automation against an open pull
       -- request, so the opened arm admits nonterminal origins only.
       AND (
            origin.target_kind <> 'pull_request'
            OR EXISTS (
                SELECT 1
                  FROM (
                        SELECT lifecycle.event_id, lifecycle.event_kind
                          FROM repo_watch_event AS lifecycle
                         WHERE lifecycle.repository = origin.repository
                           AND lifecycle.pull_request_number
                                = origin.pull_request_number
                           AND lifecycle.event_kind IN (
                                'pull_request_opened',
                                'pull_request_closed',
                                'pull_request_merged'
                           )
                         ORDER BY lifecycle.cursor_generation DESC,
                                  lifecycle.event_ordinal DESC
                         LIMIT 1
                  ) AS latest_lifecycle
                 WHERE (
                        latest_lifecycle.event_kind = 'pull_request_opened'
                        AND origin.event_kind NOT IN (
                            'pull_request_closed',
                            'pull_request_merged'
                        )
                 )
                    OR latest_lifecycle.event_id = origin.event_id
            )
       )
    -- Only an active obligation on this singleton may absorb a termination.
    -- A bare conflict clause would also swallow an identifier collision with
    -- an already settled obligation and silently drop the requeue.
    ON CONFLICT (rule_id, rule_version, singleton_scope, singleton_repository,
                 singleton_pull_request_number,
                 singleton_stack_root_pull_request_number)
        WHERE settled_kind IS NULL
    DO NOTHING;
END;
$$;

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
        -- A released batch has already accounted for its dispatched work. The
        -- session may afterwards accept an unrelated successor goal, whose own
        -- termination reaches this trigger through the same action link; owing a
        -- requeue for it would redispatch a repository-watch event that already
        -- converged. The release below is written after this call, so a batch
        -- releasing now is still unreleased here.
        IF NOT EXISTS (
            SELECT 1
              FROM repo_watch_dispatch_release AS released
             WHERE released.dispatch_id = candidate_dispatch_id
        ) THEN
            PERFORM repo_watch_owe_dispatch_requeue(
                candidate_dispatch_id,
                completed_session_id
            );
        END IF;

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
