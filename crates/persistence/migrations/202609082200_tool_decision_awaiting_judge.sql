ALTER TABLE decide_tool_request_command
    DROP CONSTRAINT decide_tool_request_command_rejection_closed,
    DROP CONSTRAINT decide_tool_request_command_result_shape,
    ADD CONSTRAINT decide_tool_request_command_rejection_closed CHECK (
        rejection_kind IS NULL OR rejection_kind IN (
            'request_not_found', 'already_resolved', 'not_earliest_undecided',
            'awaiting_approval_judge'
        )
    ),
    ADD CONSTRAINT decide_tool_request_command_result_shape CHECK (
        (result_kind = 'applied' AND rejection_kind IS NULL
            AND result_earliest_undecided_request_id IS NULL)
        OR (result_kind = 'rejected'
            AND rejection_kind IN ('request_not_found', 'already_resolved', 'awaiting_approval_judge')
            AND result_earliest_undecided_request_id IS NULL)
        OR (result_kind = 'rejected' AND rejection_kind = 'not_earliest_undecided'
            AND result_earliest_undecided_request_id IS NOT NULL
            AND result_earliest_undecided_request_id <> request_id)
    );

ALTER TABLE injection_settled_outbox_event
    DROP CONSTRAINT injection_settled_outbox_rejection_closed,
    ADD CONSTRAINT injection_settled_outbox_rejection_closed CHECK (
        rejection_kind IS NULL OR rejection_kind IN (
            'attachment_blob_not_found', 'attachment_byte_budget_exceeded',
            'no_active_turn', 'active_turn_present', 'active_turn_mismatch',
            'session_defaults_version_mismatch', 'unknown_model_alias',
            'acceptance_position_exhausted', 'interrupt_already_applied',
            'interrupt_unavailable_while_awaiting_approval', 'not_earliest_undecided',
            'awaiting_approval_judge'
        )
    );
