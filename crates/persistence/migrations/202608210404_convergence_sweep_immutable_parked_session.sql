-- Persist the exact dispatch cohort selected by guarded inactivity parking.

ALTER TABLE convergence_sweep_target
    ADD COLUMN parked_dispatch_id uuid,
    ADD COLUMN parked_session_id uuid,
    ADD COLUMN parked_dispatched_at timestamptz;

UPDATE convergence_sweep_target AS target
   SET parked_dispatch_id = coalesce(
           target.census_dispatch_id, target.last_dispatch_id
       ),
       parked_session_id = coalesce(
           target.census_session_id, target.last_session_id
       ),
       parked_dispatched_at = coalesce((
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
       ), target.last_dispatched_at)
 WHERE target.state_kind = 'parked'
   AND target.failure_kind = 'no_model_activity';

ALTER TABLE convergence_sweep_target
    ADD CONSTRAINT convergence_sweep_parked_dispatch_shape CHECK (
        (parked_dispatch_id IS NULL
            AND parked_session_id IS NULL
            AND parked_dispatched_at IS NULL)
        OR
        (state_kind = 'parked'
            AND failure_kind = 'no_model_activity'
            AND parked_dispatch_id IS NOT NULL
            AND parked_session_id IS NOT NULL
            AND parked_dispatched_at IS NOT NULL)
    ),
    ADD CONSTRAINT convergence_sweep_inactivity_park_has_dispatch CHECK (
        state_kind <> 'parked'
        OR failure_kind <> 'no_model_activity'
        OR parked_dispatch_id IS NOT NULL
    );

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
            THEN target.parked_dispatch_id ELSE target.last_dispatch_id
       END AS last_dispatch_id,
       CASE WHEN target.failure_kind = 'no_model_activity'
            THEN target.parked_session_id ELSE target.last_session_id
       END AS last_session_id,
       CASE WHEN target.failure_kind = 'no_model_activity'
            THEN target.parked_dispatched_at ELSE target.last_dispatched_at
       END AS last_dispatched_at
  FROM convergence_sweep_target AS target
 WHERE target.enrolled AND target.state_kind = 'parked';
