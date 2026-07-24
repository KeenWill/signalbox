-- Durable creation of a native session from one exact imported frontier.
--
-- Imported conversations remain immutable ingestion records. This migration
-- adds only the creation-time projection that names an imported boundary,
-- materializes its exact semantic prefix, and links the resulting local
-- context frontier to the created session.

ALTER TABLE imported_transcript_entry
    ADD CONSTRAINT imported_transcript_entry_owner_identity_key
        UNIQUE (
            imported_conversation_id,
            imported_transcript_entry_id
        ),
    ADD CONSTRAINT imported_transcript_entry_frontier_key
        UNIQUE (
            imported_conversation_id,
            imported_transcript_entry_id,
            imported_entry_position
        );

ALTER TABLE session
    ADD COLUMN imported_conversation_id uuid,
    ADD COLUMN imported_frontier_entry_id uuid,
    ADD COLUMN imported_frontier_position numeric(20, 0),
    ADD COLUMN imported_relationship_kind text,
    DROP CONSTRAINT session_ancestry_kind_closed;

ALTER TABLE session
    ADD CONSTRAINT session_ancestry_kind_closed
        CHECK (
            ancestry_kind IN (
                'none',
                'imported_conversation'
            )
        ),
    ADD CONSTRAINT session_imported_frontier_position_positive_u64
        CHECK (
            imported_frontier_position IS NULL
            OR (
                imported_frontier_position >= 1
                AND imported_frontier_position <= 18446744073709551615
            )
        ),
    ADD CONSTRAINT session_imported_relationship_closed
        CHECK (
            imported_relationship_kind IS NULL
            OR imported_relationship_kind IN ('resume', 'fork')
        ),
    ADD CONSTRAINT session_ancestry_shape
        CHECK (
            (
                ancestry_kind = 'none'
                AND imported_conversation_id IS NULL
                AND imported_frontier_entry_id IS NULL
                AND imported_frontier_position IS NULL
                AND imported_relationship_kind IS NULL
            )
            OR
            (
                ancestry_kind = 'imported_conversation'
                AND imported_conversation_id IS NOT NULL
                AND imported_frontier_entry_id IS NOT NULL
                AND imported_frontier_position IS NOT NULL
                AND imported_relationship_kind IS NOT NULL
            )
        ),
    ADD CONSTRAINT session_imported_frontier_fk
        FOREIGN KEY (
            imported_conversation_id,
            imported_frontier_entry_id,
            imported_frontier_position
        )
        REFERENCES imported_transcript_entry (
            imported_conversation_id,
            imported_transcript_entry_id,
            imported_entry_position
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    ADD CONSTRAINT session_imported_provenance_key
        UNIQUE (
            session_id,
            creation_cause,
            ancestry_kind,
            imported_conversation_id,
            imported_frontier_entry_id,
            imported_frontier_position,
            imported_relationship_kind
        );

ALTER TABLE durable_command
    DROP CONSTRAINT durable_command_kind_closed;

ALTER TABLE durable_command
    ADD CONSTRAINT durable_command_kind_closed
        CHECK (
            command_kind IN (
                'create_session',
                'create_session_from_imported_frontier',
                'replace_session_defaults',
                'submit_input'
            )
        );

CREATE TABLE create_session_from_imported_frontier_command (
    command_id uuid PRIMARY KEY,
    command_kind text NOT NULL,
    storage_version smallint NOT NULL,
    imported_conversation_id uuid NOT NULL,
    imported_frontier_entry_id uuid NOT NULL,
    imported_frontier_position numeric(20, 0) NOT NULL,
    imported_relationship_kind text NOT NULL,
    creation_cause text NOT NULL,
    ancestry_kind text NOT NULL,
    initial_defaults_version numeric(20, 0) NOT NULL,
    model_selection_kind text NOT NULL,
    direct_model_selection_id uuid,
    model_alias_id uuid,
    model_selection_reference uuid GENERATED ALWAYS AS (
        COALESCE(direct_model_selection_id, model_alias_id)
    ) STORED,
    result_kind text NOT NULL,
    created_session_id uuid NOT NULL UNIQUE,

    CONSTRAINT create_session_from_imported_frontier_command_kind_closed
        CHECK (command_kind = 'create_session_from_imported_frontier'),
    CONSTRAINT create_session_from_imported_frontier_command_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT imported_frontier_command_relationship_closed
        CHECK (imported_relationship_kind IN ('resume', 'fork')),
    CONSTRAINT create_session_from_imported_frontier_command_cause_closed
        CHECK (creation_cause = 'owner_initiated'),
    CONSTRAINT create_session_from_imported_frontier_command_ancestry_closed
        CHECK (ancestry_kind = 'imported_conversation'),
    CONSTRAINT imported_frontier_command_position_positive_u64
        CHECK (
            imported_frontier_position >= 1
            AND imported_frontier_position <= 18446744073709551615
        ),
    CONSTRAINT create_session_from_imported_frontier_command_initial_defaults
        CHECK (initial_defaults_version = 1),
    CONSTRAINT create_session_from_imported_frontier_command_model_kind_closed
        CHECK (model_selection_kind IN ('direct', 'alias')),
    CONSTRAINT create_session_from_imported_frontier_command_model_shape
        CHECK (
            (
                model_selection_kind = 'direct'
                AND direct_model_selection_id IS NOT NULL
                AND model_alias_id IS NULL
            )
            OR
            (
                model_selection_kind = 'alias'
                AND direct_model_selection_id IS NULL
                AND model_alias_id IS NOT NULL
            )
        ),
    CONSTRAINT create_session_from_imported_frontier_command_result_closed
        CHECK (result_kind = 'applied'),
    CONSTRAINT create_session_from_imported_frontier_command_registry_fk
        FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command (
            command_id,
            command_kind,
            storage_version
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT create_session_from_imported_frontier_command_frontier_fk
        FOREIGN KEY (
            imported_conversation_id,
            imported_frontier_entry_id,
            imported_frontier_position
        )
        REFERENCES imported_transcript_entry (
            imported_conversation_id,
            imported_transcript_entry_id,
            imported_entry_position
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT create_session_from_imported_frontier_command_provenance_fk
        FOREIGN KEY (
            created_session_id,
            creation_cause,
            ancestry_kind,
            imported_conversation_id,
            imported_frontier_entry_id,
            imported_frontier_position,
            imported_relationship_kind
        )
        REFERENCES session (
            session_id,
            creation_cause,
            ancestry_kind,
            imported_conversation_id,
            imported_frontier_entry_id,
            imported_frontier_position,
            imported_relationship_kind
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT create_session_from_imported_frontier_command_defaults_fk
        FOREIGN KEY (
            created_session_id,
            initial_defaults_version,
            model_selection_kind,
            model_selection_reference
        )
        REFERENCES session_defaults_version (
            session_id,
            version,
            model_selection_kind,
            model_selection_reference
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER create_session_from_imported_frontier_command_is_append_only
BEFORE UPDATE OR DELETE ON create_session_from_imported_frontier_command
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER imported_frontier_command_cannot_be_truncated
BEFORE TRUNCATE ON create_session_from_imported_frontier_command
FOR EACH STATEMENT
EXECUTE FUNCTION reject_imported_table_truncate();

-- The original reverse foreign key named only the baseline CreateSession
-- table. A deferred family-aware check retains the one-command-per-session
-- law without coupling imported creation to that native-only table.
ALTER TABLE session
    DROP CONSTRAINT session_create_command_fk;

CREATE FUNCTION require_session_creation_command()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    native_count bigint;
    imported_count bigint;
BEGIN
    SELECT count(*)
      INTO native_count
      FROM create_session_command
     WHERE created_session_id = NEW.session_id;

    SELECT count(*)
      INTO imported_count
      FROM create_session_from_imported_frontier_command
     WHERE created_session_id = NEW.session_id;

    IF (
        NEW.ancestry_kind = 'none'
        AND (
            native_count <> 1
            OR imported_count <> 0
        )
    )
    OR (
        NEW.ancestry_kind = 'imported_conversation'
        AND (
            native_count <> 0
            OR imported_count <> 1
        )
    )
    THEN
        RAISE EXCEPTION
            'session % requires exactly one matching creation-command family',
            NEW.session_id
            USING
                ERRCODE = '23503',
                CONSTRAINT = 'session_requires_creation_command';
    END IF;

    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER session_requires_creation_command
AFTER INSERT ON session
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_session_creation_command();

-- Dropping the native reverse FK must not make its target truncatable.
CREATE TRIGGER create_session_command_cannot_be_truncated
BEFORE TRUNCATE ON create_session_command
FOR EACH STATEMENT
EXECUTE FUNCTION reject_imported_table_truncate();

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM session AS stored_session
          LEFT JOIN create_session_command AS native_command
            ON native_command.created_session_id = stored_session.session_id
         WHERE stored_session.ancestry_kind = 'none'
         GROUP BY stored_session.session_id
        HAVING count(native_command.command_id) <> 1
    ) THEN
        RAISE EXCEPTION 'preexisting session lacks its native creation command'
            USING ERRCODE = '23503';
    END IF;
END;
$$;

CREATE OR REPLACE FUNCTION require_durable_command_typed_record()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    matching_records bigint;
BEGIN
    CASE NEW.command_kind
        WHEN 'create_session' THEN
            SELECT count(*)
              INTO matching_records
              FROM create_session_command
             WHERE command_id = NEW.command_id;
        WHEN 'create_session_from_imported_frontier' THEN
            SELECT count(*)
              INTO matching_records
              FROM create_session_from_imported_frontier_command
             WHERE command_id = NEW.command_id;
        WHEN 'replace_session_defaults' THEN
            SELECT count(*)
              INTO matching_records
              FROM replace_session_defaults_command
             WHERE command_id = NEW.command_id;
        WHEN 'submit_input' THEN
            SELECT count(*)
              INTO matching_records
              FROM submit_input_command
             WHERE command_id = NEW.command_id;
        ELSE
            RAISE EXCEPTION 'unsupported durable command kind %', NEW.command_kind
                USING ERRCODE = '23514';
    END CASE;

    IF matching_records <> 1 THEN
        RAISE EXCEPTION
            'durable command % requires exactly one % typed record',
            NEW.command_id,
            NEW.command_kind
            USING ERRCODE = '23503';
    END IF;

    RETURN NULL;
END;
$$;

ALTER TABLE semantic_transcript_entry
    ADD COLUMN imported_conversation_id uuid,
    ADD COLUMN imported_transcript_entry_id uuid,
    DROP CONSTRAINT semantic_transcript_entry_payload_kind_closed,
    DROP CONSTRAINT semantic_transcript_entry_payload_shape;

ALTER TABLE semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_payload_kind_closed
        CHECK (
            payload_kind IN (
                'imported_entry',
                'origin_accepted_input',
                'steering_accepted_input',
                'turn_failed',
                'assistant_text',
                'assistant_tool_use',
                'turn_completed',
                'turn_cancelled'
            )
        ),
    ADD CONSTRAINT semantic_transcript_entry_payload_shape
        CHECK (
            payload_kind = 'imported_entry'
            OR (
                payload_kind IN (
                    'origin_accepted_input',
                    'steering_accepted_input'
                )
                AND origin_accepted_input_id IS NOT NULL
                AND (
                    (
                        payload_kind = 'origin_accepted_input'
                        AND steering_source_turn_id IS NULL
                    )
                    OR (
                        payload_kind = 'steering_accepted_input'
                        AND steering_source_turn_id IS NOT NULL
                    )
                )
                AND failed_turn_id IS NULL
                AND assistant_text_value IS NULL
                AND producing_model_call_id IS NULL
                AND assistant_tool_request_id IS NULL
                AND completed_turn_id IS NULL
                AND cancelled_turn_id IS NULL
            )
            OR
            (
                payload_kind = 'turn_failed'
                AND origin_accepted_input_id IS NULL
                AND steering_source_turn_id IS NULL
                AND failed_turn_id IS NOT NULL
                AND assistant_text_value IS NULL
                AND producing_model_call_id IS NULL
                AND assistant_tool_request_id IS NULL
                AND completed_turn_id IS NULL
                AND cancelled_turn_id IS NULL
            )
            OR
            (
                payload_kind = 'assistant_text'
                AND origin_accepted_input_id IS NULL
                AND steering_source_turn_id IS NULL
                AND failed_turn_id IS NULL
                AND assistant_text_value IS NOT NULL
                AND producing_model_call_id IS NOT NULL
                AND assistant_tool_request_id IS NULL
                AND completed_turn_id IS NULL
                AND cancelled_turn_id IS NULL
                AND assistant_text_value <> ''
            )
            OR
            (
                payload_kind = 'assistant_tool_use'
                AND origin_accepted_input_id IS NULL
                AND steering_source_turn_id IS NULL
                AND failed_turn_id IS NULL
                AND assistant_text_value IS NULL
                AND producing_model_call_id IS NOT NULL
                AND assistant_tool_request_id IS NOT NULL
                AND completed_turn_id IS NULL
                AND cancelled_turn_id IS NULL
            )
            OR
            (
                payload_kind = 'turn_completed'
                AND origin_accepted_input_id IS NULL
                AND steering_source_turn_id IS NULL
                AND failed_turn_id IS NULL
                AND assistant_text_value IS NULL
                AND producing_model_call_id IS NULL
                AND assistant_tool_request_id IS NULL
                AND completed_turn_id IS NOT NULL
                AND cancelled_turn_id IS NULL
            )
            OR
            (
                payload_kind = 'turn_cancelled'
                AND origin_accepted_input_id IS NULL
                AND steering_source_turn_id IS NULL
                AND failed_turn_id IS NULL
                AND assistant_text_value IS NULL
                AND producing_model_call_id IS NULL
                AND assistant_tool_request_id IS NULL
                AND completed_turn_id IS NULL
                AND cancelled_turn_id IS NOT NULL
            )
        ),
    ADD CONSTRAINT semantic_transcript_entry_imported_shape
        CHECK (
            (
                payload_kind = 'imported_entry'
                AND imported_conversation_id IS NOT NULL
                AND imported_transcript_entry_id IS NOT NULL
                AND origin_accepted_input_id IS NULL
                AND steering_source_turn_id IS NULL
                AND failed_turn_id IS NULL
                AND assistant_text_value IS NULL
                AND producing_model_call_id IS NULL
                AND assistant_tool_request_id IS NULL
                AND completed_turn_id IS NULL
                AND cancelled_turn_id IS NULL
            )
            OR
            (
                payload_kind <> 'imported_entry'
                AND imported_conversation_id IS NULL
                AND imported_transcript_entry_id IS NULL
            )
        ),
    ADD CONSTRAINT semantic_transcript_entry_imported_entry_once_per_session
        UNIQUE (
            source_session_id,
            imported_conversation_id,
            imported_transcript_entry_id
        )
        DEFERRABLE INITIALLY DEFERRED,
    ADD CONSTRAINT semantic_transcript_entry_imported_entry_fk
        FOREIGN KEY (
            imported_conversation_id,
            imported_transcript_entry_id
        )
        REFERENCES imported_transcript_entry (
            imported_conversation_id,
            imported_transcript_entry_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

-- Imported entries deliberately bypass native turn-state matching. Every
-- preexisting native branch remains unchanged below this early return.
CREATE OR REPLACE FUNCTION require_semantic_entry_turn_state()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    checked_payload_kind text;
    checked_source_session_id uuid;
    checked_origin_input_id uuid;
    checked_steering_source_turn_id uuid;
    checked_failed_turn_id uuid;
    checked_producing_call_id uuid;
    checked_completed_turn_id uuid;
    checked_cancelled_turn_id uuid;
    checked_turn_id uuid;
BEGIN
    IF TG_OP = 'DELETE' THEN
        checked_payload_kind := OLD.payload_kind;
        checked_source_session_id := OLD.source_session_id;
        checked_origin_input_id := OLD.origin_accepted_input_id;
        checked_steering_source_turn_id := OLD.steering_source_turn_id;
        checked_failed_turn_id := OLD.failed_turn_id;
        checked_producing_call_id := OLD.producing_model_call_id;
        checked_completed_turn_id := OLD.completed_turn_id;
        checked_cancelled_turn_id := OLD.cancelled_turn_id;
    ELSE
        checked_payload_kind := NEW.payload_kind;
        checked_source_session_id := NEW.source_session_id;
        checked_origin_input_id := NEW.origin_accepted_input_id;
        checked_steering_source_turn_id := NEW.steering_source_turn_id;
        checked_failed_turn_id := NEW.failed_turn_id;
        checked_producing_call_id := NEW.producing_model_call_id;
        checked_completed_turn_id := NEW.completed_turn_id;
        checked_cancelled_turn_id := NEW.cancelled_turn_id;
    END IF;

    IF checked_payload_kind = 'imported_entry' THEN
        RETURN NULL;
    END IF;

    CASE checked_payload_kind
        WHEN 'origin_accepted_input' THEN
            SELECT origin_turn_id
              INTO checked_turn_id
              FROM accepted_input
             WHERE accepted_input_id = checked_origin_input_id
               AND session_id = checked_source_session_id
               AND disposition_kind IN (
                    'origin_of',
                    'reclassified_as_turn_origin'
               )
               AND origin_turn_id IS NOT NULL;

            IF NOT FOUND THEN
                RAISE EXCEPTION 'semantic origin input % is not a turn origin',
                    checked_origin_input_id
                    USING
                        ERRCODE = '23514',
                        CONSTRAINT =
                            'semantic_transcript_entry_origin_disposition';
            END IF;
        WHEN 'steering_accepted_input' THEN
            SELECT expected_active_turn_id, consuming_model_call_id
              INTO checked_turn_id, checked_producing_call_id
              FROM accepted_input
             WHERE accepted_input_id = checked_origin_input_id
               AND session_id = checked_source_session_id
               AND disposition_kind = 'consumed_as_steering'
               AND expected_active_turn_id = checked_steering_source_turn_id
               AND consuming_model_call_id IS NOT NULL;

            IF NOT FOUND THEN
                RAISE EXCEPTION
                    'semantic steering input is not consumed by its source turn call'
                    USING ERRCODE = '23514';
            END IF;
        WHEN 'turn_failed' THEN
            checked_turn_id := checked_failed_turn_id;
        WHEN 'turn_completed' THEN
            checked_turn_id := checked_completed_turn_id;
        WHEN 'turn_cancelled' THEN
            checked_turn_id := checked_cancelled_turn_id;
        WHEN 'assistant_text' THEN
            SELECT turn_id
              INTO checked_turn_id
              FROM model_call
             WHERE model_call_id = checked_producing_call_id
               AND state_kind = 'terminal'
               AND terminal_disposition_kind = 'completed';

            IF NOT FOUND THEN
                RAISE EXCEPTION
                    'assistant text requires its outcome-authoritative completed call'
                    USING ERRCODE = '23514';
            END IF;
        ELSE
            RAISE EXCEPTION
                'semantic payload kind % lacks construction authority',
                checked_payload_kind
                USING ERRCODE = '23514';
    END CASE;

    PERFORM assert_turn_lifecycle_final_state(checked_turn_id);
    IF checked_producing_call_id IS NOT NULL THEN
        PERFORM assert_model_call_final_state(checked_producing_call_id);
    END IF;
    RETURN NULL;
END;
$$;

CREATE TABLE imported_session_seed (
    session_id uuid PRIMARY KEY,
    seed_context_frontier_id uuid NOT NULL UNIQUE,
    creation_transaction_id xid8 NOT NULL,

    CONSTRAINT imported_session_seed_frontier_fk
        FOREIGN KEY (session_id, seed_context_frontier_id)
        REFERENCES context_frontier (
            owning_session_id,
            context_frontier_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE FUNCTION stamp_imported_session_seed_transaction()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    -- pg_current_xact_id() deliberately returns the top-level transaction ID
    -- even when this INSERT runs inside a savepoint. Persisting it avoids
    -- confusing the tuple's subtransaction xmin with a previously committed
    -- seed while the rest of the prefix is assembled.
    NEW.creation_transaction_id := pg_current_xact_id();
    RETURN NEW;
END;
$$;

CREATE TRIGGER imported_session_seed_records_creation_transaction
BEFORE INSERT ON imported_session_seed
FOR EACH ROW
EXECUTE FUNCTION stamp_imported_session_seed_transaction();

CREATE TRIGGER imported_session_seed_is_append_only
BEFORE UPDATE OR DELETE ON imported_session_seed
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER imported_session_seed_cannot_be_truncated
BEFORE TRUNCATE ON imported_session_seed
FOR EACH STATEMENT
EXECUTE FUNCTION reject_imported_table_truncate();

CREATE FUNCTION assert_imported_session_seed_complete(
    checked_session_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    checked_ancestry_kind text;
    checked_imported_conversation_id uuid;
    checked_frontier_entry_id uuid;
    checked_frontier_position numeric(20, 0);
    seed_frontier_id uuid;
    seed_present boolean;
    seed_member_count numeric(20, 0);
    imported_semantic_count numeric(20, 0);
BEGIN
    SELECT ancestry_kind,
           imported_conversation_id,
           imported_frontier_entry_id,
           imported_frontier_position
      INTO checked_ancestry_kind,
           checked_imported_conversation_id,
           checked_frontier_entry_id,
           checked_frontier_position
      FROM session
     WHERE session_id = checked_session_id;

    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT seed_context_frontier_id
      INTO seed_frontier_id
      FROM imported_session_seed
     WHERE session_id = checked_session_id;
    seed_present := FOUND;

    SELECT count(*)::numeric(20, 0)
      INTO imported_semantic_count
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session_id
       AND payload_kind = 'imported_entry';

    IF checked_ancestry_kind <> 'imported_conversation' THEN
        IF seed_present OR imported_semantic_count <> 0 THEN
            RAISE EXCEPTION
                'non-imported session % cannot own an imported seed',
                checked_session_id
                USING
                    ERRCODE = '23514',
                    CONSTRAINT = 'imported_session_seed_requires_imported_ancestry';
        END IF;
        RETURN;
    END IF;

    IF NOT seed_present THEN
        RAISE EXCEPTION
            'imported session % requires exactly one seed frontier',
            checked_session_id
            USING
                ERRCODE = '23503',
                CONSTRAINT = 'imported_session_requires_seed';
    END IF;

    SELECT member_count
      INTO seed_member_count
      FROM context_frontier
     WHERE owning_session_id = checked_session_id
       AND context_frontier_id = seed_frontier_id;

    IF NOT FOUND OR seed_member_count <> checked_frontier_position THEN
        RAISE EXCEPTION
            'imported session % seed has the wrong prefix length',
            checked_session_id
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'imported_session_seed_exact_prefix';
    END IF;

    PERFORM assert_context_frontier_complete_membership(
        checked_session_id,
        seed_frontier_id
    );

    IF imported_semantic_count <> checked_frontier_position
       OR EXISTS (
           SELECT 1
             FROM context_frontier_member AS member
             LEFT JOIN semantic_transcript_entry AS semantic_entry
               ON semantic_entry.source_session_id = member.source_session_id
              AND semantic_entry.semantic_entry_id = member.semantic_entry_id
             LEFT JOIN imported_transcript_entry AS imported_entry
               ON imported_entry.imported_conversation_id =
                      semantic_entry.imported_conversation_id
              AND imported_entry.imported_transcript_entry_id =
                      semantic_entry.imported_transcript_entry_id
            WHERE member.owning_session_id = checked_session_id
              AND member.context_frontier_id = seed_frontier_id
              AND (
                  member.source_session_id IS DISTINCT FROM checked_session_id
                  OR semantic_entry.payload_kind IS DISTINCT FROM 'imported_entry'
                  OR semantic_entry.imported_conversation_id
                        IS DISTINCT FROM checked_imported_conversation_id
                  OR imported_entry.imported_entry_position
                        IS DISTINCT FROM member.member_position
              )
       )
       OR EXISTS (
           SELECT 1
             FROM semantic_transcript_entry AS semantic_entry
             LEFT JOIN context_frontier_member AS member
               ON member.owning_session_id = checked_session_id
              AND member.context_frontier_id = seed_frontier_id
              AND member.source_session_id = semantic_entry.source_session_id
              AND member.semantic_entry_id = semantic_entry.semantic_entry_id
             JOIN imported_transcript_entry AS imported_entry
               ON imported_entry.imported_conversation_id =
                      semantic_entry.imported_conversation_id
              AND imported_entry.imported_transcript_entry_id =
                      semantic_entry.imported_transcript_entry_id
            WHERE semantic_entry.source_session_id = checked_session_id
              AND semantic_entry.payload_kind = 'imported_entry'
              AND (
                  member.semantic_entry_id IS NULL
                  OR member.member_position
                        IS DISTINCT FROM imported_entry.imported_entry_position
                  OR imported_entry.imported_conversation_id
                        IS DISTINCT FROM checked_imported_conversation_id
                  OR imported_entry.imported_entry_position >
                        checked_frontier_position
              )
       )
    THEN
        RAISE EXCEPTION
            'imported session % seed is not its exact ordered imported prefix',
            checked_session_id
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'imported_session_seed_exact_prefix';
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM imported_transcript_entry
         WHERE imported_conversation_id = checked_imported_conversation_id
           AND imported_transcript_entry_id = checked_frontier_entry_id
           AND imported_entry_position = checked_frontier_position
    ) THEN
        RAISE EXCEPTION
            'imported session % ancestry boundary is unavailable',
            checked_session_id
            USING
                ERRCODE = '23503',
                CONSTRAINT = 'imported_session_seed_source_boundary';
    END IF;
END;
$$;

CREATE FUNCTION require_imported_seed_for_session()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM assert_imported_session_seed_complete(
        CASE WHEN TG_OP = 'DELETE' THEN OLD.session_id ELSE NEW.session_id END
    );
    RETURN NULL;
END;
$$;

CREATE FUNCTION require_imported_ancestry_for_seed()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    checked_ancestry_kind text;
BEGIN
    SELECT ancestry_kind
      INTO checked_ancestry_kind
      FROM session
     WHERE session_id = NEW.session_id;

    IF NOT FOUND OR checked_ancestry_kind <> 'imported_conversation' THEN
        RAISE EXCEPTION
            'imported seed requires imported session ancestry'
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'imported_session_seed_requires_imported_ancestry';
    END IF;

    RETURN NULL;
END;
$$;

-- Once a seed transaction commits, the imported semantic prefix is sealed.
-- Rows belonging to the transaction that inserts the seed link remain
-- order-independent; the one deferred full-prefix check validates their final
-- shape without queueing one scan per member.
CREATE FUNCTION reject_imported_semantic_entry_after_seed()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    checked_ancestry_kind text;
    checked_imported_conversation_id uuid;
    checked_frontier_position numeric(20, 0);
BEGIN
    IF NEW.payload_kind <> 'imported_entry' THEN
        RETURN NEW;
    END IF;

    SELECT
        ancestry_kind,
        imported_conversation_id,
        imported_frontier_position
      INTO
        checked_ancestry_kind,
        checked_imported_conversation_id,
        checked_frontier_position
      FROM session
     WHERE session_id = NEW.source_session_id;

    IF NOT FOUND OR checked_ancestry_kind <> 'imported_conversation' THEN
        RAISE EXCEPTION
            'imported semantic entry requires imported session ancestry'
            USING
                ERRCODE = '23514',
                CONSTRAINT =
                    'imported_semantic_entry_requires_imported_ancestry';
    END IF;

    -- Each selected-prefix source entry is unique per session. Restricting
    -- every imported semantic row to the selected conversation and inclusive
    -- boundary therefore prevents a same-transaction row from extending an
    -- already validated seed, even after SET CONSTRAINTS ... IMMEDIATE has
    -- discharged the one deferred full-prefix check.
    IF NEW.imported_conversation_id IS DISTINCT FROM
           checked_imported_conversation_id
       OR NOT EXISTS (
        SELECT 1
          FROM imported_transcript_entry AS imported_entry
         WHERE imported_entry.imported_conversation_id =
                   NEW.imported_conversation_id
           AND imported_entry.imported_transcript_entry_id =
                   NEW.imported_transcript_entry_id
           AND imported_entry.imported_entry_position <=
                   checked_frontier_position
    ) THEN
        RAISE EXCEPTION
            'imported semantic entry lies outside the selected prefix'
            USING
                ERRCODE = '23514',
                CONSTRAINT =
                    'imported_semantic_entry_requires_selected_prefix';
    END IF;

    IF EXISTS (
        SELECT 1
          FROM imported_session_seed AS seed
         WHERE seed.session_id = NEW.source_session_id
           AND seed.creation_transaction_id <> pg_current_xact_id()
    ) THEN
        RAISE EXCEPTION
            'imported session % semantic seed prefix is already sealed',
            NEW.source_session_id
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'imported_semantic_entry_seed_is_sealed';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER imported_semantic_entry_seed_is_sealed
AFTER INSERT ON semantic_transcript_entry
FOR EACH ROW
EXECUTE FUNCTION reject_imported_semantic_entry_after_seed();

CREATE TRIGGER context_frontier_member_cannot_be_truncated
BEFORE TRUNCATE ON context_frontier_member
FOR EACH STATEMENT
EXECUTE FUNCTION reject_imported_table_truncate();

CREATE CONSTRAINT TRIGGER session_requires_imported_seed
AFTER INSERT ON session
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_imported_seed_for_session();

CREATE CONSTRAINT TRIGGER imported_seed_requires_imported_ancestry
AFTER INSERT ON imported_session_seed
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_imported_ancestry_for_seed();
