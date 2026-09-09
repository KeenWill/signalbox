ALTER TABLE session_lifecycle
    ADD COLUMN checkout_provisioning_pending boolean NOT NULL DEFAULT false;
