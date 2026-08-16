-- Durable program-run frame journals and their per-run append allocators.

CREATE TABLE program_run_journal_stream (
    run_id uuid PRIMARY KEY,
    frame_contract_version bigint NOT NULL,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    CONSTRAINT program_run_journal_stream_contract_v1
        CHECK (frame_contract_version = 1)
);

CREATE TABLE program_run_journal_sequence_state (
    run_id uuid PRIMARY KEY,
    last_position numeric(20, 0) NOT NULL DEFAULT 0,
    last_request_ordinal numeric(20, 0) NOT NULL DEFAULT 0,
    last_delivery_ordinal numeric(20, 0) NOT NULL DEFAULT 0,

    CONSTRAINT program_run_journal_sequence_state_run_fk
        FOREIGN KEY (run_id)
        REFERENCES program_run_journal_stream (run_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT program_run_journal_sequence_state_ordinals_u64
        CHECK (
            last_position BETWEEN 0 AND 18446744073709551615
            AND last_request_ordinal BETWEEN 0 AND 18446744073709551615
            AND last_delivery_ordinal BETWEEN 0 AND 18446744073709551615
        )
);

CREATE TABLE program_run_journal_entry (
    run_id uuid NOT NULL,
    journal_position numeric(20, 0) NOT NULL,
    frame_direction text COLLATE "C" NOT NULL,
    frame_kind text COLLATE "C" NOT NULL,
    request_ordinal numeric(20, 0),
    delivery_ordinal numeric(20, 0),
    resolves_request_ordinal numeric(20, 0),
    request_scope_ordinal numeric(20, 0),
    scope_operation text COLLATE "C",
    declared_scope_ordinal numeric(20, 0),
    parent_scope_ordinal numeric(20, 0),
    effect_capability text COLLATE "C",
    effect_method text,
    reject_reason text COLLATE "C",
    fault_cause text COLLATE "C",
    payload_inline bytea NOT NULL,
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    CONSTRAINT program_run_journal_entry_pk
        PRIMARY KEY (run_id, journal_position),
    CONSTRAINT program_run_journal_entry_request_unique
        UNIQUE (run_id, request_ordinal),
    CONSTRAINT program_run_journal_entry_delivery_unique
        UNIQUE (run_id, delivery_ordinal),
    CONSTRAINT program_run_journal_entry_resolution_unique
        UNIQUE (run_id, resolves_request_ordinal),
    CONSTRAINT program_run_journal_entry_run_fk
        FOREIGN KEY (run_id)
        REFERENCES program_run_journal_stream (run_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT program_run_journal_entry_resolution_fk
        FOREIGN KEY (run_id, resolves_request_ordinal)
        REFERENCES program_run_journal_entry (run_id, request_ordinal)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT program_run_journal_entry_positive_ordinals
        CHECK (
            journal_position BETWEEN 1 AND 18446744073709551615
            AND (
                request_ordinal IS NULL
                OR request_ordinal BETWEEN 1 AND 18446744073709551615
            )
            AND (
                delivery_ordinal IS NULL
                OR delivery_ordinal BETWEEN 1 AND 18446744073709551615
            )
            AND (
                resolves_request_ordinal IS NULL
                OR resolves_request_ordinal BETWEEN 1 AND 18446744073709551615
            )
            AND (
                request_scope_ordinal IS NULL
                OR request_scope_ordinal BETWEEN 1 AND 18446744073709551615
            )
            AND (
                declared_scope_ordinal IS NULL
                OR declared_scope_ordinal BETWEEN 1 AND 18446744073709551615
            )
            AND (
                parent_scope_ordinal IS NULL
                OR parent_scope_ordinal BETWEEN 1 AND 18446744073709551615
            )
        ),
    CONSTRAINT program_run_journal_entry_direction_shape
        CHECK (
            (
                frame_direction = 'request'
                AND request_ordinal IS NOT NULL
                AND delivery_ordinal IS NULL
                AND frame_kind IN (
                    'now', 'random', 'sleep', 'await_event', 'effect', 'scope', 'terminal'
                )
            )
            OR
            (
                frame_direction = 'delivery'
                AND request_ordinal IS NULL
                AND delivery_ordinal IS NOT NULL
                AND request_scope_ordinal IS NULL
                AND frame_kind IN (
                    'answer', 'wake', 'reject', 'cancel', 'run_cancel', 'fault'
                )
            )
        ),
    CONSTRAINT program_run_journal_entry_resolution_shape
        CHECK (
            (frame_kind IN ('answer', 'wake', 'reject', 'cancel'))
            = (resolves_request_ordinal IS NOT NULL)
        ),
    CONSTRAINT program_run_journal_entry_scope_shape
        CHECK (
            (
                (
                    frame_kind = 'scope'
                    AND scope_operation IS NOT NULL
                    AND declared_scope_ordinal IS NOT NULL
                )
                OR
                (
                    frame_kind <> 'scope'
                    AND scope_operation IS NULL
                    AND declared_scope_ordinal IS NULL
                    AND parent_scope_ordinal IS NULL
                )
            )
            AND (scope_operation IS NULL OR scope_operation IN ('open', 'close'))
        ),
    CONSTRAINT program_run_journal_entry_effect_shape
        CHECK (
            (
                (
                    frame_kind = 'effect'
                    AND effect_capability IS NOT NULL
                    AND effect_method IS NOT NULL
                )
                OR
                (
                    frame_kind <> 'effect'
                    AND effect_capability IS NULL
                    AND effect_method IS NULL
                )
            )
            AND (
                effect_capability IS NULL
                OR effect_capability IN (
                    'time', 'random', 'sleep', 'subscribe', 'session', 'judge',
                    'exec-stage', 'corpus', 'eval-record', 'blob', 'register'
                )
            )
        ),
    CONSTRAINT program_run_journal_entry_reject_shape
        CHECK (
            (frame_kind = 'reject') = (reject_reason IS NOT NULL)
            AND (reject_reason IS NULL OR reject_reason = 'outstanding_requests')
        ),
    CONSTRAINT program_run_journal_entry_fault_shape
        CHECK (
            (frame_kind = 'fault') = (fault_cause IS NOT NULL)
            AND (
                fault_cause IS NULL
                OR fault_cause IN (
                    'timeout', 'memory', 'nondeterminism', 'program_error',
                    'contract_retired', 'journal_bound', 'payload_too_large'
                )
            )
        ),
    CONSTRAINT program_run_journal_entry_payload_shape
        CHECK (
            octet_length(payload_inline) = 0
            OR (
                frame_kind NOT IN ('scope', 'reject')
                AND NOT (
                    frame_kind = 'fault'
                    AND fault_cause = 'nondeterminism'
                )
            )
        )
);

CREATE TABLE program_run_journal_nondeterminism (
    run_id uuid NOT NULL,
    journal_position numeric(20, 0) NOT NULL,
    expected_request_ordinal numeric(20, 0) NOT NULL,
    expected_request_scope_ordinal numeric(20, 0),
    expected_kind text COLLATE "C" NOT NULL,
    expected_scope_operation text COLLATE "C",
    expected_declared_scope_ordinal numeric(20, 0),
    expected_parent_scope_ordinal numeric(20, 0),
    expected_effect_capability text COLLATE "C",
    expected_effect_method text,
    expected_payload_inline bytea NOT NULL,
    observed_request_ordinal numeric(20, 0) NOT NULL,
    observed_request_scope_ordinal numeric(20, 0),
    observed_kind text COLLATE "C" NOT NULL,
    observed_scope_operation text COLLATE "C",
    observed_declared_scope_ordinal numeric(20, 0),
    observed_parent_scope_ordinal numeric(20, 0),
    observed_effect_capability text COLLATE "C",
    observed_effect_method text,
    observed_payload_inline bytea NOT NULL,

    CONSTRAINT program_run_journal_nondeterminism_pk
        PRIMARY KEY (run_id, journal_position),
    CONSTRAINT program_run_journal_nondeterminism_entry_fk
        FOREIGN KEY (run_id, journal_position)
        REFERENCES program_run_journal_entry (run_id, journal_position)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT program_run_journal_nondeterminism_ordinals_positive
        CHECK (
            expected_request_ordinal BETWEEN 1 AND 18446744073709551615
            AND observed_request_ordinal BETWEEN 1 AND 18446744073709551615
            AND (
                expected_request_scope_ordinal IS NULL
                OR expected_request_scope_ordinal BETWEEN 1 AND 18446744073709551615
            )
            AND (
                observed_request_scope_ordinal IS NULL
                OR observed_request_scope_ordinal BETWEEN 1 AND 18446744073709551615
            )
            AND (
                expected_declared_scope_ordinal IS NULL
                OR expected_declared_scope_ordinal BETWEEN 1 AND 18446744073709551615
            )
            AND (
                expected_parent_scope_ordinal IS NULL
                OR expected_parent_scope_ordinal BETWEEN 1 AND 18446744073709551615
            )
            AND (
                observed_declared_scope_ordinal IS NULL
                OR observed_declared_scope_ordinal BETWEEN 1 AND 18446744073709551615
            )
            AND (
                observed_parent_scope_ordinal IS NULL
                OR observed_parent_scope_ordinal BETWEEN 1 AND 18446744073709551615
            )
        ),
    CONSTRAINT program_run_journal_nondeterminism_request_kinds
        CHECK (
            expected_kind IN (
                'now', 'random', 'sleep', 'await_event', 'effect', 'scope', 'terminal'
            )
            AND observed_kind IN (
                'now', 'random', 'sleep', 'await_event', 'effect', 'scope', 'terminal'
            )
        ),
    CONSTRAINT program_run_journal_nondeterminism_expected_scope_shape
        CHECK (
            (
                expected_kind = 'scope'
                AND expected_scope_operation IS NOT NULL
                AND expected_scope_operation IN ('open', 'close')
                AND expected_declared_scope_ordinal IS NOT NULL
            )
            OR
            (
                expected_kind <> 'scope'
                AND expected_scope_operation IS NULL
                AND expected_declared_scope_ordinal IS NULL
                AND expected_parent_scope_ordinal IS NULL
            )
        ),
    CONSTRAINT program_run_journal_nondeterminism_observed_scope_shape
        CHECK (
            (
                observed_kind = 'scope'
                AND observed_scope_operation IS NOT NULL
                AND observed_scope_operation IN ('open', 'close')
                AND observed_declared_scope_ordinal IS NOT NULL
            )
            OR
            (
                observed_kind <> 'scope'
                AND observed_scope_operation IS NULL
                AND observed_declared_scope_ordinal IS NULL
                AND observed_parent_scope_ordinal IS NULL
            )
        ),
    CONSTRAINT program_run_journal_nondeterminism_expected_effect_shape
        CHECK (
            (
                expected_kind = 'effect'
                AND expected_effect_capability IS NOT NULL
                AND expected_effect_capability IN (
                    'time', 'random', 'sleep', 'subscribe', 'session', 'judge',
                    'exec-stage', 'corpus', 'eval-record', 'blob', 'register'
                )
                AND expected_effect_method IS NOT NULL
            )
            OR
            (
                expected_kind <> 'effect'
                AND expected_effect_capability IS NULL
                AND expected_effect_method IS NULL
            )
        ),
    CONSTRAINT program_run_journal_nondeterminism_observed_effect_shape
        CHECK (
            (
                observed_kind = 'effect'
                AND observed_effect_capability IS NOT NULL
                AND observed_effect_capability IN (
                    'time', 'random', 'sleep', 'subscribe', 'session', 'judge',
                    'exec-stage', 'corpus', 'eval-record', 'blob', 'register'
                )
                AND observed_effect_method IS NOT NULL
            )
            OR
            (
                observed_kind <> 'effect'
                AND observed_effect_capability IS NULL
                AND observed_effect_method IS NULL
            )
        ),
    CONSTRAINT program_run_journal_nondeterminism_payload_shape
        CHECK (
            (
                expected_kind <> 'scope'
                OR octet_length(expected_payload_inline) = 0
            )
            AND (
                observed_kind <> 'scope'
                OR octet_length(observed_payload_inline) = 0
            )
        )
);

CREATE FUNCTION require_program_journal_append_sequence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    sequence program_run_journal_sequence_state%ROWTYPE;
BEGIN
    SELECT *
      INTO sequence
      FROM program_run_journal_sequence_state
     WHERE run_id = NEW.run_id
     FOR UPDATE;

    IF NOT FOUND
       OR NEW.journal_position <> sequence.last_position + 1
       OR (
           NEW.frame_direction = 'request'
           AND NEW.request_ordinal <> sequence.last_request_ordinal + 1
       )
       OR (
           NEW.frame_direction = 'delivery'
           AND NEW.delivery_ordinal <> sequence.last_delivery_ordinal + 1
       ) THEN
        RAISE EXCEPTION
            'program journal frame does not extend every applicable sequence by one'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER program_run_journal_entry_sequence_is_contiguous
BEFORE INSERT ON program_run_journal_entry
FOR EACH ROW
EXECUTE FUNCTION require_program_journal_append_sequence();

CREATE FUNCTION require_program_journal_sequence_matches_entries()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    sequence program_run_journal_sequence_state%ROWTYPE;
    maximum_position numeric(20, 0);
    maximum_request numeric(20, 0);
    maximum_delivery numeric(20, 0);
    position_count bigint;
    request_count bigint;
    delivery_count bigint;
BEGIN
    SELECT *
      INTO sequence
      FROM program_run_journal_sequence_state
     WHERE run_id = NEW.run_id;
    SELECT COALESCE(max(journal_position), 0),
           COALESCE(max(request_ordinal), 0),
           COALESCE(max(delivery_ordinal), 0),
           count(*),
           count(request_ordinal),
           count(delivery_ordinal)
      INTO maximum_position, maximum_request, maximum_delivery,
           position_count, request_count, delivery_count
      FROM program_run_journal_entry
     WHERE run_id = NEW.run_id;

    IF sequence.run_id IS NULL
       OR sequence.last_position <> maximum_position
       OR sequence.last_request_ordinal <> maximum_request
       OR sequence.last_delivery_ordinal <> maximum_delivery
       OR position_count <> maximum_position
       OR request_count <> maximum_request
       OR delivery_count <> maximum_delivery THEN
        RAISE EXCEPTION
            'program journal sequence state disagrees with committed frames'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER program_run_journal_entry_sequence_complete
AFTER INSERT ON program_run_journal_entry
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_program_journal_sequence_matches_entries();

CREATE CONSTRAINT TRIGGER program_run_journal_sequence_state_complete
AFTER INSERT OR UPDATE ON program_run_journal_sequence_state
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_program_journal_sequence_matches_entries();

CREATE FUNCTION require_program_journal_stream_sequence_state()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM program_run_journal_sequence_state
         WHERE run_id = NEW.run_id
    ) THEN
        RAISE EXCEPTION
            'program journal stream requires one sequence state row'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER program_run_journal_stream_sequence_state_complete
AFTER INSERT ON program_run_journal_stream
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_program_journal_stream_sequence_state();

CREATE FUNCTION require_program_journal_nondeterminism_evidence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    cause text;
    evidence_count bigint;
BEGIN
    SELECT fault_cause
      INTO cause
      FROM program_run_journal_entry
     WHERE run_id = NEW.run_id
       AND journal_position = NEW.journal_position;

    SELECT count(*)
      INTO evidence_count
      FROM program_run_journal_nondeterminism
     WHERE run_id = NEW.run_id
       AND journal_position = NEW.journal_position;

    IF (cause IS NOT DISTINCT FROM 'nondeterminism') <> (evidence_count = 1) THEN
        RAISE EXCEPTION
            'nondeterminism fault and its complete twin frames must commit together'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER program_run_journal_entry_nondeterminism_complete
AFTER INSERT ON program_run_journal_entry
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_program_journal_nondeterminism_evidence();

CREATE CONSTRAINT TRIGGER program_run_journal_nondeterminism_complete
AFTER INSERT ON program_run_journal_nondeterminism
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_program_journal_nondeterminism_evidence();

CREATE FUNCTION reject_program_journal_invalid_resolution()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.resolves_request_ordinal IS NOT NULL AND NOT EXISTS (
        SELECT 1 FROM program_run_journal_entry AS request
         WHERE request.run_id = NEW.run_id
           AND request.request_ordinal = NEW.resolves_request_ordinal
           AND request.journal_position < NEW.journal_position
           AND request.frame_kind <> 'scope'
           AND (
               NEW.frame_kind <> 'reject'
               OR NEW.reject_reason <> 'outstanding_requests'
               OR request.frame_kind = 'terminal'
           )
    ) THEN
        RAISE EXCEPTION
            'delivery must resolve one earlier compatible request'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER program_run_journal_scope_requests_are_unanswered
BEFORE INSERT ON program_run_journal_entry
FOR EACH ROW
EXECUTE FUNCTION reject_program_journal_invalid_resolution();

CREATE TRIGGER program_run_journal_stream_is_append_only
BEFORE UPDATE OR DELETE ON program_run_journal_stream
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER program_run_journal_sequence_state_cannot_be_deleted
BEFORE DELETE ON program_run_journal_sequence_state
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER program_run_journal_sequence_state_identity_is_immutable
BEFORE UPDATE OF run_id ON program_run_journal_sequence_state
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER program_run_journal_entry_is_append_only
BEFORE UPDATE OR DELETE ON program_run_journal_entry
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER program_run_journal_nondeterminism_is_append_only
BEFORE UPDATE OR DELETE ON program_run_journal_nondeterminism
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION reject_program_journal_truncate()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION
        'program journal tables cannot be truncated'
        USING ERRCODE = '55000';
END;
$$;

CREATE TRIGGER program_run_journal_stream_cannot_be_truncated
BEFORE TRUNCATE ON program_run_journal_stream
FOR EACH STATEMENT
EXECUTE FUNCTION reject_program_journal_truncate();

CREATE TRIGGER program_run_journal_sequence_state_cannot_be_truncated
BEFORE TRUNCATE ON program_run_journal_sequence_state
FOR EACH STATEMENT
EXECUTE FUNCTION reject_program_journal_truncate();

CREATE TRIGGER program_run_journal_entry_cannot_be_truncated
BEFORE TRUNCATE ON program_run_journal_entry
FOR EACH STATEMENT
EXECUTE FUNCTION reject_program_journal_truncate();

CREATE TRIGGER program_run_journal_nondeterminism_cannot_be_truncated
BEFORE TRUNCATE ON program_run_journal_nondeterminism
FOR EACH STATEMENT
EXECUTE FUNCTION reject_program_journal_truncate();
