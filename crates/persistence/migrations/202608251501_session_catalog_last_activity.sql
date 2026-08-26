-- Bounded last-activity keysets for the session catalog. The attention journal
-- remains authoritative; this column is only its indexed latest timestamp.

ALTER TABLE session_timeline_fact
    ADD COLUMN attention_activity_recorded_at timestamptz;

UPDATE session_timeline_fact AS facts
   SET attention_activity_recorded_at = activity.recorded_at
  FROM (
      SELECT DISTINCT ON (change.session_id)
             change.session_id, change.recorded_at
        FROM operator_attention_change AS change
       ORDER BY change.session_id, change.change_sequence DESC
  ) AS activity
 WHERE activity.session_id = facts.session_id;

CREATE INDEX session_timeline_fact_by_attention_activity
    ON session_timeline_fact (attention_activity_recorded_at DESC, session_id);

CREATE FUNCTION maintain_session_catalog_last_activity()
RETURNS trigger
LANGUAGE plpgsql
SET search_path FROM CURRENT
AS $$
BEGIN
    UPDATE session_timeline_fact
       SET attention_activity_recorded_at = NEW.recorded_at
     WHERE session_id = NEW.session_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'attention activity requires a session timeline fact'
            USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;

CREATE TRIGGER operator_attention_change_maintains_catalog_activity
AFTER INSERT ON operator_attention_change
FOR EACH ROW EXECUTE FUNCTION maintain_session_catalog_last_activity();

-- Metadata changes alter catalog membership and presentation. Publish them
-- through the same durable cursor so fleet followers replace affected rows.
CREATE FUNCTION record_operator_attention_metadata_change()
RETURNS trigger
LANGUAGE plpgsql
SET search_path FROM CURRENT
AS $$
BEGIN
    INSERT INTO operator_attention_change (session_id, fact_kind)
    VALUES (COALESCE(NEW.session_id, OLD.session_id), 'session');
    RETURN NULL;
END;
$$;

CREATE TRIGGER session_metadata_records_operator_attention_change
AFTER INSERT OR UPDATE ON session_metadata
FOR EACH ROW EXECUTE FUNCTION record_operator_attention_metadata_change();

CREATE TRIGGER session_metadata_tag_records_operator_attention_change
AFTER INSERT OR DELETE ON session_metadata_tag
FOR EACH ROW EXECUTE FUNCTION record_operator_attention_metadata_change();
