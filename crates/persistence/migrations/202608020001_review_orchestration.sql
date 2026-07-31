-- Durable review-orchestration attempts, stage barriers, and owner-global
-- command receipts. Canonical review aggregates remain in review_workflow.

ALTER TABLE durable_command
    DROP CONSTRAINT durable_command_kind_closed,
    DROP CONSTRAINT durable_command_storage_version_supported;

ALTER TABLE durable_command
    ADD CONSTRAINT durable_command_kind_closed
        CHECK (
            command_kind IN (
                'create_session',
                'create_session_from_imported_frontier',
                'replace_session_defaults',
                'replace_session_metadata',
                'submit_input',
                'decide_tool_request',
                'review_workflow',
                'review_orchestration',
                'compact_session'
            )
        ),
    ADD CONSTRAINT durable_command_storage_version_supported
        CHECK (
            (command_kind = 'create_session' AND storage_version IN (1, 2, 3, 4))
            OR (
                command_kind IN (
                    'create_session_from_imported_frontier',
                    'replace_session_defaults'
                )
                AND storage_version IN (1, 2, 3)
            )
            OR (
                command_kind IN (
                    'replace_session_metadata',
                    'submit_input',
                    'decide_tool_request',
                    'review_workflow',
                    'review_orchestration',
                    'compact_session'
                )
                AND storage_version = 1
            )
        );

ALTER TABLE review_workflow_command
    DROP CONSTRAINT review_workflow_command_operation_closed,
    DROP CONSTRAINT review_workflow_command_result_closed,
    DROP CONSTRAINT review_workflow_command_operation_result,
    DROP CONSTRAINT review_workflow_command_finding_status_closed,
    DROP CONSTRAINT review_workflow_command_result_shape;

