CREATE TABLE oauth_credential_profile (
    profile text PRIMARY KEY CHECK (octet_length(profile) BETWEEN 1 AND 256 AND profile = btrim(profile)),
    generation bigint NOT NULL DEFAULT 0 CHECK (generation >= 0)
);

CREATE TABLE oauth_credential_registration (
    profile text PRIMARY KEY REFERENCES oauth_credential_profile(profile),
    tuple jsonb NOT NULL
);

CREATE TABLE oauth_credential_authorization (
    profile text PRIMARY KEY REFERENCES oauth_credential_profile(profile),
    tuple jsonb NOT NULL,
    refresh_token text NOT NULL,
    identity_token text NOT NULL,
    account_identity jsonb NOT NULL,
    generation bigint NOT NULL CHECK (generation > 0),
    refresh_in_progress boolean NOT NULL DEFAULT false,
    quarantined boolean NOT NULL DEFAULT false
);

CREATE TABLE oauth_credential_exchange (
    command_id uuid PRIMARY KEY REFERENCES durable_command(command_id),
    profile text NOT NULL REFERENCES oauth_credential_profile(profile),
    tuple jsonb NOT NULL,
    starting_generation bigint NOT NULL CHECK (starting_generation >= 0)
);
CREATE TRIGGER oauth_credential_exchange_is_append_only
    BEFORE UPDATE OR DELETE ON oauth_credential_exchange
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE oauth_credential_authorization_progress (
    command_id uuid PRIMARY KEY REFERENCES oauth_credential_exchange(command_id),
    user_code text NOT NULL,
    verification_uri text NOT NULL
);
CREATE TRIGGER oauth_credential_authorization_progress_is_append_only
    BEFORE UPDATE OR DELETE ON oauth_credential_authorization_progress
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION require_oauth_pool_account_independence() RETURNS trigger
    LANGUAGE plpgsql AS $$
DECLARE profile_name text;
BEGIN
    FOR profile_name IN
        SELECT name FROM (
            SELECT NEW.credential_reference AS name UNION
            SELECT credential_reference FROM model_call_credential_pool_member WHERE model_call_id = NEW.model_call_id
        ) members ORDER BY name COLLATE "C"
    LOOP
        INSERT INTO oauth_credential_profile (profile) VALUES (profile_name) ON CONFLICT DO NOTHING;
        PERFORM 1 FROM oauth_credential_profile WHERE profile = profile_name FOR UPDATE;
    END LOOP;
    IF EXISTS (
        SELECT 1 FROM model_call_credential_pool_member member
        JOIN oauth_credential_authorization candidate ON candidate.profile = NEW.credential_reference
        JOIN oauth_credential_authorization peer ON peer.profile = member.credential_reference
        WHERE member.model_call_id = NEW.model_call_id
          AND member.credential_reference <> NEW.credential_reference
          AND candidate.account_identity = peer.account_identity
    ) THEN
        RAISE EXCEPTION 'OAuth pool account independence failed' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER oauth_pool_account_independence
    BEFORE INSERT ON model_call_credential_pool_member
    FOR EACH ROW EXECUTE FUNCTION require_oauth_pool_account_independence();
