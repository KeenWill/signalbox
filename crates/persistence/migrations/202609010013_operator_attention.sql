-- Operator attention: the attention change feed and the judge facts
-- projection, maintained by triggers layered across the schema's
-- attention-worthy tables.

-- Function bodies may read tables a later section or file creates;
-- 202609010000_core.sql explains why validation is deferred.
SET check_function_bodies = false;

--
-- Functions.
--

--
-- Name: next_operator_attention_change_sequence(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION next_operator_attention_change_sequence() RETURNS bigint
    LANGUAGE plpgsql
    AS $$
BEGIN
    -- Outbox-producing transactions already hold this row through commit.
    -- Direct attention facts take the same lock, so every publisher uses one
    -- lock order and attention cursors remain commit-monotonic.
    PERFORM 1
      FROM outbox_sequence_state
     WHERE singleton
     FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'attention sequence requires outbox sequence state'
            USING ERRCODE = '23503';
    END IF;
    RETURN nextval('operator_attention_change_sequence');
END;
$$;


--
-- Name: record_operator_attention_judge_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION record_operator_attention_judge_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    INSERT INTO operator_attention_change (session_id, fact_kind)
    VALUES (NEW.session_id, 'approval_judge');
    RETURN NULL;
END;
$$;


--
-- Name: record_operator_attention_metadata_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION record_operator_attention_metadata_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    INSERT INTO operator_attention_change (session_id, fact_kind)
    VALUES (COALESCE(NEW.session_id, OLD.session_id), 'session');
    RETURN NULL;
END;
$$;


--
-- Name: record_operator_attention_outbox_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION record_operator_attention_outbox_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    INSERT INTO operator_attention_change (session_id, fact_kind)
    VALUES (
        NEW.session_id,
        CASE NEW.event_kind
            WHEN 'session_created' THEN 'session'
            WHEN 'session_model_settings_changed' THEN 'session'
            WHEN 'goal_turn_retired' THEN 'goal'
            WHEN 'runner_state_transition' THEN 'runner'
            WHEN 'delegation_update' THEN 'turn'
            WHEN 'delegation_wake' THEN 'turn'
            WHEN 'turn_model_settings_resolved' THEN 'turn'
            WHEN 'input_accepted' THEN 'turn'
            WHEN 'turn_activated' THEN 'turn'
            WHEN 'turn_failed' THEN 'turn'
            WHEN 'model_call_transition' THEN 'turn'
            WHEN 'tool_batch_transition' THEN 'turn'
            WHEN 'tool_approval_decided' THEN 'turn'
            WHEN 'context_compacted' THEN 'turn'
            WHEN 'turn_completed' THEN 'turn'
            WHEN 'turn_refused' THEN 'turn'
            WHEN 'turn_cancelled' THEN 'turn'
            WHEN 'turn_reconciliation_required' THEN 'turn'
            ELSE NULL
        END
    );
    RETURN NULL;
END;
$$;


--
-- Name: record_operator_attention_runner_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION record_operator_attention_runner_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    INSERT INTO operator_attention_change (session_id, fact_kind)
    VALUES (NEW.session_id, 'runner');
    RETURN NULL;
END;
$$;


--
-- Name: update_operator_attention_judge_facts(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION update_operator_attention_judge_facts() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    old_actionable bigint := 0;
    old_completed bigint := 0;
    old_escalated bigint := 0;
    old_failed bigint := 0;
    new_actionable bigint;
    new_completed bigint;
    new_escalated bigint;
    new_failed bigint;
BEGIN
    IF TG_OP = 'UPDATE' THEN
        old_actionable := (OLD.state_kind <> 'terminal')::integer;
        old_completed := COALESCE((OLD.terminal_disposition_kind = 'completed'
            AND OLD.recommendation_kind <> 'escalate_to_human')::integer, 0);
        old_escalated := COALESCE((OLD.terminal_disposition_kind = 'completed'
            AND OLD.recommendation_kind = 'escalate_to_human')::integer, 0);
        old_failed := COALESCE((OLD.state_kind = 'terminal'
            AND OLD.terminal_disposition_kind <> 'completed')::integer, 0);
    END IF;
    new_actionable := (NEW.state_kind <> 'terminal')::integer;
    new_completed := COALESCE((NEW.terminal_disposition_kind = 'completed'
        AND NEW.recommendation_kind <> 'escalate_to_human')::integer, 0);
    new_escalated := COALESCE((NEW.terminal_disposition_kind = 'completed'
        AND NEW.recommendation_kind = 'escalate_to_human')::integer, 0);
    new_failed := COALESCE((NEW.state_kind = 'terminal'
        AND NEW.terminal_disposition_kind <> 'completed')::integer, 0);

    -- Seed the counter row before applying the deltas, then add them with an
    -- UPDATE. Carrying a delta through the INSERT of an upsert cannot work
    -- here: PostgreSQL validates CHECK constraints against the proposed insert
    -- tuple before the ON CONFLICT arbiter runs, so a transition that retires
    -- an actionable call (delta -1) is rejected by `actionable >= 0` even when
    -- the conflicting row makes the resulting sum non-negative. The seed tuple
    -- is all zeros and so always satisfies the constraints, and the UPDATE
    -- still checks the summed row -- which is the invariant worth holding.
    INSERT INTO operator_attention_judge_facts
        (session_id, actionable, completed, escalated, failed)
    VALUES (NEW.session_id, 0, 0, 0, 0)
    ON CONFLICT (session_id) DO NOTHING;

    UPDATE operator_attention_judge_facts
       SET actionable = actionable + (new_actionable - old_actionable),
           completed = completed + (new_completed - old_completed),
           escalated = escalated + (new_escalated - old_escalated),
           failed = failed + (new_failed - old_failed)
     WHERE session_id = NEW.session_id;
    RETURN NULL;
END;
$$;


--
-- Tables.
--

--
-- Name: operator_attention_change; Type: TABLE; Schema: public
--

CREATE TABLE operator_attention_change (
    change_sequence bigint DEFAULT next_operator_attention_change_sequence() NOT NULL,
    session_id uuid NOT NULL,
    fact_kind text NOT NULL,
    recorded_at timestamp with time zone DEFAULT clock_timestamp() NOT NULL,
    CONSTRAINT operator_attention_change_change_sequence_check CHECK ((change_sequence > 0)),
    CONSTRAINT operator_attention_change_fact_kind_check CHECK ((fact_kind = ANY (ARRAY['session'::text, 'turn'::text, 'goal'::text, 'approval_judge'::text, 'runner'::text])))
);


--
-- Name: operator_attention_change_sequence; Type: SEQUENCE; Schema: public
--

CREATE SEQUENCE operator_attention_change_sequence
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: operator_attention_judge_facts; Type: TABLE; Schema: public
--

CREATE TABLE operator_attention_judge_facts (
    session_id uuid NOT NULL,
    actionable bigint NOT NULL,
    completed bigint NOT NULL,
    escalated bigint NOT NULL,
    failed bigint NOT NULL,
    CONSTRAINT operator_attention_judge_facts_actionable_check CHECK ((actionable >= 0)),
    CONSTRAINT operator_attention_judge_facts_completed_check CHECK ((completed >= 0)),
    CONSTRAINT operator_attention_judge_facts_escalated_check CHECK ((escalated >= 0)),
    CONSTRAINT operator_attention_judge_facts_failed_check CHECK ((failed >= 0))
);


--
-- Constraints.
--

--
-- Name: operator_attention_change operator_attention_change_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY operator_attention_change
    ADD CONSTRAINT operator_attention_change_pkey PRIMARY KEY (change_sequence);


--
-- Name: operator_attention_judge_facts operator_attention_judge_facts_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY operator_attention_judge_facts
    ADD CONSTRAINT operator_attention_judge_facts_pkey PRIMARY KEY (session_id);


--
-- Indexes.
--

--
-- Name: operator_attention_change_by_session_sequence; Type: INDEX; Schema: public
--

CREATE INDEX operator_attention_change_by_session_sequence ON operator_attention_change USING btree (session_id, change_sequence DESC);


--
-- Triggers.
--

--
-- Name: delegation_outbox_event delegation_outbox_event_records_operator_attention_change; Type: TRIGGER; Schema: public
--

CREATE TRIGGER delegation_outbox_event_records_operator_attention_change AFTER INSERT ON delegation_outbox_event FOR EACH ROW EXECUTE FUNCTION record_operator_attention_outbox_change();


--
-- Name: operator_attention_change operator_attention_change_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER operator_attention_change_is_append_only BEFORE DELETE OR UPDATE ON operator_attention_change FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: operator_attention_change operator_attention_change_maintains_catalog_activity; Type: TRIGGER; Schema: public
--

CREATE TRIGGER operator_attention_change_maintains_catalog_activity AFTER INSERT ON operator_attention_change FOR EACH ROW EXECUTE FUNCTION maintain_session_catalog_last_activity();


--
-- Name: operator_attention_change operator_attention_change_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER operator_attention_change_rejects_truncate BEFORE TRUNCATE ON operator_attention_change FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: outbox_event outbox_event_records_operator_attention_change; Type: TRIGGER; Schema: public
--

CREATE TRIGGER outbox_event_records_operator_attention_change AFTER INSERT ON outbox_event FOR EACH ROW EXECUTE FUNCTION record_operator_attention_outbox_change();


--
-- Name: runner_session_placement_record runner_placement_records_operator_attention_change; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_placement_records_operator_attention_change AFTER INSERT ON runner_session_placement_record FOR EACH ROW EXECUTE FUNCTION record_operator_attention_runner_change();


--
-- Name: session_metadata session_metadata_records_operator_attention_change; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_metadata_records_operator_attention_change AFTER INSERT OR UPDATE ON session_metadata FOR EACH ROW EXECUTE FUNCTION record_operator_attention_metadata_change();


--
-- Name: session_metadata_tag session_metadata_tag_records_operator_attention_change; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_metadata_tag_records_operator_attention_change AFTER INSERT OR DELETE ON session_metadata_tag FOR EACH ROW EXECUTE FUNCTION record_operator_attention_metadata_change();


--
-- Name: tool_approval_judge_model_call tool_approval_judge_records_operator_attention_change; Type: TRIGGER; Schema: public
--

CREATE TRIGGER tool_approval_judge_records_operator_attention_change AFTER INSERT OR UPDATE ON tool_approval_judge_model_call FOR EACH ROW EXECUTE FUNCTION record_operator_attention_judge_change();


--
-- Name: tool_approval_judge_model_call tool_approval_judge_updates_operator_attention_facts; Type: TRIGGER; Schema: public
--

CREATE TRIGGER tool_approval_judge_updates_operator_attention_facts AFTER INSERT OR UPDATE ON tool_approval_judge_model_call FOR EACH ROW EXECUTE FUNCTION update_operator_attention_judge_facts();


--
-- Foreign keys.
--

--
-- Name: operator_attention_change operator_attention_change_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY operator_attention_change
    ADD CONSTRAINT operator_attention_change_session_id_fkey FOREIGN KEY (session_id) REFERENCES session(session_id) ON DELETE RESTRICT;


--
-- Name: operator_attention_judge_facts operator_attention_judge_facts_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY operator_attention_judge_facts
    ADD CONSTRAINT operator_attention_judge_facts_session_id_fkey FOREIGN KEY (session_id) REFERENCES session(session_id) ON DELETE RESTRICT;


--
-- Search-path pins for this file's constraint-reachable functions.
--
-- The pin has to name the schema the migration selected rather than a
-- literal, so it is applied here through current_schema instead of inline
-- in each CREATE FUNCTION (the full rationale is in 202609010000_core.sql;
-- crates/persistence/tests/search_path_postgres.rs is the guard).
--

DO $search_path_pins$
DECLARE
    signature text;
BEGIN
    -- the server default captured at creation time by SET search_path FROM CURRENT
    FOREACH signature IN ARRAY ARRAY[
        'record_operator_attention_metadata_change()'
    ] LOOP
        EXECUTE format('ALTER FUNCTION %s SET search_path TO "$user", %I',
                   signature, current_schema);
    END LOOP;
END
$search_path_pins$;


--
-- Restore body validation for anything applied after this file.
--

RESET check_function_bodies;
