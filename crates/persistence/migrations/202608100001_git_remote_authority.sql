-- Durable operator-minted Git push destinations and their withdrawal.

-- The admitted shape is narrower than Git's own reference grammar so that one
-- durable row, one Git reference component, and one command argument admit
-- exactly the same values. The trailing clauses mirror the reference rules that
-- `gix_validate` applies to `refs/remotes/<name>/probe`, which the push
-- executor's `ConfiguredGitRemote` builds: no leading dot, no `..`, no trailing
-- dot, and no `.lock` suffix.
CREATE FUNCTION configured_git_remote_name_is_valid(candidate text)
RETURNS boolean
LANGUAGE sql
IMMUTABLE
STRICT
PARALLEL SAFE
AS $$
    SELECT octet_length(candidate) BETWEEN 1 AND 255
       AND candidate COLLATE "C" ~ '^[A-Za-z0-9._-]+$'
       AND candidate COLLATE "C" !~ '^\.'
       AND candidate COLLATE "C" !~ '\.$'
       AND candidate COLLATE "C" !~ '\.\.'
       AND candidate COLLATE "C" !~ '\.lock$'
$$;

-- Only https destinations are storable. The push transport compiles no SSH
-- support, so a non-https destination could never be dispatched; rejecting it
-- here keeps the durable record and the transport agreed on one scheme.
--
-- A destination must also carry an authority, because a scheme-only URL such as
-- `https://?` names no host and no HTTPS transport could dispatch it.
--
-- The first predicate admits printable ASCII only. `[:space:]` and `[:cntrl:]`
-- under the `C` collation classify no non-ASCII byte, so those classes would
-- admit a destination — `U+00A0` in a path, for one — that `GitRemoteUrl`
-- refuses, and the append-only store would then hold a row the domain type
-- cannot represent. A bracketed IP-literal host is refused for the same
-- reason: a POSIX regular expression cannot express the IPv6 grammar, so the
-- two sides could not be held in agreement.
CREATE FUNCTION configured_git_remote_url_is_valid(candidate text)
RETURNS boolean
LANGUAGE sql
IMMUTABLE
STRICT
PARALLEL SAFE
AS $$
    SELECT octet_length(candidate) BETWEEN 9 AND 4096
       AND candidate COLLATE "C" ~ '^[!-~]+$'
       AND candidate COLLATE "C" ~ (
               '^https://([^@/?#]*@)?'
            || '[A-Za-z0-9._~-]+(:[0-9]{1,5})?'
            || '([/?#].*)?$'
           )
       AND coalesce(
               (substring(candidate COLLATE "C"
                          from '^https://[^/?#]*:([0-9]{1,5})(?:[/?#]|$)'))::int,
               0
           ) <= 65535
$$;

-- A workspace has no durable record in this schema, so a minted destination is
-- scoped by the absolute canonical root the daemon pins for its tool suites.
--
-- The byte bound is 1024 rather than a full `PATH_MAX`, because the root is
-- indexed together with the remote name and a B-tree index tuple may not exceed
-- roughly a third of an 8 KiB page. A 4096-byte root would let an otherwise
-- valid mint fail on index insertion instead of on validation.
CREATE FUNCTION configured_git_remote_workspace_is_valid(candidate text)
RETURNS boolean
LANGUAGE sql
IMMUTABLE
STRICT
PARALLEL SAFE
AS $$
    SELECT octet_length(candidate) BETWEEN 1 AND 1024
       AND candidate COLLATE "C" ~ '^/[^[:cntrl:]]*$'
$$;

-- The rebuilt version constraint must carry every version supported immediately
-- before this migration; `202608030003_model_session_settings.sql` is the
-- predecessor that last reissued it.
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
        (command_kind = 'create_session'
            AND storage_version IN (1, 2, 3, 4, 5, 6, 7))
        OR (command_kind IN (
            'replace_session_defaults'
        ) AND storage_version IN (1, 2, 3, 4))
        OR (command_kind = 'create_session_from_imported_frontier'
            AND storage_version IN (1, 2, 3, 5))
        OR (command_kind = 'submit_input' AND storage_version IN (1, 2))
        OR (command_kind IN (
            'replace_session_metadata', 'decide_tool_request',
            'review_workflow', 'review_orchestration', 'compact_session',
            'goal', 'update_session_placement', 'mint_git_remote',
            'withdraw_git_remote'
        ) AND storage_version = 1)
    );

