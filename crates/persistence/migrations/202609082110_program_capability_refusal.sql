ALTER TABLE program_run_journal_entry
    DROP CONSTRAINT program_run_journal_entry_reject_shape,
    ADD CONSTRAINT program_run_journal_entry_reject_shape CHECK (
        (frame_kind = 'reject') = (reject_reason IS NOT NULL)
        AND (reject_reason IS NULL OR reject_reason IN (
            'outstanding_requests', 'capability_denied', 'unsupported_operation'
        ))
    );
