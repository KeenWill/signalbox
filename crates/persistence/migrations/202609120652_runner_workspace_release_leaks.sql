ALTER TABLE runner_replacement_workspace_ready
    ADD COLUMN manifest_digest text NOT NULL CHECK (manifest_digest ~ '^[0-9a-f]{64}$');

DROP TABLE runner_replacement_workspace_release_reauthorization;
DROP TABLE runner_replacement_workspace_released;
DROP TABLE runner_replacement_workspace_release;

CREATE TABLE runner_workspace_release (
    manifest_id uuid PRIMARY KEY,
    session_id uuid NOT NULL REFERENCES session(session_id),
    placement_revision numeric(20,0) NOT NULL CHECK (placement_revision BETWEEN 1 AND 18446744073709551615),
    runner_id uuid NOT NULL REFERENCES runner_enrollment(runner_id),
    enrollment_id uuid NOT NULL REFERENCES runner_enrollment(enrollment_id),
    connection_epoch numeric(20,0) NOT NULL,
    connection_event_ordinal numeric(20,0) NOT NULL,
    relative_path text NOT NULL,
    source_event_ordinal numeric(20,0),
    retired_event_ordinal numeric(20,0),
    authorization_id uuid UNIQUE REFERENCES runner_replacement_workspace_ready(authorization_id),
    FOREIGN KEY (session_id, source_event_ordinal) REFERENCES runner_session_placement_record(session_id, event_ordinal),
    FOREIGN KEY (session_id, retired_event_ordinal) REFERENCES runner_session_placement_record(session_id, event_ordinal),
    FOREIGN KEY (enrollment_id, connection_epoch, connection_event_ordinal) REFERENCES runner_connection_event(enrollment_id, connection_epoch, event_ordinal),
    CHECK ((authorization_id IS NOT NULL AND source_event_ordinal IS NULL AND retired_event_ordinal IS NULL)
        OR (authorization_id IS NULL AND source_event_ordinal IS NOT NULL AND retired_event_ordinal IS NOT NULL AND retired_event_ordinal > source_event_ordinal))
);

CREATE FUNCTION require_runner_workspace_release_authority() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM runner_enrollment enrollment
        JOIN runner_connection_authority_head head USING (enrollment_id)
        JOIN runner_connection_event event ON event.enrollment_id = head.enrollment_id
            AND event.connection_epoch = head.connection_epoch AND event.event_ordinal = head.connection_event_ordinal
        WHERE enrollment.enrollment_id = NEW.enrollment_id AND enrollment.runner_id = NEW.runner_id
            AND enrollment.state_kind <> 'revoked'
            AND head.connection_epoch = NEW.connection_epoch AND head.connection_event_ordinal = NEW.connection_event_ordinal
            AND (event.state_kind = 'connected' OR (NEW.authorization_id IS NOT NULL AND event.state_kind = 'suspect'))
    ) THEN RAISE EXCEPTION 'workspace release requires its connected owner'; END IF;
    IF NEW.authorization_id IS NOT NULL THEN
        IF NOT EXISTS (
            SELECT 1 FROM runner_replacement_provisioning_authorization operation
            JOIN runner_replacement_workspace_ready ready USING (authorization_id)
            JOIN replace_lost_runner_result result USING (command_id)
            WHERE operation.authorization_id = NEW.authorization_id AND result.result_kind = 'rejected'
                AND operation.session_id = NEW.session_id AND operation.placement_revision = NEW.placement_revision
                AND operation.runner_id = NEW.runner_id AND operation.registration_enrollment_id = NEW.enrollment_id
                AND ready.manifest_id = NEW.manifest_id AND ready.relative_path = NEW.relative_path
                AND NOT EXISTS (SELECT 1 FROM runner_replacement_workspace_consumption consumed WHERE consumed.authorization_id = NEW.authorization_id)
        ) THEN RAISE EXCEPTION 'workspace release requires rejected provisioning'; END IF;
    ELSE
        IF NOT EXISTS (
            SELECT 1 FROM runner_session_placement_record source
            JOIN runner_session_placement_record retired ON retired.session_id = source.session_id
            WHERE source.session_id = NEW.session_id AND source.event_ordinal = NEW.source_event_ordinal
                AND retired.event_ordinal = NEW.retired_event_ordinal
                AND source.workspace_manifest_id = NEW.manifest_id AND source.workspace_placement_revision = NEW.placement_revision
                AND source.pinned_runner_id = NEW.runner_id AND source.registration_enrollment_id = NEW.enrollment_id
                AND source.workspace_relative_path = NEW.relative_path
                AND (retired.state_kind = 'runner_abandoned' OR retired.workspace_manifest_id IS DISTINCT FROM source.workspace_manifest_id)
        ) THEN RAISE EXCEPTION 'workspace release requires retired placement'; END IF;
    END IF;
    IF EXISTS (
        SELECT 1 FROM runner_lease_generation lease
        JOIN runner_current_lease_event current USING (lease_id, generation)
        JOIN runner_lease_event event USING (lease_id, generation, event_ordinal)
        JOIN runner_session_placement_record placement ON placement.session_id = lease.session_id
            AND placement.event_ordinal = lease.placement_event_ordinal
        WHERE placement.workspace_manifest_id = NEW.manifest_id AND event.state_kind IN ('offered', 'claimed')
    ) THEN RAISE EXCEPTION 'workspace release has unsettled execution'; END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER runner_workspace_release_requires_authority BEFORE INSERT ON runner_workspace_release
    FOR EACH ROW EXECUTE FUNCTION require_runner_workspace_release_authority();
