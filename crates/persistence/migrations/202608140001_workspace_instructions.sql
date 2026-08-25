-- Daemon-owned workspace instruction discovery, registration, and exact
-- empty turn-start provenance. No row admits content in this slice.

CREATE TABLE instruction_discovery (
    instruction_discovery_id uuid PRIMARY KEY,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    limit_set_version smallint NOT NULL,
    classified_entry_count bigint NOT NULL,
    finding_count bigint NOT NULL,
    candidate_source_byte_count bigint NOT NULL,
    elapsed_millis bigint NOT NULL,
    scan_complete boolean NOT NULL,

    CONSTRAINT instruction_discovery_limit_version_v1
        CHECK (limit_set_version = 1),
    CONSTRAINT instruction_discovery_entry_count_bounded
        CHECK (classified_entry_count BETWEEN 0 AND 100000),
    CONSTRAINT instruction_discovery_finding_count_bounded
        CHECK (finding_count BETWEEN 0 AND 4096),
    CONSTRAINT instruction_discovery_source_bytes_bounded
        CHECK (candidate_source_byte_count BETWEEN 0 AND 67108864),
    CONSTRAINT instruction_discovery_elapsed_nonnegative
        CHECK (elapsed_millis >= 0),
    CONSTRAINT instruction_discovery_correlation_key
        UNIQUE (instruction_discovery_id, session_id, turn_id),

    CONSTRAINT instruction_discovery_turn_fk
        FOREIGN KEY (turn_id, session_id)
        REFERENCES turn_lifecycle (turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

CREATE TABLE instruction_discovery_root (
    instruction_discovery_id uuid NOT NULL,
    root_ordinal bigint NOT NULL,
    root_kind text NOT NULL,
    root_path text NOT NULL,

    CONSTRAINT instruction_discovery_root_pk
        PRIMARY KEY (instruction_discovery_id, root_ordinal),
    CONSTRAINT instruction_discovery_root_ordinal_positive
        CHECK (root_ordinal > 0),
    CONSTRAINT instruction_discovery_root_kind_closed
        CHECK (root_kind IN ('workspace', 'configured')),
    CONSTRAINT instruction_discovery_root_path_bounded
        CHECK (
            octet_length(root_path) BETWEEN 2 AND 4096
            AND root_path LIKE '/%'
            AND root_path !~ '(^|/)(\.|\.\.)($|/)'
            AND root_path !~ '//'
            AND right(root_path, 1) <> '/'
        ),
    CONSTRAINT instruction_discovery_root_scan_fk
        FOREIGN KEY (instruction_discovery_id)
        REFERENCES instruction_discovery (instruction_discovery_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE registered_instruction_bundle (
    instruction_bundle_id uuid PRIMARY KEY,
    root_kind text NOT NULL,
    root_path text NOT NULL,
    source_path text NOT NULL,
    bundle_kind text NOT NULL,
    skill_name text,
    skill_description text,
    source_byte_length numeric NOT NULL,
    source_hash_algorithm text NOT NULL,
    source_hash bytea NOT NULL,

    CONSTRAINT registered_instruction_bundle_evidence_key
        UNIQUE (root_kind, root_path, source_path, source_hash),
    CONSTRAINT registered_instruction_bundle_root_kind_closed
        CHECK (root_kind IN ('workspace', 'configured')),
    CONSTRAINT registered_instruction_bundle_kind_closed
        CHECK (bundle_kind IN ('agent_document', 'agent_skill')),
    CONSTRAINT registered_instruction_bundle_skill_shape
        CHECK (
            (bundle_kind = 'agent_document' AND skill_name IS NULL AND skill_description IS NULL)
            OR
            (bundle_kind = 'agent_skill' AND skill_name IS NOT NULL AND skill_description IS NOT NULL)
        ),
    CONSTRAINT registered_instruction_bundle_source_kind_shape
        CHECK (
            (bundle_kind = 'agent_document' AND right(source_path, 10) = '/AGENTS.md')
            OR
            (
                bundle_kind = 'agent_skill'
                AND right(source_path, char_length(skill_name) + 10)
                    = '/' || skill_name || '/SKILL.md'
            )
        ),
    CONSTRAINT registered_instruction_bundle_skill_name_valid
        CHECK (
            skill_name IS NULL
            OR (
                octet_length(skill_name) BETWEEN 1 AND 64
                AND skill_name ~ '^[a-z0-9]+(-[a-z0-9]+)*$'
            )
        ),
    CONSTRAINT registered_instruction_bundle_description_bounded
        CHECK (skill_description IS NULL OR char_length(skill_description) BETWEEN 1 AND 1024),
    CONSTRAINT registered_instruction_bundle_source_length_u64
        CHECK (
            source_byte_length = trunc(source_byte_length)
            AND source_byte_length BETWEEN 0 AND 18446744073709551615
        ),
    CONSTRAINT registered_instruction_bundle_hash_shape
        CHECK (source_hash_algorithm = 'sha256_v1' AND octet_length(source_hash) = 32),
    CONSTRAINT registered_instruction_bundle_root_path_bounded
        CHECK (
            octet_length(root_path) BETWEEN 2 AND 4096
            AND root_path LIKE '/%'
            AND root_path !~ '(^|/)(\.|\.\.)($|/)'
            AND root_path !~ '//'
            AND right(root_path, 1) <> '/'
        ),
    CONSTRAINT registered_instruction_bundle_source_path_bounded
        CHECK (
            octet_length(source_path) BETWEEN 2 AND 8193
            AND octet_length(source_path) - octet_length(root_path) - 1
                BETWEEN 1 AND 4096
            AND source_path LIKE '/%'
            AND source_path !~ '(^|/)(\.|\.\.)($|/)'
            AND source_path !~ '//'
            AND right(source_path, 1) <> '/'
            AND left(source_path, char_length(root_path) + 1) = root_path || '/'
        )
);

CREATE TABLE instruction_discovery_candidate (
    instruction_discovery_id uuid NOT NULL,
    candidate_ordinal bigint NOT NULL,
    instruction_bundle_id uuid NOT NULL,

    CONSTRAINT instruction_discovery_candidate_pk
        PRIMARY KEY (instruction_discovery_id, candidate_ordinal),
    CONSTRAINT instruction_discovery_candidate_once
        UNIQUE (instruction_discovery_id, instruction_bundle_id),
    CONSTRAINT instruction_discovery_candidate_ordinal_positive
        CHECK (candidate_ordinal > 0),
    CONSTRAINT instruction_discovery_candidate_scan_fk
        FOREIGN KEY (instruction_discovery_id)
        REFERENCES instruction_discovery (instruction_discovery_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT instruction_discovery_candidate_bundle_fk
        FOREIGN KEY (instruction_bundle_id)
        REFERENCES registered_instruction_bundle (instruction_bundle_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

CREATE TABLE instruction_discovery_finding (
    instruction_discovery_id uuid NOT NULL,
    finding_ordinal bigint NOT NULL,
    source_path text NOT NULL,
    finding_kind text NOT NULL,

    CONSTRAINT instruction_discovery_finding_pk
        PRIMARY KEY (instruction_discovery_id, finding_ordinal),
    CONSTRAINT instruction_discovery_finding_ordinal_positive
        CHECK (finding_ordinal > 0),
    CONSTRAINT instruction_discovery_finding_kind_closed
        CHECK (finding_kind IN (
            'root_unavailable',
            'entry_unreadable',
            'non_utf8_source_path',
            'non_utf8_source',
            'invalid_skill',
            'limit_classified_entries',
            'limit_findings',
            'limit_candidate_source_bytes',
            'limit_elapsed_time'
        )),
    CONSTRAINT instruction_discovery_finding_path_bounded
        CHECK (
            octet_length(source_path) BETWEEN 2 AND 4096
            AND source_path LIKE '/%'
            AND source_path !~ '(^|/)(\.|\.\.)($|/)'
            AND source_path !~ '//'
            AND right(source_path, 1) <> '/'
        ),
    CONSTRAINT instruction_discovery_finding_scan_fk
        FOREIGN KEY (instruction_discovery_id)
        REFERENCES instruction_discovery (instruction_discovery_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE turn_instruction_manifest (
    turn_instruction_manifest_id uuid PRIMARY KEY,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    instruction_discovery_id uuid NOT NULL UNIQUE,
    boundary_kind text NOT NULL,
    eligibility_hash_algorithm text NOT NULL,
    eligibility_hash bytea NOT NULL,
    admitted_set_hash_algorithm text NOT NULL,
    admitted_set_hash bytea NOT NULL,
    manifest_hash_algorithm text NOT NULL,
    manifest_hash bytea NOT NULL,

    CONSTRAINT turn_instruction_manifest_turn_boundary_key
        UNIQUE (session_id, turn_id, boundary_kind),
    CONSTRAINT turn_instruction_manifest_correlation_key
        UNIQUE (turn_instruction_manifest_id, session_id, turn_id),
    CONSTRAINT turn_instruction_manifest_boundary_closed
        CHECK (boundary_kind = 'turn_start'),
    CONSTRAINT turn_instruction_manifest_hash_shape
        CHECK (
            eligibility_hash_algorithm = 'sha256_v1'
            AND octet_length(eligibility_hash) = 32
            AND admitted_set_hash_algorithm = 'sha256_v1'
            AND octet_length(admitted_set_hash) = 32
            AND manifest_hash_algorithm = 'sha256_v1'
            AND octet_length(manifest_hash) = 32
        ),
    CONSTRAINT turn_instruction_manifest_turn_fk
        FOREIGN KEY (turn_id, session_id)
        REFERENCES turn_lifecycle (turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT turn_instruction_manifest_discovery_fk
        FOREIGN KEY (instruction_discovery_id, session_id, turn_id)
        REFERENCES instruction_discovery (instruction_discovery_id, session_id, turn_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

CREATE FUNCTION reject_instruction_evidence_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'workspace instruction evidence is append-only'
        USING ERRCODE = '23514';
END;
$$;

CREATE FUNCTION reject_instruction_discovery_child_after_seal()
RETURNS trigger
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

CREATE FUNCTION validate_instruction_discovery_seal()
RETURNS trigger
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

CREATE FUNCTION validate_turn_instruction_manifest_discovery()
RETURNS trigger
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

CREATE TRIGGER instruction_discovery_validates_before_seal
BEFORE INSERT ON instruction_discovery
FOR EACH ROW EXECUTE FUNCTION validate_instruction_discovery_seal();

CREATE TRIGGER turn_instruction_manifest_validates_discovery
BEFORE INSERT ON turn_instruction_manifest
FOR EACH ROW EXECUTE FUNCTION validate_turn_instruction_manifest_discovery();

CREATE TRIGGER instruction_discovery_root_insert_before_seal
BEFORE INSERT ON instruction_discovery_root
FOR EACH ROW EXECUTE FUNCTION reject_instruction_discovery_child_after_seal();
CREATE TRIGGER instruction_discovery_candidate_insert_before_seal
BEFORE INSERT ON instruction_discovery_candidate
FOR EACH ROW EXECUTE FUNCTION reject_instruction_discovery_child_after_seal();
CREATE TRIGGER instruction_discovery_finding_insert_before_seal
BEFORE INSERT ON instruction_discovery_finding
FOR EACH ROW EXECUTE FUNCTION reject_instruction_discovery_child_after_seal();

CREATE TRIGGER instruction_discovery_is_append_only
BEFORE UPDATE OR DELETE ON instruction_discovery
FOR EACH ROW EXECUTE FUNCTION reject_instruction_evidence_change();
CREATE TRIGGER instruction_discovery_root_is_append_only
BEFORE UPDATE OR DELETE ON instruction_discovery_root
FOR EACH ROW EXECUTE FUNCTION reject_instruction_evidence_change();
CREATE TRIGGER registered_instruction_bundle_is_append_only
BEFORE UPDATE OR DELETE ON registered_instruction_bundle
FOR EACH ROW EXECUTE FUNCTION reject_instruction_evidence_change();
CREATE TRIGGER instruction_discovery_candidate_is_append_only
BEFORE UPDATE OR DELETE ON instruction_discovery_candidate
FOR EACH ROW EXECUTE FUNCTION reject_instruction_evidence_change();
CREATE TRIGGER instruction_discovery_finding_is_append_only
BEFORE UPDATE OR DELETE ON instruction_discovery_finding
FOR EACH ROW EXECUTE FUNCTION reject_instruction_evidence_change();
CREATE TRIGGER turn_instruction_manifest_is_append_only
BEFORE UPDATE OR DELETE ON turn_instruction_manifest
FOR EACH ROW EXECUTE FUNCTION reject_instruction_evidence_change();

CREATE TRIGGER instruction_discovery_rejects_truncate
BEFORE TRUNCATE ON instruction_discovery
FOR EACH STATEMENT EXECUTE FUNCTION reject_instruction_evidence_change();
CREATE TRIGGER instruction_discovery_root_rejects_truncate
BEFORE TRUNCATE ON instruction_discovery_root
FOR EACH STATEMENT EXECUTE FUNCTION reject_instruction_evidence_change();
CREATE TRIGGER registered_instruction_bundle_rejects_truncate
BEFORE TRUNCATE ON registered_instruction_bundle
FOR EACH STATEMENT EXECUTE FUNCTION reject_instruction_evidence_change();
CREATE TRIGGER instruction_discovery_candidate_rejects_truncate
BEFORE TRUNCATE ON instruction_discovery_candidate
FOR EACH STATEMENT EXECUTE FUNCTION reject_instruction_evidence_change();
CREATE TRIGGER instruction_discovery_finding_rejects_truncate
BEFORE TRUNCATE ON instruction_discovery_finding
FOR EACH STATEMENT EXECUTE FUNCTION reject_instruction_evidence_change();
CREATE TRIGGER turn_instruction_manifest_rejects_truncate
BEFORE TRUNCATE ON turn_instruction_manifest
FOR EACH STATEMENT EXECUTE FUNCTION reject_instruction_evidence_change();

-- Existing pre-alpha model calls ran with no instruction projection. Their
-- synthetic discoveries therefore have no roots, candidates, or findings and
-- are complete evidence for the empty manifest each already-prepared call
-- used. Queued turns and active turns without a call remain unbound so their
-- first call performs ordinary discovery after this migration.
INSERT INTO instruction_discovery (
    instruction_discovery_id,
    session_id,
    turn_id,
    limit_set_version,
    classified_entry_count,
    finding_count,
    candidate_source_byte_count,
    elapsed_millis,
    scan_complete
)
SELECT turn_id, session_id, turn_id, 1, 0, 0, 0, 0, true
  FROM turn_lifecycle AS lifecycle
 WHERE EXISTS (
           SELECT 1
             FROM model_call AS call
            WHERE call.session_id = lifecycle.session_id
              AND call.turn_id = lifecycle.turn_id
       );

INSERT INTO turn_instruction_manifest (
    turn_instruction_manifest_id,
    session_id,
    turn_id,
    instruction_discovery_id,
    boundary_kind,
    eligibility_hash_algorithm,
    eligibility_hash,
    admitted_set_hash_algorithm,
    admitted_set_hash,
    manifest_hash_algorithm,
    manifest_hash
)
SELECT
    turn_id,
    session_id,
    turn_id,
    turn_id,
    'turn_start',
    'sha256_v1',
    sha256(convert_to('signalbox-instruction-eligibility-v1', 'UTF8')),
    'sha256_v1',
    sha256(
        convert_to('signalbox-instruction-admitted-set-v1', 'UTF8')
        || '\x0000000000000000'::bytea
    ),
    'sha256_v1',
    sha256(
        convert_to('signalbox-turn-instruction-manifest-v1', 'UTF8')
        || uuid_send(session_id)
        || uuid_send(turn_id)
        || sha256(convert_to('signalbox-instruction-eligibility-v1', 'UTF8'))
        || sha256(
               convert_to('signalbox-instruction-admitted-set-v1', 'UTF8')
               || '\x0000000000000000'::bytea
           )
        || convert_to('turn_start', 'UTF8')
    )
  FROM turn_lifecycle AS lifecycle
 WHERE EXISTS (
           SELECT 1
             FROM model_call AS call
            WHERE call.session_id = lifecycle.session_id
              AND call.turn_id = lifecycle.turn_id
       );

ALTER TABLE model_call
    ADD COLUMN turn_instruction_manifest_id uuid;

-- This one-time write populates only the newly added correlation column. The
-- existing guards reject terminal-row updates and schedule final-state checks
-- for every update. Suspend only those three named guards for the bounded
-- correlation backfill and restore them before adding the immutable manifest
-- guard. No existing authorization or lifecycle fact changes here.
ALTER TABLE model_call
    DISABLE TRIGGER model_call_changes_are_guarded,
    DISABLE TRIGGER model_call_requires_complete_final_state,
    DISABLE TRIGGER model_call_requires_failed_terminal_execution;

UPDATE model_call
   SET turn_instruction_manifest_id = turn_id;

ALTER TABLE model_call
    ENABLE TRIGGER model_call_changes_are_guarded,
    ENABLE TRIGGER model_call_requires_complete_final_state,
    ENABLE TRIGGER model_call_requires_failed_terminal_execution;

ALTER TABLE model_call
    ALTER COLUMN turn_instruction_manifest_id SET NOT NULL,
    ADD CONSTRAINT model_call_instruction_manifest_fk
        FOREIGN KEY (turn_instruction_manifest_id, session_id, turn_id)
        REFERENCES turn_instruction_manifest (turn_instruction_manifest_id, session_id, turn_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

CREATE FUNCTION reject_model_call_instruction_manifest_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.turn_instruction_manifest_id IS DISTINCT FROM OLD.turn_instruction_manifest_id THEN
        RAISE EXCEPTION 'model call instruction manifest is immutable'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER model_call_instruction_manifest_is_immutable
BEFORE UPDATE ON model_call
FOR EACH ROW EXECUTE FUNCTION reject_model_call_instruction_manifest_change();
