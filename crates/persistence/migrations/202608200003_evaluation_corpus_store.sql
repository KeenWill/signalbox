-- Portable evaluation-corpus registrations and their ordered database cases.

CREATE TABLE evaluation_corpus (
    corpus_name text COLLATE "C" NOT NULL,
    corpus_version text COLLATE "C" NOT NULL,
    format_version integer NOT NULL,
    corpus_digest bytea NOT NULL,
    replay_digest bytea NOT NULL,
    case_count bigint NOT NULL,
    source_kind text COLLATE "C" NOT NULL,
    source_repository text,
    source_path text,
    source_sha256 bytea,
    source_blob_store text COLLATE "C",
    source_blob_digest bytea,
    source_blob_byte_length numeric(20, 0),
    registered_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    CONSTRAINT evaluation_corpus_pk
        PRIMARY KEY (corpus_name, corpus_version),
    CONSTRAINT evaluation_corpus_name_bounded
        CHECK (
            octet_length(corpus_name) BETWEEN 1 AND 128
            AND corpus_name !~ '^[[:space:]]*$'
            AND corpus_name !~ '[[:cntrl:]]'
        ),
    CONSTRAINT evaluation_corpus_version_bounded
        CHECK (
            octet_length(corpus_version) BETWEEN 1 AND 128
            AND corpus_version !~ '^[[:space:]]*$'
            AND corpus_version !~ '[[:cntrl:]]'
        ),
    CONSTRAINT evaluation_corpus_format_version_supported
        CHECK (format_version = 1),
    CONSTRAINT evaluation_corpus_digest_sha256
        CHECK (octet_length(corpus_digest) = 32),
    CONSTRAINT evaluation_corpus_replay_digest_sha256
        CHECK (octet_length(replay_digest) = 32),
    CONSTRAINT evaluation_corpus_case_count_positive
        CHECK (case_count > 0),
    CONSTRAINT evaluation_corpus_source_kind_closed
        CHECK (source_kind IN ('repository', 'database_native', 'blob_reference')),
    CONSTRAINT evaluation_corpus_source_shape
        CHECK (
            (
                source_kind = 'repository'
                AND source_repository IS NOT NULL
                AND octet_length(source_repository) BETWEEN 1 AND 2048
                AND source_repository !~ '^[[:space:]]*$'
                AND source_repository !~ '[[:cntrl:]]'
                AND source_path IS NOT NULL
                AND octet_length(source_path) BETWEEN 1 AND 1024
                AND source_path !~ '^[[:space:]]*$'
                AND source_path !~ '[[:cntrl:]<>:"|?*]'
                AND strpos(source_path, chr(92)) = 0
                AND source_path !~ '^/'
                AND source_path !~ '/$'
                AND source_path !~ '//'
                AND source_path !~ '(^|/)\.{1,2}(/|$)'
                AND source_path !~ '(^|/)[^/]*[. ](/|$)'
                AND source_path !~* '(^|/)(CON|PRN|AUX|NUL|COM[1-9¹²³]|LPT[1-9¹²³])(\.|/|$)'
                AND source_sha256 IS NOT NULL
                AND octet_length(source_sha256) = 32
                AND source_blob_store IS NULL
                AND source_blob_digest IS NULL
                AND source_blob_byte_length IS NULL
            )
            OR (
                source_kind = 'database_native'
                AND source_repository IS NULL
                AND source_path IS NULL
                AND source_sha256 IS NULL
                AND source_blob_store IS NULL
                AND source_blob_digest IS NULL
                AND source_blob_byte_length IS NULL
            )
            OR (
                source_kind = 'blob_reference'
                AND source_repository IS NULL
                AND source_path IS NULL
                AND source_sha256 IS NULL
                AND (
                    source_blob_store IS NULL
                    OR (
                        octet_length(source_blob_store) BETWEEN 1 AND 64
                        AND source_blob_store ~ '^[a-z][a-z0-9_-]*$'
                    )
                )
                AND source_blob_digest IS NOT NULL
                AND octet_length(source_blob_digest) = 32
                AND source_blob_byte_length IS NOT NULL
                AND source_blob_byte_length >= 1
                AND source_blob_byte_length <= 18446744073709551615
            )
        )
);

CREATE TABLE evaluation_corpus_case (
    corpus_name text COLLATE "C" NOT NULL,
    corpus_version text COLLATE "C" NOT NULL,
    case_id text COLLATE "C" NOT NULL,
    replay_position bigint NOT NULL,
    case_json jsonb NOT NULL,

    CONSTRAINT evaluation_corpus_case_pk
        PRIMARY KEY (corpus_name, corpus_version, case_id),
    CONSTRAINT evaluation_corpus_case_position_unique
        UNIQUE (corpus_name, corpus_version, replay_position),
    CONSTRAINT evaluation_corpus_case_identity_bounded
        CHECK (
            octet_length(case_id) BETWEEN 1 AND 128
            AND case_id !~ '^[[:space:]]*$'
            AND case_id !~ '[[:cntrl:]]'
        ),
    CONSTRAINT evaluation_corpus_case_position_nonnegative
        CHECK (replay_position >= 0),
    CONSTRAINT evaluation_corpus_case_json_object
        CHECK (jsonb_typeof(case_json) = 'object'),
    CONSTRAINT evaluation_corpus_case_corpus_fk
        FOREIGN KEY (corpus_name, corpus_version)
        REFERENCES evaluation_corpus (corpus_name, corpus_version)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);
