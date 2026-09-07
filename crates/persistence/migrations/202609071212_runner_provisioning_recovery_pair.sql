ALTER TABLE runner_replacement_provisioning_authorization
    ADD CONSTRAINT runner_replacement_repository_recovery_pair
    CHECK ((repository_key IS NULL) = (checkout_revision IS NULL));
