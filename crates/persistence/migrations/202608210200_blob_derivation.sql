-- Immutable blob-to-blob derivation provenance and deterministic cache identity.

CREATE TABLE blob_derivation (
    derivation_id uuid PRIMARY KEY,
    deterministic_key bytea UNIQUE,
    transformation_name text COLLATE "C" NOT NULL,
    transformation_version bigint NOT NULL,
    parameters_json jsonb NOT NULL,
    producer_class text COLLATE "C" NOT NULL,
    implementation_digest bytea,
    execution_id uuid,
    model_call_id uuid,
    input_count smallint NOT NULL,
    output_count smallint NOT NULL,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    CONSTRAINT blob_derivation_model_call_fk
        FOREIGN KEY (model_call_id)
        REFERENCES model_call (model_call_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT blob_derivation_key_shape
        CHECK (deterministic_key IS NULL OR octet_length(deterministic_key) = 32),
    CONSTRAINT blob_derivation_name_shape
        CHECK (
            octet_length(transformation_name) BETWEEN 1 AND 64
            AND transformation_name ~ '^[a-z][a-z0-9_.-]*$'
        ),
    CONSTRAINT blob_derivation_version_shape
        CHECK (transformation_version BETWEEN 1 AND 4294967295),
    CONSTRAINT blob_derivation_parameter_bound
        CHECK (octet_length(parameters_json::text) <= 4096),
    CONSTRAINT blob_derivation_counts
        CHECK (input_count BETWEEN 1 AND 16 AND output_count BETWEEN 1 AND 16),
    CONSTRAINT blob_derivation_producer_shape
        CHECK (
            (
                producer_class = 'deterministic'
                AND deterministic_key IS NOT NULL
                AND octet_length(implementation_digest) = 32
                AND execution_id IS NULL
                AND model_call_id IS NULL
            )
            OR
            (
                producer_class = 'executed'
                AND deterministic_key IS NULL
                AND octet_length(implementation_digest) = 32
                AND execution_id IS NOT NULL
                AND model_call_id IS NULL
            )
            OR
            (
                producer_class = 'model_derived'
                AND deterministic_key IS NULL
                AND implementation_digest IS NULL
                AND execution_id IS NULL
                AND model_call_id IS NOT NULL
            )
        )
);

CREATE TABLE blob_derivation_input (
    derivation_id uuid NOT NULL,
    input_ordinal smallint NOT NULL,
    digest bytea NOT NULL,

    CONSTRAINT blob_derivation_input_pk PRIMARY KEY (derivation_id, input_ordinal),
    CONSTRAINT blob_derivation_input_root_fk
        FOREIGN KEY (derivation_id)
        REFERENCES blob_derivation (derivation_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT blob_derivation_input_blob_fk
        FOREIGN KEY (digest)
        REFERENCES blob (digest)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT blob_derivation_input_ordinal CHECK (input_ordinal BETWEEN 0 AND 15),
    CONSTRAINT blob_derivation_input_digest CHECK (octet_length(digest) = 32)
);

CREATE TABLE blob_derivation_output (
    derivation_id uuid NOT NULL,
    output_ordinal smallint NOT NULL,
    digest bytea NOT NULL,

    CONSTRAINT blob_derivation_output_pk PRIMARY KEY (derivation_id, output_ordinal),
    CONSTRAINT blob_derivation_output_root_fk
        FOREIGN KEY (derivation_id)
        REFERENCES blob_derivation (derivation_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT blob_derivation_output_blob_fk
        FOREIGN KEY (digest)
        REFERENCES blob (digest)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT blob_derivation_output_ordinal CHECK (output_ordinal BETWEEN 0 AND 15),
    CONSTRAINT blob_derivation_output_digest CHECK (octet_length(digest) = 32)
);

CREATE FUNCTION reject_blob_derivation_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'blob derivation records are immutable';
END;
$$;

CREATE TRIGGER blob_derivation_immutable
BEFORE UPDATE OR DELETE ON blob_derivation
FOR EACH ROW EXECUTE FUNCTION reject_blob_derivation_mutation();

CREATE TRIGGER blob_derivation_input_immutable
BEFORE UPDATE OR DELETE ON blob_derivation_input
FOR EACH ROW EXECUTE FUNCTION reject_blob_derivation_mutation();

CREATE TRIGGER blob_derivation_output_immutable
BEFORE UPDATE OR DELETE ON blob_derivation_output
FOR EACH ROW EXECUTE FUNCTION reject_blob_derivation_mutation();

CREATE TRIGGER blob_derivation_no_truncate
BEFORE TRUNCATE ON blob_derivation
FOR EACH STATEMENT EXECUTE FUNCTION reject_blob_derivation_mutation();

CREATE TRIGGER blob_derivation_input_no_truncate
BEFORE TRUNCATE ON blob_derivation_input
FOR EACH STATEMENT EXECUTE FUNCTION reject_blob_derivation_mutation();

CREATE TRIGGER blob_derivation_output_no_truncate
BEFORE TRUNCATE ON blob_derivation_output
FOR EACH STATEMENT EXECUTE FUNCTION reject_blob_derivation_mutation();

CREATE FUNCTION check_blob_derivation_complete()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    selected_id uuid;
    expected_inputs smallint;
    expected_outputs smallint;
    observed_inputs bigint;
    observed_outputs bigint;
BEGIN
    selected_id := NEW.derivation_id;
    SELECT input_count, output_count
      INTO expected_inputs, expected_outputs
      FROM blob_derivation
     WHERE derivation_id = selected_id;
    SELECT count(*) INTO observed_inputs
      FROM blob_derivation_input
     WHERE derivation_id = selected_id;
    SELECT count(*) INTO observed_outputs
      FROM blob_derivation_output
     WHERE derivation_id = selected_id;
    IF expected_inputs IS NULL
       OR observed_inputs <> expected_inputs
       OR observed_outputs <> expected_outputs THEN
        RAISE EXCEPTION 'blob derivation record is incomplete';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER blob_derivation_root_complete
AFTER INSERT ON blob_derivation
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION check_blob_derivation_complete();

CREATE CONSTRAINT TRIGGER blob_derivation_input_complete
AFTER INSERT ON blob_derivation_input
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION check_blob_derivation_complete();

CREATE CONSTRAINT TRIGGER blob_derivation_output_complete
AFTER INSERT ON blob_derivation_output
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION check_blob_derivation_complete();
