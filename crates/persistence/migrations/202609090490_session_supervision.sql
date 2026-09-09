-- Durable operator evidence for session-scoped execution and reconstitution failures.
ALTER TABLE session_lifecycle
    ADD COLUMN supervision_failure_class text,
    ADD COLUMN supervision_cause_code text,
    ADD COLUMN supervision_pending boolean NOT NULL DEFAULT false,
    ADD CONSTRAINT session_supervision_shape CHECK (
        (supervision_failure_class IS NULL) = (supervision_cause_code IS NULL)
        AND (NOT supervision_pending OR supervision_failure_class IS NOT NULL)
        AND (supervision_failure_class IS NULL OR supervision_failure_class IN (
            'infrastructure', 'commit_ambiguous', 'corruption', 'identity_collision', 'bug'
        ))
    );
