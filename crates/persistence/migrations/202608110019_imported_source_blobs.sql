-- Pre-production reset: a database containing relational imported-source
-- bytes does not cross this schema boundary. Imported conversations can have
-- produced native sessions and commands, so remove the complete imported-rooted
-- graph while retaining unrelated rows in every shared table. The migration
-- role owns these tables; it neither needs nor assumes superuser privileges.
DO $$
DECLARE
    constraint_record record;
    table_record record;
BEGIN
    IF NOT EXISTS (SELECT 1 FROM imported_raw_source_record) THEN
        RETURN;
    END IF;

    CREATE TEMPORARY TABLE imported_reset_table (
        relation_oid oid PRIMARY KEY
    ) ON COMMIT DROP;
    CREATE TEMPORARY TABLE imported_reset_foreign_key (
        relation_oid oid NOT NULL,
        constraint_name name NOT NULL,
        definition text NOT NULL,
        cascade_definition text NOT NULL,
        PRIMARY KEY (relation_oid, constraint_name)
    ) ON COMMIT DROP;

    -- Follow only foreign keys whose referenced rows can be reached from an
    -- imported root. This includes all durable effects of imported sessions,
    -- without changing or deleting an unrelated root row.
    INSERT INTO imported_reset_table (relation_oid)
    WITH RECURSIVE reachable(relation_oid) AS (
        SELECT relation_oid
          FROM unnest(ARRAY[
              'session'::regclass::oid,
              'durable_command'::regclass::oid,
              'imported_conversation'::regclass::oid,
              'imported_raw_source_record'::regclass::oid
          ]) AS root(relation_oid)
        UNION
        SELECT foreign_key.conrelid
          FROM reachable
          JOIN pg_constraint AS foreign_key
            ON foreign_key.contype = 'f'
           AND foreign_key.confrelid = reachable.relation_oid
    )
    SELECT relation_oid FROM reachable;

    INSERT INTO imported_reset_foreign_key (
        relation_oid, constraint_name, definition, cascade_definition
    )
    SELECT foreign_key.conrelid,
           foreign_key.conname,
           pg_get_constraintdef(foreign_key.oid, true),
           CASE
               WHEN pg_get_constraintdef(foreign_key.oid, true) ~
                        ' ON DELETE (NO ACTION|RESTRICT|CASCADE|SET NULL|SET DEFAULT)'
               THEN regexp_replace(
                   pg_get_constraintdef(foreign_key.oid, true),
                   ' ON DELETE (NO ACTION|RESTRICT|CASCADE|SET NULL|SET DEFAULT)',
                   ' ON DELETE CASCADE'
               )
               WHEN pg_get_constraintdef(foreign_key.oid, true) LIKE '% DEFERRABLE%'
               THEN replace(
                   pg_get_constraintdef(foreign_key.oid, true),
                   ' DEFERRABLE',
                   ' ON DELETE CASCADE DEFERRABLE'
               )
               ELSE pg_get_constraintdef(foreign_key.oid, true) ||
                    ' ON DELETE CASCADE'
           END
      FROM pg_constraint AS foreign_key
      JOIN imported_reset_table AS reachable
        ON reachable.relation_oid = foreign_key.confrelid
     WHERE foreign_key.contype = 'f';

    FOR constraint_record IN
        SELECT relation_oid, constraint_name, cascade_definition
          FROM imported_reset_foreign_key
         ORDER BY relation_oid, constraint_name
    LOOP
        EXECUTE format(
            'ALTER TABLE %s DROP CONSTRAINT %I',
            constraint_record.relation_oid::regclass,
            constraint_record.constraint_name
        );
        EXECUTE format(
            'ALTER TABLE %s ADD CONSTRAINT %I %s',
            constraint_record.relation_oid::regclass,
            constraint_record.constraint_name,
            constraint_record.cascade_definition
        );
    END LOOP;

    FOR table_record IN
        SELECT relation_oid FROM imported_reset_table ORDER BY relation_oid
    LOOP
        EXECUTE format(
            'ALTER TABLE %s DISABLE TRIGGER USER',
            table_record.relation_oid::regclass
        );
    END LOOP;

    DELETE FROM session
     WHERE ancestry_kind = 'imported_conversation';
    DELETE FROM durable_command
     WHERE command_kind = 'create_session_from_imported_frontier';
    DELETE FROM imported_conversation;
    DELETE FROM imported_raw_source_record;

    FOR table_record IN
        SELECT relation_oid FROM imported_reset_table ORDER BY relation_oid
    LOOP
        EXECUTE format(
            'ALTER TABLE %s ENABLE TRIGGER USER',
            table_record.relation_oid::regclass
        );
    END LOOP;

    FOR constraint_record IN
        SELECT relation_oid, constraint_name, definition
          FROM imported_reset_foreign_key
         ORDER BY relation_oid, constraint_name
    LOOP
        EXECUTE format(
            'ALTER TABLE %s DROP CONSTRAINT %I',
            constraint_record.relation_oid::regclass,
            constraint_record.constraint_name
        );
        EXECUTE format(
            'ALTER TABLE %s ADD CONSTRAINT %I %s',
            constraint_record.relation_oid::regclass,
            constraint_record.constraint_name,
            constraint_record.definition
        );
    END LOOP;
END;
$$;

ALTER TABLE imported_raw_source_record
    DROP COLUMN raw_bytes;

ALTER TABLE imported_raw_source_record
    ADD CONSTRAINT imported_raw_source_record_blob_fk
    FOREIGN KEY (content_hash)
    REFERENCES blob (digest);

-- The reset also retires the old display-title transition state. Every row in
-- the final schema is inserted with its resolved title facts, and the header
-- is append-only immediately after insertion.
ALTER TABLE imported_conversation
    ALTER COLUMN display_title_state DROP DEFAULT,
    DROP CONSTRAINT imported_conversation_display_title_state_closed,
    ADD CONSTRAINT imported_conversation_display_title_state_closed CHECK (
        display_title_state IN ('derived', 'underivable')
    );

DROP TRIGGER imported_conversation_is_append_only ON imported_conversation;
DROP FUNCTION reject_non_display_title_backfill_change();

CREATE FUNCTION reject_imported_conversation_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'imported_conversation is append-only'
        USING ERRCODE = '23514';
END;
$$;

CREATE TRIGGER imported_conversation_is_append_only
BEFORE UPDATE OR DELETE ON imported_conversation
FOR EACH ROW
EXECUTE FUNCTION reject_imported_conversation_change();