CREATE TABLE configured_git_remote_mint (
    mint_id uuid PRIMARY KEY,
    command_id uuid NOT NULL,
    command_kind text NOT NULL CHECK (command_kind = 'mint_git_remote'),
    storage_version smallint NOT NULL CHECK (storage_version = 1),
    workspace_root text NOT NULL,
    remote_name text NOT NULL,
    remote_url text NOT NULL,
    minted_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    CONSTRAINT configured_git_remote_mint_command_fk
        FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command (command_id, command_kind, storage_version)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
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
    command_kind text NOT NULL CHECK (command_kind = 'withdraw_git_remote'),
    storage_version smallint NOT NULL CHECK (storage_version = 1),
    withdrawn_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    CONSTRAINT configured_git_remote_withdrawal_mint_fk
        FOREIGN KEY (mint_id)
        REFERENCES configured_git_remote_mint(mint_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT configured_git_remote_withdrawal_command_fk
        FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command (command_id, command_kind, storage_version)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT configured_git_remote_withdrawal_command_key
        UNIQUE (command_id),
    CONSTRAINT configured_git_remote_withdrawal_mint_key
        UNIQUE (mint_id)
);

CREATE INDEX configured_git_remote_mint_workspace_name
    ON configured_git_remote_mint (workspace_root, remote_name);

-- Both new kinds carry a typed record, so the registry's exhaustive case must
-- reach them; without these branches an admitted kind raises at commit.
CREATE OR REPLACE FUNCTION require_durable_command_typed_record()
RETURNS trigger LANGUAGE plpgsql AS $$
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
        WHEN 'review_workflow' THEN SELECT count(*) INTO matching_records FROM review_workflow_command WHERE command_id = NEW.command_id;
        WHEN 'review_orchestration' THEN SELECT (SELECT count(*) FROM review_orchestration_command WHERE command_id = NEW.command_id) + (SELECT count(*) FROM review_orchestration_command_intent WHERE command_id = NEW.command_id) INTO matching_records;
        WHEN 'compact_session' THEN SELECT count(*) INTO matching_records FROM compact_session_command WHERE command_id = NEW.command_id;
        WHEN 'goal' THEN SELECT count(*) INTO matching_records FROM goal_command WHERE command_id = NEW.command_id;
        WHEN 'update_session_placement' THEN SELECT count(*) INTO matching_records FROM update_session_placement_command WHERE command_id = NEW.command_id;
        WHEN 'mint_git_remote' THEN SELECT count(*) INTO matching_records FROM configured_git_remote_mint WHERE command_id = NEW.command_id;
        WHEN 'withdraw_git_remote' THEN SELECT count(*) INTO matching_records FROM configured_git_remote_withdrawal WHERE command_id = NEW.command_id;
        ELSE RAISE EXCEPTION 'unsupported durable command kind %', NEW.command_kind USING ERRCODE = '23514';
    END CASE;
    IF matching_records <> 1 THEN
        RAISE EXCEPTION 'durable command % requires exactly one % typed record', NEW.command_id, NEW.command_kind USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;

-- Two transactions minting the same name concurrently would each see only their
-- own uncommitted row at the deferred check and both commit, so inserts for one
-- workspace and name are serialized on an advisory lock held to end of
-- transaction, following the review-identity guard's precedent. A withdrawal
-- that would have freed the name but commits afterwards makes the mint fail
-- closed rather than admit a second live destination.
CREATE FUNCTION guard_configured_git_remote_mint_insert()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM pg_advisory_xact_lock(
        hashtextextended(
            NEW.workspace_root || chr(31) || NEW.remote_name,
            0
        )
    );
    RETURN NEW;
END;
$$;

CREATE TRIGGER configured_git_remote_mint_serializes_name
BEFORE INSERT ON configured_git_remote_mint
FOR EACH ROW
EXECUTE FUNCTION guard_configured_git_remote_mint_insert();

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
