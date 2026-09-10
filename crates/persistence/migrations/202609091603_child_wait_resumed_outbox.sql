ALTER TABLE tool_batch_transition_outbox_event
    DROP CONSTRAINT tool_batch_transition_outbox_shape,
    DROP CONSTRAINT tool_batch_transition_outbox_state_closed;

ALTER TABLE tool_batch_transition_outbox_event
    ADD CONSTRAINT tool_batch_transition_outbox_shape CHECK (
        (
            transition_kind IN ('proposed', 'results_projected')
            AND frontier_id IS NOT NULL
            AND tool_attempt_id IS NULL
        )
        OR (
            transition_kind IN ('recovery_required', 'child_wait_resumed')
            AND frontier_id IS NULL
            AND tool_attempt_id IS NOT NULL
        )
    ),
    ADD CONSTRAINT tool_batch_transition_outbox_state_closed CHECK (
        transition_kind IN (
            'proposed',
            'results_projected',
            'recovery_required',
            'child_wait_resumed'
        )
    );
