-- Durable workspaces, and the operator-minted Git push destinations they scope.

-- A workspace root is stored in canonical form: absolute, every component
-- non-empty and neither `.` nor `..`, and no trailing separator. Resolving
-- symbolic links needs the filesystem and happens once, at the boundary that
-- mints the row; this predicate is what makes that single resolution binding.
--
-- Without it `/srv/workspace`, `/srv/workspace/.` and
-- `/srv/workspace/nested/../workspace` are one directory under three spellings,
-- and a rule scoped "one per workspace" would admit three. The alternative —
-- normalizing at each comparison — spreads the same resolution across every
-- reader and fails the moment one of them forgets.
--
-- The byte bound is 1024 rather than a full `PATH_MAX`, because the root is
-- uniquely indexed and a B-tree index tuple may not exceed roughly a third of
-- an 8 KiB page. A 4096-byte root would let an otherwise valid workspace fail
-- on index insertion instead of on validation.
CREATE FUNCTION workspace_root_path_is_valid(candidate text)
RETURNS boolean
LANGUAGE sql
IMMUTABLE
STRICT
PARALLEL SAFE
AS $$
    SELECT octet_length(candidate) BETWEEN 2 AND 1024
       AND candidate COLLATE "C" ~ '^(/[^/[:cntrl:]]+)+$'
       AND candidate COLLATE "C" !~ '(^|/)[.][.]?(/|$)'
$$;

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
--
-- A `userinfo@` prefix is refused rather than admitted. This column is
-- append-only, so a destination such as `https://user:token@example.test/repo`
-- would durably record that token where every database reader and every backup
-- can see it, and no later act could remove it. The credential policy for a
-- push is undecided (`docs/open-questions.md`, daemon Git push transport), and
-- admitting userinfo now would settle it by accident: the narrower grammar can
-- be widened once credentials have an approved representation, whereas a stored
-- secret cannot be recalled.
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
               '^https://'
            || '[A-Za-z0-9._~-]+(:[0-9]{1,5})?'
            || '([/?#].*)?$'
           )
       AND coalesce(
               (substring(candidate COLLATE "C"
                          from '^https://[^/?#]*:([0-9]{1,5})(?:[/?#]|$)'))::int,
               0
           ) <= 65535
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
            'update_session_placement', 'register_workspace',
            'mint_git_remote', 'withdraw_git_remote'
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
            'goal', 'update_session_placement', 'register_workspace',
            'mint_git_remote', 'withdraw_git_remote'
        ) AND storage_version = 1)
    );

-- One durable workspace: the unit an authority grant is scoped to.
--
-- The identity is what grants are keyed by, never the path. A path cannot be a
-- key, because two spellings of one directory are two keys and the rule above
-- them silently stops holding; the canonical form is fixed once here, and every
-- later comparison is between UUIDs.
--
-- `origin` records which minting tier produced the row, because the tiers carry
-- different authority. An `operator_registered` workspace is a new authority
-- scope and is a human act, so it carries the durable command that registered
-- it. A `daemon_derived` row records a root the per-session derivation already
-- materialized from the configured base: authority still flows from that base
-- and its fixed formula, so the row carries no command and nothing reads it to
-- decide which roots the daemon may open. The `CHECK` below binds the two
-- facts together, so a derived row cannot claim command provenance and a
-- registered one cannot omit it.
--
-- Both legal shapes are spelled out rather than compared as one equality
-- between `origin` and "provenance is present". Under that shorter form a
-- derived row carrying only `command_id` made both sides false and passed,
-- and the composite foreign key's default `MATCH SIMPLE` then skipped
-- validation because one of its columns was null — precisely the partial
-- provenance this comment says a derived row cannot carry. `MATCH FULL` on
-- that key refuses a partly-null reference for the same reason, so the two
-- declarations now agree.
CREATE TABLE workspace (
    workspace_id uuid PRIMARY KEY,
    root_path text NOT NULL,
    origin text NOT NULL,
    command_id uuid,
    command_kind text,
    storage_version smallint,
    registered_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    CONSTRAINT workspace_root_path_valid
        CHECK (workspace_root_path_is_valid(root_path)),
    CONSTRAINT workspace_root_path_key
        UNIQUE (root_path),
    CONSTRAINT workspace_origin_closed
        CHECK (origin IN ('operator_registered', 'daemon_derived')),
    CONSTRAINT workspace_command_kind_closed
        CHECK (command_kind IS NULL OR command_kind = 'register_workspace'),
    CONSTRAINT workspace_storage_version_closed
        CHECK (storage_version IS NULL OR storage_version = 1),
    CONSTRAINT workspace_command_matches_origin
        CHECK (
            (origin = 'operator_registered'
             AND command_id IS NOT NULL
             AND command_kind IS NOT NULL
             AND storage_version IS NOT NULL)
            OR (origin = 'daemon_derived'
                AND command_id IS NULL
                AND command_kind IS NULL
                AND storage_version IS NULL)
        ),
    CONSTRAINT workspace_command_fk
        FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command (command_id, command_kind, storage_version)
        MATCH FULL
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT workspace_command_key
        UNIQUE (command_id)
);