ALTER TABLE review_workflow_command
    ADD CONSTRAINT review_workflow_command_operation_closed
        CHECK (
            operation_kind IN (
                'create_target', 'start_run', 'activate_pass', 'complete_pass',
                'record_findings', 'record_finding_event',
                'reserve_external_link', 'attach_external_link'
            )
        ),
    ADD CONSTRAINT review_workflow_command_result_closed
        CHECK (
            result_kind IN (
                'target_created', 'run_started', 'pass_activated', 'pass_completed',
                'findings_recorded', 'finding_event_recorded',
                'external_link_reserved', 'external_link_attached'
            )
        ),
    ADD CONSTRAINT review_workflow_command_operation_result
        CHECK (
            (operation_kind = 'create_target' AND result_kind = 'target_created')
            OR (operation_kind = 'start_run' AND result_kind = 'run_started')
            OR (operation_kind = 'activate_pass' AND result_kind = 'pass_activated')
            OR (operation_kind = 'complete_pass' AND result_kind = 'pass_completed')
            OR (operation_kind = 'record_findings' AND result_kind = 'findings_recorded')
            OR (operation_kind = 'record_finding_event' AND result_kind = 'finding_event_recorded')
            OR (operation_kind = 'reserve_external_link' AND result_kind = 'external_link_reserved')
            OR (operation_kind = 'attach_external_link' AND result_kind = 'external_link_attached')
        ),
    ADD CONSTRAINT review_workflow_command_finding_status_closed
        CHECK (
            result_finding_status IS NULL
            OR result_finding_status IN (
                'open', 'accepted', 'rejected', 'duplicate', 'superseded',
                'stale', 'posted', 'fixed', 'blocked_with_reason',
                'succeeded', 'failed', 'blocked', 'cancelled'
            )
        ),
    ADD CONSTRAINT review_workflow_command_result_shape
        CHECK (
            (result_kind = 'target_created' AND result_target_id IS NOT NULL
                AND result_run_id IS NULL AND result_pass_id IS NULL
                AND result_finding_id IS NULL AND result_external_link_id IS NULL
                AND result_finding_count IS NULL AND result_finding_status IS NULL
                AND result_external_object_key IS NULL)
            OR (result_kind IN ('run_started', 'pass_activated')
                AND result_target_id IS NULL AND result_run_id IS NOT NULL
                AND result_pass_id IS NOT NULL AND result_finding_id IS NULL
                AND result_external_link_id IS NULL AND result_finding_count IS NULL
                AND result_finding_status IS NULL AND result_external_object_key IS NULL)
            OR (result_kind = 'pass_completed' AND result_target_id IS NULL
                AND result_run_id IS NOT NULL AND result_pass_id IS NOT NULL
                AND result_finding_id IS NULL AND result_external_link_id IS NULL
                AND result_finding_count IS NULL AND result_finding_status IN
                    ('succeeded', 'failed', 'blocked', 'cancelled')
                AND result_external_object_key IS NULL)
            OR (result_kind = 'findings_recorded' AND result_target_id IS NULL
                AND result_run_id IS NOT NULL AND result_pass_id IS NOT NULL
                AND result_finding_id IS NULL AND result_external_link_id IS NULL
                AND result_finding_count IS NOT NULL AND result_finding_count >= 0
                AND result_finding_status IS NULL AND result_external_object_key IS NULL)
            OR (result_kind = 'finding_event_recorded' AND result_target_id IS NULL
                AND result_run_id IS NULL AND result_pass_id IS NULL
                AND result_finding_id IS NOT NULL AND result_external_link_id IS NULL
                AND result_finding_count IS NULL AND result_finding_status IN
                    ('open', 'accepted', 'rejected', 'duplicate', 'superseded',
                     'stale', 'posted', 'fixed', 'blocked_with_reason')
                AND result_external_object_key IS NULL)
            OR (result_kind = 'external_link_reserved' AND result_target_id IS NULL
                AND result_run_id IS NULL AND result_pass_id IS NULL
                AND result_finding_id IS NULL AND result_external_link_id IS NOT NULL
                AND result_finding_count IS NULL AND result_finding_status IS NULL
                AND result_external_object_key IS NULL)
            OR (result_kind = 'external_link_attached' AND result_target_id IS NULL
                AND result_run_id IS NULL AND result_pass_id IS NULL
                AND result_finding_id IS NULL AND result_external_link_id IS NOT NULL
                AND result_finding_count IS NULL AND result_finding_status IS NULL
                AND result_external_object_key IS NOT NULL)
        );

CREATE TABLE review_orchestration_attempt (
    attempt_id uuid PRIMARY KEY,
    target_id uuid NOT NULL REFERENCES review_target(target_id) ON DELETE RESTRICT,
    policy_version bigint NOT NULL CHECK (policy_version BETWEEN 1 AND 4294967295),
    minimum_judge_confidence integer NOT NULL CHECK (minimum_judge_confidence BETWEEN 0 AND 10000),
    minimum_publication_confidence integer NOT NULL CHECK (minimum_publication_confidence BETWEEN 0 AND 10000),
    concern_set_version text NOT NULL CHECK (octet_length(concern_set_version) BETWEEN 1 AND 1024),
    import_template_digest bytea NOT NULL CHECK (octet_length(import_template_digest) = 32),
    judgment_template_digest bytea NOT NULL CHECK (octet_length(judgment_template_digest) = 32),
    repair_template_digest bytea NOT NULL CHECK (octet_length(repair_template_digest) = 32),
    publication_template_digest bytea NOT NULL CHECK (octet_length(publication_template_digest) = 32)
);

CREATE TABLE review_orchestration_concern (
    attempt_id uuid NOT NULL REFERENCES review_orchestration_attempt(attempt_id) ON DELETE RESTRICT,
    concern_ordinal integer NOT NULL CHECK (concern_ordinal >= 0),
    concern_key text NOT NULL CHECK (octet_length(concern_key) BETWEEN 1 AND 1024),
    template_digest bytea NOT NULL CHECK (octet_length(template_digest) = 32),
    PRIMARY KEY (attempt_id, concern_ordinal),
    UNIQUE (attempt_id, concern_key)
);

