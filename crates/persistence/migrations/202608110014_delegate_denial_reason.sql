-- A delegate denial now surfaces its judge rationale as the denial reason the
-- denied session reads, derived under the reason's tighter bounds (control
-- characters become spaces, edge spaces are trimmed, text is cut to 1024
-- bytes on a character boundary). The previous shape forced delegate denials
-- to carry no reason, which rendered every judge denial as an unexplained
-- `detail: null`. Existing delegate denials are backfilled with the same
-- derivation, so a null reason afterwards means exactly one thing
-- everywhere: the rationale sanitizes to nothing. Readers reject every other
-- null as corruption rather than tolerating a shape no deployment produces.

-- The old shape constraint requires delegate denials to keep a null reason,
-- and DISABLE TRIGGER USER does not touch CHECK constraints, so the swap
-- must precede the backfill or the backfill aborts on the first populated
-- database. The character checks in the replacement name exact scalar sets
-- rather than POSIX classes because the domain validator's sets are
-- byte-precise while `[[:cntrl:]]`/`[[:space:]]` follow the database
-- collation: a reason with an admitted Unicode separator at an edge (for
-- example EM SPACE) must insert, not roll back the completing judge call.
-- Control means C0, DEL, and C1; edge whitespace means exactly the six
-- POSIX ASCII bytes.

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

-- The backfill mirrors ToolDenialReason::from_rationale exactly: control
-- scalars become spaces, edge spaces trim, and oversized text drops one
-- character at a time until it fits the 1024-byte bound — the same
-- largest-character-prefix cut the domain derivation takes — then trims
-- trailing spaces again. The decision table is append-only behind a BEFORE
-- UPDATE trigger that raises unconditionally, so the rewrite runs with user
-- triggers disabled, following 202608110001's rewrite discipline: DISABLE
-- TRIGGER USER needs only table ownership and leaves foreign-key
-- enforcement intact. A fresh database has no rows here and every statement
-- is a no-op.
ALTER TABLE tool_approval_decision DISABLE TRIGGER USER;

DO $$
DECLARE
    denial RECORD;
    derived text;
BEGIN
    FOR denial IN
        SELECT request_id, rationale
          FROM tool_approval_decision
         WHERE decision_source = 'delegate'
           AND decision_kind = 'deny'
           AND denial_reason IS NULL
    LOOP
        derived := btrim(
            regexp_replace(
                denial.rationale,
                e'[\\x01-\\x1f\\x7f\\u0080-\\u009f]',
                ' ',
                'g'
            ),
            ' '
        );
        WHILE octet_length(derived) > 1024 LOOP
            derived := rtrim(left(derived, char_length(derived) - 1), ' ');
        END LOOP;
        IF derived <> '' THEN
            UPDATE tool_approval_decision
               SET denial_reason = derived
             WHERE request_id = denial.request_id;
        END IF;
    END LOOP;
END $$;

ALTER TABLE tool_approval_decision ENABLE TRIGGER USER;
