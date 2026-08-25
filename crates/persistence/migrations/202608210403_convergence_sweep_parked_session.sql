-- Project the exact inactive census session through the convergence operator view.

CREATE OR REPLACE VIEW convergence_sweep_parked_target AS
SELECT target.repository,
       target.pull_request_number,
       target.failure_kind,
       target.consecutive_failures,
       target.parked_at,
       target.operator_need,
       target.last_head_sha,
       target.last_unresolved_threads,
       CASE WHEN target.failure_kind = 'no_model_activity'
            THEN coalesce(target.census_dispatch_id, target.last_dispatch_id)
            ELSE target.last_dispatch_id
       END AS last_dispatch_id,
       CASE WHEN target.failure_kind = 'no_model_activity'
            THEN coalesce(target.census_session_id, target.last_session_id)
            ELSE target.last_session_id
       END AS last_session_id,
       CASE WHEN target.failure_kind = 'no_model_activity'
            THEN coalesce(census.recorded_at, target.last_dispatched_at)
            ELSE target.last_dispatched_at
       END AS last_dispatched_at
  FROM convergence_sweep_target AS target
  LEFT JOIN LATERAL (
       SELECT source.recorded_at
         FROM (
              SELECT dispatch.recorded_at
                FROM commissioned_dispatch AS dispatch
               WHERE dispatch.dispatch_id = target.census_dispatch_id
                 AND dispatch.session_id = target.census_session_id
              UNION ALL
              SELECT action.recorded_at
                FROM repo_watch_dispatch_action AS action
               WHERE action.dispatch_id = target.census_dispatch_id
                 AND action.session_id = target.census_session_id
         ) AS source
        ORDER BY source.recorded_at DESC
        LIMIT 1
  ) AS census ON true
 WHERE target.enrolled AND target.state_kind = 'parked';
