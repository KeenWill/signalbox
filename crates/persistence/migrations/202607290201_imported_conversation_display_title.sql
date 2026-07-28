-- One bounded, evidence-derived display title per imported conversation for
-- the unified conversation listing. Insertion derives the title once from the
-- converted aggregate; rows inserted before this column exist are marked
-- 'pending' and resolved by the daemon's one-time startup backfill, a pure
-- re-derivation from the preserved raw records. The title is presentation
-- evidence only: it never participates in the source digest, the
-- imported-conversation identity, or the unique source-identity constraint.

ALTER TABLE imported_conversation
    ADD COLUMN display_title text,
    ADD COLUMN display_title_state text NOT NULL DEFAULT 'pending';

ALTER TABLE imported_conversation
    ALTER COLUMN display_title_state DROP DEFAULT;

ALTER TABLE imported_conversation
    ADD CONSTRAINT imported_conversation_display_title_state_closed CHECK (
        display_title_state IN ('pending', 'derived', 'underivable')
    ),
    ADD CONSTRAINT imported_conversation_display_title_presence CHECK (
        (display_title_state = 'derived') = (display_title IS NOT NULL)
    ),
    ADD CONSTRAINT imported_conversation_display_title_shape CHECK (
        display_title IS NULL
        OR (
            char_length(display_title) BETWEEN 1 AND 256
            AND position(chr(10) IN display_title) = 0
            AND position(chr(13) IN display_title) = 0
            AND btrim(display_title, E' \t') = display_title
        )
    );

-- The header stays append-only for every other column and for deletion. The
-- one admitted update is the guarded startup backfill resolving a 'pending'
-- row to its derived or underivable title while changing nothing else,
-- following the sanctioned fail-closed backfill precedent.
CREATE FUNCTION reject_non_display_title_backfill_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'UPDATE'
        AND OLD.display_title_state = 'pending'
        AND NEW.display_title_state IN ('derived', 'underivable')
        AND NEW.imported_conversation_id = OLD.imported_conversation_id
        AND NEW.storage_version = OLD.storage_version
        AND NEW.source_format = OLD.source_format
        AND NEW.converter_version = OLD.converter_version
        AND NEW.source_digest = OLD.source_digest
        AND NEW.source_session_id IS NOT DISTINCT FROM OLD.source_session_id
        AND NEW.declared_raw_record_count = OLD.declared_raw_record_count
        AND NEW.declared_entry_count = OLD.declared_entry_count
    THEN
        RETURN NEW;
    END IF;
    RAISE EXCEPTION
        '% admits only the pending display-title backfill update', TG_TABLE_NAME
        USING ERRCODE = '23514';
END;
$$;

DROP TRIGGER imported_conversation_is_append_only ON imported_conversation;

CREATE TRIGGER imported_conversation_is_append_only
BEFORE UPDATE OR DELETE ON imported_conversation
FOR EACH ROW
EXECUTE FUNCTION reject_non_display_title_backfill_change();
