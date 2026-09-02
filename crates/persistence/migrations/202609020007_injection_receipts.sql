-- Injection contract (docs/proposals/session-lifecycle.md §8).
--
-- Pending steering gains its third disposition, `closed_not_delivered`: the
-- session terminalized, or committed to its terminal outcome, before any
-- boundary could carry the input. The change guard admits `pending_steering`
-- → `closed_not_delivered`, and a deferred trigger requires the session to be
-- terminal or to hold a committed pending terminal. `injection_settled`
-- closes its rejection vocabulary. Every constraint here widens; no stored
-- row changes.

-- Supersedes the definition in 202609010002_turns.
ALTER TABLE accepted_input
    DROP CONSTRAINT accepted_input_disposition_closed,
    ADD CONSTRAINT accepted_input_disposition_closed CHECK (
        disposition_kind = ANY (ARRAY[
            'origin_of'::text,
            'pending_steering'::text,
            'consumed_as_steering'::text,
            'reclassified_as_turn_origin'::text,
            'closed_not_delivered'::text
        ])
    );

-- Supersedes the definition in 202609010002_turns.
ALTER TABLE accepted_input
    DROP CONSTRAINT accepted_input_delivery_shape,
    ADD CONSTRAINT accepted_input_delivery_shape CHECK (
        (
            disposition_kind = 'origin_of'::text
            AND delivery_kind = ANY (ARRAY[
                'start_when_no_active_turn'::text,
                'after_current_turn'::text,
                'interrupt'::text
            ])
            AND (
                (delivery_kind = 'start_when_no_active_turn'::text
                    AND expected_active_turn_id IS NULL)
                OR (delivery_kind = ANY (ARRAY['after_current_turn'::text, 'interrupt'::text])
                    AND expected_active_turn_id IS NOT NULL)
            )
            AND expected_defaults_version IS NOT NULL
            AND model_override_kind IS NOT NULL
            AND origin_turn_id IS NOT NULL
            AND consuming_model_call_id IS NULL
        )
        OR (
            disposition_kind = ANY (ARRAY[
                'pending_steering'::text,
                'consumed_as_steering'::text,
                'closed_not_delivered'::text
            ])
            AND delivery_kind = 'next_safe_point'::text
            AND expected_active_turn_id IS NOT NULL
            AND expected_defaults_version IS NULL
            AND model_override_kind IS NULL
            AND replacement_model_kind IS NULL
            AND replacement_direct_model_selection_id IS NULL
            AND replacement_model_alias_id IS NULL
            AND origin_turn_id IS NULL
            AND (
                (disposition_kind = ANY (ARRAY['pending_steering'::text, 'closed_not_delivered'::text])
                    AND consuming_model_call_id IS NULL)
                OR (disposition_kind = 'consumed_as_steering'::text
                    AND consuming_model_call_id IS NOT NULL)
            )
        )
        OR (
            disposition_kind = 'reclassified_as_turn_origin'::text
            AND delivery_kind = 'next_safe_point'::text
            AND expected_active_turn_id IS NOT NULL
            AND expected_defaults_version IS NULL
            AND model_override_kind IS NULL
            AND replacement_model_kind IS NULL
            AND replacement_direct_model_selection_id IS NULL
            AND replacement_model_alias_id IS NULL
            AND origin_turn_id IS NOT NULL
            AND consuming_model_call_id IS NULL
        )
    );

CREATE OR REPLACE FUNCTION reject_invalid_accepted_input_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'accepted_input is not deletable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.disposition_kind = 'pending_steering'
       AND NEW.disposition_kind IN (
            'consumed_as_steering',
            'reclassified_as_turn_origin',
            'closed_not_delivered'
       )
       AND OLD.origin_turn_id IS NULL
       AND OLD.consuming_model_call_id IS NULL
       AND (
            (
                NEW.disposition_kind = 'consumed_as_steering'
                AND NEW.origin_turn_id IS NULL
                AND NEW.consuming_model_call_id IS NOT NULL
            )
            OR
            (
                NEW.disposition_kind = 'reclassified_as_turn_origin'
                AND NEW.origin_turn_id IS NOT NULL
                AND NEW.consuming_model_call_id IS NULL
            )
            OR
            (
                NEW.disposition_kind = 'closed_not_delivered'
                AND NEW.origin_turn_id IS NULL
                AND NEW.consuming_model_call_id IS NULL
            )
       )
       AND ROW(
            OLD.accepted_input_id,
            OLD.accepting_command_id,
            OLD.session_id,
            OLD.delivery_kind,
            OLD.expected_active_turn_id,
            OLD.expected_defaults_version,
            OLD.model_override_kind,
            OLD.replacement_model_kind,
            OLD.replacement_direct_model_selection_id,
            OLD.replacement_model_alias_id,
            OLD.acceptance_position
       ) IS NOT DISTINCT FROM ROW(
            NEW.accepted_input_id,
            NEW.accepting_command_id,
            NEW.session_id,
            NEW.delivery_kind,
            NEW.expected_active_turn_id,
            NEW.expected_defaults_version,
            NEW.model_override_kind,
            NEW.replacement_model_kind,
            NEW.replacement_direct_model_selection_id,
            NEW.replacement_model_alias_id,
            NEW.acceptance_position
       )
    THEN
        RETURN NEW;
    END IF;

    RAISE EXCEPTION
        'accepted_input is immutable outside pending-steering disposition'
        USING ERRCODE = '23514';
END;
$$;

CREATE FUNCTION require_closed_steering_terminal_session() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.disposition_kind = 'closed_not_delivered'
       AND NOT EXISTS (
            SELECT 1
              FROM session_lifecycle
             WHERE session_id = NEW.session_id
               AND (
                    state_kind = 'terminal'
                    OR pending_terminal_outcome_kind IS NOT NULL
               )
       )
    THEN
        RAISE EXCEPTION
            'closed steering % requires a terminal or closing session',
            NEW.accepted_input_id
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'accepted_input_closed_requires_terminal_session';
    END IF;

    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER accepted_input_closed_requires_terminal_session
    AFTER INSERT OR UPDATE ON accepted_input
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION require_closed_steering_terminal_session();

ALTER TABLE injection_settled_outbox_event
    ADD CONSTRAINT injection_settled_outbox_rejection_closed CHECK (
        rejection_kind IS NULL
        OR rejection_kind = ANY (ARRAY[
            'attachment_blob_not_found'::text,
            'attachment_byte_budget_exceeded'::text,
            'no_active_turn'::text,
            'active_turn_present'::text,
            'active_turn_mismatch'::text,
            'session_defaults_version_mismatch'::text,
            'unknown_model_alias'::text,
            'acceptance_position_exhausted'::text,
            'interrupt_already_applied'::text,
            'interrupt_unavailable_while_awaiting_approval'::text,
            'not_earliest_undecided'::text
        ])
    );
