ALTER TABLE submit_input_command
    ADD COLUMN actor_program_run_id uuid REFERENCES program_run_journal_stream(run_id),
    DROP CONSTRAINT submit_input_command_storage_version_supported,
    ADD CONSTRAINT submit_input_command_storage_version_supported
        CHECK (storage_version IN (3, 4)),
    DROP CONSTRAINT submit_input_command_actor_kind_closed,
    ADD CONSTRAINT submit_input_command_actor_kind_closed
        CHECK (actor_kind IN ('user', 'core', 'model', 'recovery', 'tool', 'program')),
    DROP CONSTRAINT submit_input_command_actor_shape,
    ADD CONSTRAINT submit_input_command_actor_shape CHECK (
        (actor_kind IN ('user', 'core', 'recovery')
            AND num_nonnulls(actor_turn_id, actor_tool_request_id, actor_program_run_id) = 0)
        OR (actor_kind = 'model' AND actor_turn_id IS NOT NULL
            AND actor_tool_request_id IS NULL AND actor_program_run_id IS NULL)
        OR (actor_kind = 'tool' AND actor_tool_request_id IS NOT NULL
            AND actor_turn_id IS NULL AND actor_program_run_id IS NULL)
        OR (actor_kind = 'program' AND actor_program_run_id IS NOT NULL
            AND actor_turn_id IS NULL AND actor_tool_request_id IS NULL
            AND storage_version >= 4)
    );

DO $$
DECLARE
    definition text;
BEGIN
    SELECT pg_get_constraintdef(oid) INTO STRICT definition
      FROM pg_constraint
     WHERE conrelid = 'durable_command'::regclass
       AND conname = 'durable_command_storage_version_supported';
    ALTER TABLE durable_command DROP CONSTRAINT durable_command_storage_version_supported;
    EXECUTE 'ALTER TABLE durable_command ADD CONSTRAINT durable_command_storage_version_supported CHECK ('
        || substring(definition FROM 7)
        || ' OR (command_kind = ''submit_input'' AND storage_version = 4))';
END $$;