CREATE TABLE review_orchestration_import (
    attempt_id uuid PRIMARY KEY REFERENCES review_orchestration_attempt(attempt_id) ON DELETE RESTRICT,
    outcome_kind text NOT NULL CHECK (outcome_kind IN ('succeeded', 'failed', 'blocked', 'cancelled')),
    pass_id uuid REFERENCES review_pass(pass_id) ON DELETE RESTRICT,
    external_link_id uuid REFERENCES review_external_link(external_link_id) ON DELETE RESTRICT,
    template_digest bytea NOT NULL CHECK (octet_length(template_digest) = 32),
    context_digest bytea CHECK (context_digest IS NULL OR octet_length(context_digest) = 32),
    CHECK (
        (outcome_kind = 'succeeded' AND pass_id IS NOT NULL AND context_digest IS NOT NULL)
        OR (outcome_kind IN ('failed', 'blocked') AND pass_id IS NOT NULL
            AND external_link_id IS NULL AND context_digest IS NULL)
        OR (outcome_kind = 'cancelled' AND external_link_id IS NULL AND context_digest IS NULL)
    )
);

CREATE TABLE review_orchestration_concern_claim (
    attempt_id uuid NOT NULL,
    concern_key text NOT NULL,
    claim_ordinal integer NOT NULL CHECK (claim_ordinal >= 0),
    template_digest bytea NOT NULL CHECK (octet_length(template_digest) = 32),
    outcome_kind text NOT NULL CHECK (outcome_kind IN ('succeeded', 'failed', 'blocked', 'cancelled', 'superseded')),
    pass_id uuid REFERENCES review_pass(pass_id) ON DELETE RESTRICT,
    PRIMARY KEY (attempt_id, concern_key, claim_ordinal),
    FOREIGN KEY (attempt_id, concern_key)
        REFERENCES review_orchestration_concern(attempt_id, concern_key) ON DELETE RESTRICT,
    CHECK ((outcome_kind = 'cancelled') OR pass_id IS NOT NULL)
);

CREATE TABLE review_orchestration_concern_finding (
    attempt_id uuid NOT NULL,
    concern_key text NOT NULL,
    claim_ordinal integer NOT NULL,
    finding_ordinal integer NOT NULL CHECK (finding_ordinal >= 0),
    finding_id uuid NOT NULL REFERENCES review_finding(finding_id) ON DELETE RESTRICT,
    PRIMARY KEY (attempt_id, concern_key, claim_ordinal, finding_ordinal),
    UNIQUE (attempt_id, concern_key, claim_ordinal, finding_id),
    FOREIGN KEY (attempt_id, concern_key, claim_ordinal)
        REFERENCES review_orchestration_concern_claim(attempt_id, concern_key, claim_ordinal)
        ON DELETE RESTRICT
);

CREATE TABLE review_orchestration_fanout_seal (
    attempt_id uuid PRIMARY KEY REFERENCES review_orchestration_attempt(attempt_id) ON DELETE RESTRICT
);

CREATE TABLE review_orchestration_fanout_member (
    attempt_id uuid NOT NULL REFERENCES review_orchestration_fanout_seal(attempt_id) ON DELETE RESTRICT,
    member_ordinal integer NOT NULL CHECK (member_ordinal >= 0),
    concern_key text NOT NULL,
    claim_ordinal integer NOT NULL,
    PRIMARY KEY (attempt_id, member_ordinal),
    UNIQUE (attempt_id, concern_key),
    FOREIGN KEY (attempt_id, concern_key, claim_ordinal)
        REFERENCES review_orchestration_concern_claim(attempt_id, concern_key, claim_ordinal)
        ON DELETE RESTRICT
);

CREATE TABLE review_orchestration_judgment_plan (
    attempt_id uuid PRIMARY KEY REFERENCES review_orchestration_fanout_seal(attempt_id) ON DELETE RESTRICT,
    analysis_pass_id uuid NOT NULL REFERENCES review_pass(pass_id) ON DELETE RESTRICT,
    template_digest bytea NOT NULL CHECK (octet_length(template_digest) = 32)
);

