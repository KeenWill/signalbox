-- Operator-commissioned dispatch: the immutable repository/head/base authority
-- fence for a session commissioned directly through the command surface rather
-- than by a repository-watch rule. The approval judge consumes this fence
-- exactly as it consumes the repository-watch dispatch fence, and the
-- unattended-escalation closeout gains a commissioned audit arm so a
-- commissioned session escalating with nobody attending terminalizes instead
-- of parking forever.

CREATE TABLE commissioned_dispatch (
    dispatch_id uuid PRIMARY KEY,
    session_id uuid NOT NULL UNIQUE,
    create_command_id uuid NOT NULL UNIQUE,
    template_name text NOT NULL,
    template_content_digest bytea NOT NULL,
    target_kind text NOT NULL,
    repository text NOT NULL,
    pull_request_number numeric(20, 0),
    head_sha text,
    head_repository text,
    head_branch text,
    base_branch text,
    branch text,
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    UNIQUE (dispatch_id, session_id),
    FOREIGN KEY (session_id)
        REFERENCES session(session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    FOREIGN KEY (create_command_id)
        REFERENCES durable_command(command_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CHECK (octet_length(template_name) BETWEEN 1 AND 128),
    CHECK (octet_length(template_content_digest) = 32),
    CHECK (target_kind IN ('pull_request', 'branch')),
    CHECK (repo_watch_repository_is_valid(repository)),
    CHECK (
        pull_request_number IS NULL
        OR (
            pull_request_number > 0
            AND pull_request_number <= 18446744073709551615
        )
    ),
    CHECK (head_sha IS NULL OR head_sha COLLATE "C" ~ '^[0-9a-f]{40}$'),
    CHECK (head_repository IS NULL OR repo_watch_repository_is_valid(head_repository)),
    CHECK (head_branch IS NULL OR repo_watch_branch_is_valid(head_branch)),
    CHECK (base_branch IS NULL OR repo_watch_branch_is_valid(base_branch)),
    CHECK (branch IS NULL OR repo_watch_branch_is_valid(branch)),
    CHECK (
        (
            target_kind = 'pull_request'
            AND pull_request_number IS NOT NULL
            AND head_sha IS NOT NULL
            AND head_repository IS NOT NULL
            AND head_branch IS NOT NULL
            AND base_branch IS NOT NULL
            AND branch IS NULL
        )
        OR (
            target_kind = 'branch'
            AND pull_request_number IS NULL
            AND head_sha IS NULL
            AND head_repository IS NULL
            AND head_branch IS NULL
            AND base_branch IS NULL
            AND branch IS NOT NULL
        )
    )
);

CREATE FUNCTION reject_commissioned_dispatch_table_truncate()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION '% cannot be truncated', TG_TABLE_NAME
        USING ERRCODE = '23514';
END;
$$;

CREATE TRIGGER commissioned_dispatch_is_append_only
BEFORE UPDATE OR DELETE ON commissioned_dispatch
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER commissioned_dispatch_reject_truncate
BEFORE TRUNCATE ON commissioned_dispatch
FOR EACH STATEMENT
EXECUTE FUNCTION reject_commissioned_dispatch_table_truncate();

-- Audit for an unattended approval escalation closed out on a commissioned
-- session, mirroring `repo_watch_headless_approval_escalation` with the
-- commissioned dispatch as its authority source. The linked failed turn and
-- blocked goal remain the ordinary lifecycle authorities; a commissioned
-- dispatch has no batch to release and no requeue obligation to settle.

CREATE TABLE commissioned_dispatch_headless_approval_escalation (
    model_call_id uuid PRIMARY KEY,
    request_id uuid NOT NULL UNIQUE,
    dispatch_id uuid NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    terminal_attempt_id uuid NOT NULL UNIQUE,
    failure_entry_id uuid NOT NULL UNIQUE,
    terminal_frontier_id uuid NOT NULL UNIQUE,
    escalated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    FOREIGN KEY (model_call_id, session_id)
        REFERENCES tool_approval_judge_model_call (model_call_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    FOREIGN KEY (request_id, turn_id, session_id)
        REFERENCES tool_request (request_id, turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    FOREIGN KEY (dispatch_id, session_id)
        REFERENCES commissioned_dispatch (dispatch_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    FOREIGN KEY (terminal_attempt_id, turn_id, session_id)
        REFERENCES turn_attempt (turn_attempt_id, turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    FOREIGN KEY (session_id, failure_entry_id)
        REFERENCES semantic_transcript_entry (source_session_id, semantic_entry_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    FOREIGN KEY (session_id, terminal_frontier_id)
        REFERENCES context_frontier (owning_session_id, context_frontier_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    FOREIGN KEY (turn_id, session_id)
        REFERENCES turn_lifecycle (turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
);

CREATE TRIGGER commissioned_dispatch_headless_escalation_is_append_only
BEFORE UPDATE OR DELETE ON commissioned_dispatch_headless_approval_escalation
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER commissioned_dispatch_headless_escalation_reject_truncate
BEFORE TRUNCATE ON commissioned_dispatch_headless_approval_escalation
FOR EACH STATEMENT
EXECUTE FUNCTION reject_commissioned_dispatch_table_truncate();

CREATE VIEW commissioned_dispatch_headless_approval_escalation_audit AS
SELECT escalation.model_call_id,
       escalation.request_id,
       escalation.dispatch_id,
       escalation.session_id,
       escalation.turn_id,
       escalation.terminal_attempt_id,
       escalation.failure_entry_id,
       escalation.terminal_frontier_id,
       judge.rationale,
       escalation.escalated_at
  FROM commissioned_dispatch_headless_approval_escalation AS escalation
  JOIN tool_approval_judge_model_call AS judge
    ON judge.model_call_id = escalation.model_call_id;

-- A failed tool-loop turn closes on a terminal model call, the exact
-- crash-lost tool attempt, or a completed unattended judge escalation. The
-- escalation cause now has two audit homes — one per dispatch authority
-- source — and either one, correlated to the terminal attempt and completed
-- judge result, is admissible.
CREATE OR REPLACE FUNCTION assert_failed_terminal_execution_without_cancellation(
    checked_turn_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    lifecycle turn_lifecycle%ROWTYPE;
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM tool_round
         WHERE turn_id = checked_turn_id
    ) THEN
        PERFORM assert_failed_terminal_execution_without_tool_loop(
            checked_turn_id
        );
        RETURN;
    END IF;

    SELECT *
      INTO lifecycle
      FROM turn_lifecycle
     WHERE turn_id = checked_turn_id
       AND state_kind = 'terminal'
       AND terminal_disposition_kind = 'failed';
    IF NOT FOUND THEN
        RETURN;
    END IF;

    IF lifecycle.terminal_attempt_id IS NULL
       OR NOT EXISTS (
            SELECT 1
              FROM turn_attempt
             WHERE turn_attempt_id = lifecycle.terminal_attempt_id
               AND turn_id = lifecycle.turn_id
               AND session_id = lifecycle.session_id
               AND state_kind = 'ended'
               AND end_variant = 'without_stop'
               AND end_disposition IN ('known_failure', 'lost')
       )
       OR EXISTS (
            SELECT 1
              FROM turn_attempt
             WHERE turn_id = lifecycle.turn_id
               AND session_id = lifecycle.session_id
               AND turn_attempt_id <> lifecycle.terminal_attempt_id
               AND (
                    state_kind <> 'ended'
                    OR end_variant <> 'without_stop'
                    OR end_disposition <> 'yielded_to_durable_wait'
               )
       )
    THEN
        RAISE EXCEPTION
            'failed tool-loop turn % lacks its exact linear ended attempt',
            checked_turn_id
            USING ERRCODE = '23514';
    END IF;

    IF lifecycle.terminal_model_call_id IS NOT NULL THEN
        IF NOT EXISTS (
            SELECT 1
              FROM model_call
             WHERE model_call_id = lifecycle.terminal_model_call_id
               AND turn_attempt_id = lifecycle.terminal_attempt_id
               AND turn_id = lifecycle.turn_id
               AND session_id = lifecycle.session_id
               AND state_kind = 'terminal'
               AND terminal_disposition_kind IN ('known_failed', 'cancelled')
        ) THEN
            RAISE EXCEPTION
                'failed tool-loop turn % lacks its exact terminal call',
                checked_turn_id
                USING ERRCODE = '23514';
        END IF;
        PERFORM assert_model_call_final_state(
            lifecycle.terminal_model_call_id
        );
    ELSIF NOT EXISTS (
        SELECT 1
          FROM tool_attempt
         WHERE issuing_turn_attempt_id = lifecycle.terminal_attempt_id
           AND turn_id = lifecycle.turn_id
           AND session_id = lifecycle.session_id
           AND state_kind = 'terminal'
           AND terminal_disposition_kind = 'known_failed'
           AND error_kind = 'crash_lost'
    ) AND NOT EXISTS (
        SELECT 1
          FROM repo_watch_headless_approval_escalation AS escalation
          JOIN tool_approval_judge_model_call AS judge
            ON judge.model_call_id = escalation.model_call_id
           AND judge.session_id = escalation.session_id
           AND judge.turn_id = escalation.turn_id
           AND judge.request_id = escalation.request_id
         WHERE escalation.turn_id = lifecycle.turn_id
           AND escalation.session_id = lifecycle.session_id
           AND escalation.terminal_attempt_id = lifecycle.terminal_attempt_id
           AND judge.state_kind = 'terminal'
           AND judge.terminal_disposition_kind = 'completed'
           AND judge.recommendation_kind = 'escalate_to_human'
    ) AND NOT EXISTS (
        SELECT 1
          FROM commissioned_dispatch_headless_approval_escalation AS escalation
          JOIN tool_approval_judge_model_call AS judge
            ON judge.model_call_id = escalation.model_call_id
           AND judge.session_id = escalation.session_id
           AND judge.turn_id = escalation.turn_id
           AND judge.request_id = escalation.request_id
         WHERE escalation.turn_id = lifecycle.turn_id
           AND escalation.session_id = lifecycle.session_id
           AND escalation.terminal_attempt_id = lifecycle.terminal_attempt_id
           AND judge.state_kind = 'terminal'
           AND judge.terminal_disposition_kind = 'completed'
           AND judge.recommendation_kind = 'escalate_to_human'
    ) THEN
        RAISE EXCEPTION
            'failed tool-loop turn % lacks its exact terminal execution cause',
            checked_turn_id
            USING ERRCODE = '23514';
    END IF;
END;
$$;
