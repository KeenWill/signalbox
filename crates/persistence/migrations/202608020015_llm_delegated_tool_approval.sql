-- Freeze per-request approval authority and record delegate calls as dedicated
-- model calls with decision provenance.

ALTER TABLE tool_request
    ADD COLUMN approval_posture text NOT NULL DEFAULT 'human';

ALTER TABLE tool_request
    DISABLE TRIGGER tool_request_is_append_only;

UPDATE tool_request AS request
   SET approval_posture = 'auto'
 WHERE EXISTS (
     SELECT 1
       FROM tool_approval_decision AS approval
      WHERE approval.request_id = request.request_id
        AND approval.decision_source IN ('policy_auto', 'session_blanket')
 );

SET CONSTRAINTS ALL IMMEDIATE;

ALTER TABLE tool_request
    ENABLE TRIGGER tool_request_is_append_only;

ALTER TABLE tool_request
    ADD CONSTRAINT tool_request_approval_posture_closed
        CHECK (approval_posture IN ('auto', 'delegated', 'human'));

ALTER TABLE model_call_identity
    DROP CONSTRAINT model_call_identity_kind_closed,
    ADD CONSTRAINT model_call_identity_kind_closed
        CHECK (call_kind IN ('ordinary', 'context_compaction', 'approval_judge'));