CREATE TABLE review_orchestration_judgment_member (
    attempt_id uuid NOT NULL REFERENCES review_orchestration_judgment_plan(attempt_id) ON DELETE RESTRICT,
    member_ordinal integer NOT NULL CHECK (member_ordinal >= 0),
    finding_id uuid NOT NULL REFERENCES review_finding(finding_id) ON DELETE RESTRICT,
    finding_run_id uuid NOT NULL REFERENCES review_run(run_id) ON DELETE RESTRICT,
    finding_pass_id uuid NOT NULL REFERENCES review_pass(pass_id) ON DELETE RESTRICT,
    disposition_kind text NOT NULL CHECK (disposition_kind IN ('accepted', 'rejected', 'duplicate', 'superseded', 'stale')),
    reason text,
    referenced_finding_id uuid REFERENCES review_finding(finding_id) ON DELETE RESTRICT,
    referenced_run_id uuid REFERENCES review_run(run_id) ON DELETE RESTRICT,
    referenced_pass_id uuid REFERENCES review_pass(pass_id) ON DELETE RESTRICT,
    PRIMARY KEY (attempt_id, member_ordinal),
    UNIQUE (attempt_id, member_ordinal, finding_id),
    UNIQUE (attempt_id, finding_id),
    CHECK (
        (disposition_kind = 'rejected' AND reason IS NOT NULL
            AND referenced_finding_id IS NULL AND referenced_run_id IS NULL AND referenced_pass_id IS NULL)
        OR (disposition_kind IN ('duplicate', 'superseded') AND reason IS NULL
            AND referenced_finding_id IS NOT NULL AND referenced_run_id IS NOT NULL AND referenced_pass_id IS NOT NULL)
        OR (disposition_kind IN ('accepted', 'stale') AND reason IS NULL
            AND referenced_finding_id IS NULL AND referenced_run_id IS NULL AND referenced_pass_id IS NULL)
    )
);

CREATE TABLE review_orchestration_judgment_effect (
    attempt_id uuid NOT NULL REFERENCES review_orchestration_judgment_plan(attempt_id) ON DELETE RESTRICT,
    effect_ordinal integer NOT NULL CHECK (effect_ordinal >= 0),
    finding_id uuid NOT NULL,
    PRIMARY KEY (attempt_id, effect_ordinal),
    UNIQUE (attempt_id, finding_id),
    FOREIGN KEY (attempt_id, effect_ordinal, finding_id)
        REFERENCES review_orchestration_judgment_member(attempt_id, member_ordinal, finding_id)
        ON DELETE RESTRICT
);

CREATE TABLE review_orchestration_repair_inventory_seal (
    attempt_id uuid PRIMARY KEY REFERENCES review_orchestration_judgment_plan(attempt_id) ON DELETE RESTRICT
);

CREATE TABLE review_orchestration_repair_inventory (
    attempt_id uuid NOT NULL REFERENCES review_orchestration_repair_inventory_seal(attempt_id) ON DELETE RESTRICT,
    member_ordinal integer NOT NULL CHECK (member_ordinal >= 0),
    finding_id uuid NOT NULL REFERENCES review_finding(finding_id) ON DELETE RESTRICT,
    finding_run_id uuid NOT NULL REFERENCES review_run(run_id) ON DELETE RESTRICT,
    finding_pass_id uuid NOT NULL REFERENCES review_pass(pass_id) ON DELETE RESTRICT,
    PRIMARY KEY (attempt_id, member_ordinal),
    UNIQUE (attempt_id, finding_id)
);

CREATE TABLE review_orchestration_repair_outcome_seal (
    attempt_id uuid PRIMARY KEY REFERENCES review_orchestration_repair_inventory_seal(attempt_id) ON DELETE RESTRICT
);

