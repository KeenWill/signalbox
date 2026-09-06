-- Record the exact serving target selected after fast-mode mapping.

ALTER TABLE model_call
    ADD COLUMN effective_provider_model_identity_id uuid NOT NULL;
