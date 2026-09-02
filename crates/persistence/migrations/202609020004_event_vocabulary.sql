--
-- Session lifecycle §5 / §10: the module-facing outbox vocabulary.
--
-- Eight module-facing kinds, each with one typed record table:
-- `session_created` (storage version 2, carrying §6 provenance and the
-- ownership bit), `session_state_changed`, `session_terminal`,
-- `turn_terminal`, `goal_changed`, `command_settled`, `injection_settled`,
-- and `session_ownership_changed`. `turn_terminal` replaces the five
-- per-disposition turn kinds and `goal_turn_retired`, which becomes the
-- `retired` disposition. The nine core-internal kinds are untouched.
--
-- The ratified reset means no row under a replaced kind exists, so the
-- replaced tables are dropped rather than migrated.
--

SET check_function_bodies = false;

--
-- Header. The session column is nullable for exactly `command_settled`; the
-- turn disposition is denormalized onto the header so the turn-progress
-- frontier's partial index can exclude `turn_terminal{retired}`.
--

ALTER TABLE outbox_event
    ALTER COLUMN session_id DROP NOT NULL;

ALTER TABLE outbox_event
    ADD COLUMN turn_disposition text;

-- Supersedes 202609010000_core.
ALTER TABLE outbox_event
    DROP CONSTRAINT outbox_event_kind_closed;

ALTER TABLE outbox_event
    ADD CONSTRAINT outbox_event_kind_closed CHECK (
        event_kind = ANY (ARRAY[
            'session_created'::text,
            'session_state_changed'::text,
            'session_terminal'::text,
            'turn_terminal'::text,
            'goal_changed'::text,
            'command_settled'::text,
            'injection_settled'::text,
            'session_ownership_changed'::text,
            'session_model_settings_changed'::text,
            'turn_model_settings_resolved'::text,
            'input_accepted'::text,
            'turn_activated'::text,
            'model_call_transition'::text,
            'tool_batch_transition'::text,
            'tool_approval_decided'::text,
            'context_compacted'::text,
            'runner_state_transition'::text
        ])
    );

-- Supersedes 202609010000_core.
ALTER TABLE outbox_event
    DROP CONSTRAINT outbox_event_storage_version_supported;

ALTER TABLE outbox_event
    ADD CONSTRAINT outbox_event_storage_version_supported CHECK (
        storage_version = CASE event_kind
            WHEN 'session_created'::text THEN 2
            ELSE 1
        END
    );

ALTER TABLE outbox_event
    ADD CONSTRAINT outbox_event_session_required CHECK (
        (session_id IS NOT NULL) OR (event_kind = 'command_settled'::text)
    );

ALTER TABLE outbox_event
    ADD CONSTRAINT outbox_event_turn_disposition_shape CHECK (
        ((event_kind = 'turn_terminal'::text) = (turn_disposition IS NOT NULL))
        AND ((turn_disposition IS NULL) OR (turn_disposition = ANY (ARRAY[
            'completed'::text,
            'refused'::text,
            'failed'::text,
            'cancelled'::text,
            'reconciliation_required'::text,
            'retired'::text
        ])))
    );

-- The sessionless typed record authenticates its header through this key.
ALTER TABLE outbox_event
    ADD CONSTRAINT outbox_event_kind_version_key
        UNIQUE (event_sequence, event_kind, storage_version);

DROP INDEX outbox_event_turn_progress_by_session;

CREATE INDEX outbox_event_turn_progress_by_session
    ON outbox_event USING btree (session_id, event_sequence)
    WHERE (
        (event_kind = ANY (ARRAY[
            'turn_activated'::text,
            'model_call_transition'::text,
            'tool_batch_transition'::text,
            'tool_approval_decided'::text,
            'context_compacted'::text
        ]))
        OR ((event_kind = 'turn_terminal'::text)
            AND (turn_disposition <> 'retired'::text))
    );

--
-- Replaced typed record tables.
--

DROP TABLE goal_turn_retired_outbox_event;
DROP FUNCTION require_goal_turn_retired_outbox_state();
DROP TABLE turn_completed_outbox_event;
DROP TABLE turn_refused_outbox_event;
DROP TABLE turn_failed_outbox_event;
DROP TABLE turn_cancelled_outbox_event;
DROP TABLE turn_reconciliation_required_outbox_event;
DROP TABLE session_created_outbox_event;

--
-- `session_created`, storage version 2.
--

