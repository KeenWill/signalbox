-- Bound how often one repository-watch obligation lineage may be redispatched.

-- The lineage is the singleton, not the row: settlement retires an obligation
-- and its requeue inserts a successor, so the count travels through the
-- settled row that names the dispatch being accounted for.
ALTER TABLE repo_watch_dispatch_obligation
    ADD COLUMN failed_attempts bigint NOT NULL DEFAULT 0,
    ADD COLUMN last_failed_attempt_at timestamptz,
    ADD COLUMN parked_at timestamptz;

ALTER TABLE repo_watch_dispatch_obligation
    ADD CONSTRAINT repo_watch_dispatch_obligation_failed_attempts_check
        CHECK (failed_attempts >= 0),
    ADD CONSTRAINT repo_watch_dispatch_obligation_failed_attempt_time_check
        CHECK ((last_failed_attempt_at IS NULL) = (failed_attempts = 0)),
    -- Settlement leaves the parking stamp in place as the record of why the
    -- obligation stopped being dispatched, so only an unfailed obligation is
    -- refused the stamp.
    ADD CONSTRAINT repo_watch_dispatch_obligation_parked_shape_check
        CHECK (parked_at IS NULL OR failed_attempts > 0);

CREATE TABLE repo_watch_dispatch_obligation_park (
    obligation_id uuid NOT NULL,
    transition_ordinal integer NOT NULL,
    transition_kind text NOT NULL,
    failed_attempts bigint NOT NULL,
    release_reason text,
    release_event_id uuid,
    release_actor text,
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    PRIMARY KEY (obligation_id, transition_ordinal),
    FOREIGN KEY (obligation_id)
        REFERENCES repo_watch_dispatch_obligation(obligation_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    FOREIGN KEY (release_event_id)
        REFERENCES repo_watch_event(event_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CHECK (transition_ordinal > 0),
    CHECK (transition_kind IN ('parked', 'released')),
    CHECK (failed_attempts >= 0),
    CHECK ((transition_kind = 'released') = (release_reason IS NOT NULL)),
    CHECK (
        release_reason IS NULL
        OR release_reason IN ('operator', 'pull_request_progress')
    ),
    -- Written with IS NOT DISTINCT FROM so the pairing is decided for a parked
    -- row too, where a plain equality would compare against NULL and admit any
    -- release detail on a transition that has no release.
    CHECK (
        (release_event_id IS NOT NULL)
        = (release_reason IS NOT DISTINCT FROM 'pull_request_progress')
    ),
    CHECK (
        (release_actor IS NOT NULL)
        = (release_reason IS NOT DISTINCT FROM 'operator')
    ),
    CHECK (release_actor IS NULL OR length(release_actor) BETWEEN 1 AND 200)
);

CREATE TRIGGER repo_watch_dispatch_obligation_park_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_dispatch_obligation_park
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_dispatch_obligation_park_reject_truncate
BEFORE TRUNCATE ON repo_watch_dispatch_obligation_park
FOR EACH STATEMENT
EXECUTE FUNCTION reject_repo_watch_table_truncate();

-- Appends one park transition, assigning its ordinal under the obligation row
-- the caller already holds. Every park and release goes through here so the
-- ordinal is minted in exactly one place.
CREATE FUNCTION repo_watch_record_dispatch_obligation_park_transition(
    subject_obligation_id uuid,
    kind text,
    attempts bigint,
    reason text,
    progress_event_id uuid,
    actor text
)
RETURNS void
LANGUAGE sql
AS $$
    INSERT INTO repo_watch_dispatch_obligation_park
        (obligation_id, transition_ordinal, transition_kind, failed_attempts,
         release_reason, release_event_id, release_actor)
    SELECT subject_obligation_id,
           coalesce(
               (SELECT max(park.transition_ordinal)
                  FROM repo_watch_dispatch_obligation_park AS park
                 WHERE park.obligation_id = subject_obligation_id),
               0
           ) + 1,
           kind,
           attempts,
           reason,
           progress_event_id,
           actor;
$$;

-- Returns a parked obligation to dispatch on an operator's say-so. The whole
-- budget is restored: an operator asking for another attempt is asking for the
-- same allowance a lineage that had never failed would get.
CREATE FUNCTION repo_watch_release_parked_dispatch_obligation(
    parked_obligation_id uuid,
    releasing_actor text
)
RETURNS boolean
LANGUAGE plpgsql
AS $$
DECLARE
    parked_attempts bigint;
BEGIN
    SELECT obligation.failed_attempts
      INTO parked_attempts
      FROM repo_watch_dispatch_obligation AS obligation
     WHERE obligation.obligation_id = parked_obligation_id
       AND obligation.settled_kind IS NULL
       AND obligation.parked_at IS NOT NULL
       FOR UPDATE;

    IF NOT FOUND THEN
        RETURN false;
    END IF;

    PERFORM repo_watch_record_dispatch_obligation_park_transition(
        parked_obligation_id,
        'released',
        parked_attempts,
        'operator',
        NULL,
        releasing_actor
    );

    UPDATE repo_watch_dispatch_obligation
       SET parked_at = NULL,
           failed_attempts = 0,
           last_failed_attempt_at = NULL
     WHERE obligation_id = parked_obligation_id;

    RETURN true;
END;
$$;

-- Supersedes the requeue from
-- 202608170002_repo_watch_dispatch_termination_obligation.sql, which owed a
-- successor without counting the attempt that produced it.
CREATE OR REPLACE FUNCTION repo_watch_owe_dispatch_requeue(
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
         blocking_dispatch_id, failed_attempts, last_failed_attempt_at)
    SELECT gen_random_uuid(), origin.repository, batch.rule_id,
           batch.rule_version, batch.singleton_scope,
           batch.singleton_repository, batch.singleton_pull_request_number,
           batch.singleton_stack_root_pull_request_number, origin.repository,
           origin.event_id, origin.event_id, 1, batch.dispatch_id,
           -- The settled predecessor carries the lineage count; a dispatch
           -- admitted from a fresh match settled none and starts the lineage.
           coalesce(settled.failed_attempts, 0) + 1,
           clock_timestamp()
      FROM repo_watch_dispatch_batch AS batch
      JOIN repo_watch_event AS origin ON origin.event_id = batch.event_id
      LEFT JOIN repo_watch_event AS delivered
        ON delivered.event_id = batch.delivered_state_event_id
      LEFT JOIN repo_watch_dispatch_obligation AS settled
        ON settled.settled_dispatch_id = batch.dispatch_id
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
    -- The absorbing obligation carries the lineage count forward. GREATEST
    -- rather than addition because sibling actions of one batch each reach
    -- this with the same successor count: one batch is one attempt however
    -- many of its sessions terminate, and a match that opened the row while
    -- the batch ran contributes a count of zero that must not erase the
    -- lineage.
    DO UPDATE SET
        failed_attempts = GREATEST(
            repo_watch_dispatch_obligation.failed_attempts,
            EXCLUDED.failed_attempts
        ),
        last_failed_attempt_at = clock_timestamp();
END;
$$;

-- Supersedes the projection from
-- 202608140003_repo_watch_dispatch_obligation.sql. Readiness now excludes a
-- parked obligation; the backoff between attempts is applied by the dispatch
-- loader against constants this projection cannot see, so an obligation
-- reported ready may still be waiting out its delay.
CREATE OR REPLACE VIEW repo_watch_outstanding_dispatch_obligation AS
SELECT obligation.obligation_id,
       obligation.repository,
       obligation.rule_id,
       obligation.rule_version,
       obligation.singleton_scope,
       obligation.singleton_repository,
       obligation.singleton_pull_request_number,
       obligation.singleton_stack_root_pull_request_number,
       obligation.first_repository,
       obligation.first_event_id,
       obligation.latest_event_id,
       obligation.matched_event_count,
       obligation.owed_since,
       obligation.latest_match_at,
       occupying.dispatch_id AS occupying_dispatch_id,
       occupying.session_ids AS occupying_session_ids,
       cooldown.eligible_at,
       occupying.dispatch_id IS NULL
           AND (cooldown.eligible_at IS NULL OR cooldown.eligible_at <= clock_timestamp())
           AND obligation.parked_at IS NULL
           AS ready,
       obligation.failed_attempts,
       obligation.last_failed_attempt_at,
       obligation.parked_at
  FROM repo_watch_dispatch_obligation AS obligation
  LEFT JOIN LATERAL (
        SELECT batch.dispatch_id,
               array_agg(action.session_id ORDER BY action.action_ordinal) AS session_ids
          FROM repo_watch_dispatch_batch AS batch
          JOIN repo_watch_dispatch_action AS action
            ON action.dispatch_id = batch.dispatch_id
         WHERE batch.rule_id = obligation.rule_id
           AND batch.rule_version = obligation.rule_version
           AND batch.singleton_scope = obligation.singleton_scope
           AND batch.singleton_repository
                IS NOT DISTINCT FROM obligation.singleton_repository
           AND batch.singleton_pull_request_number
                IS NOT DISTINCT FROM obligation.singleton_pull_request_number
           AND batch.singleton_stack_root_pull_request_number
                IS NOT DISTINCT FROM obligation.singleton_stack_root_pull_request_number
           AND NOT EXISTS (
                SELECT 1
                  FROM repo_watch_dispatch_release AS released
                 WHERE released.dispatch_id = batch.dispatch_id
           )
         GROUP BY batch.dispatch_id, batch.admitted_at
         ORDER BY batch.admitted_at
         LIMIT 1
  ) AS occupying ON true
  LEFT JOIN LATERAL (
        SELECT max(CASE
            WHEN batch.cooldown_seconds::numeric <= extract(epoch FROM (
                '294276-12-31 23:59:59+00'::timestamptz - released.released_at
            ))
            THEN released.released_at
                + batch.cooldown_seconds * interval '1 second'
            ELSE 'infinity'::timestamptz
        END) AS eligible_at
          FROM repo_watch_dispatch_release AS released
          JOIN repo_watch_dispatch_batch AS batch
            ON batch.dispatch_id = released.dispatch_id
         WHERE batch.rule_id = obligation.rule_id
           AND batch.rule_version = obligation.rule_version
           AND batch.singleton_scope = obligation.singleton_scope
           AND batch.singleton_repository
                IS NOT DISTINCT FROM obligation.singleton_repository
           AND batch.singleton_pull_request_number
                IS NOT DISTINCT FROM obligation.singleton_pull_request_number
           AND batch.singleton_stack_root_pull_request_number
                IS NOT DISTINCT FROM obligation.singleton_stack_root_pull_request_number
  ) AS cooldown ON true
 WHERE obligation.settled_kind IS NULL;

CREATE VIEW repo_watch_parked_dispatch_obligation AS
SELECT obligation.obligation_id,
       obligation.repository,
       obligation.rule_id,
       obligation.rule_version,
       obligation.singleton_scope,
       obligation.singleton_repository,
       obligation.singleton_pull_request_number,
       obligation.singleton_stack_root_pull_request_number,
       obligation.latest_event_id,
       obligation.matched_event_count,
       obligation.owed_since,
       obligation.latest_match_at,
       obligation.failed_attempts,
       obligation.last_failed_attempt_at,
       obligation.parked_at,
       parked_state.pull_request_number,
       parked_state.head_sha,
       (SELECT max(park.recorded_at)
          FROM repo_watch_dispatch_obligation_park AS park
         WHERE park.obligation_id = obligation.obligation_id
           AND park.transition_kind = 'parked') AS latest_park_recorded_at
  FROM repo_watch_dispatch_obligation AS obligation
  JOIN repo_watch_event AS parked_state
    ON parked_state.event_id = obligation.latest_event_id
 WHERE obligation.settled_kind IS NULL
   AND obligation.parked_at IS NOT NULL;

CREATE INDEX repo_watch_dispatch_obligation_parked
    ON repo_watch_dispatch_obligation (repository, rule_id, rule_version)
    WHERE settled_kind IS NULL AND parked_at IS NOT NULL;
