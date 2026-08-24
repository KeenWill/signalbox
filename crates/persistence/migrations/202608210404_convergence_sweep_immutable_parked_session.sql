-- Persist the exact dispatch cohort selected by guarded inactivity parking.

ALTER TABLE convergence_sweep_target
    ADD COLUMN parked_dispatch_id uuid,
    ADD COLUMN parked_session_id uuid,
    ADD COLUMN parked_dispatched_at timestamptz;

-- 202608210400 through 202608210404 all land in the same unmerged slice, so no
-- database has ever held a convergence sweep target parked without these
-- columns: the shape below is the schema's only shape, and both constraints are
-- validated. Backfilling the columns, or admitting an unvalidated arm for rows
-- an intermediate 0400-0403 database could have held, would be data-upgrade
-- scaffolding for a schema that was never deployed.
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
    -- Every inactivity park carries an operator-visible dispatch identity, so
    -- the parked-target view never projects a null identity for one.
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
