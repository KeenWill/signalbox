ALTER TABLE outbox_event
    DROP CONSTRAINT outbox_event_storage_version_supported;

ALTER TABLE outbox_event
    ADD CONSTRAINT outbox_event_storage_version_supported CHECK (
        storage_version = CASE event_kind
            WHEN 'session_created' THEN 3
            WHEN 'tool_batch_transition' THEN 2
            ELSE 1
        END
        OR (event_kind = 'session_created' AND storage_version = 2)
        OR (event_kind = 'tool_batch_transition' AND storage_version = 1)
    );

ALTER TABLE tool_batch_transition_outbox_event
    DROP CONSTRAINT tool_batch_transition_outbox_shape,
    DROP CONSTRAINT tool_batch_transition_outbox_state_closed,
    DROP CONSTRAINT tool_batch_transition_outbox_version_supported;

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
    ),
    ADD CONSTRAINT tool_batch_transition_outbox_version_supported CHECK (
        storage_version IN (1, 2)
    );
