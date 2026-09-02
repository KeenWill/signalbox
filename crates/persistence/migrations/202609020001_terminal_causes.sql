--
-- Session lifecycle §4: mandatory cause classification on turn terminalization.
--
-- Every turn that reaches `terminal` records exactly one typed cause from a
-- closed vocabulary; a non-terminal turn carries none. The unconstrained
-- dogfood-database reset is ratified, so no causeless terminal row survives to
-- be exempted and the shape constraint is validated rather than deferred — no
-- `unclassified_historical` spelling is owed.
--
-- `unclassified_failure` is the sole catch-all: §12 measures cause
-- completeness as the share of terminal turns carrying a cause outside that
-- set, so widening the catch-all silently weakens the acceptance criterion.
--
-- Every spelling below has a producing terminalization path except
-- `unclassified_failure`, which exists so a later path that genuinely cannot
-- classify has a legal spelling instead of a null.
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
-- A cause belongs to exactly one disposition.
--
-- The vocabulary constraint above admits any spelling anywhere, so a writer
-- could pair `failed` with `completed` and satisfy both rules while recording a
-- turn that says two contradictory things about how it ended. The map below is
-- a total function from cause to disposition, so every admissible pair is
-- named once and no other pair exists. Enforcing it here rather than in one
-- caller's argument type is what makes it hold for every write path — the
-- repositories, the raw statements the tests write, and whatever later
-- lifecycle work adds.
--

ALTER TABLE turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_terminal_cause_matches_disposition CHECK (
        (terminal_cause_kind IS NULL)
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
            ])))
    );
