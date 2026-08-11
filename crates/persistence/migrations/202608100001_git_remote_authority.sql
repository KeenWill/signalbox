-- Durable operator-minted Git push destinations and their withdrawal.

CREATE FUNCTION configured_git_remote_name_is_valid(candidate text)
RETURNS boolean
LANGUAGE sql
IMMUTABLE
STRICT
PARALLEL SAFE
AS $$
    SELECT octet_length(candidate) BETWEEN 1 AND 255
       AND candidate COLLATE "C" ~ '^[A-Za-z0-9._-]+$'
$$;

-- Only https destinations are storable. The push transport compiles no SSH
-- support, so a non-https destination could never be dispatched; rejecting it
-- here keeps the durable record and the transport agreed on one scheme.
CREATE FUNCTION configured_git_remote_url_is_valid(candidate text)
RETURNS boolean
LANGUAGE sql
IMMUTABLE
STRICT
PARALLEL SAFE
AS $$
    SELECT octet_length(candidate) BETWEEN 9 AND 4096
       AND candidate COLLATE "C" ~ '^https://[^[:cntrl:][:space:]]+$'
$$;

-- A workspace has no durable record in this schema, so a minted destination is
-- scoped by the absolute canonical root the daemon pins for its tool suites.
CREATE FUNCTION configured_git_remote_workspace_is_valid(candidate text)
RETURNS boolean
LANGUAGE sql
IMMUTABLE
STRICT
PARALLEL SAFE
AS $$
    SELECT octet_length(candidate) BETWEEN 1 AND 4096
       AND candidate COLLATE "C" ~ '^/[^[:cntrl:]]*$'
$$;

ALTER TABLE durable_command
    DROP CONSTRAINT durable_command_kind_closed,
    DROP CONSTRAINT durable_command_storage_version_supported;

ALTER TABLE durable_command
    ADD CONSTRAINT durable_command_kind_closed CHECK (
        command_kind IN (
            'create_session', 'create_session_from_imported_frontier',
            'replace_session_defaults', 'replace_session_metadata',
            'submit_input', 'decide_tool_request', 'review_workflow',
            'review_orchestration', 'compact_session', 'goal',
            'update_session_placement', 'mint_git_remote',
            'withdraw_git_remote'
        )
    ),
    ADD CONSTRAINT durable_command_storage_version_supported CHECK (
        (command_kind = 'create_session' AND storage_version IN (1, 2, 3, 4, 5, 6))
        OR (command_kind IN (
            'create_session_from_imported_frontier', 'replace_session_defaults'
        ) AND storage_version IN (1, 2, 3))
        OR (command_kind IN (
            'replace_session_metadata', 'submit_input', 'decide_tool_request',
            'review_workflow', 'review_orchestration', 'compact_session',
            'goal', 'update_session_placement', 'mint_git_remote',
            'withdraw_git_remote'
        ) AND storage_version = 1)
    );

CREATE TABLE configured_git_remote_mint (
    mint_id uuid PRIMARY KEY,
    command_id uuid NOT NULL,
    workspace_root text NOT NULL,
    remote_name text NOT NULL,
    remote_url text NOT NULL,
    minted_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    CONSTRAINT configured_git_remote_mint_command_fk
        FOREIGN KEY (command_id)
        REFERENCES durable_command(command_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT configured_git_remote_mint_command_key
        UNIQUE (command_id),
    CONSTRAINT configured_git_remote_mint_workspace_valid
        CHECK (configured_git_remote_workspace_is_valid(workspace_root)),
    CONSTRAINT configured_git_remote_mint_name_valid
        CHECK (configured_git_remote_name_is_valid(remote_name)),
    CONSTRAINT configured_git_remote_mint_url_valid
        CHECK (configured_git_remote_url_is_valid(remote_url))
);

CREATE TABLE configured_git_remote_withdrawal (
    withdrawal_id uuid PRIMARY KEY,
    mint_id uuid NOT NULL,
    command_id uuid NOT NULL,
    withdrawn_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    CONSTRAINT configured_git_remote_withdrawal_mint_fk
        FOREIGN KEY (mint_id)
        REFERENCES configured_git_remote_mint(mint_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT configured_git_remote_withdrawal_command_fk
        FOREIGN KEY (command_id)
        REFERENCES durable_command(command_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT configured_git_remote_withdrawal_command_key
        UNIQUE (command_id),
    CONSTRAINT configured_git_remote_withdrawal_mint_key
        UNIQUE (mint_id)
);

CREATE INDEX configured_git_remote_mint_workspace_name
    ON configured_git_remote_mint (workspace_root, remote_name);

CREATE INDEX configured_git_remote_withdrawal_mint
    ON configured_git_remote_withdrawal (mint_id);

-- Resolving a remote name must never be ambiguous, so at most one mint for one
-- workspace and name may stand un-withdrawn. The check is deferred because a
-- withdrawal and its replacement mint legitimately land in one transaction.
CREATE FUNCTION configured_git_remote_has_one_live_mint()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    live_count bigint;
BEGIN
    SELECT count(*)
      INTO live_count
      FROM configured_git_remote_mint AS mint
     WHERE mint.workspace_root = NEW.workspace_root
       AND mint.remote_name = NEW.remote_name
       AND NOT EXISTS (
             SELECT 1
               FROM configured_git_remote_withdrawal AS withdrawal
              WHERE withdrawal.mint_id = mint.mint_id
           );
    IF live_count > 1 THEN
        RAISE EXCEPTION
            'configured Git remote name is already minted for this workspace'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER configured_git_remote_mint_stays_unambiguous
AFTER INSERT ON configured_git_remote_mint
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION configured_git_remote_has_one_live_mint();

CREATE FUNCTION reject_configured_git_remote_table_truncate()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION
        'configured Git remote tables are append-only and cannot be truncated'
        USING ERRCODE = '23514';
END;
$$;

CREATE TRIGGER configured_git_remote_mint_is_append_only
BEFORE UPDATE OR DELETE ON configured_git_remote_mint
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER configured_git_remote_mint_rejects_truncate
BEFORE TRUNCATE ON configured_git_remote_mint
FOR EACH STATEMENT
EXECUTE FUNCTION reject_configured_git_remote_table_truncate();

CREATE TRIGGER configured_git_remote_withdrawal_is_append_only
BEFORE UPDATE OR DELETE ON configured_git_remote_withdrawal
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER configured_git_remote_withdrawal_rejects_truncate
BEFORE TRUNCATE ON configured_git_remote_withdrawal
FOR EACH STATEMENT
EXECUTE FUNCTION reject_configured_git_remote_table_truncate();
