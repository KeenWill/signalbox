--
-- Session lifecycle §5: the durable session-timeline projection.
--
-- Window, descriptor, and region reads walked both outbox header families
-- directly. `session_timeline_item` carries one row per appended header, so
-- those reads have one ordered relation to scan instead of a union.
--

CREATE TABLE session_timeline_item (
    event_sequence numeric(20,0) NOT NULL,
    session_id uuid,
    event_kind text NOT NULL,
    turn_disposition text,
    CONSTRAINT session_timeline_item_pkey PRIMARY KEY (event_sequence),
    CONSTRAINT session_timeline_item_sequence_positive_u64 CHECK (
        (event_sequence >= (1)::numeric)
        AND (event_sequence <= '18446744073709551615'::numeric)
    ),
    CONSTRAINT session_timeline_item_session_fk
        FOREIGN KEY (session_id) REFERENCES session (session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

CREATE INDEX session_timeline_item_by_session_sequence
    ON session_timeline_item USING btree (session_id, event_sequence);

CREATE TRIGGER session_timeline_item_cannot_be_truncated
    BEFORE TRUNCATE ON session_timeline_item
    FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();

CREATE TRIGGER session_timeline_item_is_append_only
    BEFORE DELETE OR UPDATE ON session_timeline_item
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

INSERT INTO session_timeline_item (
    event_sequence, session_id, event_kind, turn_disposition
)
SELECT event_sequence, session_id, event_kind, turn_disposition
  FROM outbox_event
 UNION ALL
SELECT event_sequence, session_id, event_kind, NULL::text
  FROM delegation_outbox_event;

--
-- The delegation header carries no disposition, so the column is read through
-- the row's JSON form rather than by name.
--

CREATE FUNCTION project_session_timeline_item() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    INSERT INTO session_timeline_item (
        event_sequence, session_id, event_kind, turn_disposition
    )
    VALUES (
        NEW.event_sequence, NEW.session_id, NEW.event_kind,
        to_jsonb(NEW) ->> 'turn_disposition'
    );
    RETURN NULL;
END;
$$;

CREATE TRIGGER outbox_event_projects_timeline_item
    AFTER INSERT ON outbox_event
    FOR EACH ROW EXECUTE FUNCTION project_session_timeline_item();

CREATE TRIGGER delegation_outbox_event_projects_timeline_item
    AFTER INSERT ON delegation_outbox_event
    FOR EACH ROW EXECUTE FUNCTION project_session_timeline_item();
