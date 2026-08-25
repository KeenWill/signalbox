-- A fresh repository-watch match can be blocked either by another watch
-- dispatch or by an independently commissioned live session for the same pull
-- request. Retain the exact blocker so the obligation is durable and becomes
-- ready only after that external session stops being live.
ALTER TABLE repo_watch_dispatch_obligation
    ADD COLUMN external_blocking_session_id uuid,
    ALTER COLUMN blocking_dispatch_id DROP NOT NULL;

ALTER TABLE repo_watch_dispatch_obligation
    ADD CONSTRAINT repo_watch_dispatch_obligation_external_session_id_fkey
        FOREIGN KEY (external_blocking_session_id)
        REFERENCES session(session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    ADD CONSTRAINT repo_watch_dispatch_obligation_blocker_shape_check
        CHECK (num_nonnulls(blocking_dispatch_id, external_blocking_session_id) = 1);

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
       coalesce(
           occupying.session_ids,
           CASE WHEN external_blocker.session_id IS NULL THEN NULL
                ELSE ARRAY[external_blocker.session_id]::uuid[] END
       ) AS occupying_session_ids,
       cooldown.eligible_at,
       occupying.dispatch_id IS NULL
           AND external_blocker.session_id IS NULL
           AND (cooldown.eligible_at IS NULL OR cooldown.eligible_at <= clock_timestamp())
           AND obligation.parked_at IS NULL
           AND obligation.failed_attempts < repo_watch_dispatch_attempt_budget()
           AS ready,
       obligation.failed_attempts,
       obligation.last_failed_attempt_at,
       obligation.parked_at,
       obligation.external_blocking_session_id
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
        SELECT obligation.external_blocking_session_id AS session_id
         WHERE (
               SELECT event.event_kind IN ('commissioned', 'resumed', 'superseded')
                 FROM goal_event AS event
                WHERE event.session_id = obligation.external_blocking_session_id
                ORDER BY event.event_ordinal DESC
                LIMIT 1
           )
  ) AS external_blocker ON true
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
