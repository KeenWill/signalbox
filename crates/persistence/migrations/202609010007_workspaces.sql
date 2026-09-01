-- Workspaces and instructions: the workspace record, configured git remotes
-- (minted, live, withdrawn), registered instruction bundles, instruction
-- discovery with its roots, candidates, and findings, and the per-turn
-- instruction manifest.

-- Function bodies may read tables a later section or file creates;
-- 202609010000_core.sql explains why validation is deferred.
SET check_function_bodies = false;

--
-- Functions.
--

--
-- Name: configured_git_remote_live_agrees_with_the_facts(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION configured_git_remote_live_agrees_with_the_facts() RETURNS trigger
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


--
-- Name: configured_git_remote_name_is_valid(text); Type: FUNCTION; Schema: public
--

CREATE FUNCTION configured_git_remote_name_is_valid(candidate text) RETURNS boolean
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    AS $_$
    SELECT octet_length(candidate) BETWEEN 1 AND 255
       AND candidate COLLATE "C" ~ '^[A-Za-z0-9._-]+$'
       AND candidate COLLATE "C" !~ '^\.'
       AND candidate COLLATE "C" !~ '\.$'
       AND candidate COLLATE "C" !~ '\.\.'
       AND candidate COLLATE "C" !~ '\.lock$'
$_$;


--
-- Name: configured_git_remote_url_is_valid(text); Type: FUNCTION; Schema: public
--

CREATE FUNCTION configured_git_remote_url_is_valid(candidate text) RETURNS boolean
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    AS $_$
    SELECT octet_length(candidate) BETWEEN 9 AND 4096
       AND candidate COLLATE "C" ~ '^[!-~]+$'
       AND candidate COLLATE "C" ~ (
               '^https://'
            || '[A-Za-z0-9._~-]+(:[0-9]{1,5})?'
            || '(/[^?#]*)?$'
           )
       AND coalesce(
               (substring(candidate COLLATE "C"
                          from '^https://[^/?#]*:([0-9]{1,5})(?:[/?#]|$)'))::int,
               1
           ) BETWEEN 1 AND 65535
$_$;


--
-- Name: record_configured_git_remote_as_live(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION record_configured_git_remote_as_live() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    INSERT INTO configured_git_remote_live (workspace_id, remote_name, mint_id)
    VALUES (NEW.workspace_id, NEW.remote_name, NEW.mint_id);
    RETURN NULL;
END;
$$;


--
-- Name: reject_instruction_discovery_child_after_seal(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_instruction_discovery_child_after_seal() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM instruction_discovery
         WHERE instruction_discovery_id = NEW.instruction_discovery_id
    ) THEN
        RAISE EXCEPTION 'instruction discovery child inventory is sealed'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'instruction_discovery_membership_sealed';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: reject_instruction_evidence_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_instruction_evidence_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RAISE EXCEPTION 'workspace instruction evidence is append-only'
        USING ERRCODE = '23514';
END;
$$;


--
-- Name: reject_workspace_authority_table_truncate(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_workspace_authority_table_truncate() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RAISE EXCEPTION
        'workspace authority tables cannot be truncated'
        USING ERRCODE = '23514';
END;
$$;


--
-- Name: retire_configured_git_remote_from_live(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION retire_configured_git_remote_from_live() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    DELETE FROM configured_git_remote_live WHERE mint_id = NEW.mint_id;
    RETURN NULL;
END;
$$;


--
-- Name: validate_instruction_discovery_seal(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION validate_instruction_discovery_seal() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    root_count bigint;
    root_max_ordinal bigint;
    candidate_count bigint;
    candidate_max_ordinal bigint;
    candidate_source_byte_sum numeric;
    actual_finding_count bigint;
    finding_max_ordinal bigint;
    limit_finding_count bigint;
    limit_finding_max_ordinal bigint;
BEGIN
    SELECT count(*), coalesce(max(root_ordinal), 0)
      INTO root_count, root_max_ordinal
      FROM instruction_discovery_root
     WHERE instruction_discovery_id = NEW.instruction_discovery_id;
    IF root_count <> root_max_ordinal THEN
        RAISE EXCEPTION 'instruction discovery root inventory is not contiguous'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'instruction_discovery_root_inventory_exact';
    END IF;

    SELECT count(*), coalesce(max(candidate_ordinal), 0)
      INTO candidate_count, candidate_max_ordinal
      FROM instruction_discovery_candidate AS candidate
     WHERE candidate.instruction_discovery_id = NEW.instruction_discovery_id;
    IF candidate_count <> candidate_max_ordinal THEN
        RAISE EXCEPTION 'instruction discovery candidate inventory is not contiguous'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'instruction_discovery_candidate_inventory_exact';
    END IF;
    SELECT coalesce(sum(bundle.source_byte_length), 0)
      INTO candidate_source_byte_sum
      FROM instruction_discovery_candidate AS candidate
      JOIN registered_instruction_bundle AS bundle
        ON bundle.instruction_bundle_id = candidate.instruction_bundle_id
     WHERE candidate.instruction_discovery_id = NEW.instruction_discovery_id;
    IF candidate_count > NEW.classified_entry_count
        OR candidate_source_byte_sum > NEW.candidate_source_byte_count
    THEN
        RAISE EXCEPTION 'instruction discovery candidates exceed consumed resources'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'instruction_discovery_candidate_usage_within_consumed';
    END IF;
    IF EXISTS (
        SELECT 1
          FROM instruction_discovery_candidate AS candidate
          JOIN registered_instruction_bundle AS bundle
            ON bundle.instruction_bundle_id = candidate.instruction_bundle_id
         WHERE candidate.instruction_discovery_id = NEW.instruction_discovery_id
           AND NOT EXISTS (
                SELECT 1
                  FROM instruction_discovery_root AS root
                 WHERE root.instruction_discovery_id = NEW.instruction_discovery_id
                   AND root.root_kind = bundle.root_kind
                   AND root.root_path = bundle.root_path
           )
    ) THEN
        RAISE EXCEPTION 'instruction discovery candidate root is absent from inventory'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'instruction_discovery_candidate_root_in_inventory';
    END IF;

    SELECT count(*), coalesce(max(finding_ordinal), 0)
      INTO actual_finding_count, finding_max_ordinal
      FROM instruction_discovery_finding
     WHERE instruction_discovery_id = NEW.instruction_discovery_id;
    IF actual_finding_count <> NEW.finding_count
        OR actual_finding_count <> finding_max_ordinal
    THEN
        RAISE EXCEPTION 'instruction discovery finding count disagrees with inventory'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'instruction_discovery_finding_inventory_exact';
    END IF;
    SELECT count(*), coalesce(max(finding_ordinal), 0)
      INTO limit_finding_count, limit_finding_max_ordinal
      FROM instruction_discovery_finding
     WHERE instruction_discovery_id = NEW.instruction_discovery_id
       AND finding_kind LIKE 'limit_%';
    IF (NEW.scan_complete AND limit_finding_count <> 0)
        OR (
            NOT NEW.scan_complete
            AND (
                limit_finding_count <> 1
                OR limit_finding_max_ordinal <> NEW.finding_count
            )
        )
    THEN
        RAISE EXCEPTION 'instruction discovery completeness disagrees with terminal finding'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'instruction_discovery_completeness_exact';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: validate_turn_instruction_manifest_discovery(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION validate_turn_instruction_manifest_discovery() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM instruction_discovery AS discovery
         WHERE discovery.instruction_discovery_id = NEW.instruction_discovery_id
           AND discovery.session_id = NEW.session_id
           AND discovery.turn_id = NEW.turn_id
           AND discovery.scan_complete
    ) THEN
        RAISE EXCEPTION 'turn instruction manifest requires a complete discovery'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'turn_instruction_manifest_discovery_complete';
    END IF;
    IF NEW.eligibility_hash <> sha256(
        convert_to('signalbox-instruction-eligibility-v1', 'UTF8')
    ) OR NEW.admitted_set_hash <> sha256(
        convert_to('signalbox-instruction-admitted-set-v1', 'UTF8')
        || '\x0000000000000000'::bytea
    ) OR NEW.manifest_hash <> sha256(
        convert_to('signalbox-turn-instruction-manifest-v1', 'UTF8')
        || uuid_send(NEW.session_id)
        || uuid_send(NEW.turn_id)
        || NEW.eligibility_hash
        || NEW.admitted_set_hash
        || convert_to(NEW.boundary_kind, 'UTF8')
    ) THEN
        RAISE EXCEPTION 'turn instruction manifest hashes are not canonical'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'turn_instruction_manifest_hash_shape';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: workspace_root_path_is_valid(text); Type: FUNCTION; Schema: public
--

CREATE FUNCTION workspace_root_path_is_valid(candidate text) RETURNS boolean
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    AS $_$
    SELECT octet_length(candidate) BETWEEN 2 AND 1024
       AND candidate COLLATE "C" ~ '^(/[^/[:cntrl:]]+)+$'
       AND candidate COLLATE "C" !~ '(^|/)[.][.]?(/|$)'
$_$;


--
-- Tables.
--

--
-- Name: configured_git_remote_live; Type: TABLE; Schema: public
--

CREATE TABLE configured_git_remote_live (
    workspace_id uuid NOT NULL,
    remote_name text NOT NULL,
    mint_id uuid NOT NULL
);


--
-- Name: configured_git_remote_mint; Type: TABLE; Schema: public
--

CREATE TABLE configured_git_remote_mint (
    mint_id uuid NOT NULL,
    command_id uuid NOT NULL,
    command_kind text NOT NULL,
    storage_version smallint NOT NULL,
    workspace_id uuid NOT NULL,
    remote_name text NOT NULL,
    remote_url text NOT NULL,
    minted_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    CONSTRAINT configured_git_remote_mint_command_kind_check CHECK ((command_kind = 'mint_git_remote'::text)),
    CONSTRAINT configured_git_remote_mint_name_valid CHECK (configured_git_remote_name_is_valid(remote_name)),
    CONSTRAINT configured_git_remote_mint_storage_version_check CHECK ((storage_version = 1)),
    CONSTRAINT configured_git_remote_mint_url_valid CHECK (configured_git_remote_url_is_valid(remote_url))
);


--
-- Name: configured_git_remote_withdrawal; Type: TABLE; Schema: public
--

CREATE TABLE configured_git_remote_withdrawal (
    withdrawal_id uuid NOT NULL,
    mint_id uuid NOT NULL,
    command_id uuid NOT NULL,
    command_kind text NOT NULL,
    storage_version smallint NOT NULL,
    withdrawn_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    CONSTRAINT configured_git_remote_withdrawal_command_kind_check CHECK ((command_kind = 'withdraw_git_remote'::text)),
    CONSTRAINT configured_git_remote_withdrawal_storage_version_check CHECK ((storage_version = 1))
);


--
-- Name: instruction_discovery; Type: TABLE; Schema: public
--

CREATE TABLE instruction_discovery (
    instruction_discovery_id uuid NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    limit_set_version smallint NOT NULL,
    classified_entry_count bigint NOT NULL,
    finding_count bigint NOT NULL,
    candidate_source_byte_count bigint NOT NULL,
    elapsed_millis bigint NOT NULL,
    scan_complete boolean NOT NULL,
    CONSTRAINT instruction_discovery_elapsed_nonnegative CHECK ((elapsed_millis >= 0)),
    CONSTRAINT instruction_discovery_entry_count_bounded CHECK (((classified_entry_count >= 0) AND (classified_entry_count <= 100000))),
    CONSTRAINT instruction_discovery_finding_count_bounded CHECK (((finding_count >= 0) AND (finding_count <= 4096))),
    CONSTRAINT instruction_discovery_limit_version_v1 CHECK ((limit_set_version = 1)),
    CONSTRAINT instruction_discovery_source_bytes_bounded CHECK (((candidate_source_byte_count >= 0) AND (candidate_source_byte_count <= 67108864)))
);


--
-- Name: instruction_discovery_candidate; Type: TABLE; Schema: public
--

CREATE TABLE instruction_discovery_candidate (
    instruction_discovery_id uuid CONSTRAINT instruction_discovery_candida_instruction_discovery_id_not_null NOT NULL,
    candidate_ordinal bigint NOT NULL,
    instruction_bundle_id uuid NOT NULL,
    CONSTRAINT instruction_discovery_candidate_ordinal_positive CHECK ((candidate_ordinal > 0))
);


--
-- Name: instruction_discovery_finding; Type: TABLE; Schema: public
--

CREATE TABLE instruction_discovery_finding (
    instruction_discovery_id uuid NOT NULL,
    finding_ordinal bigint NOT NULL,
    source_path text NOT NULL,
    finding_kind text NOT NULL,
    CONSTRAINT instruction_discovery_finding_kind_closed CHECK ((finding_kind = ANY (ARRAY['root_unavailable'::text, 'entry_unreadable'::text, 'non_utf8_source_path'::text, 'non_utf8_source'::text, 'invalid_skill'::text, 'limit_classified_entries'::text, 'limit_findings'::text, 'limit_candidate_source_bytes'::text, 'limit_elapsed_time'::text]))),
    CONSTRAINT instruction_discovery_finding_ordinal_positive CHECK ((finding_ordinal > 0)),
    CONSTRAINT instruction_discovery_finding_path_bounded CHECK (((octet_length(source_path) BETWEEN 2 AND 4096) AND (source_path ~~ '/%'::text) AND (source_path !~ '(^|/)(\.|\.\.)($|/)'::text) AND (source_path !~ '//'::text) AND ("right"(source_path, 1) <> '/'::text)))
);


--
-- Name: instruction_discovery_root; Type: TABLE; Schema: public
--

CREATE TABLE instruction_discovery_root (
    instruction_discovery_id uuid NOT NULL,
    root_ordinal bigint NOT NULL,
    root_kind text NOT NULL,
    root_path text NOT NULL,
    CONSTRAINT instruction_discovery_root_kind_closed CHECK ((root_kind = ANY (ARRAY['workspace'::text, 'configured'::text]))),
    CONSTRAINT instruction_discovery_root_ordinal_positive CHECK ((root_ordinal > 0)),
    CONSTRAINT instruction_discovery_root_path_bounded CHECK (((octet_length(root_path) BETWEEN 2 AND 4096) AND (root_path ~~ '/%'::text) AND (root_path !~ '(^|/)(\.|\.\.)($|/)'::text) AND (root_path !~ '//'::text) AND ("right"(root_path, 1) <> '/'::text)))
);


--
-- Name: registered_instruction_bundle; Type: TABLE; Schema: public
--

CREATE TABLE registered_instruction_bundle (
    instruction_bundle_id uuid NOT NULL,
    root_kind text NOT NULL,
    root_path text NOT NULL,
    source_path text NOT NULL,
    bundle_kind text NOT NULL,
    skill_name text,
    skill_description text,
    source_byte_length numeric NOT NULL,
    source_hash_algorithm text NOT NULL,
    source_hash bytea NOT NULL,
    CONSTRAINT registered_instruction_bundle_description_bounded CHECK (((skill_description IS NULL) OR ((char_length(skill_description) >= 1) AND (char_length(skill_description) <= 1024)))),
    CONSTRAINT registered_instruction_bundle_hash_shape CHECK (((source_hash_algorithm = 'sha256_v1'::text) AND (octet_length(source_hash) = 32))),
    CONSTRAINT registered_instruction_bundle_kind_closed CHECK ((bundle_kind = ANY (ARRAY['agent_document'::text, 'agent_skill'::text]))),
    CONSTRAINT registered_instruction_bundle_root_kind_closed CHECK ((root_kind = ANY (ARRAY['workspace'::text, 'configured'::text]))),
    CONSTRAINT registered_instruction_bundle_root_path_bounded CHECK (((octet_length(root_path) BETWEEN 2 AND 4096) AND (root_path ~~ '/%'::text) AND (root_path !~ '(^|/)(\.|\.\.)($|/)'::text) AND (root_path !~ '//'::text) AND ("right"(root_path, 1) <> '/'::text))),
    CONSTRAINT registered_instruction_bundle_skill_name_valid CHECK (((skill_name IS NULL) OR ((octet_length(skill_name) BETWEEN 1 AND 64) AND (skill_name ~ '^[a-z0-9]+(-[a-z0-9]+)*$'::text)))),
    CONSTRAINT registered_instruction_bundle_skill_shape CHECK ((((bundle_kind = 'agent_document'::text) AND (skill_name IS NULL) AND (skill_description IS NULL)) OR ((bundle_kind = 'agent_skill'::text) AND (skill_name IS NOT NULL) AND (skill_description IS NOT NULL)))),
    CONSTRAINT registered_instruction_bundle_source_kind_shape CHECK ((((bundle_kind = 'agent_document'::text) AND ("right"(source_path, 10) = '/AGENTS.md'::text)) OR ((bundle_kind = 'agent_skill'::text) AND ("right"(source_path, (char_length(skill_name) + 10)) = (('/'::text || skill_name) || '/SKILL.md'::text))))),
    CONSTRAINT registered_instruction_bundle_source_length_u64 CHECK (((source_byte_length = trunc(source_byte_length)) AND ((source_byte_length >= (0)::numeric) AND (source_byte_length <= '18446744073709551615'::numeric)))),
    CONSTRAINT registered_instruction_bundle_source_path_bounded CHECK (((octet_length(source_path) BETWEEN 2 AND 8193) AND (((octet_length(source_path) - octet_length(root_path)) - 1) BETWEEN 1 AND 4096) AND (source_path ~~ '/%'::text) AND (source_path !~ '(^|/)(\.|\.\.)($|/)'::text) AND (source_path !~ '//'::text) AND ("right"(source_path, 1) <> '/'::text) AND ("left"(source_path, (char_length(root_path) + 1)) = (root_path || '/'::text))))
);


--
-- Name: turn_instruction_manifest; Type: TABLE; Schema: public
--

CREATE TABLE turn_instruction_manifest (
    turn_instruction_manifest_id uuid NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    instruction_discovery_id uuid NOT NULL,
    boundary_kind text NOT NULL,
    eligibility_hash_algorithm text NOT NULL,
    eligibility_hash bytea NOT NULL,
    admitted_set_hash_algorithm text NOT NULL,
    admitted_set_hash bytea NOT NULL,
    manifest_hash_algorithm text NOT NULL,
    manifest_hash bytea NOT NULL,
    CONSTRAINT turn_instruction_manifest_boundary_closed CHECK ((boundary_kind = 'turn_start'::text)),
    CONSTRAINT turn_instruction_manifest_hash_shape CHECK (((eligibility_hash_algorithm = 'sha256_v1'::text) AND (octet_length(eligibility_hash) = 32) AND (admitted_set_hash_algorithm = 'sha256_v1'::text) AND (octet_length(admitted_set_hash) = 32) AND (manifest_hash_algorithm = 'sha256_v1'::text) AND (octet_length(manifest_hash) = 32)))
);


--
-- Name: workspace; Type: TABLE; Schema: public
--

CREATE TABLE workspace (
    workspace_id uuid NOT NULL,
    root_path text NOT NULL,
    origin text NOT NULL,
    command_id uuid,
    command_kind text,
    storage_version smallint,
    registered_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    CONSTRAINT workspace_command_kind_closed CHECK (((command_kind IS NULL) OR (command_kind = 'register_workspace'::text))),
    CONSTRAINT workspace_command_matches_origin CHECK ((((origin = 'operator_registered'::text) AND (command_id IS NOT NULL) AND (command_kind IS NOT NULL) AND (storage_version IS NOT NULL)) OR ((origin = 'daemon_derived'::text) AND (command_id IS NULL) AND (command_kind IS NULL) AND (storage_version IS NULL)))),
    CONSTRAINT workspace_origin_closed CHECK ((origin = ANY (ARRAY['operator_registered'::text, 'daemon_derived'::text]))),
    CONSTRAINT workspace_root_path_valid CHECK (workspace_root_path_is_valid(root_path)),
    CONSTRAINT workspace_storage_version_closed CHECK (((storage_version IS NULL) OR (storage_version = 1)))
);


--
-- Constraints.
--

--
-- Name: configured_git_remote_live configured_git_remote_live_mint_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY configured_git_remote_live
    ADD CONSTRAINT configured_git_remote_live_mint_key UNIQUE (mint_id);


--
-- Name: configured_git_remote_live configured_git_remote_live_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY configured_git_remote_live
    ADD CONSTRAINT configured_git_remote_live_pk PRIMARY KEY (workspace_id, remote_name) DEFERRABLE INITIALLY DEFERRED;


--
-- Name: configured_git_remote_mint configured_git_remote_mint_command_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY configured_git_remote_mint
    ADD CONSTRAINT configured_git_remote_mint_command_key UNIQUE (command_id);


--
-- Name: configured_git_remote_mint configured_git_remote_mint_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY configured_git_remote_mint
    ADD CONSTRAINT configured_git_remote_mint_pkey PRIMARY KEY (mint_id);


--
-- Name: configured_git_remote_withdrawal configured_git_remote_withdrawal_command_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY configured_git_remote_withdrawal
    ADD CONSTRAINT configured_git_remote_withdrawal_command_key UNIQUE (command_id);


--
-- Name: configured_git_remote_withdrawal configured_git_remote_withdrawal_mint_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY configured_git_remote_withdrawal
    ADD CONSTRAINT configured_git_remote_withdrawal_mint_key UNIQUE (mint_id);


--
-- Name: configured_git_remote_withdrawal configured_git_remote_withdrawal_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY configured_git_remote_withdrawal
    ADD CONSTRAINT configured_git_remote_withdrawal_pkey PRIMARY KEY (withdrawal_id);


--
-- Name: instruction_discovery_candidate instruction_discovery_candidate_once; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY instruction_discovery_candidate
    ADD CONSTRAINT instruction_discovery_candidate_once UNIQUE (instruction_discovery_id, instruction_bundle_id);


--
-- Name: instruction_discovery_candidate instruction_discovery_candidate_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY instruction_discovery_candidate
    ADD CONSTRAINT instruction_discovery_candidate_pk PRIMARY KEY (instruction_discovery_id, candidate_ordinal);


--
-- Name: instruction_discovery instruction_discovery_correlation_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY instruction_discovery
    ADD CONSTRAINT instruction_discovery_correlation_key UNIQUE (instruction_discovery_id, session_id, turn_id);


--
-- Name: instruction_discovery_finding instruction_discovery_finding_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY instruction_discovery_finding
    ADD CONSTRAINT instruction_discovery_finding_pk PRIMARY KEY (instruction_discovery_id, finding_ordinal);


--
-- Name: instruction_discovery instruction_discovery_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY instruction_discovery
    ADD CONSTRAINT instruction_discovery_pkey PRIMARY KEY (instruction_discovery_id);


--
-- Name: instruction_discovery_root instruction_discovery_root_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY instruction_discovery_root
    ADD CONSTRAINT instruction_discovery_root_pk PRIMARY KEY (instruction_discovery_id, root_ordinal);


--
-- Name: registered_instruction_bundle registered_instruction_bundle_evidence_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY registered_instruction_bundle
    ADD CONSTRAINT registered_instruction_bundle_evidence_key UNIQUE (root_kind, root_path, source_path, source_hash);


--
-- Name: registered_instruction_bundle registered_instruction_bundle_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY registered_instruction_bundle
    ADD CONSTRAINT registered_instruction_bundle_pkey PRIMARY KEY (instruction_bundle_id);


--
-- Name: turn_instruction_manifest turn_instruction_manifest_correlation_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_instruction_manifest
    ADD CONSTRAINT turn_instruction_manifest_correlation_key UNIQUE (turn_instruction_manifest_id, session_id, turn_id);


--
-- Name: turn_instruction_manifest turn_instruction_manifest_instruction_discovery_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_instruction_manifest
    ADD CONSTRAINT turn_instruction_manifest_instruction_discovery_id_key UNIQUE (instruction_discovery_id);


--
-- Name: turn_instruction_manifest turn_instruction_manifest_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_instruction_manifest
    ADD CONSTRAINT turn_instruction_manifest_pkey PRIMARY KEY (turn_instruction_manifest_id);


--
-- Name: turn_instruction_manifest turn_instruction_manifest_turn_boundary_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_instruction_manifest
    ADD CONSTRAINT turn_instruction_manifest_turn_boundary_key UNIQUE (session_id, turn_id, boundary_kind);


--
-- Name: workspace workspace_command_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY workspace
    ADD CONSTRAINT workspace_command_key UNIQUE (command_id);


--
-- Name: workspace workspace_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY workspace
    ADD CONSTRAINT workspace_pkey PRIMARY KEY (workspace_id);


--
-- Name: workspace workspace_root_path_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY workspace
    ADD CONSTRAINT workspace_root_path_key UNIQUE (root_path);


--
-- Indexes.
--

--
-- Name: configured_git_remote_mint_workspace_name; Type: INDEX; Schema: public
--

CREATE INDEX configured_git_remote_mint_workspace_name ON configured_git_remote_mint USING btree (workspace_id, remote_name);


--
-- Triggers.
--

--
-- Name: configured_git_remote_live configured_git_remote_live_is_never_updated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER configured_git_remote_live_is_never_updated BEFORE UPDATE ON configured_git_remote_live FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: configured_git_remote_live configured_git_remote_live_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER configured_git_remote_live_rejects_truncate BEFORE TRUNCATE ON configured_git_remote_live FOR EACH STATEMENT EXECUTE FUNCTION reject_workspace_authority_table_truncate();


--
-- Name: configured_git_remote_live configured_git_remote_live_stays_derived; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER configured_git_remote_live_stays_derived AFTER INSERT OR DELETE ON configured_git_remote_live DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION configured_git_remote_live_agrees_with_the_facts();


--
-- Name: configured_git_remote_mint configured_git_remote_mint_becomes_live; Type: TRIGGER; Schema: public
--

CREATE TRIGGER configured_git_remote_mint_becomes_live AFTER INSERT ON configured_git_remote_mint FOR EACH ROW EXECUTE FUNCTION record_configured_git_remote_as_live();


--
-- Name: configured_git_remote_mint configured_git_remote_mint_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER configured_git_remote_mint_is_append_only BEFORE DELETE OR UPDATE ON configured_git_remote_mint FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: configured_git_remote_mint configured_git_remote_mint_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER configured_git_remote_mint_rejects_truncate BEFORE TRUNCATE ON configured_git_remote_mint FOR EACH STATEMENT EXECUTE FUNCTION reject_workspace_authority_table_truncate();


--
-- Name: configured_git_remote_mint configured_git_remote_mint_stands_live; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER configured_git_remote_mint_stands_live AFTER INSERT ON configured_git_remote_mint DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION configured_git_remote_live_agrees_with_the_facts();


--
-- Name: configured_git_remote_withdrawal configured_git_remote_withdrawal_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER configured_git_remote_withdrawal_is_append_only BEFORE DELETE OR UPDATE ON configured_git_remote_withdrawal FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: configured_git_remote_withdrawal configured_git_remote_withdrawal_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER configured_git_remote_withdrawal_rejects_truncate BEFORE TRUNCATE ON configured_git_remote_withdrawal FOR EACH STATEMENT EXECUTE FUNCTION reject_workspace_authority_table_truncate();


--
-- Name: configured_git_remote_withdrawal configured_git_remote_withdrawal_retires_the_mint; Type: TRIGGER; Schema: public
--

CREATE TRIGGER configured_git_remote_withdrawal_retires_the_mint AFTER INSERT ON configured_git_remote_withdrawal FOR EACH ROW EXECUTE FUNCTION retire_configured_git_remote_from_live();


--
-- Name: configured_git_remote_withdrawal configured_git_remote_withdrawal_stands_down; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER configured_git_remote_withdrawal_stands_down AFTER INSERT ON configured_git_remote_withdrawal DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION configured_git_remote_live_agrees_with_the_facts();


--
-- Name: instruction_discovery_candidate instruction_discovery_candidate_insert_before_seal; Type: TRIGGER; Schema: public
--

CREATE TRIGGER instruction_discovery_candidate_insert_before_seal BEFORE INSERT ON instruction_discovery_candidate FOR EACH ROW EXECUTE FUNCTION reject_instruction_discovery_child_after_seal();


--
-- Name: instruction_discovery_candidate instruction_discovery_candidate_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER instruction_discovery_candidate_is_append_only BEFORE DELETE OR UPDATE ON instruction_discovery_candidate FOR EACH ROW EXECUTE FUNCTION reject_instruction_evidence_change();


--
-- Name: instruction_discovery_candidate instruction_discovery_candidate_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER instruction_discovery_candidate_rejects_truncate BEFORE TRUNCATE ON instruction_discovery_candidate FOR EACH STATEMENT EXECUTE FUNCTION reject_instruction_evidence_change();


--
-- Name: instruction_discovery_finding instruction_discovery_finding_insert_before_seal; Type: TRIGGER; Schema: public
--

CREATE TRIGGER instruction_discovery_finding_insert_before_seal BEFORE INSERT ON instruction_discovery_finding FOR EACH ROW EXECUTE FUNCTION reject_instruction_discovery_child_after_seal();


--
-- Name: instruction_discovery_finding instruction_discovery_finding_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER instruction_discovery_finding_is_append_only BEFORE DELETE OR UPDATE ON instruction_discovery_finding FOR EACH ROW EXECUTE FUNCTION reject_instruction_evidence_change();


--
-- Name: instruction_discovery_finding instruction_discovery_finding_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER instruction_discovery_finding_rejects_truncate BEFORE TRUNCATE ON instruction_discovery_finding FOR EACH STATEMENT EXECUTE FUNCTION reject_instruction_evidence_change();


--
-- Name: instruction_discovery instruction_discovery_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER instruction_discovery_is_append_only BEFORE DELETE OR UPDATE ON instruction_discovery FOR EACH ROW EXECUTE FUNCTION reject_instruction_evidence_change();


--
-- Name: instruction_discovery instruction_discovery_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER instruction_discovery_rejects_truncate BEFORE TRUNCATE ON instruction_discovery FOR EACH STATEMENT EXECUTE FUNCTION reject_instruction_evidence_change();


--
-- Name: instruction_discovery_root instruction_discovery_root_insert_before_seal; Type: TRIGGER; Schema: public
--

CREATE TRIGGER instruction_discovery_root_insert_before_seal BEFORE INSERT ON instruction_discovery_root FOR EACH ROW EXECUTE FUNCTION reject_instruction_discovery_child_after_seal();


--
-- Name: instruction_discovery_root instruction_discovery_root_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER instruction_discovery_root_is_append_only BEFORE DELETE OR UPDATE ON instruction_discovery_root FOR EACH ROW EXECUTE FUNCTION reject_instruction_evidence_change();


--
-- Name: instruction_discovery_root instruction_discovery_root_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER instruction_discovery_root_rejects_truncate BEFORE TRUNCATE ON instruction_discovery_root FOR EACH STATEMENT EXECUTE FUNCTION reject_instruction_evidence_change();


--
-- Name: instruction_discovery instruction_discovery_validates_before_seal; Type: TRIGGER; Schema: public
--

CREATE TRIGGER instruction_discovery_validates_before_seal BEFORE INSERT ON instruction_discovery FOR EACH ROW EXECUTE FUNCTION validate_instruction_discovery_seal();


--
-- Name: registered_instruction_bundle registered_instruction_bundle_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER registered_instruction_bundle_is_append_only BEFORE DELETE OR UPDATE ON registered_instruction_bundle FOR EACH ROW EXECUTE FUNCTION reject_instruction_evidence_change();


--
-- Name: registered_instruction_bundle registered_instruction_bundle_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER registered_instruction_bundle_rejects_truncate BEFORE TRUNCATE ON registered_instruction_bundle FOR EACH STATEMENT EXECUTE FUNCTION reject_instruction_evidence_change();


--
-- Name: turn_instruction_manifest turn_instruction_manifest_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_instruction_manifest_is_append_only BEFORE DELETE OR UPDATE ON turn_instruction_manifest FOR EACH ROW EXECUTE FUNCTION reject_instruction_evidence_change();


--
-- Name: turn_instruction_manifest turn_instruction_manifest_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_instruction_manifest_rejects_truncate BEFORE TRUNCATE ON turn_instruction_manifest FOR EACH STATEMENT EXECUTE FUNCTION reject_instruction_evidence_change();


--
-- Name: turn_instruction_manifest turn_instruction_manifest_validates_discovery; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_instruction_manifest_validates_discovery BEFORE INSERT ON turn_instruction_manifest FOR EACH ROW EXECUTE FUNCTION validate_turn_instruction_manifest_discovery();


--
-- Name: workspace workspace_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER workspace_is_append_only BEFORE DELETE OR UPDATE ON workspace FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: workspace workspace_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER workspace_rejects_truncate BEFORE TRUNCATE ON workspace FOR EACH STATEMENT EXECUTE FUNCTION reject_workspace_authority_table_truncate();


--
-- Foreign keys.
--

--
-- Name: configured_git_remote_live configured_git_remote_live_mint_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY configured_git_remote_live
    ADD CONSTRAINT configured_git_remote_live_mint_fk FOREIGN KEY (mint_id) REFERENCES configured_git_remote_mint(mint_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: configured_git_remote_mint configured_git_remote_mint_command_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY configured_git_remote_mint
    ADD CONSTRAINT configured_git_remote_mint_command_fk FOREIGN KEY (command_id, command_kind, storage_version) REFERENCES durable_command(command_id, command_kind, storage_version) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: configured_git_remote_mint configured_git_remote_mint_workspace_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY configured_git_remote_mint
    ADD CONSTRAINT configured_git_remote_mint_workspace_fk FOREIGN KEY (workspace_id) REFERENCES workspace(workspace_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: configured_git_remote_withdrawal configured_git_remote_withdrawal_command_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY configured_git_remote_withdrawal
    ADD CONSTRAINT configured_git_remote_withdrawal_command_fk FOREIGN KEY (command_id, command_kind, storage_version) REFERENCES durable_command(command_id, command_kind, storage_version) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: configured_git_remote_withdrawal configured_git_remote_withdrawal_mint_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY configured_git_remote_withdrawal
    ADD CONSTRAINT configured_git_remote_withdrawal_mint_fk FOREIGN KEY (mint_id) REFERENCES configured_git_remote_mint(mint_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: instruction_discovery_candidate instruction_discovery_candidate_bundle_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY instruction_discovery_candidate
    ADD CONSTRAINT instruction_discovery_candidate_bundle_fk FOREIGN KEY (instruction_bundle_id) REFERENCES registered_instruction_bundle(instruction_bundle_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: instruction_discovery_candidate instruction_discovery_candidate_scan_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY instruction_discovery_candidate
    ADD CONSTRAINT instruction_discovery_candidate_scan_fk FOREIGN KEY (instruction_discovery_id) REFERENCES instruction_discovery(instruction_discovery_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: instruction_discovery_finding instruction_discovery_finding_scan_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY instruction_discovery_finding
    ADD CONSTRAINT instruction_discovery_finding_scan_fk FOREIGN KEY (instruction_discovery_id) REFERENCES instruction_discovery(instruction_discovery_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: instruction_discovery_root instruction_discovery_root_scan_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY instruction_discovery_root
    ADD CONSTRAINT instruction_discovery_root_scan_fk FOREIGN KEY (instruction_discovery_id) REFERENCES instruction_discovery(instruction_discovery_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: instruction_discovery instruction_discovery_turn_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY instruction_discovery
    ADD CONSTRAINT instruction_discovery_turn_fk FOREIGN KEY (turn_id, session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: turn_instruction_manifest turn_instruction_manifest_discovery_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_instruction_manifest
    ADD CONSTRAINT turn_instruction_manifest_discovery_fk FOREIGN KEY (instruction_discovery_id, session_id, turn_id) REFERENCES instruction_discovery(instruction_discovery_id, session_id, turn_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: turn_instruction_manifest turn_instruction_manifest_turn_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_instruction_manifest
    ADD CONSTRAINT turn_instruction_manifest_turn_fk FOREIGN KEY (turn_id, session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: workspace workspace_command_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY workspace
    ADD CONSTRAINT workspace_command_fk FOREIGN KEY (command_id, command_kind, storage_version) REFERENCES durable_command(command_id, command_kind, storage_version) MATCH FULL ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;

--
-- Search-path pins for this file's constraint-reachable functions.
--
-- The pin has to name the schema the migration selected rather than a
-- literal, so it is applied here through current_schema instead of inline
-- in each CREATE FUNCTION (the full rationale is in 202609010000_core.sql;
-- crates/persistence/tests/search_path_postgres.rs is the guard).
--

DO $search_path_pins$
DECLARE
    signature text;
BEGIN
    -- the canonical restore-safe pin: the migration-selected schema, then pg_catalog, then pg_temp
    FOREACH signature IN ARRAY ARRAY[
        'configured_git_remote_name_is_valid(text)',
        'configured_git_remote_url_is_valid(text)',
        'workspace_root_path_is_valid(text)'
    ] LOOP
        EXECUTE format('ALTER FUNCTION %s SET search_path TO %I, pg_catalog, pg_temp',
                   signature, current_schema);
    END LOOP;
END
$search_path_pins$;


--
-- Restore body validation for anything applied after this file.
--

RESET check_function_bodies;
