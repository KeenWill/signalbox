-- Program substrate: the program run journal — entries, streams, recorded
-- nondeterminism, and the per-run sequence state.

-- Function bodies may read tables a later section or file creates;
-- 202609010000_core.sql explains why validation is deferred.
SET check_function_bodies = false;

--
-- Functions.
--

--
-- Name: reject_program_journal_invalid_resolution(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_program_journal_invalid_resolution() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.resolves_request_ordinal IS NOT NULL AND NOT EXISTS (
        SELECT 1 FROM program_run_journal_entry AS request
         WHERE request.run_id = NEW.run_id
           AND request.request_ordinal = NEW.resolves_request_ordinal
           AND request.journal_position < NEW.journal_position
           AND request.frame_kind <> 'scope'
           AND (
               NEW.frame_kind <> 'reject'
               OR NEW.reject_reason <> 'outstanding_requests'
               OR request.frame_kind = 'terminal'
           )
    ) THEN
        RAISE EXCEPTION
            'delivery must resolve one earlier compatible request'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: reject_program_journal_truncate(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_program_journal_truncate() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RAISE EXCEPTION
        'program journal tables cannot be truncated'
        USING ERRCODE = '55000';
END;
$$;


--
-- Name: require_program_journal_append_sequence(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_program_journal_append_sequence() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    sequence program_run_journal_sequence_state%ROWTYPE;
BEGIN
    SELECT *
      INTO sequence
      FROM program_run_journal_sequence_state
     WHERE run_id = NEW.run_id
     FOR UPDATE;

    IF NOT FOUND
       OR NEW.journal_position <> sequence.last_position + 1
       OR (
           NEW.frame_direction = 'request'
           AND NEW.request_ordinal <> sequence.last_request_ordinal + 1
       )
       OR (
           NEW.frame_direction = 'delivery'
           AND NEW.delivery_ordinal <> sequence.last_delivery_ordinal + 1
       ) THEN
        RAISE EXCEPTION
            'program journal frame does not extend every applicable sequence by one'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: require_program_journal_nondeterminism_evidence(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_program_journal_nondeterminism_evidence() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    cause text;
    evidence_count bigint;
BEGIN
    SELECT fault_cause
      INTO cause
      FROM program_run_journal_entry
     WHERE run_id = NEW.run_id
       AND journal_position = NEW.journal_position;

    SELECT count(*)
      INTO evidence_count
      FROM program_run_journal_nondeterminism
     WHERE run_id = NEW.run_id
       AND journal_position = NEW.journal_position;

    IF (cause IS NOT DISTINCT FROM 'nondeterminism') <> (evidence_count = 1) THEN
        RAISE EXCEPTION
            'nondeterminism fault and its complete twin frames must commit together'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_program_journal_sequence_matches_entries(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_program_journal_sequence_matches_entries() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    sequence program_run_journal_sequence_state%ROWTYPE;
    maximum_position numeric(20, 0);
    maximum_request numeric(20, 0);
    maximum_delivery numeric(20, 0);
    position_count bigint;
    request_count bigint;
    delivery_count bigint;
BEGIN
    SELECT *
      INTO sequence
      FROM program_run_journal_sequence_state
     WHERE run_id = NEW.run_id;
    SELECT COALESCE(max(journal_position), 0),
           COALESCE(max(request_ordinal), 0),
           COALESCE(max(delivery_ordinal), 0),
           count(*),
           count(request_ordinal),
           count(delivery_ordinal)
      INTO maximum_position, maximum_request, maximum_delivery,
           position_count, request_count, delivery_count
      FROM program_run_journal_entry
     WHERE run_id = NEW.run_id;

    IF sequence.run_id IS NULL
       OR sequence.last_position <> maximum_position
       OR sequence.last_request_ordinal <> maximum_request
       OR sequence.last_delivery_ordinal <> maximum_delivery
       OR position_count <> maximum_position
       OR request_count <> maximum_request
       OR delivery_count <> maximum_delivery THEN
        RAISE EXCEPTION
            'program journal sequence state disagrees with committed frames'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_program_journal_stream_sequence_state(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_program_journal_stream_sequence_state() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM program_run_journal_sequence_state
         WHERE run_id = NEW.run_id
    ) THEN
        RAISE EXCEPTION
            'program journal stream requires one sequence state row'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Tables.
--

--
-- Name: program_run_journal_entry; Type: TABLE; Schema: public
--

CREATE TABLE program_run_journal_entry (
    run_id uuid NOT NULL,
    journal_position numeric(20,0) NOT NULL,
    frame_direction text NOT NULL COLLATE pg_catalog."C",
    frame_kind text NOT NULL COLLATE pg_catalog."C",
    request_ordinal numeric(20,0),
    delivery_ordinal numeric(20,0),
    resolves_request_ordinal numeric(20,0),
    request_scope_ordinal numeric(20,0),
    scope_operation text COLLATE pg_catalog."C",
    declared_scope_ordinal numeric(20,0),
    parent_scope_ordinal numeric(20,0),
    effect_capability text COLLATE pg_catalog."C",
    effect_method text,
    reject_reason text COLLATE pg_catalog."C",
    fault_cause text COLLATE pg_catalog."C",
    payload_inline bytea NOT NULL,
    recorded_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    CONSTRAINT program_run_journal_entry_direction_shape CHECK ((((frame_direction = 'request'::text) AND (request_ordinal IS NOT NULL) AND (delivery_ordinal IS NULL) AND (frame_kind = ANY (ARRAY['now'::text, 'random'::text, 'sleep'::text, 'await_event'::text, 'effect'::text, 'scope'::text, 'terminal'::text]))) OR ((frame_direction = 'delivery'::text) AND (request_ordinal IS NULL) AND (delivery_ordinal IS NOT NULL) AND (request_scope_ordinal IS NULL) AND (frame_kind = ANY (ARRAY['answer'::text, 'wake'::text, 'reject'::text, 'cancel'::text, 'run_cancel'::text, 'fault'::text]))))),
    CONSTRAINT program_run_journal_entry_effect_shape CHECK (((((frame_kind = 'effect'::text) AND (effect_capability IS NOT NULL) AND (effect_method IS NOT NULL)) OR ((frame_kind <> 'effect'::text) AND (effect_capability IS NULL) AND (effect_method IS NULL))) AND ((effect_capability IS NULL) OR (effect_capability = ANY (ARRAY['time'::text, 'random'::text, 'sleep'::text, 'subscribe'::text, 'session'::text, 'judge'::text, 'exec-stage'::text, 'corpus'::text, 'eval-record'::text, 'blob'::text, 'register'::text]))))),
    CONSTRAINT program_run_journal_entry_fault_shape CHECK ((((frame_kind = 'fault'::text) = (fault_cause IS NOT NULL)) AND ((fault_cause IS NULL) OR (fault_cause = ANY (ARRAY['timeout'::text, 'memory'::text, 'nondeterminism'::text, 'program_error'::text, 'contract_retired'::text, 'journal_bound'::text, 'payload_too_large'::text]))))),
    CONSTRAINT program_run_journal_entry_payload_shape CHECK (((octet_length(payload_inline) = 0) OR ((frame_kind <> ALL (ARRAY['scope'::text, 'reject'::text])) AND (NOT ((frame_kind = 'fault'::text) AND (fault_cause = 'nondeterminism'::text)))))),
    CONSTRAINT program_run_journal_entry_positive_ordinals CHECK (((journal_position BETWEEN (1)::numeric AND '18446744073709551615'::numeric) AND ((request_ordinal IS NULL) OR (request_ordinal BETWEEN (1)::numeric AND '18446744073709551615'::numeric)) AND ((delivery_ordinal IS NULL) OR (delivery_ordinal BETWEEN (1)::numeric AND '18446744073709551615'::numeric)) AND ((resolves_request_ordinal IS NULL) OR (resolves_request_ordinal BETWEEN (1)::numeric AND '18446744073709551615'::numeric)) AND ((request_scope_ordinal IS NULL) OR (request_scope_ordinal BETWEEN (1)::numeric AND '18446744073709551615'::numeric)) AND ((declared_scope_ordinal IS NULL) OR (declared_scope_ordinal BETWEEN (1)::numeric AND '18446744073709551615'::numeric)) AND ((parent_scope_ordinal IS NULL) OR (parent_scope_ordinal BETWEEN (1)::numeric AND '18446744073709551615'::numeric)))),
    CONSTRAINT program_run_journal_entry_reject_shape CHECK ((((frame_kind = 'reject'::text) = (reject_reason IS NOT NULL)) AND ((reject_reason IS NULL) OR (reject_reason = 'outstanding_requests'::text)))),
    CONSTRAINT program_run_journal_entry_resolution_shape CHECK (((frame_kind = ANY (ARRAY['answer'::text, 'wake'::text, 'reject'::text, 'cancel'::text])) = (resolves_request_ordinal IS NOT NULL))),
    CONSTRAINT program_run_journal_entry_scope_shape CHECK (((((frame_kind = 'scope'::text) AND (scope_operation IS NOT NULL) AND (declared_scope_ordinal IS NOT NULL)) OR ((frame_kind <> 'scope'::text) AND (scope_operation IS NULL) AND (declared_scope_ordinal IS NULL) AND (parent_scope_ordinal IS NULL))) AND ((scope_operation IS NULL) OR (scope_operation = ANY (ARRAY['open'::text, 'close'::text])))))
);


--
-- Name: program_run_journal_nondeterminism; Type: TABLE; Schema: public
--

CREATE TABLE program_run_journal_nondeterminism (
    run_id uuid NOT NULL,
    journal_position numeric(20,0) NOT NULL,
    expected_request_ordinal numeric(20,0) CONSTRAINT program_run_journal_nondeterm_expected_request_ordinal_not_null NOT NULL,
    expected_request_scope_ordinal numeric(20,0),
    expected_kind text NOT NULL COLLATE pg_catalog."C",
    expected_scope_operation text COLLATE pg_catalog."C",
    expected_declared_scope_ordinal numeric(20,0),
    expected_parent_scope_ordinal numeric(20,0),
    expected_effect_capability text COLLATE pg_catalog."C",
    expected_effect_method text,
    expected_payload_inline bytea CONSTRAINT program_run_journal_nondetermi_expected_payload_inline_not_null NOT NULL,
    observed_request_ordinal numeric(20,0) CONSTRAINT program_run_journal_nondeterm_observed_request_ordinal_not_null NOT NULL,
    observed_request_scope_ordinal numeric(20,0),
    observed_kind text NOT NULL COLLATE pg_catalog."C",
    observed_scope_operation text COLLATE pg_catalog."C",
    observed_declared_scope_ordinal numeric(20,0),
    observed_parent_scope_ordinal numeric(20,0),
    observed_effect_capability text COLLATE pg_catalog."C",
    observed_effect_method text,
    observed_payload_inline bytea CONSTRAINT program_run_journal_nondetermi_observed_payload_inline_not_null NOT NULL,
    CONSTRAINT program_run_journal_nondeterminism_expected_effect_shape CHECK ((((expected_kind = 'effect'::text) AND (expected_effect_capability IS NOT NULL) AND (expected_effect_capability = ANY (ARRAY['time'::text, 'random'::text, 'sleep'::text, 'subscribe'::text, 'session'::text, 'judge'::text, 'exec-stage'::text, 'corpus'::text, 'eval-record'::text, 'blob'::text, 'register'::text])) AND (expected_effect_method IS NOT NULL)) OR ((expected_kind <> 'effect'::text) AND (expected_effect_capability IS NULL) AND (expected_effect_method IS NULL)))),
    CONSTRAINT program_run_journal_nondeterminism_expected_scope_shape CHECK ((((expected_kind = 'scope'::text) AND (expected_scope_operation IS NOT NULL) AND (expected_scope_operation = ANY (ARRAY['open'::text, 'close'::text])) AND (expected_declared_scope_ordinal IS NOT NULL)) OR ((expected_kind <> 'scope'::text) AND (expected_scope_operation IS NULL) AND (expected_declared_scope_ordinal IS NULL) AND (expected_parent_scope_ordinal IS NULL)))),
    CONSTRAINT program_run_journal_nondeterminism_observed_effect_shape CHECK ((((observed_kind = 'effect'::text) AND (observed_effect_capability IS NOT NULL) AND (observed_effect_capability = ANY (ARRAY['time'::text, 'random'::text, 'sleep'::text, 'subscribe'::text, 'session'::text, 'judge'::text, 'exec-stage'::text, 'corpus'::text, 'eval-record'::text, 'blob'::text, 'register'::text])) AND (observed_effect_method IS NOT NULL)) OR ((observed_kind <> 'effect'::text) AND (observed_effect_capability IS NULL) AND (observed_effect_method IS NULL)))),
    CONSTRAINT program_run_journal_nondeterminism_observed_scope_shape CHECK ((((observed_kind = 'scope'::text) AND (observed_scope_operation IS NOT NULL) AND (observed_scope_operation = ANY (ARRAY['open'::text, 'close'::text])) AND (observed_declared_scope_ordinal IS NOT NULL)) OR ((observed_kind <> 'scope'::text) AND (observed_scope_operation IS NULL) AND (observed_declared_scope_ordinal IS NULL) AND (observed_parent_scope_ordinal IS NULL)))),
    CONSTRAINT program_run_journal_nondeterminism_ordinals_positive CHECK (((expected_request_ordinal BETWEEN (1)::numeric AND '18446744073709551615'::numeric) AND (observed_request_ordinal BETWEEN (1)::numeric AND '18446744073709551615'::numeric) AND ((expected_request_scope_ordinal IS NULL) OR (expected_request_scope_ordinal BETWEEN (1)::numeric AND '18446744073709551615'::numeric)) AND ((observed_request_scope_ordinal IS NULL) OR (observed_request_scope_ordinal BETWEEN (1)::numeric AND '18446744073709551615'::numeric)) AND ((expected_declared_scope_ordinal IS NULL) OR (expected_declared_scope_ordinal BETWEEN (1)::numeric AND '18446744073709551615'::numeric)) AND ((expected_parent_scope_ordinal IS NULL) OR (expected_parent_scope_ordinal BETWEEN (1)::numeric AND '18446744073709551615'::numeric)) AND ((observed_declared_scope_ordinal IS NULL) OR (observed_declared_scope_ordinal BETWEEN (1)::numeric AND '18446744073709551615'::numeric)) AND ((observed_parent_scope_ordinal IS NULL) OR (observed_parent_scope_ordinal BETWEEN (1)::numeric AND '18446744073709551615'::numeric)))),
    CONSTRAINT program_run_journal_nondeterminism_payload_shape CHECK ((((expected_kind <> 'scope'::text) OR (octet_length(expected_payload_inline) = 0)) AND ((observed_kind <> 'scope'::text) OR (octet_length(observed_payload_inline) = 0)))),
    CONSTRAINT program_run_journal_nondeterminism_request_kinds CHECK (((expected_kind = ANY (ARRAY['now'::text, 'random'::text, 'sleep'::text, 'await_event'::text, 'effect'::text, 'scope'::text, 'terminal'::text])) AND (observed_kind = ANY (ARRAY['now'::text, 'random'::text, 'sleep'::text, 'await_event'::text, 'effect'::text, 'scope'::text, 'terminal'::text]))))
);


--
-- Name: program_run_journal_sequence_state; Type: TABLE; Schema: public
--

CREATE TABLE program_run_journal_sequence_state (
    run_id uuid NOT NULL,
    last_position numeric(20,0) DEFAULT 0 NOT NULL,
    last_request_ordinal numeric(20,0) DEFAULT 0 CONSTRAINT program_run_journal_sequence_stat_last_request_ordinal_not_null NOT NULL,
    last_delivery_ordinal numeric(20,0) DEFAULT 0 CONSTRAINT program_run_journal_sequence_sta_last_delivery_ordinal_not_null NOT NULL,
    CONSTRAINT program_run_journal_sequence_state_ordinals_u64 CHECK (((last_position BETWEEN (0)::numeric AND '18446744073709551615'::numeric) AND (last_request_ordinal BETWEEN (0)::numeric AND '18446744073709551615'::numeric) AND (last_delivery_ordinal BETWEEN (0)::numeric AND '18446744073709551615'::numeric)))
);


--
-- Name: program_run_journal_stream; Type: TABLE; Schema: public
--

CREATE TABLE program_run_journal_stream (
    run_id uuid NOT NULL,
    frame_contract_version bigint NOT NULL,
    created_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    CONSTRAINT program_run_journal_stream_contract_v1 CHECK ((frame_contract_version = 1))
);


--
-- Constraints.
--

--
-- Name: program_run_journal_entry program_run_journal_entry_delivery_unique; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY program_run_journal_entry
    ADD CONSTRAINT program_run_journal_entry_delivery_unique UNIQUE (run_id, delivery_ordinal);


--
-- Name: program_run_journal_entry program_run_journal_entry_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY program_run_journal_entry
    ADD CONSTRAINT program_run_journal_entry_pk PRIMARY KEY (run_id, journal_position);


--
-- Name: program_run_journal_entry program_run_journal_entry_request_unique; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY program_run_journal_entry
    ADD CONSTRAINT program_run_journal_entry_request_unique UNIQUE (run_id, request_ordinal);


--
-- Name: program_run_journal_entry program_run_journal_entry_resolution_unique; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY program_run_journal_entry
    ADD CONSTRAINT program_run_journal_entry_resolution_unique UNIQUE (run_id, resolves_request_ordinal);


--
-- Name: program_run_journal_nondeterminism program_run_journal_nondeterminism_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY program_run_journal_nondeterminism
    ADD CONSTRAINT program_run_journal_nondeterminism_pk PRIMARY KEY (run_id, journal_position);


--
-- Name: program_run_journal_sequence_state program_run_journal_sequence_state_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY program_run_journal_sequence_state
    ADD CONSTRAINT program_run_journal_sequence_state_pkey PRIMARY KEY (run_id);


--
-- Name: program_run_journal_stream program_run_journal_stream_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY program_run_journal_stream
    ADD CONSTRAINT program_run_journal_stream_pkey PRIMARY KEY (run_id);


--
-- Triggers.
--

--
-- Name: program_run_journal_entry program_run_journal_entry_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER program_run_journal_entry_cannot_be_truncated BEFORE TRUNCATE ON program_run_journal_entry FOR EACH STATEMENT EXECUTE FUNCTION reject_program_journal_truncate();


--
-- Name: program_run_journal_entry program_run_journal_entry_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER program_run_journal_entry_is_append_only BEFORE DELETE OR UPDATE ON program_run_journal_entry FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: program_run_journal_entry program_run_journal_entry_nondeterminism_complete; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER program_run_journal_entry_nondeterminism_complete AFTER INSERT ON program_run_journal_entry DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_program_journal_nondeterminism_evidence();


--
-- Name: program_run_journal_entry program_run_journal_entry_sequence_complete; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER program_run_journal_entry_sequence_complete AFTER INSERT ON program_run_journal_entry DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_program_journal_sequence_matches_entries();


--
-- Name: program_run_journal_entry program_run_journal_entry_sequence_is_contiguous; Type: TRIGGER; Schema: public
--

CREATE TRIGGER program_run_journal_entry_sequence_is_contiguous BEFORE INSERT ON program_run_journal_entry FOR EACH ROW EXECUTE FUNCTION require_program_journal_append_sequence();


--
-- Name: program_run_journal_nondeterminism program_run_journal_nondeterminism_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER program_run_journal_nondeterminism_cannot_be_truncated BEFORE TRUNCATE ON program_run_journal_nondeterminism FOR EACH STATEMENT EXECUTE FUNCTION reject_program_journal_truncate();


--
-- Name: program_run_journal_nondeterminism program_run_journal_nondeterminism_complete; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER program_run_journal_nondeterminism_complete AFTER INSERT ON program_run_journal_nondeterminism DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_program_journal_nondeterminism_evidence();


--
-- Name: program_run_journal_nondeterminism program_run_journal_nondeterminism_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER program_run_journal_nondeterminism_is_append_only BEFORE DELETE OR UPDATE ON program_run_journal_nondeterminism FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: program_run_journal_entry program_run_journal_scope_requests_are_unanswered; Type: TRIGGER; Schema: public
--

CREATE TRIGGER program_run_journal_scope_requests_are_unanswered BEFORE INSERT ON program_run_journal_entry FOR EACH ROW EXECUTE FUNCTION reject_program_journal_invalid_resolution();


--
-- Name: program_run_journal_sequence_state program_run_journal_sequence_state_cannot_be_deleted; Type: TRIGGER; Schema: public
--

CREATE TRIGGER program_run_journal_sequence_state_cannot_be_deleted BEFORE DELETE ON program_run_journal_sequence_state FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: program_run_journal_sequence_state program_run_journal_sequence_state_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER program_run_journal_sequence_state_cannot_be_truncated BEFORE TRUNCATE ON program_run_journal_sequence_state FOR EACH STATEMENT EXECUTE FUNCTION reject_program_journal_truncate();


--
-- Name: program_run_journal_sequence_state program_run_journal_sequence_state_complete; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER program_run_journal_sequence_state_complete AFTER INSERT OR UPDATE ON program_run_journal_sequence_state DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_program_journal_sequence_matches_entries();


--
-- Name: program_run_journal_sequence_state program_run_journal_sequence_state_identity_is_immutable; Type: TRIGGER; Schema: public
--

CREATE TRIGGER program_run_journal_sequence_state_identity_is_immutable BEFORE UPDATE OF run_id ON program_run_journal_sequence_state FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: program_run_journal_stream program_run_journal_stream_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER program_run_journal_stream_cannot_be_truncated BEFORE TRUNCATE ON program_run_journal_stream FOR EACH STATEMENT EXECUTE FUNCTION reject_program_journal_truncate();


--
-- Name: program_run_journal_stream program_run_journal_stream_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER program_run_journal_stream_is_append_only BEFORE DELETE OR UPDATE ON program_run_journal_stream FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: program_run_journal_stream program_run_journal_stream_sequence_state_complete; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER program_run_journal_stream_sequence_state_complete AFTER INSERT ON program_run_journal_stream DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_program_journal_stream_sequence_state();


--
-- Foreign keys.
--

--
-- Name: program_run_journal_entry program_run_journal_entry_resolution_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY program_run_journal_entry
    ADD CONSTRAINT program_run_journal_entry_resolution_fk FOREIGN KEY (run_id, resolves_request_ordinal) REFERENCES program_run_journal_entry(run_id, request_ordinal) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: program_run_journal_entry program_run_journal_entry_run_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY program_run_journal_entry
    ADD CONSTRAINT program_run_journal_entry_run_fk FOREIGN KEY (run_id) REFERENCES program_run_journal_stream(run_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: program_run_journal_nondeterminism program_run_journal_nondeterminism_entry_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY program_run_journal_nondeterminism
    ADD CONSTRAINT program_run_journal_nondeterminism_entry_fk FOREIGN KEY (run_id, journal_position) REFERENCES program_run_journal_entry(run_id, journal_position) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: program_run_journal_sequence_state program_run_journal_sequence_state_run_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY program_run_journal_sequence_state
    ADD CONSTRAINT program_run_journal_sequence_state_run_fk FOREIGN KEY (run_id) REFERENCES program_run_journal_stream(run_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Restore body validation for anything applied after this file.
--

RESET check_function_bodies;
