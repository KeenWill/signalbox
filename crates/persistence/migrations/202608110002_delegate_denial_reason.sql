-- A delegate denial now surfaces its judge rationale as the denial reason the
-- denied session reads, derived under the reason's tighter bounds (control
-- characters become spaces, surrounding whitespace is trimmed, text is cut to
-- 1024 bytes). The previous shape forced delegate denials to carry no reason,
-- which rendered every judge denial as an unexplained `detail: null`. Rows
-- written before this migration keep their absent reason; readers treat that
-- shape as legacy rather than corruption.

-- The deny branch no longer enumerates decision sources: the source-shape
-- constraint already restricts automatic sources to approvals, so every
-- admitted denial simply carries an absent or checked reason.

ALTER TABLE tool_approval_decision
    -- Supersedes tool_approval_decision_shape from
    -- 202608020015_llm_delegated_tool_approval.sql.
    DROP CONSTRAINT tool_approval_decision_shape,
    ADD CONSTRAINT tool_approval_decision_shape
        CHECK (
            (decision_kind = 'approve' AND denial_reason IS NULL)
            OR (
                decision_kind = 'deny'
                AND (
                    denial_reason IS NULL
                    OR (
                        octet_length(denial_reason) BETWEEN 1 AND 1024
                        AND denial_reason !~ '[[:cntrl:]]'
                        AND denial_reason !~ '^[[:space:]]'
                        AND denial_reason !~ '[[:space:]]$'
                    )
                )
            )
        );
