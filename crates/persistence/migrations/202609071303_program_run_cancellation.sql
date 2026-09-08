DO $$
DECLARE expression text;
BEGIN
 SELECT pg_get_expr(conbin, conrelid) INTO STRICT expression FROM pg_constraint
   WHERE conrelid = 'durable_command'::regclass AND conname = 'durable_command_kind_closed';
 ALTER TABLE durable_command DROP CONSTRAINT durable_command_kind_closed;
 EXECUTE format('ALTER TABLE durable_command ADD CONSTRAINT durable_command_kind_closed CHECK ((%s) OR command_kind = %L)', expression, 'cancel_program_run');
END;
$$;

DO $$
DECLARE item record;
BEGIN
 FOR item IN SELECT conname, pg_get_expr(conbin, conrelid) AS expression FROM pg_constraint
   WHERE conrelid = 'durable_command'::regclass AND conname = 'durable_command_storage_version_supported' LOOP
   EXECUTE format('ALTER TABLE durable_command DROP CONSTRAINT %I', item.conname);
   EXECUTE format('ALTER TABLE durable_command ADD CONSTRAINT %I CHECK ((%s) OR (command_kind = %L AND storage_version = 1))', item.conname, item.expression, 'cancel_program_run');
 END LOOP;
END;
$$;
CREATE TABLE cancel_program_run_command (
    command_id uuid PRIMARY KEY,
    command_kind text NOT NULL DEFAULT 'cancel_program_run' CHECK (command_kind = 'cancel_program_run'),
    storage_version smallint NOT NULL DEFAULT 1 CHECK (storage_version = 1),
    run_id uuid NOT NULL,
    outcome text NOT NULL CHECK (outcome IN ('applied', 'not_found', 'already_terminal')),
    terminal_state text CHECK (terminal_state IN ('cancelled', 'faulted')),
    cancellation_position numeric,
    CHECK ((outcome = 'applied' AND terminal_state IS NOT NULL AND terminal_state = 'cancelled' AND cancellation_position IS NOT NULL)
        OR (outcome = 'not_found' AND terminal_state IS NULL AND cancellation_position IS NULL)
        OR (outcome = 'already_terminal' AND terminal_state IS NOT NULL AND cancellation_position IS NULL)),
    FOREIGN KEY (command_id, command_kind, storage_version) REFERENCES durable_command(command_id, command_kind, storage_version) DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (run_id, cancellation_position) REFERENCES program_run_journal_entry(run_id, journal_position) DEFERRABLE INITIALLY DEFERRED
);
CREATE TRIGGER cancel_program_run_command_immutable BEFORE UPDATE OR DELETE ON cancel_program_run_command
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE FUNCTION authenticate_program_cancellation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.outcome = 'applied' AND NOT EXISTS (
        SELECT 1 FROM program_run_journal_entry
         WHERE run_id = NEW.run_id AND journal_position = NEW.cancellation_position
           AND frame_direction = 'delivery' AND frame_kind = 'run_cancel'
           AND resolves_request_ordinal IS NULL
           AND payload_inline = convert_to(NEW.command_id::text, 'UTF8')
    ) THEN
        RAISE EXCEPTION 'applied program cancellation requires its terminal delivery' USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER cancel_program_run_delivery AFTER INSERT ON cancel_program_run_command
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION authenticate_program_cancellation();

CREATE OR REPLACE FUNCTION require_durable_command_typed_record() RETURNS trigger
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
        WHEN 'session_lifecycle' THEN SELECT count(*) INTO matching_records FROM session_lifecycle_command WHERE command_id = NEW.command_id;
        WHEN 'provision_oauth_credential' THEN SELECT count(*) INTO matching_records FROM provision_oauth_credential_command WHERE command_id = NEW.command_id;
        WHEN 'reprovision_oauth_credential' THEN SELECT count(*) INTO matching_records FROM reprovision_oauth_credential_command WHERE command_id = NEW.command_id;
        WHEN 'delete_oauth_credential' THEN SELECT count(*) INTO matching_records FROM delete_oauth_credential_command WHERE command_id = NEW.command_id;
        WHEN 'clear_credential_exclusion' THEN SELECT count(*) INTO matching_records FROM clear_credential_exclusion_command WHERE command_id = NEW.command_id;
        WHEN 'reload_configuration' THEN SELECT count(*) INTO matching_records FROM reload_configuration_command WHERE command_id = NEW.command_id;
        WHEN 'replace_lost_runner' THEN SELECT count(*) INTO matching_records FROM replace_lost_runner_command WHERE command_id = NEW.command_id;
        WHEN 'abandon_lost_runner' THEN SELECT count(*) INTO matching_records FROM abandon_lost_runner_command WHERE command_id = NEW.command_id;
        WHEN 'promote_pending_runner' THEN SELECT count(*) INTO matching_records FROM promote_pending_runner_command WHERE command_id = NEW.command_id;
        WHEN 'cancel_program_run' THEN SELECT count(*) INTO matching_records FROM cancel_program_run_command WHERE command_id = NEW.command_id;
        ELSE RAISE EXCEPTION 'unsupported durable command kind %', NEW.command_kind USING ERRCODE = '23514';
    END CASE;
    IF matching_records <> 1 THEN
        RAISE EXCEPTION 'durable command % requires exactly one % typed record', NEW.command_id, NEW.command_kind USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;
