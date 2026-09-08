ALTER TABLE submit_input_command ADD COLUMN result_attachment_verified_prefix bytea[];
ALTER TABLE submit_input_command ADD CONSTRAINT attachment_missing_requires_verified_prefix
    CHECK ((rejection_kind IS NOT DISTINCT FROM 'attachment_blob_not_found') =
           (result_attachment_verified_prefix IS NOT NULL)) NOT VALID;