CREATE TABLE tool_approval_judge_model_call (
    model_call_id uuid PRIMARY KEY,
    request_id uuid NOT NULL UNIQUE,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    direct_model_selection_id uuid NOT NULL,
    resolved_provider_model_identity_id uuid NOT NULL,
    credential_reference text NOT NULL,
    usage_input_includes_cache_tokens boolean NOT NULL DEFAULT false,
    usage_provenance_kind text NOT NULL DEFAULT 'reported',
    state_kind text NOT NULL,
    terminal_disposition_kind text,
    recommendation_kind text,
    rationale text,
    input_tokens numeric,
    output_tokens numeric,
    cache_read_input_tokens numeric,
    cache_creation_input_tokens numeric,

    CONSTRAINT tool_approval_judge_call_state_closed
        CHECK (state_kind IN ('prepared', 'in_flight', 'terminal')),
    CONSTRAINT tool_approval_judge_call_disposition_closed
        CHECK (
            terminal_disposition_kind IS NULL
            OR terminal_disposition_kind IN (
                'completed', 'known_failed', 'refused', 'cancelled', 'ambiguous'
            )
        ),
    CONSTRAINT tool_approval_judge_call_recommendation_closed
        CHECK (
            recommendation_kind IS NULL
            OR recommendation_kind IN ('approve', 'deny', 'escalate_to_human')
        ),
    CONSTRAINT tool_approval_judge_call_state_shape
        CHECK (
            (
                state_kind <> 'terminal'
                AND terminal_disposition_kind IS NULL
                AND recommendation_kind IS NULL
                AND rationale IS NULL
            )
            OR (
                state_kind = 'terminal'
                AND terminal_disposition_kind = 'completed'
                AND recommendation_kind IS NOT NULL
                AND rationale IS NOT NULL
                AND octet_length(rationale) BETWEEN 1 AND 4096
            )
            OR (
                state_kind = 'terminal'
                AND terminal_disposition_kind IS NOT NULL
                AND terminal_disposition_kind <> 'completed'
                AND recommendation_kind IS NULL
                AND rationale IS NULL
            )
        ),
    CONSTRAINT tool_approval_judge_call_credential_nonempty
        CHECK (char_length(credential_reference) > 0),
    CONSTRAINT tool_approval_judge_call_usage_provenance_closed
        CHECK (usage_provenance_kind IN ('reported', 'estimated')),
    CONSTRAINT tool_approval_judge_call_usage_u64_range
        CHECK (
            (
                input_tokens IS NULL
                OR (
                    input_tokens = trunc(input_tokens)
                    AND input_tokens BETWEEN 0 AND 18446744073709551615
                )
            )
            AND (
                output_tokens IS NULL
                OR (
                    output_tokens = trunc(output_tokens)
                    AND output_tokens BETWEEN 0 AND 18446744073709551615
                )
            )
            AND (
                cache_read_input_tokens IS NULL
                OR (
                    cache_read_input_tokens = trunc(cache_read_input_tokens)
                    AND cache_read_input_tokens
                        BETWEEN 0 AND 18446744073709551615
                )
            )
            AND (
                cache_creation_input_tokens IS NULL
                OR (
                    cache_creation_input_tokens =
                        trunc(cache_creation_input_tokens)
                    AND cache_creation_input_tokens
                        BETWEEN 0 AND 18446744073709551615
                )
            )
        ),
    CONSTRAINT tool_approval_judge_call_cancelled_usage_is_unreported
        CHECK (
            terminal_disposition_kind IS DISTINCT FROM 'cancelled'
            OR (
                input_tokens IS NULL
                AND output_tokens IS NULL
                AND cache_read_input_tokens IS NULL
                AND cache_creation_input_tokens IS NULL
            )
        ),
    CONSTRAINT tool_approval_judge_call_session_key
        UNIQUE (model_call_id, session_id),
    CONSTRAINT tool_approval_judge_call_request_fk
        FOREIGN KEY (request_id, turn_id, session_id)
        REFERENCES tool_request (request_id, turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    CONSTRAINT tool_approval_judge_call_session_fk
        FOREIGN KEY (session_id) REFERENCES session (session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
);

CREATE INDEX tool_approval_judge_usage_by_session_state_turn_call
    ON tool_approval_judge_model_call
       (session_id, state_kind, turn_id, model_call_id);

CREATE TRIGGER tool_approval_judge_call_reserves_global_identity
BEFORE INSERT ON tool_approval_judge_model_call
FOR EACH ROW
EXECUTE FUNCTION reserve_model_call_identity('approval_judge');

CREATE FUNCTION reject_tool_approval_judge_call_invalid_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    active_wait boolean;
    request_posture text;
BEGIN
    IF TG_OP = 'INSERT' THEN
        SELECT true INTO active_wait
          FROM turn_lifecycle AS lifecycle
         WHERE lifecycle.turn_id = NEW.turn_id
           AND lifecycle.session_id = NEW.session_id
           AND lifecycle.state_kind = 'active'
           AND lifecycle.active_phase_kind = 'awaiting_tool_approval'
           AND lifecycle.approval_tool_request_id = NEW.request_id
           FOR UPDATE;
        SELECT approval_posture INTO request_posture
          FROM tool_request
         WHERE request_id = NEW.request_id
           FOR UPDATE;
        IF active_wait IS DISTINCT FROM true OR EXISTS (
            SELECT 1
              FROM tool_approval_decision AS decision
             WHERE decision.request_id = NEW.request_id
        ) THEN
            RAISE EXCEPTION 'approval judge call lacks an active approval wait'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'tool_approval_judge_requires_active_wait';
        END IF;
        IF NEW.usage_provenance_kind <> 'reported' THEN
            RAISE EXCEPTION 'prepared approval judge usage must be reported'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'tool_approval_judge_prepared_usage_is_reported';
        END IF;
        IF NEW.state_kind <> 'prepared'
            OR NEW.terminal_disposition_kind IS NOT NULL
            OR NEW.recommendation_kind IS NOT NULL
            OR NEW.rationale IS NOT NULL
            OR NEW.input_tokens IS NOT NULL
            OR NEW.output_tokens IS NOT NULL
            OR NEW.cache_read_input_tokens IS NOT NULL
            OR NEW.cache_creation_input_tokens IS NOT NULL
        THEN
            RAISE EXCEPTION 'approval judge call must be inserted as Prepared'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'approval judge call is not deletable'
            USING ERRCODE = '23514';
    END IF;
    IF ROW(
        OLD.model_call_id, OLD.request_id, OLD.session_id, OLD.turn_id,
        OLD.direct_model_selection_id,
        OLD.resolved_provider_model_identity_id,
        OLD.credential_reference, OLD.usage_input_includes_cache_tokens
    ) IS DISTINCT FROM ROW(
        NEW.model_call_id, NEW.request_id, NEW.session_id, NEW.turn_id,
        NEW.direct_model_selection_id,
        NEW.resolved_provider_model_identity_id,
        NEW.credential_reference, NEW.usage_input_includes_cache_tokens
    ) OR (
        NEW.usage_provenance_kind IS DISTINCT FROM OLD.usage_provenance_kind
        AND NOT (
            OLD.state_kind <> 'terminal'
            AND NEW.state_kind = 'terminal'
        )
    ) THEN
        RAISE EXCEPTION 'approval judge authorization facts are immutable'
            USING ERRCODE = '23514';
    END IF;
    IF OLD.state_kind = 'terminal' THEN
        RAISE EXCEPTION 'terminal approval judge call is immutable'
            USING ERRCODE = '23514';
    END IF;
    IF OLD.state_kind = 'prepared'
       AND NEW.state_kind = 'terminal'
       AND NEW.terminal_disposition_kind NOT IN ('known_failed', 'cancelled')
    THEN
        RAISE EXCEPTION 'prepared approval judge cannot record provider outcome'
            USING ERRCODE = '23514';
    END IF;
    IF OLD.state_kind = 'prepared'
       AND NEW.state_kind = 'terminal'
       AND (
            NEW.input_tokens IS NOT NULL
            OR NEW.output_tokens IS NOT NULL
            OR NEW.cache_read_input_tokens IS NOT NULL
            OR NEW.cache_creation_input_tokens IS NOT NULL
       )
    THEN
        RAISE EXCEPTION 'unsent approval judge cannot record provider usage'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'tool_approval_judge_unsent_has_no_usage';
    END IF;
    IF NOT (
        (OLD.state_kind = 'prepared' AND NEW.state_kind IN ('in_flight', 'terminal'))
        OR (OLD.state_kind = 'in_flight' AND NEW.state_kind = 'terminal')
    ) THEN
        RAISE EXCEPTION 'invalid approval judge call transition'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.state_kind <> 'terminal' AND (
        NEW.input_tokens IS NOT NULL
        OR NEW.output_tokens IS NOT NULL
        OR NEW.cache_read_input_tokens IS NOT NULL
        OR NEW.cache_creation_input_tokens IS NOT NULL
    ) THEN
        RAISE EXCEPTION 'approval judge usage is terminal evidence'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.state_kind = 'terminal'
       AND NEW.terminal_disposition_kind = 'completed'
    THEN
        SELECT approval_posture INTO request_posture
          FROM tool_request WHERE request_id = NEW.request_id;
        IF request_posture = 'auto'
           OR (request_posture = 'human'
               AND NEW.recommendation_kind <> 'escalate_to_human')
        THEN
            RAISE EXCEPTION 'approval judge recommendation exceeds frozen posture'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'tool_approval_judge_recommendation_within_posture';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER tool_approval_judge_call_changes_are_guarded
BEFORE INSERT OR UPDATE OR DELETE ON tool_approval_judge_model_call
FOR EACH ROW
EXECUTE FUNCTION reject_tool_approval_judge_call_invalid_change();

ALTER TABLE tool_approval_decision
    ADD COLUMN delegate_model_selection_id uuid,
    ADD COLUMN delegate_model_call_id uuid UNIQUE,
    ADD COLUMN rationale text,
    DROP CONSTRAINT tool_approval_decision_source_closed,
    DROP CONSTRAINT tool_approval_decision_shape,
    DROP CONSTRAINT tool_approval_decision_source_shape,
    ADD CONSTRAINT tool_approval_decision_source_closed
        CHECK (
            decision_source IN (
                'owner_command', 'policy_auto', 'session_blanket', 'delegate'
            )
        ),
    ADD CONSTRAINT tool_approval_decision_shape
        CHECK (
            (decision_kind = 'approve' AND denial_reason IS NULL)
            OR (
                decision_kind = 'deny'
                AND decision_source = 'owner_command'
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
            OR (
                decision_kind = 'deny'
                AND decision_source = 'delegate'
                AND denial_reason IS NULL
            )
        ),
    ADD CONSTRAINT tool_approval_decision_source_shape
        CHECK (
            (
                decision_source = 'owner_command'
                AND owner_command_id IS NOT NULL
                AND delegate_model_selection_id IS NULL
                AND delegate_model_call_id IS NULL
                AND rationale IS NULL
            )
            OR (
                decision_source IN ('policy_auto', 'session_blanket')
                AND decision_kind = 'approve'
                AND owner_command_id IS NULL
                AND delegate_model_selection_id IS NULL
                AND delegate_model_call_id IS NULL
                AND rationale IS NULL
            )
            OR (
                decision_source = 'delegate'
                AND owner_command_id IS NULL
                AND delegate_model_selection_id IS NOT NULL
                AND delegate_model_call_id IS NOT NULL
                AND rationale IS NOT NULL
                AND octet_length(rationale) BETWEEN 1 AND 4096
            )
        ),
    ADD CONSTRAINT tool_approval_decision_delegate_call_fk
        FOREIGN KEY (delegate_model_call_id)
        REFERENCES tool_approval_judge_model_call (model_call_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

CREATE FUNCTION require_tool_approval_decision_authority()
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
    IF NEW.decision_source = 'owner_command' THEN
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

CREATE CONSTRAINT TRIGGER tool_approval_decision_authority
AFTER INSERT OR UPDATE ON tool_approval_decision
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_tool_approval_decision_authority();

CREATE FUNCTION require_completed_tool_approval_judge_decision()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    matching_decisions bigint;
BEGIN
    IF NEW.state_kind <> 'terminal'
       OR NEW.terminal_disposition_kind <> 'completed'
       OR NEW.recommendation_kind = 'escalate_to_human'
    THEN
        RETURN NULL;
    END IF;

    SELECT count(*)
      INTO matching_decisions
      FROM tool_approval_decision AS decision
     WHERE decision.request_id = NEW.request_id
       AND decision.decision_source = 'delegate'
       AND decision.decision_kind = NEW.recommendation_kind
       AND decision.delegate_model_selection_id =
           NEW.direct_model_selection_id
       AND decision.delegate_model_call_id = NEW.model_call_id
       AND decision.rationale = NEW.rationale;
    IF matching_decisions <> 1 THEN
        RAISE EXCEPTION
            'completed approval judge lacks its exact decision'
            USING
                ERRCODE = '23514',
                CONSTRAINT =
                    'tool_approval_judge_completed_requires_decision_effect';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER tool_approval_judge_completed_requires_decision_effect
AFTER INSERT OR UPDATE ON tool_approval_judge_model_call
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_completed_tool_approval_judge_decision();