CREATE TABLE review_orchestration_repair_outcome (
    attempt_id uuid NOT NULL REFERENCES review_orchestration_repair_outcome_seal(attempt_id) ON DELETE RESTRICT,
    member_ordinal integer NOT NULL CHECK (member_ordinal >= 0),
    finding_id uuid NOT NULL REFERENCES review_finding(finding_id) ON DELETE RESTRICT,
    outcome_kind text NOT NULL CHECK (outcome_kind IN ('fixed', 'failed', 'blocked', 'cancelled')),
    event_ordinal bigint CHECK (event_ordinal IS NULL OR event_ordinal BETWEEN 1 AND 4294967295),
    template_digest bytea,
    PRIMARY KEY (attempt_id, member_ordinal),
    UNIQUE (attempt_id, finding_id),
    CHECK ((outcome_kind = 'fixed' AND event_ordinal IS NOT NULL AND template_digest IS NOT NULL
            AND octet_length(template_digest) = 32)
        OR (outcome_kind <> 'fixed' AND event_ordinal IS NULL AND template_digest IS NULL))
);

CREATE TABLE review_orchestration_publication_inventory_seal (
    attempt_id uuid PRIMARY KEY REFERENCES review_orchestration_repair_outcome_seal(attempt_id) ON DELETE RESTRICT
);

CREATE TABLE review_orchestration_publication_inventory (
    attempt_id uuid NOT NULL REFERENCES review_orchestration_publication_inventory_seal(attempt_id) ON DELETE RESTRICT,
    member_ordinal integer NOT NULL CHECK (member_ordinal >= 0),
    finding_id uuid NOT NULL REFERENCES review_finding(finding_id) ON DELETE RESTRICT,
    finding_run_id uuid NOT NULL REFERENCES review_run(run_id) ON DELETE RESTRICT,
    finding_pass_id uuid NOT NULL REFERENCES review_pass(pass_id) ON DELETE RESTRICT,
    PRIMARY KEY (attempt_id, member_ordinal),
    UNIQUE (attempt_id, finding_id)
);

CREATE TABLE review_orchestration_publication_outcome_seal (
    attempt_id uuid PRIMARY KEY REFERENCES review_orchestration_publication_inventory_seal(attempt_id) ON DELETE RESTRICT
);

CREATE TABLE review_orchestration_publication_outcome (
    attempt_id uuid NOT NULL REFERENCES review_orchestration_publication_outcome_seal(attempt_id) ON DELETE RESTRICT,
    member_ordinal integer NOT NULL CHECK (member_ordinal >= 0),
    finding_id uuid NOT NULL REFERENCES review_finding(finding_id) ON DELETE RESTRICT,
    outcome_kind text NOT NULL CHECK (outcome_kind IN ('published', 'failed', 'blocked', 'cancelled')),
    external_link_id uuid REFERENCES review_external_link(external_link_id) ON DELETE RESTRICT,
    template_digest bytea,
    PRIMARY KEY (attempt_id, member_ordinal),
    UNIQUE (attempt_id, finding_id),
    CHECK ((outcome_kind = 'published' AND external_link_id IS NOT NULL
            AND template_digest IS NOT NULL AND octet_length(template_digest) = 32)
        OR (outcome_kind <> 'published' AND external_link_id IS NULL AND template_digest IS NULL))
);

