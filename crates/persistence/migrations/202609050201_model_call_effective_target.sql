-- Record the exact serving target selected after fast-mode mapping.

ALTER TABLE model_call
    ADD COLUMN effective_provider_model_identity_id uuid NOT NULL;

CREATE OR REPLACE FUNCTION reject_model_call_invalid_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.state_kind <> 'prepared' THEN
            RAISE EXCEPTION 'model call must be inserted as Prepared'
                USING
                    ERRCODE = '23514',
                    CONSTRAINT = 'model_call_inserted_prepared';
        END IF;
        RETURN NEW;
    END IF;

    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'model_call is not deletable'
            USING ERRCODE = '23514';
    END IF;

    IF ROW(
        OLD.model_call_id,
        OLD.turn_id,
        OLD.session_id,
        OLD.turn_attempt_id,
        OLD.selection_kind,
        OLD.direct_model_selection_id,
        OLD.frozen_model_alias_id,
        OLD.frozen_alias_selected_direct_id,
        OLD.resolved_provider_model_identity_id,
        OLD.effective_provider_model_identity_id,
        OLD.context_frontier_id,
        OLD.credential_reference
    ) IS DISTINCT FROM ROW(
        NEW.model_call_id,
        NEW.turn_id,
        NEW.session_id,
        NEW.turn_attempt_id,
        NEW.selection_kind,
        NEW.direct_model_selection_id,
        NEW.frozen_model_alias_id,
        NEW.frozen_alias_selected_direct_id,
        NEW.resolved_provider_model_identity_id,
        NEW.effective_provider_model_identity_id,
        NEW.context_frontier_id,
        NEW.credential_reference
    ) AND NOT (
        OLD.state_kind = 'prepared'
        AND NEW.state_kind = 'in_flight'
        AND ROW(
            OLD.model_call_id,
            OLD.turn_id,
            OLD.session_id,
            OLD.turn_attempt_id,
            OLD.selection_kind,
            OLD.direct_model_selection_id,
            OLD.frozen_model_alias_id,
            OLD.frozen_alias_selected_direct_id,
            OLD.resolved_provider_model_identity_id,
            OLD.context_frontier_id,
            OLD.credential_reference
        ) IS NOT DISTINCT FROM ROW(
            NEW.model_call_id,
            NEW.turn_id,
            NEW.session_id,
            NEW.turn_attempt_id,
            NEW.selection_kind,
            NEW.direct_model_selection_id,
            NEW.frozen_model_alias_id,
            NEW.frozen_alias_selected_direct_id,
            NEW.resolved_provider_model_identity_id,
            NEW.context_frontier_id,
            NEW.credential_reference
        )
    ) THEN
        RAISE EXCEPTION 'model call authorization facts are immutable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'terminal' THEN
        RAISE EXCEPTION 'terminal model call is immutable'
            USING ERRCODE = '23514';
    END IF;

    IF NOT (
        OLD.state_kind = NEW.state_kind
        OR (
            OLD.state_kind = 'prepared'
            AND NEW.state_kind IN ('in_flight', 'terminal')
        )
        OR (
            OLD.state_kind = 'in_flight'
            AND NEW.state_kind IN ('cancellation_requested', 'terminal')
        )
        OR (
            OLD.state_kind = 'cancellation_requested'
            AND NEW.state_kind = 'terminal'
        )
    ) THEN
        RAISE EXCEPTION 'model call transition is not monotonic'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'prepared'
       AND NEW.state_kind = 'terminal'
       AND NEW.terminal_disposition_kind NOT IN ('known_failed', 'cancelled')
    THEN
        RAISE EXCEPTION 'an unsent call has an impossible terminal disposition'
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;
