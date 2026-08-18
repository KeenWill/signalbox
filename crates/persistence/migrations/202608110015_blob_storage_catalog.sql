-- Append-only content-addressed blob identities, deployment namespace
-- bindings, and verified durable store placements.

CREATE TABLE blob_store_binding (
    store_name text COLLATE "C" PRIMARY KEY,
    namespace_id uuid NOT NULL UNIQUE,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    CONSTRAINT blob_store_binding_name_canonical
        CHECK (
            octet_length(store_name) BETWEEN 1 AND 64
            AND store_name ~ '^[a-z][a-z0-9_-]{0,63}$'
        )
);

CREATE TABLE blob (
    digest bytea PRIMARY KEY,
    byte_length numeric(20, 0) NOT NULL,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    CONSTRAINT blob_digest_size
        CHECK (octet_length(digest) = 32),
    CONSTRAINT blob_byte_length_positive_u64
        CHECK (
            byte_length >= 1
            AND byte_length <= 18446744073709551615
        )
);

CREATE TABLE blob_replica (
    digest bytea NOT NULL,
    store_name text COLLATE "C" NOT NULL,
    object_key text COLLATE "C" NOT NULL,
    verified_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    CONSTRAINT blob_replica_pk
        PRIMARY KEY (digest, store_name),
    CONSTRAINT blob_replica_store_object_unique
        UNIQUE (store_name, object_key),
    CONSTRAINT blob_replica_digest_size
        CHECK (octet_length(digest) = 32),
    CONSTRAINT blob_replica_store_name_bounded
        CHECK (
            octet_length(store_name) BETWEEN 1 AND 64
            AND store_name ~ '^[a-z][a-z0-9_-]{0,63}$'
        ),
    CONSTRAINT blob_replica_object_key_bounded
        CHECK (octet_length(object_key) BETWEEN 1 AND 1024),
    CONSTRAINT blob_replica_blob_fk
        FOREIGN KEY (digest)
        REFERENCES blob (digest)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT blob_replica_store_binding_fk
        FOREIGN KEY (store_name)
        REFERENCES blob_store_binding (store_name)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE FUNCTION require_blob_replica()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM blob_replica
         WHERE digest = NEW.digest
    ) THEN
        RAISE EXCEPTION
            'blob identity requires at least one verified replica'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER blob_requires_replica
AFTER INSERT ON blob
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_blob_replica();

CREATE FUNCTION reject_blob_catalog_row_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION
        'blob catalog rows are append-only'
        USING ERRCODE = '55000';
END;
$$;

CREATE TRIGGER blob_is_append_only
BEFORE UPDATE OR DELETE ON blob
FOR EACH ROW
EXECUTE FUNCTION reject_blob_catalog_row_mutation();

CREATE TRIGGER blob_store_binding_is_append_only
BEFORE UPDATE OR DELETE ON blob_store_binding
FOR EACH ROW
EXECUTE FUNCTION reject_blob_catalog_row_mutation();

CREATE TRIGGER blob_replica_is_append_only
BEFORE UPDATE OR DELETE ON blob_replica
FOR EACH ROW
EXECUTE FUNCTION reject_blob_catalog_row_mutation();

CREATE FUNCTION reject_blob_catalog_truncate()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION
        'blob catalog tables cannot be truncated'
        USING ERRCODE = '55000';
END;
$$;

CREATE TRIGGER blob_cannot_be_truncated
BEFORE TRUNCATE ON blob
FOR EACH STATEMENT
EXECUTE FUNCTION reject_blob_catalog_truncate();

CREATE TRIGGER blob_store_binding_cannot_be_truncated
BEFORE TRUNCATE ON blob_store_binding
FOR EACH STATEMENT
EXECUTE FUNCTION reject_blob_catalog_truncate();

CREATE TRIGGER blob_replica_cannot_be_truncated
BEFORE TRUNCATE ON blob_replica
FOR EACH STATEMENT
EXECUTE FUNCTION reject_blob_catalog_truncate();
