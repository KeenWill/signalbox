-- A user may override one exact judge denial. The override is a durable
-- user-global command that arms a one-shot pre-approval in the denied
-- request's session: the next proposal of the exact denied command — same
-- tool name, same normalized arguments — is approved under `user_override`
-- provenance instead of parking for the judge again. Arming requires a
-- terminal delegate denial (the decision row is a delegate deny and its
-- denied-result entry is materialized), the command's session must own the
-- request, and each denial admits at most one override ever. Consumption is
-- once ever per armed override: the consuming decision row names the denied
-- request through a UNIQUE column, so a second identical proposal parks for
-- the judge again. The full audit chain — judge denial, override command,
-- armed row, consuming approval — stays queryable from any link.

-- The rebuilt constraints must carry every kind and version supported
-- immediately before this migration; `202608100001` is the predecessor that
-- last reissued them.
ALTER TABLE durable_command
    DROP CONSTRAINT durable_command_kind_closed,
    DROP CONSTRAINT durable_command_storage_version_supported;

ALTER TABLE durable_command
    ADD CONSTRAINT durable_command_kind_closed CHECK (
        command_kind IN (
            'create_session', 'create_session_from_imported_frontier',
            'replace_session_defaults', 'replace_session_metadata',
            'submit_input', 'decide_tool_request',
            'override_denied_tool_request', 'review_workflow',
            'review_orchestration', 'compact_session', 'goal',
            'update_session_placement', 'register_workspace',
            'mint_git_remote', 'withdraw_git_remote'
        )
    ),
    ADD CONSTRAINT durable_command_storage_version_supported CHECK (
        (command_kind = 'create_session'
            AND storage_version IN (1, 2, 3, 4, 5, 6, 7))
        OR (command_kind IN (
            'replace_session_defaults'
        ) AND storage_version IN (1, 2, 3, 4))
        OR (command_kind = 'create_session_from_imported_frontier'
            AND storage_version IN (1, 2, 3, 5))
        OR (command_kind = 'submit_input' AND storage_version IN (1, 2))
        OR (command_kind IN (
            'replace_session_metadata', 'decide_tool_request',
            'override_denied_tool_request', 'review_workflow',
            'review_orchestration', 'compact_session', 'goal',
            'update_session_placement', 'register_workspace',
            'mint_git_remote', 'withdraw_git_remote'
        ) AND storage_version = 1)
    );

