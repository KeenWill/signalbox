CREATE TABLE oauth_credential_failure (
    profile text NOT NULL REFERENCES oauth_credential_profile(profile),
    generation bigint NOT NULL,
    cause text NOT NULL CHECK (cause IN ('tuple_mismatch', 'refresh_ambiguous', 'refresh_rejected', 'identity_changed', 'credential_home')),
    PRIMARY KEY (profile, generation)
);
CREATE TRIGGER oauth_credential_failure_is_append_only
    BEFORE UPDATE OR DELETE ON oauth_credential_failure
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

ALTER TABLE oauth_credential_authorization
    ADD COLUMN quarantine_cause text CHECK (quarantine_cause IN ('tuple_mismatch', 'refresh_ambiguous', 'refresh_rejected', 'identity_changed', 'credential_home')),
    ADD CONSTRAINT oauth_quarantine_has_cause CHECK (quarantined = (quarantine_cause IS NOT NULL));
