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
    CREATE TEMPORARY TABLE imported_reset_command (
        command_id uuid PRIMARY KEY
    ) ON COMMIT DROP;
    CREATE TEMPORARY TABLE imported_reset_session (
        session_id uuid PRIMARY KEY
    ) ON COMMIT DROP;
    CREATE TEMPORARY TABLE imported_reset_review_attempt (
        attempt_id uuid PRIMARY KEY
    ) ON COMMIT DROP;
    CREATE TEMPORARY TABLE imported_reset_review_run (
        run_id uuid PRIMARY KEY
    ) ON COMMIT DROP;

    -- Capture every session in the imported-rooted delegation graph before
    -- temporary cascades remove the relationship rows used to discover it.
    INSERT INTO imported_reset_session (session_id)
    WITH RECURSIVE imported_rooted(session_id) AS (
        SELECT session_id
          FROM session
         WHERE ancestry_kind = 'imported_conversation'
        UNION
        SELECT delegation.child_session_id
          FROM imported_rooted AS parent
          JOIN session_delegation AS delegation
            ON delegation.parent_session_id = parent.session_id
    )
    SELECT session_id FROM imported_rooted;

    -- Reset complete review runs when an imported-rooted pass participates in
    -- them. Follow references in the opposite direction too: deleting a
    -- referenced finding would otherwise cascade an event out of an unrelated
    -- subject run and leave that run as a partial aggregate.
    INSERT INTO imported_reset_review_run (run_id)
    WITH RECURSIVE affected_run(run_id) AS (
        SELECT pass.run_id
          FROM review_pass AS pass
         WHERE pass.session_id IN (
                   SELECT session_id FROM imported_reset_session
               )
        UNION
        SELECT adjacent.run_id
          FROM affected_run AS affected
          JOIN LATERAL (
                   SELECT event.finding_run_id AS run_id
                     FROM review_finding AS referenced
                     JOIN review_finding_event AS event
                       ON event.referenced_finding_id = referenced.finding_id
                    WHERE referenced.run_id = affected.run_id
                   UNION
                   SELECT event.event_pass_run_id AS run_id
                     FROM review_finding AS finding
                     JOIN review_finding_event AS event
                       ON event.finding_id = finding.finding_id
                    WHERE finding.run_id = affected.run_id
               ) AS adjacent ON true
    )
    SELECT run_id FROM affected_run;

    -- Follow only foreign keys whose referenced rows can be reached from an
    -- imported root. This includes all durable effects of imported sessions,
    -- without changing or deleting an unrelated root row.
    INSERT INTO imported_reset_table (relation_oid)
    WITH RECURSIVE reachable(relation_oid) AS (
        SELECT relation_oid
          FROM unnest(ARRAY[
              'session'::regclass::oid,
              'durable_command'::regclass::oid,
              'review_run'::regclass::oid,
              'review_orchestration_attempt'::regclass::oid,
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

    -- Preserve registry identities before deleting imported sessions. Later
    -- typed commands rooted in those sessions disappear through the temporary
    -- cascades, so their user-global claims must disappear in the same reset.
    FOR table_record IN
        SELECT reset.relation_oid
          FROM imported_reset_table AS reset
         WHERE EXISTS (
                   SELECT 1
                     FROM pg_attribute
                    WHERE attrelid = reset.relation_oid
                      AND attname = 'command_id'
                      AND atttypid = 'uuid'::regtype
                      AND attnum > 0
                      AND NOT attisdropped
               )
           AND EXISTS (
                   SELECT 1
                     FROM pg_attribute
                    WHERE attrelid = reset.relation_oid
                      AND attname = 'session_id'
                      AND atttypid = 'uuid'::regtype
                      AND attnum > 0
                      AND NOT attisdropped
               )
         ORDER BY reset.relation_oid
    LOOP
        EXECUTE format(
            'INSERT INTO imported_reset_command (command_id)
             SELECT command_id FROM %s
              WHERE session_id IN (SELECT session_id FROM imported_reset_session)
             ON CONFLICT DO NOTHING',
            table_record.relation_oid::regclass
        );
    END LOOP;

    -- Applied tool-decision commands are session-owned through their request
    -- and approval rather than a literal session_id on the typed command row.
    -- Capture their registry identities before the request graph cascades.
    INSERT INTO imported_reset_command (command_id)
    SELECT approval.user_command_id
      FROM tool_approval_decision AS approval
      JOIN tool_request AS request
        ON request.request_id = approval.request_id
     WHERE approval.user_command_id IS NOT NULL
       AND request.session_id IN (
               SELECT session_id FROM imported_reset_session
           )
    ON CONFLICT DO NOTHING;

    -- Review-workflow commands are owned indirectly through their durable
    -- result. Capture every pass and finding receipt in the complete affected
    -- run closure before deleting that review graph.
    INSERT INTO imported_reset_command (command_id)
    SELECT command.command_id
      FROM review_workflow_command AS command
      JOIN review_pass AS pass
        ON pass.pass_id = command.result_pass_id
     WHERE pass.run_id IN (
               SELECT run_id FROM imported_reset_review_run
           )
    ON CONFLICT DO NOTHING;

    -- Finding-event receipts name only their finding.
    INSERT INTO imported_reset_command (command_id)
    SELECT command.command_id
      FROM review_workflow_command AS command
      JOIN review_finding AS finding
        ON finding.finding_id = command.result_finding_id
     WHERE finding.run_id IN (
               SELECT run_id FROM imported_reset_review_run
           )
    ON CONFLICT DO NOTHING;

    -- External-link receipts name only their link. Run- and finding-associated
    -- links disappear with an affected review run, so retire their typed
    -- receipts and registry claims with them.
    INSERT INTO imported_reset_command (command_id)
    SELECT command.command_id
      FROM review_workflow_command AS command
      JOIN review_external_link AS link
        ON link.external_link_id = command.result_external_link_id
     WHERE link.run_id IN (
               SELECT run_id FROM imported_reset_review_run
           )
    ON CONFLICT DO NOTHING;

    -- An orchestration attempt is an aggregate root above its pass- and
    -- finding-owned stage rows. Capture every attempt touched by the imported
    -- review graph, then retire the complete aggregate and its command claims.
    INSERT INTO imported_reset_review_attempt (attempt_id)
    WITH affected_pass AS (
        SELECT pass_id
          FROM review_pass
         WHERE run_id IN (
                   SELECT run_id FROM imported_reset_review_run
               )
    ),
    affected_finding AS (
        SELECT finding_id
          FROM review_finding
         WHERE producing_pass_id IN (SELECT pass_id FROM affected_pass)
    ),
    affected_external_link AS (
        SELECT external_link_id
          FROM review_external_link
         WHERE run_id IN (
                   SELECT run_id FROM imported_reset_review_run
               )
    )
    SELECT attempt_id FROM review_orchestration_import
     WHERE pass_id IN (SELECT pass_id FROM affected_pass)
        OR external_link_id IN (
               SELECT external_link_id FROM affected_external_link
           )
    UNION
    SELECT attempt_id FROM review_orchestration_concern_claim
     WHERE pass_id IN (SELECT pass_id FROM affected_pass)
    UNION
    SELECT attempt_id FROM review_orchestration_concern_finding
     WHERE finding_id IN (SELECT finding_id FROM affected_finding)
    UNION
    SELECT attempt_id FROM review_orchestration_judgment_plan
     WHERE analysis_pass_id IN (SELECT pass_id FROM affected_pass)
    UNION
    SELECT attempt_id FROM review_orchestration_judgment_member
     WHERE finding_pass_id IN (SELECT pass_id FROM affected_pass)
        OR referenced_pass_id IN (SELECT pass_id FROM affected_pass)
        OR finding_id IN (SELECT finding_id FROM affected_finding)
        OR referenced_finding_id IN (SELECT finding_id FROM affected_finding)
    UNION
    SELECT attempt_id FROM review_orchestration_repair_inventory
     WHERE finding_pass_id IN (SELECT pass_id FROM affected_pass)
        OR finding_id IN (SELECT finding_id FROM affected_finding)
    UNION
    SELECT attempt_id FROM review_orchestration_repair_outcome
     WHERE finding_id IN (SELECT finding_id FROM affected_finding)
    UNION
    SELECT attempt_id FROM review_orchestration_publication_inventory
     WHERE finding_pass_id IN (SELECT pass_id FROM affected_pass)
        OR finding_id IN (SELECT finding_id FROM affected_finding)
    UNION
    SELECT attempt_id FROM review_orchestration_publication_outcome
     WHERE finding_id IN (SELECT finding_id FROM affected_finding)
        OR external_link_id IN (
               SELECT external_link_id FROM affected_external_link
           );

    INSERT INTO imported_reset_command (command_id)
    SELECT command_id
      FROM review_orchestration_command
     WHERE attempt_id IN (
               SELECT attempt_id FROM imported_reset_review_attempt
           )
    ON CONFLICT DO NOTHING;

    -- Pending orchestration commands have an intent but no replacement
    -- receipt yet. Retire those durable claims with the affected attempt too.
    INSERT INTO imported_reset_command (command_id)
    SELECT command_id
      FROM review_orchestration_command_intent
     WHERE attempt_id IN (
               SELECT attempt_id FROM imported_reset_review_attempt
           )
    ON CONFLICT DO NOTHING;

    DELETE FROM review_orchestration_attempt
     WHERE attempt_id IN (
               SELECT attempt_id FROM imported_reset_review_attempt
           );
    DELETE FROM review_run
     WHERE run_id IN (SELECT run_id FROM imported_reset_review_run);

    DELETE FROM session
     WHERE session_id IN (SELECT session_id FROM imported_reset_session);
    DELETE FROM durable_command
     WHERE command_kind = 'create_session_from_imported_frontier'
        OR command_id IN (SELECT command_id FROM imported_reset_command);
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