CREATE TABLE review_orchestration_command (
    command_id uuid PRIMARY KEY,
    command_kind text NOT NULL CHECK (command_kind = 'review_orchestration'),
    storage_version smallint NOT NULL CHECK (storage_version = 1),
    semantic_digest bytea NOT NULL CHECK (octet_length(semantic_digest) = 32),
    attempt_id uuid NOT NULL REFERENCES review_orchestration_attempt(attempt_id) ON DELETE RESTRICT,
    operation_kind text NOT NULL CHECK (operation_kind IN
        ('start', 'import', 'concern', 'judgment_plan', 'judgment_effect', 'repair', 'publication')),
    result_stage text NOT NULL CHECK (result_stage IN
        ('started', 'awaiting_import', 'import_incomplete', 'awaiting_concerns',
         'fanout_incomplete', 'awaiting_judgment', 'awaiting_judgment_effects',
         'judgment_incomplete', 'awaiting_repair', 'repair_incomplete',
         'awaiting_publication', 'publication_incomplete', 'complete')),
    import_recorded boolean NOT NULL,
    concern_claim_count bigint NOT NULL CHECK (concern_claim_count >= 0),
    fanout_sealed boolean NOT NULL,
    judgment_plan_sealed boolean NOT NULL,
    judgment_effect_count bigint NOT NULL CHECK (judgment_effect_count >= 0),
    repair_inventory_count bigint,
    repair_outcomes_recorded boolean NOT NULL,
    publication_inventory_count bigint,
    publication_outcomes_recorded boolean NOT NULL,
    FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command(command_id, command_kind, storage_version)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

CREATE OR REPLACE FUNCTION require_durable_command_typed_record()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE matching_records bigint;
BEGIN
    CASE NEW.command_kind
        WHEN 'create_session' THEN SELECT count(*) INTO matching_records FROM create_session_command WHERE command_id = NEW.command_id;
        WHEN 'create_session_from_imported_frontier' THEN SELECT count(*) INTO matching_records FROM create_session_from_imported_frontier_command WHERE command_id = NEW.command_id;
        WHEN 'replace_session_defaults' THEN SELECT count(*) INTO matching_records FROM replace_session_defaults_command WHERE command_id = NEW.command_id;
        WHEN 'replace_session_metadata' THEN SELECT count(*) INTO matching_records FROM replace_session_metadata_command WHERE command_id = NEW.command_id;
        WHEN 'submit_input' THEN SELECT count(*) INTO matching_records FROM submit_input_command WHERE command_id = NEW.command_id;
        WHEN 'decide_tool_request' THEN SELECT count(*) INTO matching_records FROM decide_tool_request_command WHERE command_id = NEW.command_id;
        WHEN 'review_workflow' THEN SELECT count(*) INTO matching_records FROM review_workflow_command WHERE command_id = NEW.command_id;
        WHEN 'review_orchestration' THEN SELECT count(*) INTO matching_records FROM review_orchestration_command WHERE command_id = NEW.command_id;
        WHEN 'compact_session' THEN SELECT count(*) INTO matching_records FROM compact_session_command WHERE command_id = NEW.command_id;
        ELSE RAISE EXCEPTION 'unsupported durable command kind %', NEW.command_kind USING ERRCODE = '23514';
    END CASE;
    IF matching_records <> 1 THEN
        RAISE EXCEPTION 'durable command % requires exactly one % typed record', NEW.command_id, NEW.command_kind USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;

DO $$
DECLARE table_name text;
BEGIN
    FOREACH table_name IN ARRAY ARRAY[
        'review_orchestration_attempt', 'review_orchestration_concern',
        'review_orchestration_import', 'review_orchestration_concern_claim',
        'review_orchestration_concern_finding', 'review_orchestration_fanout_seal',
        'review_orchestration_fanout_member', 'review_orchestration_judgment_plan',
        'review_orchestration_judgment_member', 'review_orchestration_judgment_effect',
        'review_orchestration_repair_inventory_seal', 'review_orchestration_repair_inventory',
        'review_orchestration_repair_outcome_seal',
        'review_orchestration_repair_outcome',
        'review_orchestration_publication_inventory_seal',
        'review_orchestration_publication_inventory',
        'review_orchestration_publication_outcome_seal',
        'review_orchestration_publication_outcome',
        'review_orchestration_command'
    ] LOOP
        EXECUTE format(
            'CREATE TRIGGER %I_is_append_only BEFORE UPDATE OR DELETE ON %I FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change()',
            table_name, table_name
        );
        EXECUTE format(
            'CREATE TRIGGER %I_reject_truncate BEFORE TRUNCATE ON %I FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate()',
            table_name, table_name
        );
    END LOOP;
END;
$$;
