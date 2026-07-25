-- Review-workflow bounded context above session execution.
--
-- Immutable target, finding, event, and external-link evidence uses the shared
-- append-only trigger. Run/pass current projections are the deliberate mutable
-- exceptions and carry guarded evidence-shaped transitions.

ALTER TABLE turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_review_pass_origin_key
    UNIQUE (turn_id, session_id, origin_accepted_input_id),
    ADD CONSTRAINT turn_lifecycle_review_pass_terminal_key
    UNIQUE (
        turn_id,
        session_id,
        origin_accepted_input_id,
        terminal_frontier_id
    );

CREATE TABLE review_target (
    target_id uuid PRIMARY KEY,
    provider_key text NOT NULL,
    repository_key text NOT NULL,
    subject_kind text NOT NULL,
    change_request_number numeric(20, 0),
    head_revision text NOT NULL,
    base_revision text,
    stack_parent_target_id uuid,

    CONSTRAINT review_target_key_bounds
        CHECK (
            octet_length(provider_key) BETWEEN 1 AND 1024
            AND octet_length(repository_key) BETWEEN 1 AND 1024
            AND octet_length(head_revision) BETWEEN 1 AND 1024
            AND (
                base_revision IS NULL
                OR octet_length(base_revision) BETWEEN 1 AND 1024
            )
        ),
    CONSTRAINT review_target_subject_closed
        CHECK (subject_kind IN ('change_request', 'commit')),
    CONSTRAINT review_target_subject_shape
        CHECK (
            (
                subject_kind = 'change_request'
                AND change_request_number BETWEEN 1 AND 18446744073709551615
            )
            OR (
                subject_kind = 'commit'
                AND change_request_number IS NULL
            )
        ),
    CONSTRAINT review_target_not_self_parent
        CHECK (
            stack_parent_target_id IS NULL
            OR stack_parent_target_id <> target_id
        ),
    CONSTRAINT review_target_parent_fk
        FOREIGN KEY (stack_parent_target_id)
        REFERENCES review_target (target_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE INDEX review_target_repository_subject_index
    ON review_target (provider_key, repository_key, subject_kind);

CREATE TRIGGER review_target_is_append_only
BEFORE UPDATE OR DELETE ON review_target
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE review_run (
    run_id uuid PRIMARY KEY,
    target_id uuid NOT NULL,
    workflow_kind text NOT NULL,
    policy_version bigint NOT NULL,
    minimum_judge_confidence integer NOT NULL,
    minimum_publication_confidence integer NOT NULL,
    state_kind text NOT NULL,
    state_pass_id uuid,

    CONSTRAINT review_run_target_key
        UNIQUE (run_id, target_id),
    CONSTRAINT review_run_workflow_closed
        CHECK (
            workflow_kind IN (
                'import_external_context',
                'read_only_review',
                'judge_findings',
                'dedupe_findings',
                'publish_review',
                'fix_findings',
                'propagate_stack'
            )
        ),
    CONSTRAINT review_run_policy_version_positive_u32
        CHECK (policy_version BETWEEN 1 AND 4294967295),
    CONSTRAINT review_run_confidence_bounds
        CHECK (
            minimum_judge_confidence BETWEEN 0 AND 10000
            AND minimum_publication_confidence BETWEEN 0 AND 10000
            AND minimum_publication_confidence >= minimum_judge_confidence
        ),
    CONSTRAINT review_run_state_closed
        CHECK (
            state_kind IN (
                'queued',
                'running',
                'succeeded',
                'failed',
                'blocked',
                'cancelled'
            )
        ),
    CONSTRAINT review_run_state_shape
        CHECK (
            (
                state_kind = 'queued'
                AND state_pass_id IS NULL
            )
            OR (
                state_kind IN (
                    'running',
                    'succeeded',
                    'failed',
                    'blocked'
                )
                AND state_pass_id IS NOT NULL
            )
            OR state_kind = 'cancelled'
        ),
    CONSTRAINT review_run_target_fk
        FOREIGN KEY (target_id)
        REFERENCES review_target (target_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

CREATE INDEX review_run_target_index
    ON review_run (target_id, run_id);

CREATE TRIGGER review_run_reject_delete
BEFORE DELETE ON review_run
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE review_pass (
    pass_id uuid PRIMARY KEY,
    run_id uuid NOT NULL,
    target_id uuid NOT NULL,
    pass_kind text NOT NULL,
    session_id uuid NOT NULL,
    accepted_input_id uuid NOT NULL,
    state_kind text NOT NULL,
    turn_id uuid,
    output_frontier_id uuid,

    CONSTRAINT review_pass_ancestry_key
        UNIQUE (pass_id, run_id, target_id),
    CONSTRAINT review_pass_kind_closed
        CHECK (
            pass_kind IN (
                'import_external_context',
                'read_only_review',
                'judge',
                'dedupe',
                'publish',
                'fix',
                'propagate_stack'
            )
        ),
    CONSTRAINT review_pass_state_closed
        CHECK (
            state_kind IN (
                'queued',
                'running',
                'succeeded',
                'failed',
                'blocked',
                'cancelled'
            )
        ),
    CONSTRAINT review_pass_state_shape
        CHECK (
            (
                state_kind = 'queued'
                AND turn_id IS NULL
                AND output_frontier_id IS NULL
            )
            OR (
                state_kind IN ('running', 'failed', 'blocked')
                AND turn_id IS NOT NULL
                AND output_frontier_id IS NULL
            )
            OR (
                state_kind = 'succeeded'
                AND turn_id IS NOT NULL
                AND output_frontier_id IS NOT NULL
            )
            OR (
                state_kind = 'cancelled'
                AND output_frontier_id IS NULL
            )
        ),
    CONSTRAINT review_pass_run_fk
        FOREIGN KEY (run_id, target_id)
        REFERENCES review_run (run_id, target_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT review_pass_accepted_input_fk
        FOREIGN KEY (accepted_input_id, session_id)
        REFERENCES accepted_input (accepted_input_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT review_pass_turn_fk
        FOREIGN KEY (turn_id, session_id, accepted_input_id)
        REFERENCES turn_lifecycle (
            turn_id,
            session_id,
            origin_accepted_input_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT review_pass_terminal_frontier_fk
        FOREIGN KEY (
            turn_id,
            session_id,
            accepted_input_id,
            output_frontier_id
        )
        REFERENCES turn_lifecycle (
            turn_id,
            session_id,
            origin_accepted_input_id,
            terminal_frontier_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE INDEX review_pass_run_index
    ON review_pass (run_id, target_id, pass_id);
CREATE INDEX review_pass_session_input_index
    ON review_pass (session_id, accepted_input_id);

ALTER TABLE review_run
    ADD CONSTRAINT review_run_state_pass_fk
    FOREIGN KEY (state_pass_id, run_id, target_id)
    REFERENCES review_pass (pass_id, run_id, target_id)
    ON UPDATE RESTRICT
    ON DELETE RESTRICT
    DEFERRABLE INITIALLY DEFERRED;

CREATE FUNCTION guard_review_run_update()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF (NEW.run_id, NEW.target_id, NEW.workflow_kind, NEW.policy_version,
        NEW.minimum_judge_confidence, NEW.minimum_publication_confidence)
       IS DISTINCT FROM
       (OLD.run_id, OLD.target_id, OLD.workflow_kind, OLD.policy_version,
        OLD.minimum_judge_confidence, OLD.minimum_publication_confidence)
    THEN
        RAISE EXCEPTION 'review run immutable facts cannot change'
            USING ERRCODE = '23514';
    END IF;

    IF (NEW.state_kind, NEW.state_pass_id)
       IS NOT DISTINCT FROM
       (OLD.state_kind, OLD.state_pass_id)
    THEN
        RETURN NEW;
    END IF;

    IF OLD.state_kind = 'queued' THEN
        IF NOT (
            NEW.state_kind = 'running'
            OR (
                NEW.state_kind = 'cancelled'
                AND NEW.state_pass_id IS NULL
            )
        ) THEN
            RAISE EXCEPTION 'invalid queued review run transition'
                USING ERRCODE = '23514';
        END IF;
    ELSIF OLD.state_kind = 'running' THEN
        IF NOT (
            NEW.state_kind IN (
                'succeeded',
                'failed',
                'blocked',
                'cancelled'
            )
            AND NEW.state_pass_id = OLD.state_pass_id
        ) THEN
            RAISE EXCEPTION 'invalid running review run transition'
                USING ERRCODE = '23514';
        END IF;
    ELSE
        RAISE EXCEPTION 'terminal review run cannot transition'
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER review_run_update_is_guarded
BEFORE UPDATE ON review_run
FOR EACH ROW
EXECUTE FUNCTION guard_review_run_update();

CREATE FUNCTION guard_review_pass_update()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF (NEW.pass_id, NEW.run_id, NEW.target_id, NEW.pass_kind,
        NEW.session_id, NEW.accepted_input_id)
       IS DISTINCT FROM
       (OLD.pass_id, OLD.run_id, OLD.target_id, OLD.pass_kind,
        OLD.session_id, OLD.accepted_input_id)
    THEN
        RAISE EXCEPTION 'review pass immutable facts cannot change'
            USING ERRCODE = '23514';
    END IF;

    IF (NEW.state_kind, NEW.turn_id, NEW.output_frontier_id)
       IS NOT DISTINCT FROM
       (OLD.state_kind, OLD.turn_id, OLD.output_frontier_id)
    THEN
        RETURN NEW;
    END IF;

    IF OLD.state_kind = 'queued' THEN
        IF NOT (
            NEW.state_kind = 'running'
            OR (
                NEW.state_kind = 'cancelled'
                AND NEW.turn_id IS NULL
            )
        ) THEN
            RAISE EXCEPTION 'invalid queued review pass transition'
                USING ERRCODE = '23514';
        END IF;
    ELSIF OLD.state_kind = 'running' THEN
        IF NOT (
            NEW.state_kind IN (
                'succeeded',
                'failed',
                'blocked',
                'cancelled'
            )
            AND NEW.turn_id = OLD.turn_id
        ) THEN
            RAISE EXCEPTION 'invalid running review pass transition'
                USING ERRCODE = '23514';
        END IF;
    ELSE
        RAISE EXCEPTION 'terminal review pass cannot transition'
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER review_pass_update_is_guarded
BEFORE UPDATE ON review_pass
FOR EACH ROW
EXECUTE FUNCTION guard_review_pass_update();

CREATE TRIGGER review_pass_reject_delete
BEFORE DELETE ON review_pass
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE review_finding (
    finding_id uuid PRIMARY KEY,
    run_id uuid NOT NULL,
    target_id uuid NOT NULL,
    producing_pass_id uuid NOT NULL,
    file_path text NOT NULL,
    line_start bigint,
    line_end bigint,
    diff_side text NOT NULL,
    title text NOT NULL,
    body text NOT NULL,
    severity text NOT NULL,
    confidence integer NOT NULL,
    category text NOT NULL,
    recommended_fix text,

    CONSTRAINT review_finding_ancestry_key
        UNIQUE (finding_id, run_id, target_id),
    CONSTRAINT review_finding_key_bounds
        CHECK (
            octet_length(file_path) BETWEEN 1 AND 1024
            AND octet_length(category) BETWEEN 1 AND 1024
        ),
    CONSTRAINT review_finding_text_bounds
        CHECK (
            octet_length(title) BETWEEN 1 AND 65536
            AND octet_length(body) BETWEEN 1 AND 65536
            AND (
                recommended_fix IS NULL
                OR octet_length(recommended_fix) BETWEEN 1 AND 65536
            )
        ),
    CONSTRAINT review_finding_line_shape
        CHECK (
            (
                line_start IS NULL
                AND line_end IS NULL
            )
            OR (
                line_start BETWEEN 1 AND 4294967295
                AND line_end BETWEEN line_start AND 4294967295
            )
        ),
    CONSTRAINT review_finding_diff_side_closed
        CHECK (diff_side IN ('left', 'right')),
    CONSTRAINT review_finding_severity_closed
        CHECK (
            severity IN (
                'info',
                'low',
                'medium',
                'high',
                'critical'
            )
        ),
    CONSTRAINT review_finding_confidence_bounds
        CHECK (confidence BETWEEN 0 AND 10000),
    CONSTRAINT review_finding_producing_pass_fk
        FOREIGN KEY (producing_pass_id, run_id, target_id)
        REFERENCES review_pass (pass_id, run_id, target_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

CREATE INDEX review_finding_run_index
    ON review_finding (run_id, target_id, finding_id);

CREATE TRIGGER review_finding_is_append_only
BEFORE UPDATE OR DELETE ON review_finding
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE review_external_link (
    external_link_id uuid PRIMARY KEY,
    target_id uuid NOT NULL,
    association_kind text NOT NULL,
    run_id uuid,
    finding_id uuid,
    provider_key text NOT NULL,
    object_kind text NOT NULL,

    CONSTRAINT review_external_link_target_key
        UNIQUE (external_link_id, target_id),
    CONSTRAINT review_external_link_payload_key
        UNIQUE (
            external_link_id,
            target_id,
            run_id,
            finding_id,
            association_kind
        ),
    CONSTRAINT review_external_link_attachment_key
        UNIQUE (
            external_link_id,
            target_id,
            provider_key,
            object_kind
        ),
    CONSTRAINT review_external_link_association_closed
        CHECK (association_kind IN ('target', 'run', 'finding')),
    CONSTRAINT review_external_link_association_shape
        CHECK (
            (
                association_kind = 'target'
                AND run_id IS NULL
                AND finding_id IS NULL
            )
            OR (
                association_kind = 'run'
                AND run_id IS NOT NULL
                AND finding_id IS NULL
            )
            OR (
                association_kind = 'finding'
                AND run_id IS NOT NULL
                AND finding_id IS NOT NULL
            )
        ),
    CONSTRAINT review_external_link_provider_bound
        CHECK (octet_length(provider_key) BETWEEN 1 AND 1024),
    CONSTRAINT review_external_link_object_kind_closed
        CHECK (
            object_kind IN (
                'change_request',
                'commit',
                'review',
                'review_thread',
                'review_comment',
                'change_request_comment'
            )
        ),
    CONSTRAINT review_external_link_target_fk
        FOREIGN KEY (target_id)
        REFERENCES review_target (target_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT review_external_link_run_fk
        FOREIGN KEY (run_id, target_id)
        REFERENCES review_run (run_id, target_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT review_external_link_finding_fk
        FOREIGN KEY (finding_id, run_id, target_id)
        REFERENCES review_finding (finding_id, run_id, target_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

CREATE INDEX review_external_link_association_index
    ON review_external_link (
        target_id,
        association_kind,
        run_id,
        finding_id
    );

CREATE TRIGGER review_external_link_is_append_only
BEFORE UPDATE OR DELETE ON review_external_link
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE review_external_link_attachment (
    external_link_id uuid PRIMARY KEY,
    target_id uuid NOT NULL,
    pass_run_id uuid NOT NULL,
    pass_id uuid NOT NULL,
    provider_key text NOT NULL,
    object_kind text NOT NULL,
    external_object_key text NOT NULL,

    CONSTRAINT review_external_link_attachment_target_key
        UNIQUE (external_link_id, target_id),
    CONSTRAINT review_external_object_identity_unique
        UNIQUE (provider_key, object_kind, external_object_key),
    CONSTRAINT review_external_link_attachment_object_bound
        CHECK (octet_length(external_object_key) BETWEEN 1 AND 1024),
    CONSTRAINT review_external_link_attachment_reservation_fk
        FOREIGN KEY (
            external_link_id,
            target_id,
            provider_key,
            object_kind
        )
        REFERENCES review_external_link (
            external_link_id,
            target_id,
            provider_key,
            object_kind
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT review_external_link_attachment_pass_fk
        FOREIGN KEY (pass_id, pass_run_id, target_id)
        REFERENCES review_pass (pass_id, run_id, target_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

CREATE TRIGGER review_external_link_attachment_is_append_only
BEFORE UPDATE OR DELETE ON review_external_link_attachment
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE review_finding_event (
    finding_id uuid NOT NULL,
    event_ordinal bigint NOT NULL,
    finding_run_id uuid NOT NULL,
    target_id uuid NOT NULL,
    event_pass_id uuid NOT NULL,
    event_pass_run_id uuid NOT NULL,
    event_kind text NOT NULL,
    reason text,
    referenced_finding_id uuid,
    external_link_id uuid,
    external_link_association_kind text,

    CONSTRAINT review_finding_event_pk
        PRIMARY KEY (finding_id, event_ordinal),
    CONSTRAINT review_finding_event_ordinal_positive_u32
        CHECK (event_ordinal BETWEEN 1 AND 4294967295),
    CONSTRAINT review_finding_event_kind_closed
        CHECK (
            event_kind IN (
                'accepted',
                'rejected',
                'duplicate',
                'superseded',
                'stale',
                'posted',
                'fixed',
                'blocked_with_reason'
            )
        ),
    CONSTRAINT review_finding_event_shape
        CHECK (
            (
                event_kind IN ('accepted', 'stale', 'fixed')
                AND reason IS NULL
                AND referenced_finding_id IS NULL
                AND external_link_id IS NULL
                AND external_link_association_kind IS NULL
            )
            OR (
                event_kind IN ('rejected', 'blocked_with_reason')
                AND octet_length(reason) BETWEEN 1 AND 65536
                AND referenced_finding_id IS NULL
                AND external_link_id IS NULL
                AND external_link_association_kind IS NULL
            )
            OR (
                event_kind IN ('duplicate', 'superseded')
                AND reason IS NULL
                AND referenced_finding_id IS NOT NULL
                AND referenced_finding_id <> finding_id
                AND external_link_id IS NULL
                AND external_link_association_kind IS NULL
            )
            OR (
                event_kind = 'posted'
                AND reason IS NULL
                AND referenced_finding_id IS NULL
                AND external_link_id IS NOT NULL
                AND external_link_association_kind = 'finding'
            )
        ),
    CONSTRAINT review_finding_event_finding_fk
        FOREIGN KEY (finding_id, finding_run_id, target_id)
        REFERENCES review_finding (finding_id, run_id, target_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT review_finding_event_pass_fk
        FOREIGN KEY (event_pass_id, event_pass_run_id, target_id)
        REFERENCES review_pass (pass_id, run_id, target_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT review_finding_event_referenced_finding_fk
        FOREIGN KEY (
            referenced_finding_id,
            finding_run_id,
            target_id
        )
        REFERENCES review_finding (finding_id, run_id, target_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT review_finding_event_external_link_fk
        FOREIGN KEY (
            external_link_id,
            target_id,
            finding_run_id,
            finding_id,
            external_link_association_kind
        )
        REFERENCES review_external_link (
            external_link_id,
            target_id,
            run_id,
            finding_id,
            association_kind
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE INDEX review_finding_event_pass_index
    ON review_finding_event (
        event_pass_run_id,
        target_id,
        event_pass_id
    );

CREATE FUNCTION require_review_finding_event_sequence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    previous_kind text;
    previous_status text;
    expected_ordinal bigint;
BEGIN
    PERFORM 1
      FROM review_finding
     WHERE finding_id = NEW.finding_id
     FOR UPDATE;

    SELECT event_kind
      INTO previous_kind
      FROM review_finding_event
     WHERE finding_id = NEW.finding_id
     ORDER BY event_ordinal DESC
     LIMIT 1;

    SELECT COALESCE(max(event_ordinal), 0) + 1
      INTO expected_ordinal
      FROM review_finding_event
     WHERE finding_id = NEW.finding_id;

    IF NEW.event_ordinal <> expected_ordinal THEN
        RAISE EXCEPTION
            'finding event ordinal %, expected %',
            NEW.event_ordinal,
            expected_ordinal
            USING ERRCODE = '23514';
    END IF;

    previous_status := CASE previous_kind
        WHEN 'accepted' THEN 'accepted'
        WHEN 'rejected' THEN 'rejected'
        WHEN 'duplicate' THEN 'duplicate'
        WHEN 'superseded' THEN 'superseded'
        WHEN 'stale' THEN 'stale'
        WHEN 'posted' THEN 'posted'
        WHEN 'fixed' THEN 'fixed'
        WHEN 'blocked_with_reason' THEN 'blocked_with_reason'
        ELSE 'open'
    END;

    IF NOT (
        (
            previous_status = 'open'
            AND NEW.event_kind IN (
                'accepted',
                'rejected',
                'duplicate',
                'superseded',
                'stale'
            )
        )
        OR (
            previous_status = 'accepted'
            AND NEW.event_kind IN (
                'duplicate',
                'superseded',
                'stale',
                'posted',
                'fixed',
                'blocked_with_reason'
            )
        )
        OR (
            previous_status = 'posted'
            AND NEW.event_kind IN (
                'superseded',
                'stale',
                'fixed',
                'blocked_with_reason'
            )
        )
        OR (
            previous_status = 'blocked_with_reason'
            AND NEW.event_kind IN ('superseded', 'stale', 'fixed')
        )
    ) THEN
        RAISE EXCEPTION
            'invalid finding transition from % through %',
            previous_status,
            NEW.event_kind
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER review_finding_event_sequence_is_guarded
BEFORE INSERT ON review_finding_event
FOR EACH ROW
EXECUTE FUNCTION require_review_finding_event_sequence();

CREATE TRIGGER review_finding_event_is_append_only
BEFORE UPDATE OR DELETE ON review_finding_event
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE review_external_link_observation (
    external_link_id uuid NOT NULL,
    observation_ordinal bigint NOT NULL,
    target_id uuid NOT NULL,
    pass_run_id uuid NOT NULL,
    pass_id uuid NOT NULL,
    object_state text NOT NULL,

    CONSTRAINT review_external_link_observation_pk
        PRIMARY KEY (external_link_id, observation_ordinal),
    CONSTRAINT review_external_link_observation_ordinal_positive_u32
        CHECK (observation_ordinal BETWEEN 1 AND 4294967295),
    CONSTRAINT review_external_link_observation_state_closed
        CHECK (object_state IN ('current', 'outdated', 'resolved')),
    CONSTRAINT review_external_link_observation_attachment_fk
        FOREIGN KEY (external_link_id, target_id)
        REFERENCES review_external_link_attachment (
            external_link_id,
            target_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT review_external_link_observation_pass_fk
        FOREIGN KEY (pass_id, pass_run_id, target_id)
        REFERENCES review_pass (pass_id, run_id, target_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

CREATE FUNCTION require_review_external_observation_sequence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    expected_ordinal bigint;
BEGIN
    PERFORM 1
      FROM review_external_link
     WHERE external_link_id = NEW.external_link_id
     FOR UPDATE;

    SELECT COALESCE(max(observation_ordinal), 0) + 1
      INTO expected_ordinal
      FROM review_external_link_observation
     WHERE external_link_id = NEW.external_link_id;

    IF NEW.observation_ordinal <> expected_ordinal THEN
        RAISE EXCEPTION
            'external observation ordinal %, expected %',
            NEW.observation_ordinal,
            expected_ordinal
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER review_external_link_observation_sequence_is_guarded
BEFORE INSERT ON review_external_link_observation
FOR EACH ROW
EXECUTE FUNCTION require_review_external_observation_sequence();

CREATE TRIGGER review_external_link_observation_is_append_only
BEFORE UPDATE OR DELETE ON review_external_link_observation
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();
