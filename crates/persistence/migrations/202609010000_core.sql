-- Core event and command plumbing: the transactional outbox every domain
-- appends through, the singleton cursors that allocate and deliver its
-- sequence, the hub fence generation the daemon advances at startup, and the
-- durable command envelope that every typed command record hangs off.
--
-- This file opens the baseline schema. The 2026090100NN_*.sql files are one
-- schema, split by domain, applied in filename order; a later file may layer
-- its own triggers and constraints onto tables an earlier file created, and
-- foreign keys that would otherwise point forward across files live in
-- 202609010014_cross_domain_foreign_keys.sql. The schema was regenerated from
-- a `pg_dump` of a chain-built database; provenance, equivalence proofs, and
-- the rules that survive the reset are in docs/proposals/migration-reset.md.
--
-- Three properties are load-bearing everywhere:
--
--   * nothing is schema-qualified: names resolve through the caller's search
--     path, so an installation whose migrations run in a role-selected schema
--     works exactly as one installed into public;
--   * every check-constraint-reachable function carries a pinned search path
--     naming the migration-selected schema, then pg_catalog, then pg_temp,
--     without which a `pg_restore` of a logical backup fails while evaluating
--     constraints. The pins render the schema through current_schema, in a DO
--     block at the end of each file that defines pinned functions;
--   * function bodies are created unvalidated (`check_function_bodies` is off
--     for the duration of each file, restored at its end), because a body may
--     read tables that a later section or file creates.
--
-- Forward-only immutability applies to every file of the baseline:
-- corrections ship as new migrations on top, never as edits.

SET check_function_bodies = false;

--
-- Functions.
--