-- The typed record family for the override command. The session is part of
-- the canonical payload — unlike `decide_tool_request`, where the session is
-- only a routing precondition — because the armed override is a
-- session-scoped standing fact consumed by a later proposal. `request_id`
-- carries no foreign key: a recorded `request_not_found` rejection names a
-- request no row ever had.
CREATE TABLE override_denied_tool_request_command (
    command_id uuid PRIMARY KEY,
    command_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    request_id uuid NOT NULL,
    result_kind text NOT NULL,
    rejection_kind text,

    CONSTRAINT override_denied_tool_request_command_kind_closed
        CHECK (command_kind = 'override_denied_tool_request'),
    CONSTRAINT override_denied_tool_request_command_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT override_denied_tool_request_command_result_closed
        CHECK (result_kind IN ('applied', 'rejected')),
    CONSTRAINT override_denied_tool_request_command_rejection_closed
        CHECK (
            rejection_kind IS NULL
            OR rejection_kind IN (
                'request_not_found', 'request_not_in_session',
                'not_delegate_denied', 'not_terminally_denied',
                'already_overridden'
            )
        ),
    CONSTRAINT override_denied_tool_request_command_result_shape
        CHECK (
            (result_kind = 'applied' AND rejection_kind IS NULL)
            OR (result_kind = 'rejected' AND rejection_kind IS NOT NULL)
        ),
    CONSTRAINT override_denied_tool_request_command_registry_fk
        FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command (command_id, command_kind, storage_version)
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER override_denied_tool_request_command_is_append_only
BEFORE UPDATE OR DELETE ON override_denied_tool_request_command
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

-- One armed, not-yet-consumed override per delegate-denied request. The
-- composite request foreign key pins the override to the request's owning
-- session; the judge-call foreign key pins the exact denial being
-- overridden; the deferred command foreign key ties the row to its applied
-- command inside the arming transaction.
CREATE TABLE tool_approval_user_override (
    denied_request_id uuid PRIMARY KEY,
    session_id uuid NOT NULL,
    command_id uuid NOT NULL UNIQUE,
    judge_model_call_id uuid NOT NULL,

    CONSTRAINT tool_approval_user_override_request_fk
        FOREIGN KEY (denied_request_id, session_id)
        REFERENCES tool_request (request_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    CONSTRAINT tool_approval_user_override_denial_fk
        FOREIGN KEY (denied_request_id)
        REFERENCES tool_approval_decision (request_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    CONSTRAINT tool_approval_user_override_command_fk
        FOREIGN KEY (command_id)
        REFERENCES override_denied_tool_request_command (command_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT tool_approval_user_override_judge_fk
        FOREIGN KEY (judge_model_call_id)
        REFERENCES tool_approval_judge_model_call (model_call_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
);

CREATE TRIGGER tool_approval_user_override_is_append_only
BEFORE UPDATE OR DELETE ON tool_approval_user_override
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE INDEX tool_approval_user_override_session_request_idx
    ON tool_approval_user_override (session_id, denied_request_id);

-- The armed override inventory is part of a Prepared call's immutable input.
-- Recording only the denial identity is sufficient because the armed row and
-- denied request are themselves append-only authority records.
CREATE TABLE model_call_user_override (
    model_call_id uuid NOT NULL,
    denied_request_id uuid NOT NULL,

    PRIMARY KEY (model_call_id, denied_request_id),
    CONSTRAINT model_call_user_override_call_fk
        FOREIGN KEY (model_call_id)
        REFERENCES model_call (model_call_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    CONSTRAINT model_call_user_override_armed_fk
        FOREIGN KEY (denied_request_id)
        REFERENCES tool_approval_user_override (denied_request_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
);

CREATE TRIGGER model_call_user_override_is_append_only
BEFORE UPDATE OR DELETE ON model_call_user_override
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

-- Arming authority: the named denial must be a terminal delegate denial by
-- the exact judge call the row records, and the row must correlate with its
-- applied command. The denied-result entry requirement is what makes the
-- denial terminal: a delegate denial mid-round has a decision row but no
-- materialized `tool_denied` result yet, and overriding it would race the
-- round that is still resolving.
CREATE FUNCTION require_user_override_arming_authority()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    matched bigint;
BEGIN
    SELECT count(*) INTO matched
      FROM tool_approval_decision AS denial
      JOIN semantic_transcript_entry AS denied_result
        ON denied_result.payload_kind = 'tool_denied'
       AND denied_result.tool_result_request_id = NEW.denied_request_id
     WHERE denial.request_id = NEW.denied_request_id
       AND denial.decision_kind = 'deny'
       AND denial.decision_source = 'delegate'
       AND denial.delegate_model_call_id = NEW.judge_model_call_id;
    IF matched <> 1 THEN
        RAISE EXCEPTION 'user override lacks a terminal delegate denial'
            USING ERRCODE = '23514',
                  CONSTRAINT =
                      'tool_approval_user_override_requires_terminal_denial';
    END IF;
    SELECT count(*) INTO matched
      FROM override_denied_tool_request_command AS command
     WHERE command.command_id = NEW.command_id
       AND command.result_kind = 'applied'
       AND command.request_id = NEW.denied_request_id
       AND command.session_id = NEW.session_id;
    IF matched <> 1 THEN
        RAISE EXCEPTION 'user override lacks its applied arming command'
            USING ERRCODE = '23514',
                  CONSTRAINT =
                      'tool_approval_user_override_requires_applied_command';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER user_override_requires_arming_authority
AFTER INSERT ON tool_approval_user_override
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_user_override_arming_authority();

-- The reverse correlation: an applied override command arms exactly one
-- override, and a rejected one arms none.
CREATE FUNCTION require_override_command_effect()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    armed_count bigint;
BEGIN
    SELECT count(*) INTO armed_count
      FROM tool_approval_user_override
     WHERE command_id = NEW.command_id;
    IF (NEW.result_kind = 'applied' AND armed_count <> 1)
       OR (NEW.result_kind = 'rejected' AND armed_count <> 0)
    THEN
        RAISE EXCEPTION 'override command lacks its exact armed effect'
            USING ERRCODE = '23514',
                  CONSTRAINT =
                      'override_denied_tool_request_command_requires_effect';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER override_denied_tool_request_command_requires_effect
AFTER INSERT ON override_denied_tool_request_command
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_override_command_effect();

-- Consumption: the approving decision row names the denied request it
-- consumed. The UNIQUE constraint is the durable one-shot boundary — a
-- second consuming row for the same armed override cannot exist under any
-- interleaving.
ALTER TABLE tool_approval_decision
    ADD COLUMN override_denied_request_id uuid,
    ADD CONSTRAINT tool_approval_decision_override_denied_request_id_key
        UNIQUE (override_denied_request_id),
    ADD CONSTRAINT tool_approval_decision_override_fk
        FOREIGN KEY (override_denied_request_id)
        REFERENCES tool_approval_user_override (denied_request_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT;

-- Supersedes the source constraints from
-- 202608110001_user_role_storage_vocabulary.sql: `user_override` joins the
-- closed source vocabulary as an approve-only source whose sole provenance
-- column is the consumed override. The `tool_approval_decision_shape`
-- constraint is untouched: an approve row already carries no denial reason.
ALTER TABLE tool_approval_decision
    DROP CONSTRAINT tool_approval_decision_source_closed,
    ADD CONSTRAINT tool_approval_decision_source_closed
        CHECK (
            decision_source IN (
                'user_command', 'policy_auto', 'session_blanket', 'delegate',
                'user_override'
            )
        ),
    DROP CONSTRAINT tool_approval_decision_source_shape,
    ADD CONSTRAINT tool_approval_decision_source_shape
        CHECK (
            (decision_source = 'user_command'
                AND user_command_id IS NOT NULL
                AND delegate_model_selection_id IS NULL
                AND delegate_model_call_id IS NULL
                AND rationale IS NULL
                AND override_denied_request_id IS NULL)
            OR (decision_source IN ('policy_auto', 'session_blanket')
                AND decision_kind = 'approve'
                AND user_command_id IS NULL
                AND delegate_model_selection_id IS NULL
                AND delegate_model_call_id IS NULL
                AND rationale IS NULL
                AND override_denied_request_id IS NULL)
            OR (decision_source = 'delegate'
                AND user_command_id IS NULL
                AND delegate_model_selection_id IS NOT NULL
                AND delegate_model_call_id IS NOT NULL
                AND rationale IS NOT NULL
                AND octet_length(rationale) BETWEEN 1 AND 4096
                AND override_denied_request_id IS NULL)
            OR (decision_source = 'user_override'
                AND decision_kind = 'approve'
                AND user_command_id IS NULL
                AND delegate_model_selection_id IS NULL
                AND delegate_model_call_id IS NULL
                AND rationale IS NULL
                AND override_denied_request_id IS NOT NULL)
        );

-- Registers the new typed record family; supersedes the version from
-- 202608100001_workspace_and_git_remote_authority.sql.
CREATE OR REPLACE FUNCTION require_durable_command_typed_record()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE matching_records bigint;
BEGIN
    IF NEW.command_kind <> 'review_orchestration' AND EXISTS (
        SELECT 1 FROM review_orchestration_command_recovery
         WHERE command_id = NEW.command_id
    ) THEN
        RAISE EXCEPTION 'durable command % is reserved by review orchestration recovery', NEW.command_id
            USING ERRCODE = '23505';
    END IF;
    CASE NEW.command_kind
        WHEN 'create_session' THEN SELECT count(*) INTO matching_records FROM create_session_command WHERE command_id = NEW.command_id;
        WHEN 'create_session_from_imported_frontier' THEN SELECT count(*) INTO matching_records FROM create_session_from_imported_frontier_command WHERE command_id = NEW.command_id;
        WHEN 'replace_session_defaults' THEN SELECT count(*) INTO matching_records FROM replace_session_defaults_command WHERE command_id = NEW.command_id;
        WHEN 'replace_session_metadata' THEN SELECT count(*) INTO matching_records FROM replace_session_metadata_command WHERE command_id = NEW.command_id;
        WHEN 'submit_input' THEN SELECT count(*) INTO matching_records FROM submit_input_command WHERE command_id = NEW.command_id;
        WHEN 'decide_tool_request' THEN SELECT count(*) INTO matching_records FROM decide_tool_request_command WHERE command_id = NEW.command_id;
        WHEN 'override_denied_tool_request' THEN SELECT count(*) INTO matching_records FROM override_denied_tool_request_command WHERE command_id = NEW.command_id;
        WHEN 'review_workflow' THEN SELECT count(*) INTO matching_records FROM review_workflow_command WHERE command_id = NEW.command_id;
        WHEN 'review_orchestration' THEN SELECT (SELECT count(*) FROM review_orchestration_command WHERE command_id = NEW.command_id) + (SELECT count(*) FROM review_orchestration_command_intent WHERE command_id = NEW.command_id) INTO matching_records;
        WHEN 'compact_session' THEN SELECT count(*) INTO matching_records FROM compact_session_command WHERE command_id = NEW.command_id;
        WHEN 'goal' THEN SELECT count(*) INTO matching_records FROM goal_command WHERE command_id = NEW.command_id;
        WHEN 'update_session_placement' THEN SELECT count(*) INTO matching_records FROM update_session_placement_command WHERE command_id = NEW.command_id;
        WHEN 'register_workspace' THEN SELECT count(*) INTO matching_records FROM workspace WHERE command_id = NEW.command_id;
        WHEN 'mint_git_remote' THEN SELECT count(*) INTO matching_records FROM configured_git_remote_mint WHERE command_id = NEW.command_id;
        WHEN 'withdraw_git_remote' THEN SELECT count(*) INTO matching_records FROM configured_git_remote_withdrawal WHERE command_id = NEW.command_id;
        ELSE RAISE EXCEPTION 'unsupported durable command kind %', NEW.command_kind USING ERRCODE = '23514';
    END CASE;
    IF matching_records <> 1 THEN
        RAISE EXCEPTION 'durable command % requires exactly one % typed record', NEW.command_id, NEW.command_kind USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;

-- Adds the `user_override` branch to the decision-authority gate; supersedes
-- the version from 202608110001_user_role_storage_vocabulary.sql. A consuming
-- approval is admitted only for a request frozen `delegated` — the posture
-- the judge would otherwise decide — and only while an armed override exists
-- in the request's own session.
CREATE OR REPLACE FUNCTION require_tool_approval_decision_authority()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    matched bigint;
BEGIN
    PERFORM 1
       FROM tool_request
      WHERE request_id = NEW.request_id
        FOR UPDATE;
    IF EXISTS (
        SELECT 1
          FROM tool_approval_judge_model_call AS judge
         WHERE judge.request_id = NEW.request_id
           AND judge.state_kind <> 'terminal'
    ) THEN
        RAISE EXCEPTION 'approval decision races an unfinished judge call'
            USING ERRCODE = '23514',
                  CONSTRAINT =
                      'tool_approval_decision_requires_terminal_judge';
    END IF;
    IF NEW.decision_source IN ('policy_auto', 'session_blanket') THEN
        SELECT count(*) INTO matched
          FROM tool_request
         WHERE request_id = NEW.request_id
           AND approval_posture = 'auto';
        IF matched <> 1 THEN
            RAISE EXCEPTION 'automatic decision exceeds frozen posture'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'tool_approval_automatic_requires_auto_posture';
        END IF;
        RETURN NULL;
    END IF;
    IF NEW.decision_source = 'user_override' THEN
        SELECT count(*) INTO matched
          FROM tool_request AS request
          JOIN tool_approval_user_override AS armed
            ON armed.denied_request_id = NEW.override_denied_request_id
          JOIN model_call_user_override AS frozen
            ON frozen.model_call_id = request.producing_model_call_id
           AND frozen.denied_request_id = armed.denied_request_id
          JOIN tool_request AS denied_request
            ON denied_request.request_id = armed.denied_request_id
         WHERE request.request_id = NEW.request_id
           AND request.approval_posture = 'delegated'
           AND armed.session_id = request.session_id
           AND denied_request.tool_name = request.tool_name
           AND denied_request.arguments_kind = request.arguments_kind
           AND denied_request.arguments_text = request.arguments_text;
        IF matched <> 1 THEN
            RAISE EXCEPTION
                'user override consumption lacks an armed override for a delegated request'
                USING ERRCODE = '23514',
                      CONSTRAINT =
                          'tool_approval_user_override_requires_armed_override';
        END IF;
        RETURN NULL;
    END IF;
    IF NEW.decision_source = 'user_command' THEN
        SELECT count(*) INTO matched
          FROM tool_request AS request
         WHERE request.request_id = NEW.request_id
           AND (
                request.approval_posture = 'human'
                OR (
                    request.approval_posture = 'delegated'
                    AND EXISTS (
                        SELECT 1
                          FROM tool_approval_judge_model_call AS judge
                         WHERE judge.request_id = request.request_id
                           AND judge.state_kind = 'terminal'
                           AND (
                                (
                                    judge.terminal_disposition_kind = 'completed'
                                    AND judge.recommendation_kind =
                                        'escalate_to_human'
                                )
                                OR judge.terminal_disposition_kind IN (
                                    'known_failed', 'refused', 'cancelled',
                                    'ambiguous'
                                )
                           )
                    )
                )
           );
        IF matched <> 1 THEN
            RAISE EXCEPTION 'user decision lacks human approval authority'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'tool_approval_user_requires_human_authority';
        END IF;
        RETURN NULL;
    END IF;
    IF NEW.decision_source <> 'delegate' THEN
        RETURN NULL;
    END IF;
    SELECT count(*) INTO matched
      FROM tool_request AS request
      JOIN tool_approval_judge_model_call AS judge
        ON judge.request_id = request.request_id
     WHERE request.request_id = NEW.request_id
       AND request.approval_posture = 'delegated'
       AND judge.model_call_id = NEW.delegate_model_call_id
       AND judge.direct_model_selection_id = NEW.delegate_model_selection_id
       AND judge.state_kind = 'terminal'
       AND judge.terminal_disposition_kind = 'completed'
       AND judge.recommendation_kind = NEW.decision_kind
       AND judge.rationale = NEW.rationale
       AND NOT EXISTS (
            SELECT 1 FROM tool_request AS earlier
            LEFT JOIN tool_approval_decision AS earlier_decision
              ON earlier_decision.request_id = earlier.request_id
           WHERE earlier.producing_model_call_id = request.producing_model_call_id
             AND earlier.request_ordinal < request.request_ordinal
             AND earlier_decision.request_id IS NULL
       );
    IF matched <> 1 THEN
        RAISE EXCEPTION 'delegate decision lacks matching delegated authority'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'tool_approval_delegate_requires_checked_judge';
    END IF;
    RETURN NULL;
END;
$$;

-- Adds the `user_override` branch to the explicit-effect gate; supersedes the
-- version from 202608030001_tool_approval_decision_events.sql. A consuming
-- approval is written in its request's producing transaction, so its effect
-- is the round's own committed shape — one decided event recorded in this
-- exact transaction, and a lifecycle either parked on the round's earliest
-- undecided request or running on the prepared continuation attempt. The
-- one-explicit-event-per-transaction gate of the user/delegate branch does
-- not apply: one proposing transaction may consume several armed overrides,
-- each with its own event.
CREATE OR REPLACE FUNCTION require_explicit_tool_approval_effect()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    matching_effects bigint;
BEGIN
    IF NEW.decision_source IN ('policy_auto', 'session_blanket') THEN
        RETURN NULL;
    END IF;

    IF NEW.decision_source = 'user_override' THEN
        SELECT count(*)
          INTO matching_effects
          FROM tool_approval_decided_outbox_event AS dispatched
          JOIN tool_request AS request
            ON request.request_id = dispatched.request_id
          JOIN turn_lifecycle AS lifecycle
            ON lifecycle.turn_id = request.turn_id
           AND lifecycle.session_id = request.session_id
         WHERE dispatched.request_id = NEW.request_id
           AND dispatched.recording_transaction_id = pg_current_xact_id()
           AND lifecycle.active_tool_round_call_id =
               request.producing_model_call_id
           AND (
                (
                    lifecycle.state_kind = 'active'
                    AND lifecycle.active_phase_kind = 'awaiting_tool_approval'
                    AND lifecycle.approval_tool_request_id = (
                        SELECT later.request_id
                          FROM tool_request AS later
                          LEFT JOIN tool_approval_decision AS later_decision
                            ON later_decision.request_id = later.request_id
                         WHERE later.producing_model_call_id =
                               request.producing_model_call_id
                           AND later_decision.request_id IS NULL
                         ORDER BY later.request_ordinal
                         LIMIT 1
                    )
                )
                OR
                (
                    lifecycle.state_kind = 'active'
                    AND lifecycle.active_phase_kind = 'running'
                    AND lifecycle.approval_tool_request_id IS NULL
                    AND lifecycle.recovery_tool_attempt_id IS NULL
                    AND NOT EXISTS (
                        SELECT 1
                          FROM tool_request AS undecided
                          LEFT JOIN tool_approval_decision AS undecided_decision
                            ON undecided_decision.request_id =
                               undecided.request_id
                         WHERE undecided.producing_model_call_id =
                               request.producing_model_call_id
                           AND undecided_decision.request_id IS NULL
                    )
                    AND EXISTS (
                        SELECT 1
                          FROM turn_attempt AS successor
                          JOIN model_call AS producing_call
                            ON producing_call.model_call_id =
                               request.producing_model_call_id
                           AND producing_call.turn_id = request.turn_id
                           AND producing_call.session_id = request.session_id
                         WHERE successor.turn_attempt_id =
                               lifecycle.current_attempt_id
                           AND successor.turn_id = request.turn_id
                           AND successor.session_id = request.session_id
                           AND successor.continued_from_attempt_id =
                               producing_call.turn_attempt_id
                           AND successor.state_kind = 'prepared'
                    )
                )
           );
        IF matching_effects <> 1 THEN
            RAISE EXCEPTION
                'user override consumption lacks its atomic proposal effect'
                USING
                    ERRCODE = '23514',
                    CONSTRAINT =
                        'tool_approval_user_override_requires_atomic_effect';
        END IF;
        RETURN NULL;
    END IF;

    SELECT count(*)
      INTO matching_effects
      FROM tool_approval_decided_outbox_event AS dispatched
      JOIN tool_request AS request
        ON request.request_id = dispatched.request_id
      JOIN turn_lifecycle AS lifecycle
        ON lifecycle.turn_id = request.turn_id
       AND lifecycle.session_id = request.session_id
     WHERE dispatched.request_id = NEW.request_id
       AND lifecycle.active_tool_round_call_id =
           request.producing_model_call_id
       AND (
            SELECT count(*)
              FROM tool_approval_decided_outbox_event AS transaction_event
              JOIN tool_approval_decision AS transaction_decision
                ON transaction_decision.request_id =
                   transaction_event.request_id
              JOIN tool_request AS transaction_request
                ON transaction_request.request_id =
                   transaction_decision.request_id
             WHERE transaction_request.producing_model_call_id =
                   request.producing_model_call_id
               AND transaction_decision.decision_source NOT IN (
                    'policy_auto', 'session_blanket'
               )
               AND transaction_event.recording_transaction_id =
                   dispatched.recording_transaction_id
       ) = 1
       AND NOT EXISTS (
            SELECT 1
              FROM tool_request AS earlier
              LEFT JOIN tool_approval_decision AS earlier_decision
                ON earlier_decision.request_id = earlier.request_id
             WHERE earlier.producing_model_call_id =
                   request.producing_model_call_id
               AND earlier.request_ordinal < request.request_ordinal
               AND earlier_decision.request_id IS NULL
       )
       AND (
            (
                lifecycle.state_kind = 'active'
                AND lifecycle.active_phase_kind = 'awaiting_tool_approval'
                AND lifecycle.approval_tool_request_id = (
                    SELECT later.request_id
                      FROM tool_request AS later
                      LEFT JOIN tool_approval_decision AS later_decision
                        ON later_decision.request_id = later.request_id
                     WHERE later.producing_model_call_id =
                           request.producing_model_call_id
                       AND later_decision.request_id IS NULL
                     ORDER BY later.request_ordinal
                     LIMIT 1
                )
            )
            OR
            (
                lifecycle.state_kind = 'active'
                AND lifecycle.active_phase_kind = 'running'
                AND lifecycle.approval_tool_request_id IS NULL
                AND lifecycle.recovery_tool_attempt_id IS NULL
                AND NOT EXISTS (
                    SELECT 1
                      FROM tool_request AS undecided
                      LEFT JOIN tool_approval_decision AS undecided_decision
                        ON undecided_decision.request_id = undecided.request_id
                     WHERE undecided.producing_model_call_id =
                           request.producing_model_call_id
                       AND undecided_decision.request_id IS NULL
                )
                AND EXISTS (
                    SELECT 1
                      FROM turn_attempt AS successor
                      JOIN model_call AS producing_call
                        ON producing_call.model_call_id =
                           request.producing_model_call_id
                       AND producing_call.turn_id = request.turn_id
                       AND producing_call.session_id = request.session_id
                     WHERE successor.turn_attempt_id =
                           lifecycle.current_attempt_id
                       AND successor.turn_id = request.turn_id
                       AND successor.session_id = request.session_id
                       AND successor.continued_from_attempt_id =
                           producing_call.turn_attempt_id
                       AND successor.state_kind = 'prepared'
                )
            )
       );
    IF matching_effects <> 1 THEN
        RAISE EXCEPTION
            'explicit decision lacks its outbox and lifecycle effect'
            USING
                ERRCODE = '23514',
                CONSTRAINT =
                    'tool_approval_explicit_requires_atomic_effect';
    END IF;
    RETURN NULL;
END;
$$;
