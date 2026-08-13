-- Freeze the model-operation execution locus into each durable tool request.

ALTER TABLE runner_registration
    ADD CONSTRAINT runner_registration_runner_revision_key
        UNIQUE (runner_id, registration_revision);

ALTER TABLE tool_request
    ADD COLUMN execution_locus_kind text NOT NULL DEFAULT 'daemon',
    ADD COLUMN execution_runner_id uuid,
    ADD COLUMN execution_registration_revision numeric(20, 0),
    ADD COLUMN execution_capability_class runner_catalog_name;

ALTER TABLE tool_request
    ADD CONSTRAINT tool_request_execution_registration_revision_positive_u64
        CHECK (
            execution_registration_revision IS NULL
            OR execution_registration_revision BETWEEN 1 AND 18446744073709551615
        ),
    ADD CONSTRAINT tool_request_execution_locus_shape
        CHECK (
            (
                execution_locus_kind = 'daemon'
                AND execution_runner_id IS NULL
                AND execution_registration_revision IS NULL
                AND execution_capability_class IS NULL
            )
            OR (
                execution_locus_kind = 'exact_runner'
                AND execution_runner_id IS NOT NULL
                AND execution_registration_revision IS NOT NULL
                AND execution_capability_class IS NULL
            )
            OR (
                execution_locus_kind = 'runner_capability_class'
                AND execution_runner_id IS NULL
                AND execution_registration_revision IS NULL
                AND execution_capability_class IS NOT NULL
            )
        ),
    ADD CONSTRAINT tool_request_execution_registration_fk
        FOREIGN KEY (execution_runner_id, execution_registration_revision)
        REFERENCES runner_registration (runner_id, registration_revision)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;