--
-- Name: allocate_outbox_event_sequence(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION allocate_outbox_event_sequence() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.event_sequence IS NOT NULL THEN
        RAISE EXCEPTION 'outbox event sequence is allocator-owned'
            USING ERRCODE = '23514';
    END IF;

    IF EXISTS (
        SELECT 1
          FROM outbox_delivery_state
         WHERE singleton
           AND last_delivery_xid = pg_current_xact_id()
    ) THEN
        RAISE EXCEPTION
            'outbox event append cannot follow delivery in one transaction'
            USING ERRCODE = '23514';
    END IF;

    UPDATE outbox_sequence_state
       SET last_sequence = last_sequence + 1,
           last_allocation_xid = pg_current_xact_id()
     WHERE singleton
       AND last_sequence < 18446744073709551615
    RETURNING last_sequence INTO NEW.event_sequence;

    IF NOT FOUND THEN
        RAISE EXCEPTION 'outbox event sequence exhausted'
            USING ERRCODE = '22003';
    END IF;

    RETURN NEW;
END;
$$;


--
-- Name: claim_deferred_final_state_validation(text, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION claim_deferred_final_state_validation(validation_kind text, checked_identity uuid) RETURNS boolean
    LANGUAGE plpgsql
    AS $$
DECLARE
    claims text := COALESCE(
        current_setting(
            'signalbox.deferred_final_state_validation_claims',
            true
        ),
        ''
    );
    claim text;
BEGIN
    IF validation_kind NOT IN ('turn_lifecycle', 'model_call', 'tool_round')
    THEN
        RAISE EXCEPTION 'unsupported deferred validation kind %',
            validation_kind
            USING ERRCODE = '23514';
    END IF;

    claim := validation_kind || ':' || checked_identity::text || ';';
    IF strpos(claims, claim) <> 0 THEN
        RETURN false;
    END IF;

    PERFORM set_config(
        'signalbox.deferred_final_state_validation_claims',
        claims || claim,
        true
    );
    RETURN true;
END;
$$;


--
-- Name: durable_command_belongs_to_session(uuid, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION durable_command_belongs_to_session(checked_command_id uuid, checked_session_id uuid) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    SELECT EXISTS (SELECT 1 FROM create_session_command
        WHERE command_id = checked_command_id AND created_session_id = checked_session_id)
    OR EXISTS (SELECT 1 FROM create_session_from_imported_frontier_command
        WHERE command_id = checked_command_id AND created_session_id = checked_session_id)
    OR EXISTS (SELECT 1 FROM replace_session_defaults_command
        WHERE command_id = checked_command_id AND session_id = checked_session_id)
    OR EXISTS (SELECT 1 FROM replace_session_metadata_command
        WHERE command_id = checked_command_id AND session_id = checked_session_id)
    OR EXISTS (SELECT 1 FROM submit_input_command
        WHERE command_id = checked_command_id AND session_id = checked_session_id)
    OR EXISTS (
        SELECT 1 FROM decide_tool_request_command AS command
        JOIN tool_request AS request ON request.request_id = command.request_id
        WHERE command.command_id = checked_command_id
          AND request.session_id = checked_session_id
    )
    OR EXISTS (SELECT 1 FROM compact_session_command
        WHERE command_id = checked_command_id AND session_id = checked_session_id)
    OR EXISTS (SELECT 1 FROM goal_command
        WHERE command_id = checked_command_id AND session_id = checked_session_id)
$$;


--
-- Name: reject_immutable_record_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_immutable_record_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RAISE EXCEPTION '% is append-only', TG_TABLE_NAME
        USING ERRCODE = '23514';
END;
$$;


--
-- Name: reject_invalid_hub_fence_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_invalid_hub_fence_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'hub fence state cannot be deleted'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.singleton IS DISTINCT FROM OLD.singleton
       OR NEW.generation IS DISTINCT FROM OLD.generation + 1 THEN
        RAISE EXCEPTION 'hub fence generation must advance exactly once'
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;


--
-- Name: reject_outbox_state_delete(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_outbox_state_delete() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RAISE EXCEPTION '% singleton cannot be deleted', TG_TABLE_NAME
        USING ERRCODE = '23514';
END;
$$;


--
-- Name: reject_outbox_table_truncate(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_outbox_table_truncate() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RAISE EXCEPTION '% cannot be truncated', TG_TABLE_NAME
        USING ERRCODE = '23514';
END;
$$;


--
-- Name: require_durable_command_typed_record(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_durable_command_typed_record() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_next_outbox_delivery(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_next_outbox_delivery() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.singleton <> OLD.singleton
        OR NEW.delivered_through <> OLD.delivered_through + 1 THEN
        RAISE EXCEPTION 'outbox delivery must advance by exactly one sequence'
            USING ERRCODE = '23514';
    END IF;
    IF EXISTS (
        SELECT 1 FROM outbox_sequence_state
         WHERE singleton AND last_allocation_xid = pg_current_xact_id()
    ) THEN
        RAISE EXCEPTION
            'outbox delivery cannot advance in an event-producing transaction'
            USING ERRCODE = '23514';
    END IF;
    NEW.last_delivery_xid := pg_current_xact_id();
    IF NOT EXISTS (
        SELECT 1 FROM outbox_event
         WHERE event_sequence = NEW.delivered_through
    ) AND NOT EXISTS (
        SELECT 1 FROM delegation_outbox_event
         WHERE event_sequence = NEW.delivered_through
    ) THEN
        RAISE EXCEPTION 'outbox delivery sequence % requires a committed event',
            NEW.delivered_through USING ERRCODE = '23503';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: require_outbox_event_typed_record(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_outbox_event_typed_record() RETURNS trigger
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
        WHEN 'goal_turn_retired' THEN
            SELECT count(*) INTO matching_records
              FROM goal_turn_retired_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_activated' THEN
            SELECT count(*) INTO matching_records
              FROM turn_activated_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_failed' THEN
            SELECT count(*) INTO matching_records
              FROM turn_failed_outbox_event
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
        WHEN 'turn_completed' THEN
            SELECT count(*) INTO matching_records
              FROM turn_completed_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_refused' THEN
            SELECT count(*) INTO matching_records
              FROM turn_refused_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_cancelled' THEN
            SELECT count(*) INTO matching_records
              FROM turn_cancelled_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_reconciliation_required' THEN
            SELECT count(*) INTO matching_records
              FROM turn_reconciliation_required_outbox_event
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
-- Name: require_outbox_sequence_event(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_outbox_sequence_event() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.last_sequence <> OLD.last_sequence + 1 THEN
        RAISE EXCEPTION 'outbox sequence must advance exactly once per event'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.last_allocation_xid IS DISTINCT FROM pg_current_xact_id() THEN
        RAISE EXCEPTION 'outbox allocator transaction must accompany its sequence'
            USING ERRCODE = '23514';
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM outbox_event
         WHERE event_sequence = NEW.last_sequence
    ) AND NOT EXISTS (
        SELECT 1 FROM delegation_outbox_event
         WHERE event_sequence = NEW.last_sequence
    ) THEN
        RAISE EXCEPTION 'outbox sequence % requires its event row', NEW.last_sequence
            USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Tables.
--

--
-- Name: durable_command; Type: TABLE; Schema: public
--

CREATE TABLE durable_command (
    command_id uuid NOT NULL,
    command_kind text NOT NULL,
    storage_version smallint NOT NULL,
    claimed_at timestamp with time zone NOT NULL,
    CONSTRAINT durable_command_kind_closed CHECK ((command_kind = ANY (ARRAY['create_session'::text, 'create_session_from_imported_frontier'::text, 'replace_session_defaults'::text, 'replace_session_metadata'::text, 'submit_input'::text, 'decide_tool_request'::text, 'override_denied_tool_request'::text, 'review_workflow'::text, 'review_orchestration'::text, 'compact_session'::text, 'goal'::text, 'update_session_placement'::text, 'register_workspace'::text, 'mint_git_remote'::text, 'withdraw_git_remote'::text]))),
    CONSTRAINT durable_command_storage_version_supported CHECK ((((command_kind = 'create_session'::text) AND (storage_version = ANY (ARRAY[1, 2, 3, 4, 5, 6, 7]))) OR ((command_kind = 'replace_session_defaults'::text) AND (storage_version = ANY (ARRAY[1, 2, 3, 4]))) OR ((command_kind = 'create_session_from_imported_frontier'::text) AND (storage_version = ANY (ARRAY[1, 2, 3, 5]))) OR ((command_kind = 'submit_input'::text) AND (storage_version = 3)) OR ((command_kind = ANY (ARRAY['replace_session_metadata'::text, 'decide_tool_request'::text, 'override_denied_tool_request'::text, 'review_workflow'::text, 'review_orchestration'::text, 'compact_session'::text, 'goal'::text, 'update_session_placement'::text, 'register_workspace'::text, 'mint_git_remote'::text, 'withdraw_git_remote'::text])) AND (storage_version = 1))))
);


--
-- Name: hub_fence_state; Type: TABLE; Schema: public
--

CREATE TABLE hub_fence_state (
    singleton boolean DEFAULT true NOT NULL,
    generation numeric(20,0) NOT NULL,
    CONSTRAINT hub_fence_state_generation_positive_u64 CHECK (((generation >= (1)::numeric) AND (generation <= '18446744073709551615'::numeric))),
    CONSTRAINT hub_fence_state_singleton CHECK (singleton)
);


--
-- Name: outbox_delivery_state; Type: TABLE; Schema: public
--

CREATE TABLE outbox_delivery_state (
    singleton boolean NOT NULL,
    delivered_through numeric(20,0) NOT NULL,
    last_delivery_xid xid8,
    CONSTRAINT outbox_delivery_state_singleton CHECK (singleton),
    CONSTRAINT outbox_delivery_state_transaction_recorded CHECK (((delivered_through = (0)::numeric) OR (last_delivery_xid IS NOT NULL))),
    CONSTRAINT outbox_delivery_state_u64 CHECK (((delivered_through >= (0)::numeric) AND (delivered_through <= '18446744073709551615'::numeric)))
);


--
-- Name: outbox_event; Type: TABLE; Schema: public
--

CREATE TABLE outbox_event (
    event_sequence numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    CONSTRAINT outbox_event_kind_closed CHECK ((event_kind = ANY (ARRAY['session_created'::text, 'session_model_settings_changed'::text, 'turn_model_settings_resolved'::text, 'input_accepted'::text, 'goal_turn_retired'::text, 'turn_activated'::text, 'turn_failed'::text, 'model_call_transition'::text, 'tool_batch_transition'::text, 'tool_approval_decided'::text, 'context_compacted'::text, 'turn_completed'::text, 'turn_refused'::text, 'turn_cancelled'::text, 'turn_reconciliation_required'::text, 'runner_state_transition'::text]))),
    CONSTRAINT outbox_event_sequence_positive_u64 CHECK (((event_sequence >= (1)::numeric) AND (event_sequence <= '18446744073709551615'::numeric))),
    CONSTRAINT outbox_event_storage_version_supported CHECK ((storage_version = 1))
);


--
-- Name: outbox_sequence_state; Type: TABLE; Schema: public
--

CREATE TABLE outbox_sequence_state (
    singleton boolean NOT NULL,
    last_sequence numeric(20,0) NOT NULL,
    last_allocation_xid xid8,
    CONSTRAINT outbox_sequence_state_allocator_recorded CHECK (((last_sequence = (0)::numeric) OR (last_allocation_xid IS NOT NULL))),
    CONSTRAINT outbox_sequence_state_singleton CHECK (singleton),
    CONSTRAINT outbox_sequence_state_u64 CHECK (((last_sequence >= (0)::numeric) AND (last_sequence <= '18446744073709551615'::numeric)))
);


--
-- Constraints.
--

--
-- Name: durable_command durable_command_kind_version_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY durable_command
    ADD CONSTRAINT durable_command_kind_version_key UNIQUE (command_id, command_kind, storage_version);


--
-- Name: durable_command durable_command_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY durable_command
    ADD CONSTRAINT durable_command_pkey PRIMARY KEY (command_id);


--
-- Name: hub_fence_state hub_fence_state_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY hub_fence_state
    ADD CONSTRAINT hub_fence_state_pkey PRIMARY KEY (singleton);


--
-- Name: outbox_delivery_state outbox_delivery_state_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY outbox_delivery_state
    ADD CONSTRAINT outbox_delivery_state_pkey PRIMARY KEY (singleton);


--
-- Name: outbox_event outbox_event_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY outbox_event
    ADD CONSTRAINT outbox_event_pkey PRIMARY KEY (event_sequence);


--
-- Name: outbox_event outbox_event_typed_record_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY outbox_event
    ADD CONSTRAINT outbox_event_typed_record_key UNIQUE (event_sequence, event_kind, storage_version, session_id);


--
-- Name: outbox_sequence_state outbox_sequence_state_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY outbox_sequence_state
    ADD CONSTRAINT outbox_sequence_state_pkey PRIMARY KEY (singleton);


--
-- Indexes.
--

--
-- Name: outbox_event_by_session_sequence; Type: INDEX; Schema: public
--

CREATE INDEX outbox_event_by_session_sequence ON outbox_event USING btree (session_id, event_sequence);


--
-- Name: outbox_event_turn_progress_by_session; Type: INDEX; Schema: public
--

CREATE INDEX outbox_event_turn_progress_by_session ON outbox_event USING btree (session_id, event_sequence) WHERE (event_kind <> ALL (ARRAY['session_created'::text, 'session_model_settings_changed'::text, 'turn_model_settings_resolved'::text, 'input_accepted'::text, 'goal_turn_retired'::text, 'runner_state_transition'::text]));


--
-- Triggers.
--

--
-- Name: durable_command durable_command_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER durable_command_is_append_only BEFORE DELETE OR UPDATE ON durable_command FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: durable_command durable_command_requires_typed_record; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER durable_command_requires_typed_record AFTER INSERT ON durable_command DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_durable_command_typed_record();


--
-- Name: hub_fence_state hub_fence_state_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER hub_fence_state_cannot_be_truncated BEFORE TRUNCATE ON hub_fence_state FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();


--
-- Name: hub_fence_state hub_fence_state_change_is_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER hub_fence_state_change_is_guarded BEFORE DELETE OR UPDATE ON hub_fence_state FOR EACH ROW EXECUTE FUNCTION reject_invalid_hub_fence_change();


--
-- Name: outbox_delivery_state outbox_delivery_advances_prefix; Type: TRIGGER; Schema: public
--

CREATE TRIGGER outbox_delivery_advances_prefix BEFORE UPDATE ON outbox_delivery_state FOR EACH ROW EXECUTE FUNCTION require_next_outbox_delivery();


--
-- Name: outbox_delivery_state outbox_delivery_state_cannot_be_deleted; Type: TRIGGER; Schema: public
--

CREATE TRIGGER outbox_delivery_state_cannot_be_deleted BEFORE DELETE ON outbox_delivery_state FOR EACH ROW EXECUTE FUNCTION reject_outbox_state_delete();


--
-- Name: outbox_delivery_state outbox_delivery_state_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER outbox_delivery_state_cannot_be_truncated BEFORE TRUNCATE ON outbox_delivery_state FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();


--
-- Name: outbox_event outbox_event_allocates_sequence; Type: TRIGGER; Schema: public
--

CREATE TRIGGER outbox_event_allocates_sequence BEFORE INSERT ON outbox_event FOR EACH ROW EXECUTE FUNCTION allocate_outbox_event_sequence();


--
-- Name: outbox_event outbox_event_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER outbox_event_cannot_be_truncated BEFORE TRUNCATE ON outbox_event FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();


--
-- Name: outbox_event outbox_event_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER outbox_event_is_append_only BEFORE DELETE OR UPDATE ON outbox_event FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: outbox_event outbox_event_requires_typed_record; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER outbox_event_requires_typed_record AFTER INSERT ON outbox_event DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_outbox_event_typed_record();


--
-- Name: outbox_sequence_state outbox_sequence_requires_event; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER outbox_sequence_requires_event AFTER UPDATE ON outbox_sequence_state DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_outbox_sequence_event();


--
-- Name: outbox_sequence_state outbox_sequence_state_cannot_be_deleted; Type: TRIGGER; Schema: public
--

CREATE TRIGGER outbox_sequence_state_cannot_be_deleted BEFORE DELETE ON outbox_sequence_state FOR EACH ROW EXECUTE FUNCTION reject_outbox_state_delete();


--
-- Name: outbox_sequence_state outbox_sequence_state_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER outbox_sequence_state_cannot_be_truncated BEFORE TRUNCATE ON outbox_sequence_state FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();


--
-- Singleton bootstrap rows.
--
-- A schema-only dump carries no rows, but these singletons are seeded by the
-- migration that creates them and are read before anything can write them:
-- the hub-fence generation the daemon advances at startup, and the outbox
-- sequence and delivery cursors every append allocates through. Without them
-- a fresh database boots into "outbox event sequence exhausted" on its first
-- append. The two automatic-reconciliation cursors are seeded the same way in
-- 202609010002_turns.sql.
--

INSERT INTO hub_fence_state (singleton, generation) VALUES (true, 1);
INSERT INTO outbox_delivery_state (singleton, delivered_through, last_delivery_xid) VALUES (true, 0, NULL);
INSERT INTO outbox_sequence_state (singleton, last_sequence, last_allocation_xid) VALUES (true, 0, NULL);


--
-- Restore body validation for anything applied after this file.
--

RESET check_function_bodies;
