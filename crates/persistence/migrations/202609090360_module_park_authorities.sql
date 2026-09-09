-- A restored target retains its dispatch identity until its scheduler nudge
-- is acknowledged; observed targets no longer hold module park authority.
ALTER TABLE convergence_sweep_target
    DROP CONSTRAINT convergence_sweep_parked_dispatch_shape,
    ADD CONSTRAINT convergence_sweep_parked_dispatch_shape CHECK (
        (parked_dispatch_id IS NULL AND parked_session_id IS NULL
            AND parked_dispatched_at IS NULL)
        OR (parked_dispatch_id IS NOT NULL AND parked_session_id IS NOT NULL
            AND parked_dispatched_at IS NOT NULL
            AND ((state_kind = 'parked' AND failure_kind = 'no_model_activity')
                OR (state_kind = 'observed' AND failure_kind IS NULL)))
    );