CREATE TRIGGER runner_workspace_release_is_append_only BEFORE UPDATE OR DELETE ON runner_workspace_release
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE runner_workspace_release_outcome (
    manifest_id uuid PRIMARY KEY REFERENCES runner_workspace_release(manifest_id),
    outcome text NOT NULL CHECK (outcome IN ('completed', 'unowned', 'cleanup_failed')),
    detail jsonb,
    CHECK ((outcome = 'cleanup_failed' AND detail IS NOT NULL AND jsonb_typeof(detail) = 'object') OR (outcome <> 'cleanup_failed' AND detail IS NULL))
);
CREATE TRIGGER runner_workspace_release_outcome_is_append_only BEFORE UPDATE OR DELETE ON runner_workspace_release_outcome
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE runner_workspace_leak_page (
    enrollment_id uuid NOT NULL,
    registration_revision numeric(20,0) NOT NULL,
    report_digest text NOT NULL CHECK (report_digest ~ '^[0-9a-f]{64}$'),
    page numeric(20,0) NOT NULL CHECK (page BETWEEN 1 AND 18446744073709551615),
    prior_page_digest text CHECK (prior_page_digest ~ '^[0-9a-f]{64}$'),
    final_page boolean NOT NULL,
    page_digest text NOT NULL CHECK (page_digest ~ '^[0-9a-f]{64}$'),
    facts jsonb NOT NULL CHECK (jsonb_typeof(facts) = 'array' AND jsonb_array_length(facts) <= 64),
    PRIMARY KEY (enrollment_id, registration_revision, report_digest, page),
    FOREIGN KEY (enrollment_id, registration_revision) REFERENCES runner_registration(enrollment_id, registration_revision),
    CHECK ((page = 1) = (prior_page_digest IS NULL))
);
CREATE TRIGGER runner_workspace_leak_page_is_append_only BEFORE UPDATE OR DELETE ON runner_workspace_leak_page
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE runner_workspace_leak (
    report_derived boolean NOT NULL DEFAULT false,
    runner_id uuid NOT NULL REFERENCES runner_enrollment(runner_id),
    locator text NOT NULL,
    entry_digest text NOT NULL CHECK (entry_digest ~ '^[0-9a-f]{64}$'),
    kind text NOT NULL CHECK (kind IN ('unknown_manifest', 'retired_present', 'manifest_conflict', 'cleanup_failed', 'unreconciled')),
    session_id uuid,
    placement_revision numeric(20,0) CHECK (placement_revision BETWEEN 1 AND 18446744073709551615),
    PRIMARY KEY (runner_id, locator, entry_digest)
);