CREATE TABLE session_created_outbox_event (
    event_sequence numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    creation_cause text NOT NULL,
    dispatching_module text,
    dispatch_ref uuid,
    spawning_tool_request_id uuid,
    owned boolean NOT NULL,
    CONSTRAINT session_created_outbox_event_pkey PRIMARY KEY (event_sequence),
    CONSTRAINT session_created_outbox_event_session_id_key UNIQUE (session_id),
    CONSTRAINT session_created_outbox_event_kind_closed
        CHECK (event_kind = 'session_created'::text),
    CONSTRAINT session_created_outbox_event_storage_version_supported
        CHECK (storage_version = 2),
    CONSTRAINT session_created_outbox_event_cause_shape CHECK (
        ((creation_cause = 'interactive'::text)
            AND (spawning_tool_request_id IS NULL)
            AND (dispatching_module IS NULL)
            AND (dispatch_ref IS NULL))
        OR ((creation_cause = 'module_dispatched'::text)
            AND (spawning_tool_request_id IS NULL)
            AND (dispatching_module IS NOT NULL)
            AND (dispatch_ref IS NOT NULL))
        OR ((creation_cause = 'delegated'::text)
            AND (spawning_tool_request_id IS NOT NULL)
            AND (dispatching_module IS NULL)
            AND (dispatch_ref IS NULL))
    ),
    CONSTRAINT session_created_outbox_event_module_closed CHECK (
        (dispatching_module IS NULL)
        OR (dispatching_module = ANY (ARRAY[
            'repo_watch'::text,
            'commissioned_dispatch'::text
        ]))
    ),
    CONSTRAINT session_created_outbox_event_header_fk
        FOREIGN KEY (event_sequence, event_kind, storage_version, session_id)
        REFERENCES outbox_event (event_sequence, event_kind, storage_version, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT session_created_outbox_event_session_fk
        FOREIGN KEY (session_id) REFERENCES session (session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER session_created_outbox_event_cannot_be_truncated
    BEFORE TRUNCATE ON session_created_outbox_event
    FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();

CREATE TRIGGER session_created_outbox_event_is_append_only
    BEFORE DELETE OR UPDATE ON session_created_outbox_event
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

--
-- `turn_terminal`. One record per turn terminalization; the disposition
-- selects which members are present, exactly as the replaced tables did.
--

CREATE TABLE turn_terminal_outbox_event (
    event_sequence numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    disposition_kind text NOT NULL,
    model_call_id uuid,
    tool_attempt_id uuid,
    completion_entry_id uuid,
    failure_entry_id uuid,
    cancellation_entry_id uuid,
    terminal_frontier_id uuid,
    CONSTRAINT turn_terminal_outbox_event_pkey PRIMARY KEY (event_sequence),
    CONSTRAINT turn_terminal_outbox_kind_closed
        CHECK (event_kind = 'turn_terminal'::text),
    CONSTRAINT turn_terminal_outbox_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT turn_terminal_outbox_disposition_shape CHECK (
        ((disposition_kind = 'completed'::text)
            AND (model_call_id IS NOT NULL) AND (tool_attempt_id IS NULL)
            AND (completion_entry_id IS NOT NULL) AND (failure_entry_id IS NULL)
            AND (cancellation_entry_id IS NULL) AND (terminal_frontier_id IS NOT NULL))
        OR ((disposition_kind = 'refused'::text)
            AND (model_call_id IS NOT NULL) AND (tool_attempt_id IS NULL)
            AND (completion_entry_id IS NULL) AND (failure_entry_id IS NULL)
            AND (cancellation_entry_id IS NULL) AND (terminal_frontier_id IS NOT NULL))
        OR ((disposition_kind = 'failed'::text)
            AND (model_call_id IS NULL) AND (tool_attempt_id IS NULL)
            AND (completion_entry_id IS NULL) AND (failure_entry_id IS NOT NULL)
            AND (cancellation_entry_id IS NULL) AND (terminal_frontier_id IS NOT NULL))
        OR ((disposition_kind = 'cancelled'::text)
            AND (model_call_id IS NULL) AND (tool_attempt_id IS NULL)
            AND (completion_entry_id IS NULL) AND (failure_entry_id IS NULL)
            AND (cancellation_entry_id IS NOT NULL) AND (terminal_frontier_id IS NOT NULL))
        OR ((disposition_kind = 'reconciliation_required'::text)
            AND ((((model_call_id IS NOT NULL))::integer
                  + ((tool_attempt_id IS NOT NULL))::integer) = 1)
            AND (completion_entry_id IS NULL) AND (failure_entry_id IS NULL)
            AND (cancellation_entry_id IS NULL) AND (terminal_frontier_id IS NOT NULL))
        OR ((disposition_kind = 'retired'::text)
            AND (model_call_id IS NULL) AND (tool_attempt_id IS NULL)
            AND (completion_entry_id IS NULL) AND (failure_entry_id IS NULL)
            AND (cancellation_entry_id IS NULL) AND (terminal_frontier_id IS NULL))
    ),
    CONSTRAINT turn_terminal_outbox_header_fk
        FOREIGN KEY (event_sequence, event_kind, storage_version, session_id)
        REFERENCES outbox_event (event_sequence, event_kind, storage_version, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT turn_terminal_outbox_turn_fk
        FOREIGN KEY (turn_id, session_id)
        REFERENCES turn_lifecycle (turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT turn_terminal_outbox_call_fk
        FOREIGN KEY (model_call_id, turn_id, session_id)
        REFERENCES model_call (model_call_id, turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT turn_terminal_outbox_tool_attempt_fk
        FOREIGN KEY (tool_attempt_id, turn_id, session_id)
        REFERENCES tool_attempt (attempt_id, turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT turn_terminal_outbox_frontier_fk
        FOREIGN KEY (session_id, terminal_frontier_id)
        REFERENCES context_frontier (owning_session_id, context_frontier_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT turn_terminal_outbox_completion_entry_fk
        FOREIGN KEY (session_id, completion_entry_id)
        REFERENCES semantic_transcript_entry (source_session_id, semantic_entry_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT turn_terminal_outbox_failure_entry_fk
        FOREIGN KEY (session_id, failure_entry_id)
        REFERENCES semantic_transcript_entry (source_session_id, semantic_entry_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT turn_terminal_outbox_cancellation_entry_fk
        FOREIGN KEY (session_id, cancellation_entry_id)
        REFERENCES semantic_transcript_entry (source_session_id, semantic_entry_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

CREATE INDEX turn_terminal_outbox_event_by_turn
    ON turn_terminal_outbox_event USING btree (session_id, turn_id);

CREATE TRIGGER turn_terminal_outbox_event_cannot_be_truncated
    BEFORE TRUNCATE ON turn_terminal_outbox_event
    FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();

CREATE TRIGGER turn_terminal_outbox_event_is_append_only
    BEFORE DELETE OR UPDATE ON turn_terminal_outbox_event
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

--
-- `session_state_changed` and `session_terminal`: snapshots of the satellite
-- row the transition wrote, under the satellite's own column names.
--

CREATE TABLE session_state_changed_outbox_event (
    event_sequence numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    prior_state_kind text NOT NULL,
    state_kind text NOT NULL,
    state_entered_at timestamp with time zone NOT NULL,
    actor_kind text NOT NULL,
    actor_module text,
    actor_turn_id uuid,
    actor_tool_request_id uuid,
    waiting_kind text,
    waiting_waker text,
    waiting_subject_session_id uuid,
    recovering_op text,
    blocked_reason text,
    blocked_cycle bigint,
    parked_cause text,
    parked_responder text,
    parked_since timestamp with time zone,
    parked_standing_cause_kind text,
    CONSTRAINT session_state_changed_outbox_event_pkey PRIMARY KEY (event_sequence),
    CONSTRAINT session_state_changed_outbox_kind_closed
        CHECK (event_kind = 'session_state_changed'::text),
    CONSTRAINT session_state_changed_outbox_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT session_state_changed_outbox_state_closed CHECK (
        (prior_state_kind = ANY (ARRAY[
            'created'::text, 'dispatched'::text, 'active'::text, 'waiting'::text,
            'recovering'::text, 'blocked'::text, 'parked'::text
        ]))
        AND (state_kind = ANY (ARRAY[
            'created'::text, 'dispatched'::text, 'active'::text, 'waiting'::text,
            'recovering'::text, 'blocked'::text, 'parked'::text
        ]))
    ),
    CONSTRAINT session_state_changed_outbox_detail_shape CHECK (
        ((state_kind = 'waiting'::text)
            = ((waiting_kind IS NOT NULL) AND (waiting_waker IS NOT NULL)))
        AND ((state_kind = 'recovering'::text) = (recovering_op IS NOT NULL))
        AND ((state_kind = 'blocked'::text)
            = ((blocked_reason IS NOT NULL) AND (blocked_cycle IS NOT NULL)))
        AND ((state_kind = 'parked'::text)
            = ((parked_cause IS NOT NULL) AND (parked_responder IS NOT NULL)
               AND (parked_since IS NOT NULL)))
    ),
    CONSTRAINT session_state_changed_outbox_header_fk
        FOREIGN KEY (event_sequence, event_kind, storage_version, session_id)
        REFERENCES outbox_event (event_sequence, event_kind, storage_version, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER session_state_changed_outbox_event_cannot_be_truncated
    BEFORE TRUNCATE ON session_state_changed_outbox_event
    FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();

CREATE TRIGGER session_state_changed_outbox_event_is_append_only
    BEFORE DELETE OR UPDATE ON session_state_changed_outbox_event
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE session_terminal_outbox_event (
    event_sequence numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    prior_state_kind text NOT NULL,
    actor_kind text NOT NULL,
    actor_module text,
    actor_turn_id uuid,
    actor_tool_request_id uuid,
    ended_at timestamp with time zone NOT NULL,
    terminal_outcome_kind text NOT NULL,
    terminal_cause_kind text,
    terminal_stop_sticky boolean,
    terminal_superseded_by uuid,
    parked_standing_cause_kind text,
    parked_since timestamp with time zone,
    CONSTRAINT session_terminal_outbox_event_pkey PRIMARY KEY (event_sequence),
    CONSTRAINT session_terminal_outbox_event_session_id_key UNIQUE (session_id),
    CONSTRAINT session_terminal_outbox_kind_closed
        CHECK (event_kind = 'session_terminal'::text),
    CONSTRAINT session_terminal_outbox_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT session_terminal_outbox_outcome_closed CHECK (
        terminal_outcome_kind = ANY (ARRAY[
            'achieved_verified'::text, 'failed_retryable'::text,
            'failed_structural'::text, 'failed_unknown'::text, 'stopped'::text,
            'superseded'::text, 'abandoned'::text, 'retired'::text
        ])
    ),
    CONSTRAINT session_terminal_outbox_outcome_shape CHECK (
        ((terminal_outcome_kind = 'stopped'::text) = (terminal_stop_sticky IS NOT NULL))
        AND ((terminal_superseded_by IS NULL)
             OR (terminal_outcome_kind = 'superseded'::text))
        AND ((terminal_outcome_kind = ANY (ARRAY[
                'failed_retryable'::text, 'failed_structural'::text, 'retired'::text
            ])) = (terminal_cause_kind IS NOT NULL))
    ),
    CONSTRAINT session_terminal_outbox_header_fk
        FOREIGN KEY (event_sequence, event_kind, storage_version, session_id)
        REFERENCES outbox_event (event_sequence, event_kind, storage_version, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER session_terminal_outbox_event_cannot_be_truncated
    BEFORE TRUNCATE ON session_terminal_outbox_event
    FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();

CREATE TRIGGER session_terminal_outbox_event_is_append_only
    BEFORE DELETE OR UPDATE ON session_terminal_outbox_event
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

--
-- `goal_changed` and `session_ownership_changed` name one append-only
-- journal row each.
--

CREATE TABLE goal_changed_outbox_event (
    event_sequence numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    event_ordinal numeric(20,0) NOT NULL,
    CONSTRAINT goal_changed_outbox_event_pkey PRIMARY KEY (event_sequence),
    CONSTRAINT goal_changed_outbox_event_goal_event_key UNIQUE (session_id, event_ordinal),
    CONSTRAINT goal_changed_outbox_kind_closed
        CHECK (event_kind = 'goal_changed'::text),
    CONSTRAINT goal_changed_outbox_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT goal_changed_outbox_header_fk
        FOREIGN KEY (event_sequence, event_kind, storage_version, session_id)
        REFERENCES outbox_event (event_sequence, event_kind, storage_version, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT goal_changed_outbox_goal_event_fk
        FOREIGN KEY (session_id, event_ordinal)
        REFERENCES goal_event (session_id, event_ordinal)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER goal_changed_outbox_event_cannot_be_truncated
    BEFORE TRUNCATE ON goal_changed_outbox_event
    FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();

CREATE TRIGGER goal_changed_outbox_event_is_append_only
    BEFORE DELETE OR UPDATE ON goal_changed_outbox_event
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE session_ownership_changed_outbox_event (
    event_sequence numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    event_ordinal bigint NOT NULL,
    CONSTRAINT session_ownership_changed_outbox_event_pkey PRIMARY KEY (event_sequence),
    CONSTRAINT session_ownership_changed_outbox_event_journal_key
        UNIQUE (session_id, event_ordinal),
    CONSTRAINT session_ownership_changed_outbox_kind_closed
        CHECK (event_kind = 'session_ownership_changed'::text),
    CONSTRAINT session_ownership_changed_outbox_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT session_ownership_changed_outbox_header_fk
        FOREIGN KEY (event_sequence, event_kind, storage_version, session_id)
        REFERENCES outbox_event (event_sequence, event_kind, storage_version, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT session_ownership_changed_outbox_journal_fk
        FOREIGN KEY (session_id, event_ordinal)
        REFERENCES session_ownership_event (session_id, event_ordinal)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER session_ownership_changed_outbox_event_cannot_be_truncated
    BEFORE TRUNCATE ON session_ownership_changed_outbox_event
    FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();

CREATE TRIGGER session_ownership_changed_outbox_event_is_append_only
    BEFORE DELETE OR UPDATE ON session_ownership_changed_outbox_event
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

--
-- `command_settled`: the one kind that can settle without a session. Its
-- header proof is the three-column key, with no session member to null out.
--

CREATE TABLE command_settled_outbox_event (
    event_sequence numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid,
    command_id uuid NOT NULL,
    result_kind text NOT NULL,
    rejection_kind text,
    CONSTRAINT command_settled_outbox_event_pkey PRIMARY KEY (event_sequence),
    CONSTRAINT command_settled_outbox_event_command_id_key UNIQUE (command_id),
    CONSTRAINT command_settled_outbox_kind_closed
        CHECK (event_kind = 'command_settled'::text),
    CONSTRAINT command_settled_outbox_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT command_settled_outbox_result_shape CHECK (
        (result_kind = ANY (ARRAY['applied'::text, 'rejected'::text]))
        AND ((result_kind = 'rejected'::text) = (rejection_kind IS NOT NULL))
    ),
    CONSTRAINT command_settled_outbox_header_fk
        FOREIGN KEY (event_sequence, event_kind, storage_version)
        REFERENCES outbox_event (event_sequence, event_kind, storage_version)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT command_settled_outbox_command_fk
        FOREIGN KEY (command_id) REFERENCES durable_command (command_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT command_settled_outbox_session_fk
        FOREIGN KEY (session_id) REFERENCES session (session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER command_settled_outbox_event_cannot_be_truncated
    BEFORE TRUNCATE ON command_settled_outbox_event
    FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();

CREATE TRIGGER command_settled_outbox_event_is_append_only
    BEFORE DELETE OR UPDATE ON command_settled_outbox_event
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

--
-- `injection_settled`.
--

CREATE TABLE injection_settled_outbox_event (
    event_sequence numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    command_id uuid NOT NULL,
    outcome_kind text NOT NULL,
    rejection_kind text,
    delivered_turn_id uuid,
    CONSTRAINT injection_settled_outbox_event_pkey PRIMARY KEY (event_sequence),
    CONSTRAINT injection_settled_outbox_event_command_id_key UNIQUE (command_id),
    CONSTRAINT injection_settled_outbox_kind_closed
        CHECK (event_kind = 'injection_settled'::text),
    CONSTRAINT injection_settled_outbox_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT injection_settled_outbox_outcome_shape CHECK (
        (outcome_kind = ANY (ARRAY[
            'delivered'::text, 'not_delivered'::text, 'rejected'::text
        ]))
        AND ((outcome_kind = 'rejected'::text) = (rejection_kind IS NOT NULL))
        AND ((delivered_turn_id IS NULL) OR (outcome_kind = 'delivered'::text))
    ),
    CONSTRAINT injection_settled_outbox_header_fk
        FOREIGN KEY (event_sequence, event_kind, storage_version, session_id)
        REFERENCES outbox_event (event_sequence, event_kind, storage_version, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT injection_settled_outbox_command_fk
        FOREIGN KEY (command_id) REFERENCES durable_command (command_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT injection_settled_outbox_turn_fk
        FOREIGN KEY (delivered_turn_id, session_id)
        REFERENCES turn_lifecycle (turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER injection_settled_outbox_event_cannot_be_truncated
    BEFORE TRUNCATE ON injection_settled_outbox_event
    FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();

CREATE TRIGGER injection_settled_outbox_event_is_append_only
    BEFORE DELETE OR UPDATE ON injection_settled_outbox_event
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

--
-- Exactly one typed record per header. `turn_terminal` also proves the
-- header's denormalized disposition; `command_settled` proves the session
-- both ways through the nullable member.
--

CREATE OR REPLACE FUNCTION require_outbox_event_typed_record() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    matching_records bigint;
BEGIN
    CASE NEW.event_kind
        WHEN 'session_created' THEN
            SELECT count(*) INTO matching_records
              FROM session_created_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'session_state_changed' THEN
            SELECT count(*) INTO matching_records
              FROM session_state_changed_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'session_terminal' THEN
            SELECT count(*) INTO matching_records
              FROM session_terminal_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_terminal' THEN
            SELECT count(*) INTO matching_records
              FROM turn_terminal_outbox_event
             WHERE event_sequence = NEW.event_sequence
               AND disposition_kind = NEW.turn_disposition;
        WHEN 'goal_changed' THEN
            SELECT count(*) INTO matching_records
              FROM goal_changed_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'command_settled' THEN
            SELECT count(*) INTO matching_records
              FROM command_settled_outbox_event
             WHERE event_sequence = NEW.event_sequence
               AND session_id IS NOT DISTINCT FROM NEW.session_id;
        WHEN 'injection_settled' THEN
            SELECT count(*) INTO matching_records
              FROM injection_settled_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'session_ownership_changed' THEN
            SELECT count(*) INTO matching_records
              FROM session_ownership_changed_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'session_model_settings_changed' THEN
            SELECT count(*) INTO matching_records
              FROM session_model_settings_changed_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_model_settings_resolved' THEN
            SELECT count(*) INTO matching_records
              FROM turn_model_settings_resolved_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'input_accepted' THEN
            SELECT count(*) INTO matching_records
              FROM input_accepted_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_activated' THEN
            SELECT count(*) INTO matching_records
              FROM turn_activated_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'model_call_transition' THEN
            SELECT count(*) INTO matching_records
              FROM model_call_transition_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'tool_batch_transition' THEN
            SELECT count(*) INTO matching_records
              FROM tool_batch_transition_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'tool_approval_decided' THEN
            SELECT count(*) INTO matching_records
              FROM tool_approval_decided_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'context_compacted' THEN
            SELECT count(*) INTO matching_records
              FROM context_compacted_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'runner_state_transition' THEN
            SELECT count(*) INTO matching_records
              FROM runner_state_transition_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        ELSE
            RAISE EXCEPTION 'unsupported outbox event kind %', NEW.event_kind
                USING ERRCODE = '23514';
    END CASE;

    IF matching_records <> 1 THEN
        RAISE EXCEPTION 'outbox event % requires exactly one % typed record',
            NEW.event_sequence,
            NEW.event_kind
            USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;

--
-- The timeline projects `turn_terminal` under its per-disposition spelling,
-- so the header's byte accounting charges that spelling.
--

CREATE FUNCTION outbox_event_timeline_kind(kind text, disposition text) RETURNS text
    LANGUAGE sql IMMUTABLE
    AS $$
    SELECT CASE
        WHEN kind <> 'turn_terminal' THEN kind
        WHEN disposition = 'retired' THEN 'goal_turn_retired'
        ELSE 'turn_' || disposition
    END;
$$;

-- The delegation header carries no disposition, so the column is read
-- through the row's JSON form rather than by name.
CREATE OR REPLACE FUNCTION append_session_timeline_event_fact() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    UPDATE session_timeline_fact
       SET item_count = item_count + 1,
           first_sequence = coalesce(first_sequence, NEW.event_sequence),
           latest_sequence = NEW.event_sequence,
           event_kind_bytes = event_kind_bytes + octet_length(
               outbox_event_timeline_kind(
                   NEW.event_kind, to_jsonb(NEW) ->> 'turn_disposition'
               )
           )
     WHERE session_id = NEW.session_id;
    RETURN NULL;
END
$$;

--
-- Operator attention. The lifecycle, goal, and settlement kinds journal
-- nothing here: the satellite and goal rows they describe journal their own
-- change (`lifecycle`, `goal`), and a receipt changes no fact. The delegation
-- header, which carries no disposition, keeps a function of its own.
--

CREATE OR REPLACE FUNCTION record_operator_attention_outbox_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.event_kind IN (
        'session_state_changed', 'session_terminal', 'session_ownership_changed',
        'goal_changed', 'command_settled', 'injection_settled'
    ) THEN
        RETURN NULL;
    END IF;
    INSERT INTO operator_attention_change (session_id, fact_kind)
    VALUES (
        NEW.session_id,
        CASE NEW.event_kind
            WHEN 'session_created' THEN 'session'
            WHEN 'session_model_settings_changed' THEN 'session'
            WHEN 'turn_terminal' THEN CASE NEW.turn_disposition
                WHEN 'retired' THEN 'goal'
                ELSE 'turn'
            END
            WHEN 'runner_state_transition' THEN 'runner'
            WHEN 'turn_model_settings_resolved' THEN 'turn'
            WHEN 'input_accepted' THEN 'turn'
            WHEN 'turn_activated' THEN 'turn'
            WHEN 'model_call_transition' THEN 'turn'
            WHEN 'tool_batch_transition' THEN 'turn'
            WHEN 'tool_approval_decided' THEN 'turn'
            WHEN 'context_compacted' THEN 'turn'
            ELSE NULL
        END
    );
    RETURN NULL;
END;
$$;

CREATE FUNCTION record_operator_attention_delegation_outbox_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    INSERT INTO operator_attention_change (session_id, fact_kind)
    VALUES (NEW.session_id, 'turn');
    RETURN NULL;
END;
$$;

DROP TRIGGER delegation_outbox_event_records_operator_attention_change
    ON delegation_outbox_event;

CREATE TRIGGER delegation_outbox_event_records_operator_attention_change
    AFTER INSERT ON delegation_outbox_event
    FOR EACH ROW EXECUTE FUNCTION record_operator_attention_delegation_outbox_change();

--
-- §10: `retired` is a legal terminal disposition for a queued turn that never
-- activated. It carries no lineage, no frontier, and no attempt.
--

-- Supersedes 202609010002_turns.
ALTER TABLE turn_lifecycle
    DROP CONSTRAINT turn_lifecycle_terminal_disposition_closed;

ALTER TABLE turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_terminal_disposition_closed CHECK (
        (terminal_disposition_kind IS NULL)
        OR (terminal_disposition_kind = ANY (ARRAY[
            'failed'::text, 'completed'::text, 'refused'::text,
            'cancelled'::text, 'reconciliation_required'::text, 'retired'::text
        ]))
    );

-- Supersedes 202609010002_turns.
ALTER TABLE turn_lifecycle
    DROP CONSTRAINT turn_lifecycle_state_payload_shape;

ALTER TABLE turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_state_payload_shape CHECK ((((((state_kind = 'queued'::text) AND (start_lineage_kind IS NULL) AND (immediate_predecessor_turn_id IS NULL) AND (starting_frontier_id IS NULL) AND (terminal_frontier_id IS NULL) AND (active_phase_kind IS NULL) AND (current_attempt_id IS NULL) AND (terminal_disposition_kind IS NULL) AND (recovery_model_call_id IS NULL) AND (active_tool_round_call_id IS NULL) AND (approval_tool_request_id IS NULL) AND (recovery_tool_attempt_id IS NULL) AND (terminal_attempt_id IS NULL) AND (terminal_model_call_id IS NULL) AND (terminal_tool_attempt_id IS NULL)) OR ((state_kind = 'active'::text) AND (start_lineage_kind IS NOT NULL) AND (starting_frontier_id IS NOT NULL) AND (terminal_frontier_id IS NULL) AND (active_phase_kind = 'running'::text) AND (current_attempt_id IS NOT NULL) AND (terminal_disposition_kind IS NULL) AND (recovery_model_call_id IS NULL) AND (approval_tool_request_id IS NULL) AND (recovery_tool_attempt_id IS NULL) AND (terminal_attempt_id IS NULL) AND (terminal_model_call_id IS NULL) AND (terminal_tool_attempt_id IS NULL)) OR ((state_kind = 'active'::text) AND (start_lineage_kind IS NOT NULL) AND (starting_frontier_id IS NOT NULL) AND (terminal_frontier_id IS NULL) AND (active_phase_kind = 'awaiting_model_call_recovery'::text) AND (current_attempt_id IS NOT NULL) AND (terminal_disposition_kind IS NULL) AND (recovery_model_call_id IS NOT NULL) AND (active_tool_round_call_id IS NULL) AND (approval_tool_request_id IS NULL) AND (recovery_tool_attempt_id IS NULL) AND (terminal_attempt_id IS NULL) AND (terminal_model_call_id IS NULL) AND (terminal_tool_attempt_id IS NULL)) OR ((state_kind = 'active'::text) AND (start_lineage_kind IS NOT NULL) AND (starting_frontier_id IS NOT NULL) AND (terminal_frontier_id IS NULL) AND (active_phase_kind = 'awaiting_tool_approval'::text) AND (current_attempt_id IS NULL) AND (terminal_disposition_kind IS NULL) AND (recovery_model_call_id IS NULL) AND (active_tool_round_call_id IS NOT NULL) AND (approval_tool_request_id IS NOT NULL) AND (recovery_tool_attempt_id IS NULL) AND (terminal_attempt_id IS NULL) AND (terminal_model_call_id IS NULL) AND (terminal_tool_attempt_id IS NULL)) OR ((state_kind = 'active'::text) AND (start_lineage_kind IS NOT NULL) AND (starting_frontier_id IS NOT NULL) AND (terminal_frontier_id IS NULL) AND (active_phase_kind = 'awaiting_tool_recovery'::text) AND (current_attempt_id IS NOT NULL) AND (terminal_disposition_kind IS NULL) AND (recovery_model_call_id IS NULL) AND (active_tool_round_call_id IS NOT NULL) AND (approval_tool_request_id IS NULL) AND (recovery_tool_attempt_id IS NOT NULL) AND (terminal_attempt_id IS NULL) AND (terminal_model_call_id IS NULL) AND (terminal_tool_attempt_id IS NULL)) OR ((state_kind = 'terminal'::text) AND (start_lineage_kind IS NOT NULL) AND (starting_frontier_id IS NOT NULL) AND (terminal_frontier_id IS NOT NULL) AND (active_phase_kind IS NULL) AND (current_attempt_id IS NULL) AND (terminal_disposition_kind = 'failed'::text) AND (recovery_model_call_id IS NULL) AND (active_tool_round_call_id IS NULL) AND (approval_tool_request_id IS NULL) AND (recovery_tool_attempt_id IS NULL) AND (((terminal_attempt_id IS NULL) AND (terminal_model_call_id IS NULL) AND (terminal_tool_attempt_id IS NULL)) OR ((terminal_attempt_id IS NOT NULL) AND (terminal_tool_attempt_id IS NULL)))) OR ((state_kind = 'terminal'::text) AND (start_lineage_kind IS NOT NULL) AND (starting_frontier_id IS NOT NULL) AND (terminal_frontier_id IS NOT NULL) AND (active_phase_kind IS NULL) AND (current_attempt_id IS NULL) AND (terminal_disposition_kind = ANY (ARRAY['completed'::text, 'refused'::text])) AND (recovery_model_call_id IS NULL) AND (active_tool_round_call_id IS NULL) AND (approval_tool_request_id IS NULL) AND (recovery_tool_attempt_id IS NULL) AND (terminal_attempt_id IS NOT NULL) AND (terminal_model_call_id IS NOT NULL) AND (terminal_tool_attempt_id IS NULL)) OR ((state_kind = 'terminal'::text) AND (start_lineage_kind IS NOT NULL) AND (starting_frontier_id IS NOT NULL) AND (terminal_frontier_id IS NOT NULL) AND (active_phase_kind IS NULL) AND (current_attempt_id IS NULL) AND (terminal_disposition_kind = 'cancelled'::text) AND (recovery_model_call_id IS NULL) AND (active_tool_round_call_id IS NULL) AND (approval_tool_request_id IS NULL) AND (recovery_tool_attempt_id IS NULL) AND (terminal_attempt_id IS NOT NULL) AND (terminal_tool_attempt_id IS NULL)) OR ((state_kind = 'terminal'::text) AND (start_lineage_kind IS NOT NULL) AND (starting_frontier_id IS NOT NULL) AND (terminal_frontier_id IS NOT NULL) AND (active_phase_kind IS NULL) AND (current_attempt_id IS NULL) AND (terminal_disposition_kind = 'reconciliation_required'::text) AND (recovery_model_call_id IS NULL) AND (active_tool_round_call_id IS NULL) AND (approval_tool_request_id IS NULL) AND (recovery_tool_attempt_id IS NULL) AND (terminal_attempt_id IS NOT NULL) AND (((terminal_model_call_id IS NOT NULL) AND (terminal_tool_attempt_id IS NULL)) OR ((terminal_model_call_id IS NULL) AND (terminal_tool_attempt_id IS NOT NULL)))) OR ((state_kind = 'active'::text) AND (start_lineage_kind IS NOT NULL) AND (starting_frontier_id IS NOT NULL) AND (terminal_frontier_id IS NULL) AND (active_phase_kind = 'awaiting_child'::text) AND (current_attempt_id IS NULL) AND (terminal_disposition_kind IS NULL) AND (recovery_model_call_id IS NULL) AND (active_tool_round_call_id IS NOT NULL) AND (approval_tool_request_id IS NULL) AND (recovery_tool_attempt_id IS NULL) AND (terminal_attempt_id IS NULL) AND (terminal_model_call_id IS NULL) AND (terminal_tool_attempt_id IS NULL)) OR ((state_kind = 'terminal'::text) AND (start_lineage_kind IS NOT NULL) AND (starting_frontier_id IS NOT NULL) AND (terminal_frontier_id IS NOT NULL) AND (active_phase_kind IS NULL) AND (current_attempt_id IS NULL) AND (terminal_disposition_kind = 'cancelled'::text) AND (recovery_model_call_id IS NULL) AND (active_tool_round_call_id IS NULL) AND (approval_tool_request_id IS NULL) AND (recovery_tool_attempt_id IS NULL) AND (terminal_attempt_id IS NULL) AND (terminal_model_call_id IS NULL) AND (terminal_tool_attempt_id IS NULL)) OR ((state_kind = 'terminal'::text) AND (start_lineage_kind IS NULL) AND (immediate_predecessor_turn_id IS NULL) AND (starting_frontier_id IS NULL) AND (terminal_frontier_id IS NULL) AND (active_phase_kind IS NULL) AND (current_attempt_id IS NULL) AND (terminal_disposition_kind = 'retired'::text) AND (recovery_model_call_id IS NULL) AND (active_tool_round_call_id IS NULL) AND (approval_tool_request_id IS NULL) AND (recovery_tool_attempt_id IS NULL) AND (terminal_attempt_id IS NULL) AND (terminal_model_call_id IS NULL) AND (terminal_tool_attempt_id IS NULL) AND (child_wait_request_id IS NULL))) AND (runner_recovery_runner_id IS NULL) AND (runner_recovery_placement_revision IS NULL) AND (runner_recovery_tool_attempt_id IS NULL)) OR ((state_kind = 'active'::text) AND (start_lineage_kind IS NOT NULL) AND (starting_frontier_id IS NOT NULL) AND (terminal_frontier_id IS NULL) AND (active_phase_kind = 'awaiting_runner_recovery'::text) AND (current_attempt_id IS NULL) AND (terminal_disposition_kind IS NULL) AND (recovery_model_call_id IS NULL) AND (approval_tool_request_id IS NULL) AND (recovery_tool_attempt_id IS NULL) AND (child_wait_request_id IS NULL) AND (terminal_attempt_id IS NULL) AND (terminal_model_call_id IS NULL) AND (terminal_tool_attempt_id IS NULL) AND (runner_recovery_runner_id IS NOT NULL) AND (runner_recovery_placement_revision IS NOT NULL))));

-- Supersedes 202609020001_terminal_causes.
ALTER TABLE turn_lifecycle
    DROP CONSTRAINT turn_lifecycle_terminal_cause_closed;

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
            'unclassified_failure'::text,
            'goal_turn_ineligible'::text
        ]))
    );

-- Supersedes 202609020001_terminal_causes.
ALTER TABLE turn_lifecycle
    DROP CONSTRAINT turn_lifecycle_terminal_cause_matches_disposition;

ALTER TABLE turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_terminal_cause_matches_disposition CHECK (
        (terminal_cause_kind IS NULL)
        OR ((terminal_disposition_kind = 'completed'::text)
            AND (terminal_cause_kind = 'completed'::text))
        OR ((terminal_disposition_kind = 'refused'::text)
            AND (terminal_cause_kind = 'model_refusal'::text))
        OR ((terminal_disposition_kind = 'cancelled'::text)
            AND (terminal_cause_kind = 'interrupt_applied'::text))
        OR ((terminal_disposition_kind = 'retired'::text)
            AND (terminal_cause_kind = 'goal_turn_ineligible'::text))
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

-- A retired turn never started, so the terminal final-state assertions, which
-- check a started turn's origin, frontiers, and attempts, do not apply to it.
CREATE OR REPLACE FUNCTION assert_turn_lifecycle_final_state(checked_turn_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NOT claim_deferred_final_state_validation('turn_lifecycle', checked_turn_id) THEN
        RETURN;
    END IF;

    IF EXISTS (
        SELECT 1
          FROM turn_lifecycle
         WHERE turn_id = checked_turn_id
           AND state_kind = 'terminal'
           AND terminal_disposition_kind = 'retired'
    ) THEN
        RETURN;
    END IF;

    IF EXISTS (
        SELECT 1
          FROM tool_round
         WHERE turn_id = checked_turn_id
    ) THEN
        PERFORM assert_tool_loop_turn_final_state(checked_turn_id);
    ELSE
        PERFORM assert_turn_lifecycle_final_state_without_tool_loop(
            checked_turn_id
        );
    END IF;
END;
$$;

-- A retired turn stays out of queue order and predecessor selection exactly
-- as the retired queued work it replaces did.
CREATE OR REPLACE FUNCTION goal_turn_is_queue_order_relevant(checked_session uuid, checked_turn uuid) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    SELECT COALESCE((
        SELECT (
            NOT (
                lifecycle.state_kind = 'terminal'
                AND lifecycle.terminal_disposition_kind = 'retired'
            )
            AND (
                lifecycle.state_kind <> 'queued'
                OR goal.turn_id IS NULL
                OR (
                    SELECT (
                        event.event_kind IN ('commissioned', 'resumed')
                        AND event.generation = goal.goal_generation
                    ) OR (
                        event.event_kind = 'superseded'
                        AND event.generation < 18446744073709551615
                        AND event.generation + 1 = goal.goal_generation
                    )
                      FROM goal_event AS event
                     WHERE event.session_id = checked_session
                     ORDER BY event.event_ordinal DESC
                     LIMIT 1
                )
            )
        )
          FROM turn_lifecycle AS lifecycle
          LEFT JOIN goal_turn AS goal
            ON goal.session_id = lifecycle.session_id
           AND goal.turn_id = lifecycle.turn_id
         WHERE lifecycle.session_id = checked_session
           AND lifecycle.turn_id = checked_turn
    ), true);
$$;

-- A retired turn is terminal, so the closure guard no longer needs the outbox
-- exemption the satellite migration carried until this disposition existed.
CREATE OR REPLACE FUNCTION require_terminal_session_has_no_live_turn() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    live uuid;
BEGIN
    IF NEW.state_kind <> 'terminal' THEN
        RETURN NULL;
    END IF;

    SELECT lifecycle.turn_id INTO live
      FROM turn_lifecycle AS lifecycle
     WHERE lifecycle.session_id = NEW.session_id
       AND lifecycle.state_kind <> 'terminal'
     LIMIT 1;

    IF FOUND THEN
        RAISE EXCEPTION
            'terminal session % still holds non-terminal turn %',
            NEW.session_id, live
            USING ERRCODE = '23514';
    END IF;

    RETURN NULL;
END;
$$;

--
-- Three baseline functions read the replaced tables by name. Each is
-- re-created from its stored definition with the table reference moved to
-- `turn_terminal_outbox_event` under the matching disposition; a definition
-- the rewrite does not touch fails the migration rather than passing silently.
--

DO $rewrite$
DECLARE
    definition text;
    rewritten text;
BEGIN
    definition := pg_get_functiondef('assert_cancelled_turn_final_state(uuid)'::regprocedure);
    rewritten := regexp_replace(
        definition,
        'FROM turn_cancelled_outbox_event\s+WHERE session_id = checked_session',
        'FROM turn_terminal_outbox_event WHERE disposition_kind = ''cancelled'' AND session_id = checked_session'
    );
    IF rewritten = definition THEN
        RAISE EXCEPTION 'assert_cancelled_turn_final_state did not reference the replaced table';
    END IF;
    EXECUTE rewritten;

    definition := pg_get_functiondef('assert_reconciliation_required_turn_final_state(uuid)'::regprocedure);
    rewritten := regexp_replace(
        definition,
        'FROM turn_reconciliation_required_outbox_event\s+WHERE session_id = checked_session',
        'FROM turn_terminal_outbox_event WHERE disposition_kind = ''reconciliation_required'' AND session_id = checked_session'
    );
    IF rewritten = definition THEN
        RAISE EXCEPTION 'assert_reconciliation_required_turn_final_state did not reference the replaced table';
    END IF;
    EXECUTE rewritten;

    definition := pg_get_functiondef('require_session_delegation_event_payload()'::regprocedure);
    rewritten := regexp_replace(
        definition,
        'JOIN turn_cancelled_outbox_event AS cancellation\s+ON cancellation\.session_id = lifecycle\.session_id',
        'JOIN turn_terminal_outbox_event AS cancellation ON cancellation.disposition_kind = ''cancelled'' AND cancellation.session_id = lifecycle.session_id'
    );
    IF rewritten = definition THEN
        RAISE EXCEPTION 'require_session_delegation_event_payload did not reference the replaced table';
    END IF;
    EXECUTE rewritten;
END
$rewrite$;

--
-- `session_terminal` is emitted by the satellite's own trigger: every closure
-- passes through one UPDATE of the satellite row. `session_state_changed` has
-- its typed record and decoder; its emission lands with the deadline engine.
--

CREATE FUNCTION append_session_terminal_outbox_event() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    allocated numeric(20, 0);
BEGIN
    INSERT INTO outbox_event (event_kind, storage_version, session_id)
    VALUES ('session_terminal', 1, NEW.session_id)
    RETURNING event_sequence INTO allocated;
    INSERT INTO session_terminal_outbox_event
        (event_sequence, event_kind, storage_version, session_id,
         prior_state_kind, actor_kind, actor_module, actor_turn_id,
         actor_tool_request_id, ended_at, terminal_outcome_kind,
         terminal_cause_kind, terminal_stop_sticky, terminal_superseded_by,
         parked_standing_cause_kind, parked_since)
    VALUES
        (allocated, 'session_terminal', 1, NEW.session_id,
         OLD.state_kind, NEW.actor_kind, NEW.actor_module, NEW.actor_turn_id,
         NEW.actor_tool_request_id, NEW.ended_at, NEW.terminal_outcome_kind,
         NEW.terminal_cause_kind, NEW.terminal_stop_sticky,
         NEW.terminal_superseded_by, NEW.parked_standing_cause_kind,
         NEW.parked_since);
    RETURN NULL;
END;
$$;

CREATE TRIGGER session_lifecycle_appends_terminal_outbox_event
    AFTER UPDATE OF state_kind ON session_lifecycle
    FOR EACH ROW
    WHEN (NEW.state_kind = 'terminal'::text AND OLD.state_kind <> 'terminal'::text)
    EXECUTE FUNCTION append_session_terminal_outbox_event();

-- `append_session_timeline_event_fact` is re-created above, which drops the
-- search-path pin its baseline file applied; restore it.
DO $search_path_pins$
BEGIN
    EXECUTE format('ALTER FUNCTION append_session_timeline_event_fact() SET search_path TO "$user", %I',
               current_schema);
END
$search_path_pins$;

RESET check_function_bodies;