CREATE TABLE configured_git_remote_mint (
    mint_id uuid PRIMARY KEY,
    command_id uuid NOT NULL,
    command_kind text NOT NULL CHECK (command_kind = 'mint_git_remote'),
    storage_version smallint NOT NULL CHECK (storage_version = 1),
    workspace_id uuid NOT NULL,
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
    CONSTRAINT configured_git_remote_mint_workspace_fk
        FOREIGN KEY (workspace_id)
        REFERENCES workspace (workspace_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
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

-- Which mint currently stands for one workspace and name.
--
-- The mint and withdrawal tables are the facts and stay append-only. This table
-- is their derived live view, and it is deliberately mutable: a withdrawal
-- deletes the row rather than marking it, because a marking update is what an
-- append-only fact table cannot express.
--
-- Its existence is what makes "at most one live mint per workspace and name" an
-- index-enforced rule instead of a counted one. A counting trigger cannot see
-- another transaction's uncommitted row, so two concurrent mints of one name
-- both counted one and both committed; the previous shape bought serialization
-- with an advisory lock and then had to refuse any isolation level but
-- `READ COMMITTED`, because a lock cannot refresh a snapshot. A unique
-- constraint has no such blind spot: its check consults the index directly and
-- waits on the concurrent inserter, so the rule holds at every isolation level
-- and callers regain the freedom to open a transaction however they need.
--
-- The constraint is deferred so a withdrawal and its replacement mint may land
-- in one transaction in either order. `mint_id` is unique and immediate: one
-- mint can stand live once, and that is true statement by statement.
CREATE TABLE configured_git_remote_live (
    workspace_id uuid NOT NULL,
    remote_name text NOT NULL,
    mint_id uuid NOT NULL,

    CONSTRAINT configured_git_remote_live_pk
        PRIMARY KEY (workspace_id, remote_name)
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT configured_git_remote_live_mint_key
        UNIQUE (mint_id),
    CONSTRAINT configured_git_remote_live_mint_fk
        FOREIGN KEY (mint_id)
        REFERENCES configured_git_remote_mint (mint_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

-- Resolution reads the live view, so the mint table is indexed for the other
-- question: every destination ever minted for one workspace and name, which is
-- what a withdrawal's audit trail is read through.
CREATE INDEX configured_git_remote_mint_workspace_name
    ON configured_git_remote_mint (workspace_id, remote_name);

-- Every kind that carries a typed record, so the registry's exhaustive case
-- must reach them; without these branches an admitted kind raises at commit.
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
        WHEN 'register_workspace' THEN SELECT count(*) INTO matching_records FROM workspace WHERE command_id = NEW.command_id;
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

-- A new mint stands live immediately; the unique constraint on the live view is
-- what refuses a second one for the same workspace and name.
CREATE FUNCTION record_configured_git_remote_as_live()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO configured_git_remote_live (workspace_id, remote_name, mint_id)
    VALUES (NEW.workspace_id, NEW.remote_name, NEW.mint_id);
    RETURN NULL;
END;
$$;

CREATE TRIGGER configured_git_remote_mint_becomes_live
AFTER INSERT ON configured_git_remote_mint
FOR EACH ROW
EXECUTE FUNCTION record_configured_git_remote_as_live();

-- A withdrawal retires exactly the mint it names, freeing that name for a
-- replacement in the same transaction.
CREATE FUNCTION retire_configured_git_remote_from_live()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    DELETE FROM configured_git_remote_live WHERE mint_id = NEW.mint_id;
    RETURN NULL;
END;
$$;

CREATE TRIGGER configured_git_remote_withdrawal_retires_the_mint
AFTER INSERT ON configured_git_remote_withdrawal
FOR EACH ROW
EXECUTE FUNCTION retire_configured_git_remote_from_live();

-- The live view carries the rule, so it must be provably a projection of the
-- facts rather than a table anyone may write. A row stands live exactly when
-- its mint exists and no withdrawal names it, and it stands under that mint's
-- own workspace and name. Checking it from all three tables means a dropped
-- maintenance trigger or a hand-written row fails at commit instead of
-- quietly disarming the uniqueness rule above.
CREATE FUNCTION configured_git_remote_live_agrees_with_the_facts()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    subject uuid;
    should_stand boolean;
    stands boolean;
BEGIN
    subject := CASE TG_OP WHEN 'DELETE' THEN OLD.mint_id ELSE NEW.mint_id END;

    SELECT EXISTS (
               SELECT 1 FROM configured_git_remote_mint
                WHERE mint_id = subject
           )
       AND NOT EXISTS (
               SELECT 1 FROM configured_git_remote_withdrawal
                WHERE mint_id = subject
           )
      INTO should_stand;

    SELECT EXISTS (
               SELECT 1 FROM configured_git_remote_live
                WHERE mint_id = subject
           )
      INTO stands;

    IF should_stand <> stands THEN
        RAISE EXCEPTION
            'configured Git remote live view disagrees with the facts for mint %',
            subject
            USING ERRCODE = '23514';
    END IF;

    IF stands AND NOT EXISTS (
        SELECT 1
          FROM configured_git_remote_live AS live
          JOIN configured_git_remote_mint AS mint
            ON mint.mint_id = live.mint_id
         WHERE live.mint_id = subject
           AND live.workspace_id = mint.workspace_id
           AND live.remote_name = mint.remote_name
    ) THEN
        RAISE EXCEPTION
            'configured Git remote live view misstates the scope of mint %',
            subject
            USING ERRCODE = '23514';
    END IF;

    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER configured_git_remote_mint_stands_live
AFTER INSERT ON configured_git_remote_mint
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION configured_git_remote_live_agrees_with_the_facts();

CREATE CONSTRAINT TRIGGER configured_git_remote_withdrawal_stands_down
AFTER INSERT ON configured_git_remote_withdrawal
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION configured_git_remote_live_agrees_with_the_facts();

CREATE CONSTRAINT TRIGGER configured_git_remote_live_stays_derived
AFTER INSERT OR DELETE ON configured_git_remote_live
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION configured_git_remote_live_agrees_with_the_facts();

CREATE FUNCTION reject_workspace_authority_table_truncate()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION
        'workspace authority tables cannot be truncated'
        USING ERRCODE = '23514';
END;
$$;

CREATE TRIGGER workspace_is_append_only
BEFORE UPDATE OR DELETE ON workspace
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER workspace_rejects_truncate
BEFORE TRUNCATE ON workspace
FOR EACH STATEMENT
EXECUTE FUNCTION reject_workspace_authority_table_truncate();

CREATE TRIGGER configured_git_remote_mint_is_append_only
BEFORE UPDATE OR DELETE ON configured_git_remote_mint
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER configured_git_remote_mint_rejects_truncate
BEFORE TRUNCATE ON configured_git_remote_mint
FOR EACH STATEMENT
EXECUTE FUNCTION reject_workspace_authority_table_truncate();

CREATE TRIGGER configured_git_remote_withdrawal_is_append_only
BEFORE UPDATE OR DELETE ON configured_git_remote_withdrawal
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER configured_git_remote_withdrawal_rejects_truncate
BEFORE TRUNCATE ON configured_git_remote_withdrawal
FOR EACH STATEMENT
EXECUTE FUNCTION reject_workspace_authority_table_truncate();

-- The live view is deleted from by design, so only its updates are refused: a
-- row that changed which mint stands would rewrite the rule rather than record
-- it. Truncation is refused for the same reason it is on the fact tables —
-- emptying this table would silently disarm the uniqueness constraint.
CREATE TRIGGER configured_git_remote_live_is_never_updated
BEFORE UPDATE ON configured_git_remote_live
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER configured_git_remote_live_rejects_truncate
BEFORE TRUNCATE ON configured_git_remote_live
FOR EACH STATEMENT
EXECUTE FUNCTION reject_workspace_authority_table_truncate();
