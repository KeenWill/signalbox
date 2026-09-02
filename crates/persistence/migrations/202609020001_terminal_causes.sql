--
-- Session lifecycle §4: mandatory cause classification on turn terminalization.
--
-- `unclassified_failure` is the only catch-all, and the only spelling with no
-- producing terminalization path.
--

ALTER TABLE turn_lifecycle
    ADD COLUMN terminal_cause_kind text;

ALTER TABLE turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_terminal_cause_closed CHECK (
        (terminal_cause_kind IS NULL)
        OR (terminal_cause_kind = ANY (ARRAY[
            'completed'::text,
            'model_refusal'::text,
            'interrupt_applied'::text,
            'model_call_ambiguous'::text,
            'tool_attempt_ambiguous'::text,
            'model_call_failed'::text,
            'model_target_unavailable'::text,
            'attachment_preparation_failed'::text,
            'capability_preparation_failed'::text,
            'tool_round_limit_reached'::text,
            'tool_attempt_lost'::text,
            'credential_pool_exhausted'::text,
            'headless_approval_escalation'::text,
            'abandoned_at_restart'::text,
            'watchdog_stale_turn'::text,
            'context_headroom_exhausted'::text,
            'context_compaction_wall'::text,
            'context_compaction_failed'::text,
            'reported_usage_context_compaction_exhausted'::text,
            'reported_usage_context_still_exceeded'::text,
            'unclassified_failure'::text
        ]))
    );

ALTER TABLE turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_terminal_cause_required CHECK (
        (state_kind = 'terminal'::text) = (terminal_cause_kind IS NOT NULL)
    );

--
-- A cause belongs to exactly one disposition. The map is total, so a null
-- disposition is rejected rather than leaving the check to evaluate null.
--

ALTER TABLE turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_terminal_cause_matches_disposition CHECK (
        (terminal_cause_kind IS NULL)
        OR ((terminal_disposition_kind IS NOT NULL) AND (FALSE
        OR ((terminal_disposition_kind = 'completed'::text)
            AND (terminal_cause_kind = 'completed'::text))
        OR ((terminal_disposition_kind = 'refused'::text)
            AND (terminal_cause_kind = 'model_refusal'::text))
        OR ((terminal_disposition_kind = 'cancelled'::text)
            AND (terminal_cause_kind = 'interrupt_applied'::text))
        OR ((terminal_disposition_kind = 'reconciliation_required'::text)
            AND (terminal_cause_kind = ANY (ARRAY[
                'model_call_ambiguous'::text,
                'tool_attempt_ambiguous'::text
            ])))
        OR ((terminal_disposition_kind = 'failed'::text)
            AND (terminal_cause_kind = ANY (ARRAY[
                'model_call_failed'::text,
                'model_target_unavailable'::text,
                'attachment_preparation_failed'::text,
                'capability_preparation_failed'::text,
                'tool_round_limit_reached'::text,
                'tool_attempt_lost'::text,
                'credential_pool_exhausted'::text,
                'headless_approval_escalation'::text,
                'abandoned_at_restart'::text,
                'watchdog_stale_turn'::text,
                'context_headroom_exhausted'::text,
                'context_compaction_wall'::text,
                'context_compaction_failed'::text,
                'reported_usage_context_compaction_exhausted'::text,
                'reported_usage_context_still_exceeded'::text,
                'unclassified_failure'::text
            ])))))
    );
