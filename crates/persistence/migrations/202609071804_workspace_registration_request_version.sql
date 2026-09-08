ALTER TABLE workspace
    ADD CONSTRAINT workspace_registration_request_versioned
        CHECK (registration_request_root IS NULL OR storage_version IS NOT DISTINCT FROM 2);
