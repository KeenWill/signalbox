ALTER TABLE session_deadline
    ADD COLUMN settled boolean NOT NULL DEFAULT false,
    ADD CONSTRAINT session_deadline_settled_has_no_expiry
        CHECK (NOT settled OR expires_at IS NULL);

CREATE OR REPLACE FUNCTION arm_session_deadline() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    required text;
    armed text;
BEGIN
    required := session_deadline_kind_for_state(NEW.state_kind);

    IF NOT NEW.owned OR required IS NULL THEN
        DELETE FROM session_deadline WHERE session_id = NEW.session_id;
        RETURN NULL;
    END IF;

    IF required = 'admission' AND EXISTS (
        SELECT 1 FROM turn_lifecycle
         WHERE session_id = NEW.session_id AND start_lineage_kind IS NOT NULL
    ) THEN
        INSERT INTO session_deadline
            (session_id, deadline_kind, on_expiry_kind, settled)
        VALUES (NEW.session_id, 'admission', 'retire', true)
        ON CONFLICT (session_id) DO UPDATE
           SET deadline_kind = EXCLUDED.deadline_kind,
               on_expiry_kind = EXCLUDED.on_expiry_kind,
               expires_at = NULL,
               settled = true;
        RETURN NULL;
    END IF;

    SELECT deadline_kind INTO armed
      FROM session_deadline
     WHERE session_id = NEW.session_id;

    IF TG_OP = 'UPDATE'
       AND armed IS NOT DISTINCT FROM required
       AND OLD.owned = NEW.owned
       AND (
            OLD.state_entered_at = NEW.state_entered_at
            OR required = 'admission'
       )
    THEN
        RETURN NULL;
    END IF;

    INSERT INTO session_deadline
            (session_id, deadline_kind, on_expiry_kind, armed_at)
         VALUES (
            NEW.session_id,
            required,
            session_deadline_expiry_for_kind(required),
            statement_timestamp()
         )
    ON CONFLICT (session_id) DO UPDATE
       SET deadline_kind = EXCLUDED.deadline_kind,
           on_expiry_kind = EXCLUDED.on_expiry_kind,
           expires_at = NULL,
           settled = false,
           armed_at = EXCLUDED.armed_at;

    RETURN NULL;
END;
$$;
