-- A delegate denial now surfaces its judge rationale as the denial reason the
-- denied session reads, derived under the reason's tighter bounds (control
-- characters become spaces, surrounding whitespace is trimmed, text is cut to
-- 1024 bytes). The previous shape forced delegate denials to carry no reason,
-- which rendered every judge denial as an unexplained `detail: null`. Rows
-- written before this migration keep their absent reason; readers treat that
-- shape as legacy rather than corruption.

-- The deny branch no longer enumerates decision sources: the source-shape
-- constraint already restricts automatic sources to approvals, so every
-- admitted denial simply carries an absent or checked reason. The character
-- checks name exact scalar sets rather than POSIX classes because the domain
-- validator's sets are byte-precise while `[[:cntrl:]]`/`[[:space:]]` follow
-- the database collation: a reason with an admitted Unicode separator at an
-- edge (for example EM SPACE) must insert, not roll back the completing
-- judge call. Control means C0, DEL, and C1; edge whitespace means exactly
-- the six POSIX ASCII bytes.

ALTER TABLE tool_approval_decision
    -- Supersedes tool_approval_decision_shape as recreated by
    -- 202608110001_user_role_storage_vocabulary.sql (originally from
    -- 202608020015_llm_delegated_tool_approval.sql).
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
                        AND denial_reason !~ e'[\\x01-\\x1f\\x7f]'
                        AND denial_reason !~ e'[\\u0080-\\u009f]'
                        AND denial_reason !~ e'^[ \\t\\n\\x0b\\x0c\\r]'
                        AND denial_reason !~ e'[ \\t\\n\\x0b\\x0c\\r]$'
                    )
                )
            )
        );
