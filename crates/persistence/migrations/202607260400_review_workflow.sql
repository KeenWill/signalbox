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
                AND change_request_number IS NOT NULL
                AND change_request_number BETWEEN 1 AND 18446744073709551615
                AND base_revision IS NOT NULL
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
    CONSTRAINT review_target_parent_has_base
        CHECK (
            stack_parent_target_id IS NULL
            OR base_revision IS NOT NULL
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

CREATE FUNCTION guard_review_target_parent()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    parent_provider text;
    parent_repository text;
    parent_head text;
BEGIN
    IF NEW.stack_parent_target_id IS NULL THEN
        RETURN NEW;
    END IF;
    SELECT provider_key, repository_key, head_revision
      INTO parent_provider, parent_repository, parent_head
      FROM review_target
     WHERE target_id = NEW.stack_parent_target_id;
    IF NOT FOUND
       OR parent_provider IS DISTINCT FROM NEW.provider_key
       OR parent_repository IS DISTINCT FROM NEW.repository_key
       OR parent_head IS DISTINCT FROM NEW.base_revision
    THEN
        RAISE EXCEPTION
            'review target stack parent does not match repository and base'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.subject_kind = 'change_request'
       AND EXISTS (
           WITH RECURSIVE ancestors AS (
               SELECT target_id, stack_parent_target_id,
                      provider_key, repository_key,
                      subject_kind, change_request_number
                 FROM review_target
                WHERE target_id = NEW.stack_parent_target_id
               UNION ALL
               SELECT parent.target_id, parent.stack_parent_target_id,
                      parent.provider_key, parent.repository_key,
                      parent.subject_kind, parent.change_request_number
                 FROM review_target AS parent
                 JOIN ancestors
                   ON parent.target_id = ancestors.stack_parent_target_id
           )
           SELECT 1
             FROM ancestors
            WHERE provider_key = NEW.provider_key
              AND repository_key = NEW.repository_key
              AND subject_kind = 'change_request'
              AND change_request_number = NEW.change_request_number
       )
    THEN
        RAISE EXCEPTION
            'review target stack repeats a logical change request'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER review_target_parent_is_guarded
BEFORE INSERT ON review_target
FOR EACH ROW
EXECUTE FUNCTION guard_review_target_parent();

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
            policy_version = 1
            AND minimum_judge_confidence = 7000
            AND minimum_publication_confidence = 8000
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
    origin_turn_id uuid NOT NULL,
    state_kind text NOT NULL,
    turn_id uuid,
    output_frontier_id uuid,
    result_kind text,
    result_finding_id uuid,
    result_finding_run_id uuid,
    result_finding_pass_id uuid,
    result_event_ordinal bigint,
    result_event_kind text,
    result_reason text,
    result_referenced_finding_id uuid,
    result_referenced_finding_run_id uuid,
    result_referenced_finding_pass_id uuid,
    result_referenced_finding_status text,
    result_external_link_id uuid,
    result_external_object_key text,
    result_observation_state text,

    CONSTRAINT review_pass_ancestry_key
        UNIQUE (pass_id, run_id, target_id),
    CONSTRAINT review_pass_run_unique
        UNIQUE (run_id, target_id),
    CONSTRAINT review_pass_accepted_input_unique
        UNIQUE (accepted_input_id),
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
                AND result_kind IS NULL
            )
            OR (
                state_kind IN ('running', 'failed', 'blocked')
                AND turn_id IS NOT NULL
                AND output_frontier_id IS NULL
                AND (
                    state_kind = 'blocked'
                    OR result_kind IS NULL
                )
            )
            OR (
                state_kind = 'succeeded'
                AND turn_id IS NOT NULL
                AND output_frontier_id IS NOT NULL
            )
            OR (
                state_kind = 'cancelled'
                AND output_frontier_id IS NULL
                AND result_kind IS NULL
            )
        ),
    CONSTRAINT review_pass_result_kind_closed
        CHECK (
            result_kind IS NULL
            OR result_kind IN (
                'produced_findings',
                'finding_event',
                'external_link_attachment',
                'external_link_observation',
                'external_link_no_change',
                'external_link_publication_blocked'
            )
        ),
    CONSTRAINT review_pass_result_event_ordinal_positive_u32
        CHECK (
            result_event_ordinal IS NULL
            OR result_event_ordinal BETWEEN 1 AND 4294967295
        ),
    CONSTRAINT review_pass_result_event_kind_closed
        CHECK (
            result_event_kind IS NULL
            OR result_event_kind IN (
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
    CONSTRAINT review_pass_result_reference_status_closed
        CHECK (
            result_referenced_finding_status IS NULL
            OR result_referenced_finding_status IN ('open', 'accepted')
        ),
    CONSTRAINT review_pass_result_observation_state_closed
        CHECK (
            result_observation_state IS NULL
            OR result_observation_state IN (
                'current',
                'outdated',
                'resolved'
            )
        ),
    CONSTRAINT review_pass_result_text_bounds
        CHECK (
            (
                result_reason IS NULL
                OR octet_length(result_reason) BETWEEN 1 AND 65536
            )
            AND (
                result_external_object_key IS NULL
                OR octet_length(result_external_object_key)
                    BETWEEN 1 AND 1024
            )
        ),
    CONSTRAINT review_pass_result_shape
        CHECK (
            (
                result_kind IS NULL
                AND result_finding_id IS NULL
                AND result_finding_run_id IS NULL
                AND result_finding_pass_id IS NULL
                AND result_event_ordinal IS NULL
                AND result_event_kind IS NULL
                AND result_reason IS NULL
                AND result_referenced_finding_id IS NULL
                AND result_referenced_finding_run_id IS NULL
                AND result_referenced_finding_pass_id IS NULL
                AND result_referenced_finding_status IS NULL
                AND result_external_link_id IS NULL
                AND result_external_object_key IS NULL
                AND result_observation_state IS NULL
            )
            OR (
                result_kind = 'produced_findings'
                AND result_finding_id IS NULL
                AND result_finding_run_id IS NULL
                AND result_finding_pass_id IS NULL
                AND result_event_ordinal IS NULL
                AND result_event_kind IS NULL
                AND result_reason IS NULL
                AND result_referenced_finding_id IS NULL
                AND result_referenced_finding_run_id IS NULL
                AND result_referenced_finding_pass_id IS NULL
                AND result_referenced_finding_status IS NULL
                AND result_external_link_id IS NULL
                AND result_external_object_key IS NULL
                AND result_observation_state IS NULL
            )
            OR (
                result_kind = 'finding_event'
                AND result_finding_id IS NOT NULL
                AND result_finding_run_id IS NOT NULL
                AND result_finding_pass_id IS NOT NULL
                AND result_event_ordinal IS NOT NULL
                AND result_event_kind IS NOT NULL
                AND result_event_kind <> 'posted'
                AND result_external_object_key IS NULL
                AND result_observation_state IS NULL
                AND (
                    (
                        result_event_kind IN (
                            'accepted',
                            'stale',
                            'fixed'
                        )
                        AND result_reason IS NULL
                        AND result_referenced_finding_id IS NULL
                        AND result_referenced_finding_run_id IS NULL
                        AND result_referenced_finding_pass_id IS NULL
                        AND result_referenced_finding_status IS NULL
                        AND result_external_link_id IS NULL
                    )
                    OR (
                        result_event_kind = 'rejected'
                        AND result_reason IS NOT NULL
                        AND result_referenced_finding_id IS NULL
                        AND result_referenced_finding_run_id IS NULL
                        AND result_referenced_finding_pass_id IS NULL
                        AND result_referenced_finding_status IS NULL
                        AND result_external_link_id IS NULL
                    )
                    OR (
                        result_event_kind = 'blocked_with_reason'
                        AND result_reason IS NOT NULL
                        AND result_referenced_finding_id IS NULL
                        AND result_referenced_finding_run_id IS NULL
                        AND result_referenced_finding_pass_id IS NULL
                        AND result_referenced_finding_status IS NULL
                    )
                    OR (
                        result_event_kind IN (
                            'duplicate',
                            'superseded'
                        )
                        AND result_reason IS NULL
                        AND result_referenced_finding_id IS NOT NULL
                        AND result_referenced_finding_run_id IS NOT NULL
                        AND result_referenced_finding_pass_id IS NOT NULL
                        AND result_referenced_finding_status IS NOT NULL
                        AND result_referenced_finding_status
                            IN ('open', 'accepted')
                        AND result_external_link_id IS NULL
                    )
                )
            )
            OR (
                result_kind = 'external_link_attachment'
                AND result_external_link_id IS NOT NULL
                AND result_external_object_key IS NOT NULL
                AND result_observation_state IS NULL
                AND (
                    (
                        result_finding_id IS NULL
                        AND result_finding_run_id IS NULL
                        AND result_finding_pass_id IS NULL
                        AND result_event_ordinal IS NULL
                        AND result_event_kind IS NULL
                        AND result_reason IS NULL
                        AND result_referenced_finding_id IS NULL
                        AND result_referenced_finding_run_id IS NULL
                        AND result_referenced_finding_pass_id IS NULL
                        AND result_referenced_finding_status IS NULL
                    )
                    OR (
                        result_finding_id IS NOT NULL
                        AND result_finding_run_id IS NOT NULL
                        AND result_finding_pass_id IS NOT NULL
                        AND result_event_ordinal IS NOT NULL
                        AND result_event_kind IS NOT NULL
                        AND result_event_kind = 'posted'
                        AND result_reason IS NULL
                        AND result_referenced_finding_id IS NULL
                        AND result_referenced_finding_run_id IS NULL
                        AND result_referenced_finding_pass_id IS NULL
                        AND result_referenced_finding_status IS NULL
                    )
                )
            )
            OR (
                result_kind = 'external_link_observation'
                AND result_finding_id IS NULL
                AND result_finding_run_id IS NULL
                AND result_finding_pass_id IS NULL
                AND result_event_ordinal IS NOT NULL
                AND result_event_kind IS NULL
                AND result_reason IS NULL
                AND result_referenced_finding_id IS NULL
                AND result_referenced_finding_run_id IS NULL
                AND result_referenced_finding_pass_id IS NULL
                AND result_referenced_finding_status IS NULL
                AND result_external_link_id IS NOT NULL
                AND result_external_object_key IS NULL
                AND result_observation_state IS NOT NULL
            )
            OR (
                result_kind = 'external_link_no_change'
                AND result_finding_id IS NULL
                AND result_finding_run_id IS NULL
                AND result_finding_pass_id IS NULL
                AND result_event_ordinal IS NOT NULL
                AND result_event_kind IS NULL
                AND result_reason IS NULL
                AND result_referenced_finding_id IS NULL
                AND result_referenced_finding_run_id IS NULL
                AND result_referenced_finding_pass_id IS NULL
                AND result_referenced_finding_status IS NULL
                AND result_external_link_id IS NOT NULL
                AND result_external_object_key IS NULL
                AND result_observation_state IS NOT NULL
            )
            OR (
                result_kind = 'external_link_publication_blocked'
                AND result_finding_id IS NULL
                AND result_finding_run_id IS NULL
                AND result_finding_pass_id IS NULL
                AND result_event_ordinal IS NULL
                AND result_event_kind IS NULL
                AND result_reason IS NOT NULL
                AND result_referenced_finding_id IS NULL
                AND result_referenced_finding_run_id IS NULL
                AND result_referenced_finding_pass_id IS NULL
                AND result_referenced_finding_status IS NULL
                AND result_external_link_id IS NOT NULL
                AND result_external_object_key IS NULL
                AND result_observation_state IS NULL
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
    CONSTRAINT review_pass_origin_turn_fk
        FOREIGN KEY (origin_turn_id, session_id, accepted_input_id)
        REFERENCES turn_lifecycle (
            turn_id,
            session_id,
            origin_accepted_input_id
        )
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
CREATE INDEX review_pass_external_link_result_index
    ON review_pass (result_external_link_id, pass_id)
    WHERE result_external_link_id IS NOT NULL;
ALTER TABLE review_run
    ADD CONSTRAINT review_run_state_pass_fk
    FOREIGN KEY (state_pass_id, run_id, target_id)
    REFERENCES review_pass (pass_id, run_id, target_id)
    ON UPDATE RESTRICT
    ON DELETE RESTRICT
    DEFERRABLE INITIALLY DEFERRED;

CREATE FUNCTION guard_review_run_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    canonical_pass_state text;
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.state_kind IS DISTINCT FROM 'queued'
           OR NEW.state_pass_id IS NOT NULL
        THEN
            RAISE EXCEPTION 'review run must begin queued'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;

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
            OR NEW.state_kind = 'cancelled'
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
            AND NEW.state_pass_id IS NOT DISTINCT FROM OLD.state_pass_id
        ) THEN
            RAISE EXCEPTION 'invalid running review run transition'
                USING ERRCODE = '23514';
        END IF;
    ELSE
        RAISE EXCEPTION 'terminal review run cannot transition'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.state_pass_id IS NOT NULL THEN
        SELECT state_kind
          INTO canonical_pass_state
          FROM review_pass
         WHERE pass_id = NEW.state_pass_id
           AND run_id = NEW.run_id
           AND target_id = NEW.target_id;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'review run referenced pass is missing'
                USING ERRCODE = '23514';
        END IF;
        IF canonical_pass_state IS DISTINCT FROM NEW.state_kind THEN
            RAISE EXCEPTION 'review run state contradicts canonical pass'
                USING ERRCODE = '23514';
        END IF;
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER review_run_change_is_guarded
BEFORE INSERT OR UPDATE ON review_run
FOR EACH ROW
EXECUTE FUNCTION guard_review_run_change();

CREATE FUNCTION guard_review_pass_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    canonical_run_workflow text;
    canonical_turn_state text;
    canonical_turn_disposition text;
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.state_kind IS DISTINCT FROM 'queued'
           OR NEW.turn_id IS NOT NULL
           OR NEW.output_frontier_id IS NOT NULL
        THEN
            RAISE EXCEPTION 'review pass must begin queued'
                USING ERRCODE = '23514';
        END IF;
        SELECT workflow_kind
          INTO canonical_run_workflow
          FROM review_run
         WHERE run_id = NEW.run_id
           AND target_id = NEW.target_id;
        IF canonical_run_workflow IS NULL
           OR NOT (
               (canonical_run_workflow = 'import_external_context'
                AND NEW.pass_kind = 'import_external_context')
               OR (canonical_run_workflow = 'read_only_review'
                   AND NEW.pass_kind = 'read_only_review')
               OR (canonical_run_workflow = 'judge_findings'
                   AND NEW.pass_kind = 'judge')
               OR (canonical_run_workflow = 'dedupe_findings'
                   AND NEW.pass_kind = 'dedupe')
               OR (canonical_run_workflow = 'publish_review'
                   AND NEW.pass_kind = 'publish')
               OR (canonical_run_workflow = 'fix_findings'
                   AND NEW.pass_kind = 'fix')
               OR (canonical_run_workflow = 'propagate_stack'
                   AND NEW.pass_kind = 'propagate_stack')
           )
        THEN
            RAISE EXCEPTION
                'review pass kind % contradicts run workflow %',
                NEW.pass_kind,
                canonical_run_workflow
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;

    IF (NEW.pass_id, NEW.run_id, NEW.target_id, NEW.pass_kind,
        NEW.session_id, NEW.accepted_input_id, NEW.origin_turn_id)
       IS DISTINCT FROM
       (OLD.pass_id, OLD.run_id, OLD.target_id, OLD.pass_kind,
        OLD.session_id, OLD.accepted_input_id, OLD.origin_turn_id)
    THEN
        RAISE EXCEPTION 'review pass immutable facts cannot change'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.result_kind IS NOT NULL
       AND (
           NEW.result_kind,
           NEW.result_finding_id,
           NEW.result_finding_run_id,
           NEW.result_finding_pass_id,
           NEW.result_event_ordinal,
           NEW.result_event_kind,
           NEW.result_reason,
           NEW.result_referenced_finding_id,
           NEW.result_referenced_finding_run_id,
           NEW.result_referenced_finding_pass_id,
           NEW.result_referenced_finding_status,
           NEW.result_external_link_id,
           NEW.result_external_object_key,
           NEW.result_observation_state
       ) IS DISTINCT FROM (
           OLD.result_kind,
           OLD.result_finding_id,
           OLD.result_finding_run_id,
           OLD.result_finding_pass_id,
           OLD.result_event_ordinal,
           OLD.result_event_kind,
           OLD.result_reason,
           OLD.result_referenced_finding_id,
           OLD.result_referenced_finding_run_id,
           OLD.result_referenced_finding_pass_id,
           OLD.result_referenced_finding_status,
           OLD.result_external_link_id,
           OLD.result_external_object_key,
           OLD.result_observation_state
       )
    THEN
        RAISE EXCEPTION 'bound review pass result cannot change'
            USING ERRCODE = '23514';
    END IF;

    IF (NEW.state_kind, NEW.turn_id, NEW.output_frontier_id)
       IS NOT DISTINCT FROM
       (OLD.state_kind, OLD.turn_id, OLD.output_frontier_id)
    THEN
        IF OLD.result_kind IS NULL
           AND NEW.result_kind IS NOT NULL
           AND NOT (
               (
                   NEW.result_kind = 'produced_findings'
                   AND NEW.state_kind = 'succeeded'
                   AND NEW.pass_kind = 'read_only_review'
               )
               OR (
                   NEW.result_kind = 'finding_event'
                   AND (
                       (
                           NEW.result_event_kind IN (
                               'accepted',
                               'rejected',
                               'stale'
                           )
                           AND NEW.state_kind = 'succeeded'
                           AND NEW.pass_kind = 'judge'
                       )
                       OR (
                           NEW.result_event_kind IN (
                               'duplicate',
                               'superseded'
                           )
                           AND NEW.state_kind = 'succeeded'
                           AND NEW.pass_kind = 'dedupe'
                       )
                       OR (
                           NEW.result_event_kind = 'fixed'
                           AND NEW.state_kind = 'succeeded'
                           AND NEW.pass_kind = 'fix'
                       )
                       OR (
                           NEW.result_event_kind = 'blocked_with_reason'
                           AND NEW.state_kind = 'blocked'
                           AND (
                               (
                                   NEW.pass_kind = 'publish'
                                   AND NEW.result_external_link_id
                                       IS NOT NULL
                               )
                               OR (
                                   NEW.pass_kind = 'fix'
                                   AND NEW.result_external_link_id IS NULL
                               )
                           )
                       )
                   )
               )
               OR (
                   NEW.result_kind = 'external_link_attachment'
                   AND NEW.state_kind = 'succeeded'
                   AND NEW.pass_kind IN (
                       'publish',
                       'import_external_context'
                   )
               )
               OR (
                   NEW.result_kind = 'external_link_observation'
                   AND NEW.state_kind = 'succeeded'
                   AND NEW.pass_kind = 'import_external_context'
               )
               OR (
                   NEW.result_kind = 'external_link_no_change'
                   AND NEW.state_kind = 'succeeded'
                   AND NEW.pass_kind = 'import_external_context'
               )
               OR (
                   NEW.result_kind = 'external_link_publication_blocked'
                   AND NEW.state_kind = 'blocked'
                   AND NEW.pass_kind = 'publish'
               )
           )
        THEN
            RAISE EXCEPTION
                'review pass result is incompatible with pass outcome'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;

    IF NEW.result_kind IS NOT NULL THEN
        RAISE EXCEPTION
            'review pass lifecycle transition cannot bind an effect result'
            USING ERRCODE = '23514';
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
            AND NEW.turn_id IS NOT DISTINCT FROM OLD.turn_id
        ) THEN
            RAISE EXCEPTION 'invalid running review pass transition'
                USING ERRCODE = '23514';
        END IF;
    ELSE
        RAISE EXCEPTION 'terminal review pass cannot transition'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.turn_id IS NOT NULL THEN
        SELECT state_kind, terminal_disposition_kind
          INTO canonical_turn_state, canonical_turn_disposition
          FROM turn_lifecycle
         WHERE turn_id = NEW.turn_id
           AND session_id = NEW.session_id
           AND origin_accepted_input_id = NEW.accepted_input_id;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'review pass referenced turn is missing'
                USING ERRCODE = '23514';
        END IF;
        IF NOT (
            (
                NEW.state_kind = 'running'
                AND canonical_turn_state = 'active'
                AND canonical_turn_disposition IS NULL
            )
            OR (
                NEW.state_kind = 'succeeded'
                AND canonical_turn_state = 'terminal'
                AND canonical_turn_disposition = 'completed'
            )
            OR (
                NEW.state_kind = 'failed'
                AND canonical_turn_state = 'terminal'
                AND canonical_turn_disposition IN (
                    'completed',
                    'failed',
                    'refused'
                )
            )
            OR (
                NEW.state_kind = 'blocked'
                AND canonical_turn_state = 'terminal'
                AND canonical_turn_disposition = 'reconciliation_required'
            )
            OR (
                NEW.state_kind = 'cancelled'
                AND canonical_turn_state = 'terminal'
                AND canonical_turn_disposition = 'cancelled'
            )
        ) THEN
            RAISE EXCEPTION 'review pass state contradicts canonical turn'
                USING ERRCODE = '23514';
        END IF;
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER review_pass_change_is_guarded
BEFORE INSERT OR UPDATE ON review_pass
FOR EACH ROW
EXECUTE FUNCTION guard_review_pass_change();

CREATE FUNCTION require_review_pass_run_projection()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    canonical_run_state text;
    canonical_run_pass uuid;
BEGIN
    SELECT state_kind, state_pass_id
      INTO canonical_run_state, canonical_run_pass
      FROM review_run
     WHERE run_id = NEW.run_id
       AND target_id = NEW.target_id;

    IF NEW.state_kind = 'queued' THEN
        IF canonical_run_state IS DISTINCT FROM 'queued'
           OR canonical_run_pass IS NOT NULL
        THEN
            RAISE EXCEPTION 'queued pass is not under a queued run'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.state_kind = 'cancelled' AND NEW.turn_id IS NULL THEN
        IF canonical_run_state IS DISTINCT FROM 'cancelled'
           OR canonical_run_pass IS DISTINCT FROM NEW.pass_id
        THEN
            RAISE EXCEPTION
                'pre-start cancelled pass contradicts its run projection'
                USING ERRCODE = '23514';
        END IF;
    ELSIF canonical_run_pass IS DISTINCT FROM NEW.pass_id
          OR canonical_run_state IS DISTINCT FROM NEW.state_kind
    THEN
        RAISE EXCEPTION
            'review pass state contradicts its run projection'
            USING ERRCODE = '23514';
    END IF;

    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER review_pass_run_projection_is_guarded
AFTER INSERT OR UPDATE ON review_pass
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_review_pass_run_projection();

CREATE FUNCTION require_review_run_pass_projection()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    canonical_pass_id uuid;
    canonical_pass_state text;
    canonical_pass_turn uuid;
BEGIN
    SELECT pass_id, state_kind, turn_id
      INTO canonical_pass_id, canonical_pass_state, canonical_pass_turn
      FROM review_pass
     WHERE run_id = NEW.run_id
       AND target_id = NEW.target_id;

    IF NEW.state_kind = 'queued' THEN
        IF canonical_pass_id IS NOT NULL
           AND canonical_pass_state IS DISTINCT FROM 'queued'
        THEN
            RAISE EXCEPTION 'queued run has a non-queued pass'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.state_kind = 'cancelled' AND NEW.state_pass_id IS NULL THEN
        IF canonical_pass_id IS NOT NULL THEN
            RAISE EXCEPTION
                'cancelled run without pass projection has a recorded pass'
                USING ERRCODE = '23514';
        END IF;
    ELSIF canonical_pass_id IS DISTINCT FROM NEW.state_pass_id
          OR canonical_pass_state IS DISTINCT FROM NEW.state_kind
    THEN
        RAISE EXCEPTION
            'review run state contradicts its pass projection'
            USING ERRCODE = '23514';
    END IF;

    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER review_run_pass_projection_is_guarded
AFTER INSERT OR UPDATE ON review_run
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_review_run_pass_projection();

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
    diff_side text,
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
                line_start IS NOT NULL
                AND line_end IS NOT NULL
                AND line_start BETWEEN 1 AND 4294967295
                AND line_end BETWEEN line_start AND 4294967295
            )
        ),
    CONSTRAINT review_finding_diff_side_closed
        CHECK (
            diff_side IS NULL
            OR diff_side IN ('left', 'right')
        ),
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
CREATE INDEX review_finding_producing_pass_index
    ON review_finding (
        producing_pass_id,
        target_id,
        run_id,
        finding_id
    );

ALTER TABLE review_finding
    ADD CONSTRAINT review_finding_complete_ancestry_key
    UNIQUE (finding_id, run_id, target_id, producing_pass_id);

ALTER TABLE review_pass
    ADD CONSTRAINT review_pass_result_finding_fk
    FOREIGN KEY (
        result_finding_id,
        result_finding_run_id,
        target_id,
        result_finding_pass_id
    )
    REFERENCES review_finding (
        finding_id,
        run_id,
        target_id,
        producing_pass_id
    )
    ON UPDATE RESTRICT
    ON DELETE RESTRICT
    DEFERRABLE INITIALLY DEFERRED,
    ADD CONSTRAINT review_pass_result_referenced_finding_fk
    FOREIGN KEY (
        result_referenced_finding_id,
        result_referenced_finding_run_id,
        target_id,
        result_referenced_finding_pass_id
    )
    REFERENCES review_finding (
        finding_id,
        run_id,
        target_id,
        producing_pass_id
    )
    ON UPDATE RESTRICT
    ON DELETE RESTRICT
    DEFERRABLE INITIALLY DEFERRED;

CREATE TABLE review_pass_produced_finding (
    pass_id uuid NOT NULL,
    result_ordinal bigint NOT NULL,
    finding_id uuid NOT NULL,
    finding_run_id uuid NOT NULL,
    target_id uuid NOT NULL,
    finding_pass_id uuid NOT NULL,

    CONSTRAINT review_pass_produced_finding_pk
        PRIMARY KEY (pass_id, result_ordinal),
    CONSTRAINT review_pass_produced_finding_identity_unique
        UNIQUE (finding_id),
    CONSTRAINT review_pass_produced_finding_ordinal_bounds
        CHECK (result_ordinal BETWEEN 1 AND 32),
    CONSTRAINT review_pass_produced_finding_pass_fk
        FOREIGN KEY (pass_id, finding_run_id, target_id)
        REFERENCES review_pass (pass_id, run_id, target_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT review_pass_produced_finding_finding_fk
        FOREIGN KEY (
            finding_id,
            finding_run_id,
            target_id,
            finding_pass_id
        )
        REFERENCES review_finding (
            finding_id,
            run_id,
            target_id,
            producing_pass_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT review_pass_produced_finding_owner
        CHECK (finding_pass_id = pass_id)
);

CREATE TRIGGER review_pass_produced_finding_is_append_only
BEFORE UPDATE OR DELETE ON review_pass_produced_finding
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE review_pass_finding_inventory_seal (
    pass_id uuid PRIMARY KEY,
    finding_count integer NOT NULL,

    CONSTRAINT review_pass_finding_inventory_seal_count
        CHECK (finding_count BETWEEN 0 AND 32),
    CONSTRAINT review_pass_finding_inventory_seal_pass_fk
        FOREIGN KEY (pass_id)
        REFERENCES review_pass (pass_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

CREATE FUNCTION guard_review_pass_finding_inventory_seal()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    canonical_kind text;
    canonical_findings integer;
    inventory_members integer;
BEGIN
    SELECT result_kind
      INTO canonical_kind
      FROM review_pass
     WHERE pass_id = NEW.pass_id;
    SELECT count(*)
      INTO canonical_findings
      FROM review_finding
     WHERE producing_pass_id = NEW.pass_id;
    SELECT count(*)
      INTO inventory_members
      FROM review_pass_produced_finding
     WHERE pass_id = NEW.pass_id;
    IF canonical_kind IS DISTINCT FROM 'produced_findings'
       OR NEW.finding_count IS DISTINCT FROM canonical_findings
       OR NEW.finding_count IS DISTINCT FROM inventory_members
    THEN
        RAISE EXCEPTION
            'review pass finding inventory seal differs from canonical findings'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER review_pass_finding_inventory_seal_insert_is_guarded
BEFORE INSERT ON review_pass_finding_inventory_seal
FOR EACH ROW
EXECUTE FUNCTION guard_review_pass_finding_inventory_seal();

CREATE TRIGGER review_pass_finding_inventory_seal_is_append_only
BEFORE UPDATE OR DELETE ON review_pass_finding_inventory_seal
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION reject_sealed_review_finding_inventory_expansion()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    checked_pass uuid;
BEGIN
    checked_pass := COALESCE(
        (to_jsonb(NEW) ->> 'pass_id')::uuid,
        (to_jsonb(NEW) ->> 'producing_pass_id')::uuid
    );
    IF EXISTS (
        SELECT 1
          FROM review_pass_finding_inventory_seal
         WHERE pass_id = checked_pass
    )
    THEN
        RAISE EXCEPTION 'sealed review finding inventory cannot expand'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER review_pass_produced_finding_reject_sealed_expansion
BEFORE INSERT ON review_pass_produced_finding
FOR EACH ROW
EXECUTE FUNCTION reject_sealed_review_finding_inventory_expansion();

CREATE TRIGGER review_finding_reject_sealed_expansion
BEFORE INSERT ON review_finding
FOR EACH ROW
EXECUTE FUNCTION reject_sealed_review_finding_inventory_expansion();

CREATE FUNCTION require_review_pass_finding_inventory()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    checked_pass uuid;
    canonical_kind text;
    sealed_count integer;
BEGIN
    checked_pass := COALESCE(
        (to_jsonb(NEW) ->> 'pass_id')::uuid,
        (to_jsonb(NEW) ->> 'producing_pass_id')::uuid
    );

    SELECT result_kind
      INTO canonical_kind
      FROM review_pass
     WHERE pass_id = checked_pass;
    SELECT finding_count
      INTO sealed_count
      FROM review_pass_finding_inventory_seal
     WHERE pass_id = checked_pass;

    IF canonical_kind IS DISTINCT FROM 'produced_findings'
       AND (
           EXISTS (
               SELECT 1
                 FROM review_finding
                WHERE producing_pass_id = checked_pass
           )
           OR EXISTS (
               SELECT 1
                 FROM review_pass_produced_finding
                WHERE pass_id = checked_pass
           )
           OR sealed_count IS NOT NULL
       )
    THEN
        RAISE EXCEPTION
            'review finding producer has no produced-findings result'
            USING ERRCODE = '23514';
    END IF;

    IF canonical_kind = 'produced_findings'
       AND (
           sealed_count IS NULL
           OR sealed_count IS DISTINCT FROM (
               SELECT count(*)
                 FROM review_finding
                WHERE producing_pass_id = checked_pass
           )
           OR sealed_count IS DISTINCT FROM (
               SELECT count(*)
                 FROM review_pass_produced_finding
                WHERE pass_id = checked_pass
           )
           OR
           EXISTS (
               SELECT finding_id, run_id, target_id, producing_pass_id
                 FROM review_finding
                WHERE producing_pass_id = checked_pass
               EXCEPT
               SELECT finding_id, finding_run_id, target_id, finding_pass_id
                 FROM review_pass_produced_finding
                WHERE pass_id = checked_pass
           )
           OR EXISTS (
               SELECT finding_id, finding_run_id, target_id, finding_pass_id
                 FROM review_pass_produced_finding
                WHERE pass_id = checked_pass
               EXCEPT
               SELECT finding_id, run_id, target_id, producing_pass_id
                 FROM review_finding
                WHERE producing_pass_id = checked_pass
           )
           OR EXISTS (
               SELECT 1
                 FROM (
                     SELECT result_ordinal,
                            row_number() OVER (
                                ORDER BY
                                    target_id,
                                    finding_run_id,
                                    finding_pass_id,
                                    finding_id
                            ) AS canonical_ordinal
                       FROM review_pass_produced_finding
                      WHERE pass_id = checked_pass
                 ) AS inventory
                WHERE result_ordinal <> canonical_ordinal
           )
       )
    THEN
        RAISE EXCEPTION
            'review pass finding inventory differs from canonical findings'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER review_pass_finding_inventory_from_pass
AFTER UPDATE OF result_kind ON review_pass
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_review_pass_finding_inventory();

CREATE CONSTRAINT TRIGGER review_pass_finding_inventory_from_finding
AFTER INSERT ON review_finding
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_review_pass_finding_inventory();

CREATE CONSTRAINT TRIGGER review_pass_finding_inventory_from_member
AFTER INSERT ON review_pass_produced_finding
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_review_pass_finding_inventory();

CREATE CONSTRAINT TRIGGER review_pass_finding_inventory_from_seal
AFTER INSERT ON review_pass_finding_inventory_seal
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_review_pass_finding_inventory();

CREATE FUNCTION guard_review_finding_insert()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    canonical_pass_kind text;
    canonical_pass_state text;
    canonical_result_kind text;
BEGIN
    IF NEW.diff_side IS NOT NULL
       AND NOT EXISTS (
           SELECT 1
             FROM review_target
            WHERE target_id = NEW.target_id
              AND base_revision IS NOT NULL
       )
    THEN
        RAISE EXCEPTION 'diff-relative finding requires a target base'
            USING ERRCODE = '23514';
    END IF;
    SELECT pass_kind, state_kind, result_kind
      INTO canonical_pass_kind, canonical_pass_state,
           canonical_result_kind
      FROM review_pass
     WHERE pass_id = NEW.producing_pass_id
       AND run_id = NEW.run_id
       AND target_id = NEW.target_id;
    IF canonical_pass_kind IS DISTINCT FROM 'read_only_review'
       OR canonical_pass_state IS DISTINCT FROM 'succeeded'
       OR canonical_result_kind IS DISTINCT FROM 'produced_findings'
    THEN
        RAISE EXCEPTION
            'finding producer must be a succeeded read-only-review pass'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER review_finding_insert_is_guarded
BEFORE INSERT ON review_finding
FOR EACH ROW
EXECUTE FUNCTION guard_review_finding_insert();

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
    finding_producing_pass_id uuid,
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
                AND finding_producing_pass_id IS NULL
            )
            OR (
                association_kind = 'run'
                AND run_id IS NOT NULL
                AND finding_id IS NULL
                AND finding_producing_pass_id IS NULL
            )
            OR (
                association_kind = 'finding'
                AND run_id IS NOT NULL
                AND finding_id IS NOT NULL
                AND finding_producing_pass_id IS NOT NULL
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
        FOREIGN KEY (
            finding_id,
            run_id,
            target_id,
            finding_producing_pass_id
        )
        REFERENCES review_finding (
            finding_id,
            run_id,
            target_id,
            producing_pass_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

ALTER TABLE review_pass
    ADD CONSTRAINT review_pass_result_external_link_fk
    FOREIGN KEY (result_external_link_id, target_id)
    REFERENCES review_external_link (external_link_id, target_id)
    ON UPDATE RESTRICT
    ON DELETE RESTRICT
    DEFERRABLE INITIALLY DEFERRED;

CREATE INDEX review_external_link_association_index
    ON review_external_link (
        target_id,
        association_kind,
        run_id,
        finding_id,
        finding_producing_pass_id
    );

CREATE FUNCTION guard_review_external_link_insert()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    canonical_provider text;
BEGIN
    SELECT provider_key
      INTO canonical_provider
      FROM review_target
     WHERE target_id = NEW.target_id;
    IF canonical_provider IS DISTINCT FROM NEW.provider_key THEN
        RAISE EXCEPTION
            'external-link provider differs from canonical target provider'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER review_external_link_insert_is_guarded
BEFORE INSERT ON review_external_link
FOR EACH ROW
EXECUTE FUNCTION guard_review_external_link_insert();

CREATE TRIGGER review_external_link_is_append_only
BEFORE UPDATE OR DELETE ON review_external_link
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE review_external_object_identity (
    provider_key text NOT NULL,
    object_kind text NOT NULL,
    external_object_key text NOT NULL,
    logical_target_id uuid NOT NULL,
    identity_digest text GENERATED ALWAYS AS (
        md5(
            provider_key
                || chr(31)
                || object_kind
                || chr(31)
                || external_object_key
        )
    ) STORED,

    CONSTRAINT review_external_object_identity_provider_bound
        CHECK (octet_length(provider_key) BETWEEN 1 AND 1024),
    CONSTRAINT review_external_object_identity_kind_closed
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
    CONSTRAINT review_external_object_identity_key_bound
        CHECK (octet_length(external_object_key) BETWEEN 1 AND 1024),
    CONSTRAINT review_external_object_identity_target_fk
        FOREIGN KEY (logical_target_id)
        REFERENCES review_target (target_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

CREATE INDEX review_external_object_identity_digest_index
    ON review_external_object_identity (identity_digest);

CREATE FUNCTION guard_review_external_object_identity_insert()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    canonical_provider text;
BEGIN
    PERFORM pg_advisory_xact_lock(
        hashtextextended(
            NEW.provider_key
                || chr(31)
                || NEW.object_kind
                || chr(31)
                || NEW.external_object_key,
            0
        )
    );

    SELECT provider_key
      INTO canonical_provider
      FROM review_target
     WHERE target_id = NEW.logical_target_id;
    IF canonical_provider IS DISTINCT FROM NEW.provider_key THEN
        RAISE EXCEPTION
            'external object identity target provider mismatch'
            USING ERRCODE = '23514';
    END IF;

    IF EXISTS (
        SELECT 1
         FROM review_external_object_identity
         WHERE identity_digest = md5(
                   NEW.provider_key
                       || chr(31)
                       || NEW.object_kind
                       || chr(31)
                       || NEW.external_object_key
               )
           AND provider_key = NEW.provider_key
           AND object_kind = NEW.object_kind
           AND external_object_key = NEW.external_object_key
    )
    THEN
        RAISE EXCEPTION
            'external object identity is already established'
            USING ERRCODE = '23505';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER review_external_object_identity_insert_is_guarded
BEFORE INSERT ON review_external_object_identity
FOR EACH ROW
EXECUTE FUNCTION guard_review_external_object_identity_insert();

CREATE TRIGGER review_external_object_identity_is_append_only
BEFORE UPDATE OR DELETE ON review_external_object_identity
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

CREATE FUNCTION guard_review_external_link_attachment_insert()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    canonical_pass_kind text;
    canonical_pass_state text;
    canonical_result_kind text;
    canonical_result_link uuid;
    canonical_result_object text;
    canonical_result_event_kind text;
    logical_target uuid;
BEGIN
    SELECT pass_kind, state_kind, result_kind,
           result_external_link_id, result_external_object_key,
           result_event_kind
      INTO canonical_pass_kind, canonical_pass_state,
           canonical_result_kind, canonical_result_link,
           canonical_result_object, canonical_result_event_kind
      FROM review_pass
     WHERE pass_id = NEW.pass_id
       AND run_id = NEW.pass_run_id
       AND target_id = NEW.target_id;
    IF canonical_pass_kind NOT IN ('publish', 'import_external_context')
       OR canonical_pass_state IS DISTINCT FROM 'succeeded'
       OR canonical_result_kind
            IS DISTINCT FROM 'external_link_attachment'
       OR canonical_result_link
            IS DISTINCT FROM NEW.external_link_id
       OR canonical_result_object
            IS DISTINCT FROM NEW.external_object_key
    THEN
        RAISE EXCEPTION
            'external attachment requires a succeeded attaching pass'
            USING ERRCODE = '23514';
    END IF;
    IF canonical_result_event_kind IS DISTINCT FROM 'posted'
       AND EXISTS (
           SELECT 1
             FROM review_external_link AS link
             JOIN LATERAL (
                 SELECT event_kind, external_link_id
                   FROM review_finding_event
                  WHERE finding_id = link.finding_id
                  ORDER BY event_ordinal DESC
                  LIMIT 1
             ) AS latest ON true
            WHERE link.external_link_id = NEW.external_link_id
              AND link.association_kind = 'finding'
              AND latest.event_kind = 'blocked_with_reason'
              AND latest.external_link_id = NEW.external_link_id
       )
    THEN
        RAISE EXCEPTION
            'blocked publication attachment requires an atomic posted event'
            USING ERRCODE = '23514';
    END IF;

    PERFORM pg_advisory_xact_lock(
        hashtextextended(
            NEW.provider_key
                || chr(31)
                || NEW.object_kind
                || chr(31)
                || NEW.external_object_key,
            0
        )
    );
    IF EXISTS (
        SELECT 1
          FROM review_external_link_attachment
         WHERE target_id = NEW.target_id
           AND provider_key = NEW.provider_key
           AND object_kind = NEW.object_kind
           AND external_object_key = NEW.external_object_key
    )
    THEN
        RAISE EXCEPTION
            'external object identity is already attached to this target'
            USING ERRCODE = '23505';
    END IF;
    SELECT logical_target_id
      INTO logical_target
      FROM review_external_object_identity
     WHERE identity_digest = md5(
               NEW.provider_key
                   || chr(31)
                   || NEW.object_kind
                   || chr(31)
                   || NEW.external_object_key
           )
       AND provider_key = NEW.provider_key
       AND object_kind = NEW.object_kind
       AND external_object_key = NEW.external_object_key;
    IF NOT FOUND THEN
        INSERT INTO review_external_object_identity
            (provider_key, object_kind, external_object_key,
             logical_target_id)
        VALUES
            (NEW.provider_key, NEW.object_kind, NEW.external_object_key,
             NEW.target_id);
    ELSIF logical_target <> NEW.target_id
          AND NOT EXISTS (
              SELECT 1
                FROM review_target AS established
                JOIN review_target AS candidate
                  ON candidate.target_id = NEW.target_id
               WHERE established.target_id = logical_target
                 AND established.subject_kind = 'change_request'
                 AND candidate.subject_kind = 'change_request'
                 AND established.provider_key = candidate.provider_key
                 AND established.repository_key = candidate.repository_key
                 AND established.change_request_number
                        = candidate.change_request_number
          )
    THEN
        RAISE EXCEPTION
            'external object identity belongs to another logical target'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER review_external_link_attachment_insert_is_guarded
BEFORE INSERT ON review_external_link_attachment
FOR EACH ROW
EXECUTE FUNCTION guard_review_external_link_attachment_insert();

CREATE TRIGGER review_external_link_attachment_is_append_only
BEFORE UPDATE OR DELETE ON review_external_link_attachment
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION require_review_external_identity_attachment()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM review_external_link_attachment AS attachment
          JOIN review_target AS attached_target
            ON attached_target.target_id = attachment.target_id
          JOIN review_target AS logical_target
            ON logical_target.target_id = NEW.logical_target_id
         WHERE attachment.provider_key = NEW.provider_key
           AND attachment.object_kind = NEW.object_kind
           AND attachment.external_object_key = NEW.external_object_key
           AND (
               attachment.target_id = NEW.logical_target_id
               OR (
                   attached_target.subject_kind = 'change_request'
                   AND logical_target.subject_kind = 'change_request'
                   AND attached_target.provider_key =
                       logical_target.provider_key
                   AND attached_target.repository_key =
                       logical_target.repository_key
                   AND attached_target.change_request_number =
                       logical_target.change_request_number
               )
           )
    )
    THEN
        RAISE EXCEPTION
            'external object identity lacks an establishing attachment'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER review_external_identity_attachment_is_required
AFTER INSERT ON review_external_object_identity
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_review_external_identity_attachment();

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
    referenced_finding_status text,
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
                AND referenced_finding_status IS NULL
                AND external_link_id IS NULL
                AND external_link_association_kind IS NULL
            )
            OR (
                event_kind = 'rejected'
                AND reason IS NOT NULL
                AND octet_length(reason) BETWEEN 1 AND 65536
                AND referenced_finding_id IS NULL
                AND referenced_finding_status IS NULL
                AND external_link_id IS NULL
                AND external_link_association_kind IS NULL
            )
            OR (
                event_kind = 'blocked_with_reason'
                AND reason IS NOT NULL
                AND octet_length(reason) BETWEEN 1 AND 65536
                AND referenced_finding_id IS NULL
                AND referenced_finding_status IS NULL
                AND (
                    (
                        external_link_id IS NULL
                        AND external_link_association_kind IS NULL
                    )
                    OR (
                        external_link_id IS NOT NULL
                        AND external_link_association_kind IS NOT NULL
                        AND external_link_association_kind = 'finding'
                    )
                )
            )
            OR (
                event_kind IN ('duplicate', 'superseded')
                AND reason IS NULL
                AND referenced_finding_id IS NOT NULL
                AND referenced_finding_status IN ('open', 'accepted')
                AND referenced_finding_id <> finding_id
                AND external_link_id IS NULL
                AND external_link_association_kind IS NULL
            )
            OR (
                event_kind = 'posted'
                AND reason IS NULL
                AND referenced_finding_id IS NULL
                AND referenced_finding_status IS NULL
                AND external_link_id IS NOT NULL
                AND external_link_association_kind IS NOT NULL
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

CREATE UNIQUE INDEX review_finding_event_publication_link_once
    ON review_finding_event (finding_id, external_link_id)
    WHERE event_kind = 'posted';

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
    event_pass_kind text;
    event_pass_state text;
    event_pass_result_kind text;
    event_pass_result_finding uuid;
    event_pass_result_run uuid;
    event_pass_result_pass uuid;
    event_pass_result_ordinal bigint;
    event_pass_result_event_kind text;
    event_pass_result_reason text;
    event_pass_result_referenced_finding uuid;
    event_pass_result_referenced_run uuid;
    event_pass_result_referenced_pass uuid;
    event_pass_result_referenced_status text;
    event_pass_result_external_link uuid;
    event_policy_version bigint;
    event_judge_confidence integer;
    event_publication_confidence integer;
    finding_policy_version bigint;
    finding_judge_confidence integer;
    finding_publication_confidence integer;
    finding_confidence integer;
    previous_kind text;
    previous_pass_kind text;
    previous_external_link uuid;
    previous_status text;
    referenced_status text;
    expected_ordinal bigint;
BEGIN
    PERFORM finding_id
      FROM review_finding
     WHERE finding_id IN (
         NEW.finding_id,
         NEW.referenced_finding_id
     )
     ORDER BY finding_id
     FOR NO KEY UPDATE;

    SELECT finding.confidence,
           producing_run.policy_version,
           producing_run.minimum_judge_confidence,
           producing_run.minimum_publication_confidence
      INTO finding_confidence,
           finding_policy_version,
           finding_judge_confidence,
           finding_publication_confidence
      FROM review_finding AS finding
      JOIN review_run AS producing_run
        ON producing_run.run_id = finding.run_id
       AND producing_run.target_id = finding.target_id
     WHERE finding.finding_id = NEW.finding_id
    ;

    SELECT pass.pass_kind, pass.state_kind,
           pass.result_kind,
           pass.result_finding_id,
           pass.result_finding_run_id,
           pass.result_finding_pass_id,
           pass.result_event_ordinal,
           pass.result_event_kind,
           pass.result_reason,
           pass.result_referenced_finding_id,
           pass.result_referenced_finding_run_id,
           pass.result_referenced_finding_pass_id,
           pass.result_referenced_finding_status,
           pass.result_external_link_id,
           event_run.policy_version,
           event_run.minimum_judge_confidence,
           event_run.minimum_publication_confidence
      INTO event_pass_kind, event_pass_state,
           event_pass_result_kind,
           event_pass_result_finding,
           event_pass_result_run,
           event_pass_result_pass,
           event_pass_result_ordinal,
           event_pass_result_event_kind,
           event_pass_result_reason,
           event_pass_result_referenced_finding,
           event_pass_result_referenced_run,
           event_pass_result_referenced_pass,
           event_pass_result_referenced_status,
           event_pass_result_external_link,
           event_policy_version,
           event_judge_confidence,
           event_publication_confidence
      FROM review_pass AS pass
      JOIN review_run AS event_run
        ON event_run.run_id = pass.run_id
       AND event_run.target_id = pass.target_id
     WHERE pass.pass_id = NEW.event_pass_id
       AND pass.run_id = NEW.event_pass_run_id
       AND pass.target_id = NEW.target_id;

    IF event_policy_version IS DISTINCT FROM finding_policy_version
       OR event_judge_confidence IS DISTINCT FROM finding_judge_confidence
       OR event_publication_confidence
            IS DISTINCT FROM finding_publication_confidence
    THEN
        RAISE EXCEPTION
            'finding event pass policy differs from finding policy'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.event_kind = 'accepted'
       AND finding_confidence < finding_judge_confidence
    THEN
        RAISE EXCEPTION
            'finding confidence is below the judge threshold'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.event_kind = 'posted'
       AND finding_confidence < finding_publication_confidence
    THEN
        RAISE EXCEPTION
            'finding confidence is below the publication threshold'
            USING ERRCODE = '23514';
    END IF;

    IF event_pass_kind IS NULL
       OR NOT (
           (
               NEW.event_kind IN ('accepted', 'rejected', 'stale')
               AND event_pass_kind = 'judge'
           )
           OR (
               NEW.event_kind IN ('duplicate', 'superseded')
               AND event_pass_kind = 'dedupe'
           )
           OR (
               NEW.event_kind = 'posted'
               AND event_pass_kind IN (
                   'publish',
                   'import_external_context'
               )
           )
           OR (
               NEW.event_kind = 'fixed'
               AND event_pass_kind = 'fix'
           )
           OR (
               NEW.event_kind = 'blocked_with_reason'
               AND (
                   (
                       event_pass_kind = 'publish'
                       AND NEW.external_link_id IS NOT NULL
                   )
                   OR (
                       event_pass_kind = 'fix'
                       AND NEW.external_link_id IS NULL
                   )
               )
           )
       )
    THEN
        RAISE EXCEPTION
            'finding event % is incompatible with pass kind %',
            NEW.event_kind,
            event_pass_kind
            USING ERRCODE = '23514';
    END IF;

    IF (
        NEW.event_kind = 'blocked_with_reason'
        AND event_pass_state IS DISTINCT FROM 'blocked'
    ) OR (
        NEW.event_kind <> 'blocked_with_reason'
        AND event_pass_state IS DISTINCT FROM 'succeeded'
    )
    THEN
        RAISE EXCEPTION
            'finding event % is incompatible with pass state %',
            NEW.event_kind,
            event_pass_state
            USING ERRCODE = '23514';
    END IF;

    IF event_pass_result_kind IS DISTINCT FROM (
           CASE NEW.event_kind
               WHEN 'posted' THEN 'external_link_attachment'
               ELSE 'finding_event'
           END
       )
       OR event_pass_result_finding IS DISTINCT FROM NEW.finding_id
       OR event_pass_result_run IS DISTINCT FROM NEW.finding_run_id
       OR event_pass_result_pass IS DISTINCT FROM (
           SELECT producing_pass_id
             FROM review_finding
            WHERE finding_id = NEW.finding_id
       )
       OR event_pass_result_ordinal IS DISTINCT FROM NEW.event_ordinal
       OR event_pass_result_event_kind IS DISTINCT FROM NEW.event_kind
       OR event_pass_result_reason IS DISTINCT FROM NEW.reason
       OR event_pass_result_referenced_finding
            IS DISTINCT FROM NEW.referenced_finding_id
       OR event_pass_result_referenced_run IS DISTINCT FROM (
           CASE
               WHEN NEW.referenced_finding_id IS NULL THEN NULL
               ELSE NEW.finding_run_id
           END
       )
       OR event_pass_result_referenced_pass IS DISTINCT FROM (
           SELECT producing_pass_id
             FROM review_finding
            WHERE finding_id = NEW.referenced_finding_id
              AND run_id = NEW.finding_run_id
              AND target_id = NEW.target_id
       )
       OR event_pass_result_referenced_status
            IS DISTINCT FROM NEW.referenced_finding_status
       OR event_pass_result_external_link
            IS DISTINCT FROM NEW.external_link_id
    THEN
        RAISE EXCEPTION
            'finding event is not the exact result committed by its pass'
            USING ERRCODE = '23514';
    END IF;

    SELECT event.event_kind, pass.pass_kind, event.external_link_id
      INTO previous_kind, previous_pass_kind, previous_external_link
      FROM review_finding_event AS event
      JOIN review_pass AS pass
        ON pass.pass_id = event.event_pass_id
       AND pass.run_id = event.event_pass_run_id
       AND pass.target_id = event.target_id
     WHERE event.finding_id = NEW.finding_id
     ORDER BY event.event_ordinal DESC
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

    IF NEW.referenced_finding_id IS NOT NULL THEN
        SELECT CASE latest.event_kind
                   WHEN 'accepted' THEN 'accepted'
                   WHEN 'rejected' THEN 'rejected'
                   WHEN 'duplicate' THEN 'duplicate'
                   WHEN 'superseded' THEN 'superseded'
                   WHEN 'stale' THEN 'stale'
                   WHEN 'posted' THEN 'posted'
                   WHEN 'fixed' THEN 'fixed'
                   WHEN 'blocked_with_reason' THEN 'blocked_with_reason'
                   ELSE 'open'
               END
          INTO referenced_status
          FROM review_finding AS referenced
          LEFT JOIN LATERAL (
              SELECT event_kind
                FROM review_finding_event
               WHERE finding_id = referenced.finding_id
               ORDER BY event_ordinal DESC
               LIMIT 1
          ) AS latest ON true
         WHERE referenced.finding_id = NEW.referenced_finding_id
           AND referenced.run_id = NEW.finding_run_id
           AND referenced.target_id = NEW.target_id;

        IF referenced_status NOT IN ('open', 'accepted')
           OR NEW.referenced_finding_status
                IS DISTINCT FROM referenced_status
        THEN
            RAISE EXCEPTION
                'referenced finding status % is not eligible or authenticated',
                referenced_status
                USING ERRCODE = '23514';
        END IF;

        IF EXISTS (
            WITH RECURSIVE referenced_ancestry(finding_id) AS (
                SELECT NEW.referenced_finding_id
                UNION
                SELECT latest.referenced_finding_id
                  FROM referenced_ancestry AS ancestry
                  JOIN LATERAL (
                      SELECT referenced_finding_id
                        FROM review_finding_event
                       WHERE finding_id = ancestry.finding_id
                         AND event_kind IN (
                             'duplicate',
                             'superseded'
                         )
                       ORDER BY event_ordinal DESC
                       LIMIT 1
                  ) AS latest
                    ON latest.referenced_finding_id IS NOT NULL
            )
            SELECT 1
              FROM referenced_ancestry
             WHERE finding_id = NEW.finding_id
        )
        THEN
            RAISE EXCEPTION
                'finding reference would create a cycle'
                USING ERRCODE = '23514';
        END IF;
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
            AND (
                NEW.event_kind IN ('superseded', 'stale', 'fixed')
                OR (
                    NEW.event_kind = 'posted'
                    AND previous_pass_kind = 'publish'
                    AND NEW.external_link_id
                        IS NOT DISTINCT FROM previous_external_link
                )
            )
        )
    ) THEN
        RAISE EXCEPTION
            'invalid finding transition from % through %',
            previous_status,
            NEW.event_kind
            USING ERRCODE = '23514';
    END IF;

    IF NEW.event_kind = 'blocked_with_reason'
       AND NEW.external_link_id IS NOT NULL
       AND EXISTS (
           SELECT 1
             FROM review_external_link_attachment
            WHERE external_link_id = NEW.external_link_id
              AND target_id = NEW.target_id
       )
    THEN
        RAISE EXCEPTION
            'publication block requires a pending reservation'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.event_kind = 'posted'
       AND EXISTS (
           SELECT 1
             FROM review_finding_event
            WHERE finding_id = NEW.finding_id
              AND event_kind = 'posted'
              AND external_link_id = NEW.external_link_id
       )
    THEN
        RAISE EXCEPTION
            'posted event reused consumed attachment evidence'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.event_kind = 'posted'
       AND NOT EXISTS (
           SELECT 1
             FROM review_external_link_attachment AS attachment
             JOIN review_external_link AS link
               ON link.external_link_id = attachment.external_link_id
              AND link.target_id = attachment.target_id
            WHERE attachment.external_link_id = NEW.external_link_id
              AND attachment.target_id = NEW.target_id
              AND attachment.pass_run_id = NEW.event_pass_run_id
              AND attachment.pass_id = NEW.event_pass_id
              AND link.object_kind IN (
                  'review',
                  'review_thread',
                  'review_comment',
                  'change_request_comment'
              )
       )
    THEN
        RAISE EXCEPTION
            'posted event pass did not produce its attachment'
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

CREATE FUNCTION require_review_attachment_posted_event()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    canonical_event_kind text;
    canonical_finding uuid;
    canonical_ordinal bigint;
BEGIN
    SELECT result_event_kind, result_finding_id, result_event_ordinal
      INTO canonical_event_kind, canonical_finding, canonical_ordinal
      FROM review_pass
     WHERE pass_id = NEW.pass_id
       AND run_id = NEW.pass_run_id
       AND target_id = NEW.target_id;
    IF canonical_event_kind = 'posted'
       AND NOT EXISTS (
           SELECT 1
             FROM review_finding_event
            WHERE finding_id = canonical_finding
              AND event_ordinal = canonical_ordinal
              AND target_id = NEW.target_id
              AND event_pass_id = NEW.pass_id
              AND event_pass_run_id = NEW.pass_run_id
              AND event_kind = 'posted'
              AND external_link_id = NEW.external_link_id
       )
    THEN
        RAISE EXCEPTION
            'external attachment omitted its exact posted finding event'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER review_attachment_posted_event_is_required
AFTER INSERT ON review_external_link_attachment
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_review_attachment_posted_event();

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
    latest_state text;
    canonical_pass_kind text;
    canonical_pass_state text;
    canonical_result_kind text;
    canonical_result_link uuid;
    canonical_result_ordinal bigint;
    canonical_result_state text;
BEGIN
    PERFORM 1
      FROM review_external_link
     WHERE external_link_id = NEW.external_link_id
     FOR NO KEY UPDATE;

    SELECT pass_kind, state_kind, result_kind,
           result_external_link_id, result_event_ordinal,
           result_observation_state
      INTO canonical_pass_kind, canonical_pass_state,
           canonical_result_kind, canonical_result_link,
           canonical_result_ordinal, canonical_result_state
      FROM review_pass
     WHERE pass_id = NEW.pass_id
       AND run_id = NEW.pass_run_id
       AND target_id = NEW.target_id;
    IF canonical_pass_kind IS DISTINCT FROM 'import_external_context'
       OR canonical_pass_state IS DISTINCT FROM 'succeeded'
       OR canonical_result_kind
            IS DISTINCT FROM 'external_link_observation'
       OR canonical_result_link
            IS DISTINCT FROM NEW.external_link_id
       OR canonical_result_ordinal
            IS DISTINCT FROM NEW.observation_ordinal
       OR canonical_result_state
            IS DISTINCT FROM NEW.object_state
    THEN
        RAISE EXCEPTION
            'external observation requires a succeeded import pass'
            USING ERRCODE = '23514';
    END IF;

    SELECT observation_ordinal + 1, object_state
      INTO expected_ordinal, latest_state
      FROM review_external_link_observation
     WHERE external_link_id = NEW.external_link_id
     ORDER BY observation_ordinal DESC
     LIMIT 1;
    expected_ordinal := COALESCE(expected_ordinal, 1);

    IF latest_state IS NOT NULL
       AND latest_state IS NOT DISTINCT FROM NEW.object_state
    THEN
        RAISE EXCEPTION
            'unchanged external observation is not durable evidence'
            USING ERRCODE = '23514';
    END IF;

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

CREATE FUNCTION require_review_pass_external_result()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.result_kind = 'external_link_attachment'
       AND NOT EXISTS (
           SELECT 1
             FROM review_external_link_attachment
            WHERE external_link_id = NEW.result_external_link_id
              AND target_id = NEW.target_id
              AND pass_run_id = NEW.run_id
              AND pass_id = NEW.pass_id
              AND external_object_key =
                    NEW.result_external_object_key
       )
    THEN
        RAISE EXCEPTION
            'external attachment result omitted its exact child row'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.result_kind = 'external_link_observation'
       AND NOT EXISTS (
           SELECT 1
             FROM review_external_link_observation
            WHERE external_link_id = NEW.result_external_link_id
              AND observation_ordinal = NEW.result_event_ordinal
              AND target_id = NEW.target_id
              AND pass_run_id = NEW.run_id
              AND pass_id = NEW.pass_id
              AND object_state =
                    NEW.result_observation_state
       )
    THEN
        RAISE EXCEPTION
            'external observation result omitted its exact child row'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.result_kind = 'external_link_no_change'
       AND (
           NOT EXISTS (
               SELECT 1
                 FROM review_external_link_attachment
                WHERE external_link_id = NEW.result_external_link_id
                  AND target_id = NEW.target_id
           )
           OR NEW.result_observation_state IS DISTINCT FROM (
               SELECT object_state
                 FROM review_external_link_observation
                WHERE external_link_id = NEW.result_external_link_id
                ORDER BY observation_ordinal DESC
                LIMIT 1
           )
           OR NEW.result_event_ordinal IS DISTINCT FROM (
               SELECT observation_ordinal
                 FROM review_external_link_observation
                WHERE external_link_id = NEW.result_external_link_id
                ORDER BY observation_ordinal DESC
                LIMIT 1
           )
       )
    THEN
        RAISE EXCEPTION
            'unchanged external result differs from latest durable state'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.result_kind = 'external_link_publication_blocked'
       AND EXISTS (
           SELECT 1
             FROM review_external_link_attachment
            WHERE external_link_id = NEW.result_external_link_id
              AND target_id = NEW.target_id
       )
    THEN
        RAISE EXCEPTION
            'blocked publication result requires a pending reservation'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER review_pass_external_result_is_guarded
AFTER UPDATE OF result_kind ON review_pass
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_review_pass_external_result();

CREATE FUNCTION reject_review_workflow_truncate()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'review workflow tables cannot be truncated'
        USING ERRCODE = '23514';
END;
$$;

CREATE TRIGGER review_target_reject_truncate
BEFORE TRUNCATE ON review_target
FOR EACH STATEMENT
EXECUTE FUNCTION reject_review_workflow_truncate();

CREATE TRIGGER review_run_reject_truncate
BEFORE TRUNCATE ON review_run
FOR EACH STATEMENT
EXECUTE FUNCTION reject_review_workflow_truncate();

CREATE TRIGGER review_pass_reject_truncate
BEFORE TRUNCATE ON review_pass
FOR EACH STATEMENT
EXECUTE FUNCTION reject_review_workflow_truncate();

CREATE TRIGGER review_finding_reject_truncate
BEFORE TRUNCATE ON review_finding
FOR EACH STATEMENT
EXECUTE FUNCTION reject_review_workflow_truncate();

CREATE TRIGGER review_pass_produced_finding_reject_truncate
BEFORE TRUNCATE ON review_pass_produced_finding
FOR EACH STATEMENT
EXECUTE FUNCTION reject_review_workflow_truncate();

CREATE TRIGGER review_pass_finding_inventory_seal_reject_truncate
BEFORE TRUNCATE ON review_pass_finding_inventory_seal
FOR EACH STATEMENT
EXECUTE FUNCTION reject_review_workflow_truncate();

CREATE TRIGGER review_external_link_reject_truncate
BEFORE TRUNCATE ON review_external_link
FOR EACH STATEMENT
EXECUTE FUNCTION reject_review_workflow_truncate();

CREATE TRIGGER review_external_object_identity_reject_truncate
BEFORE TRUNCATE ON review_external_object_identity
FOR EACH STATEMENT
EXECUTE FUNCTION reject_review_workflow_truncate();

CREATE TRIGGER review_external_link_attachment_reject_truncate
BEFORE TRUNCATE ON review_external_link_attachment
FOR EACH STATEMENT
EXECUTE FUNCTION reject_review_workflow_truncate();

CREATE TRIGGER review_finding_event_reject_truncate
BEFORE TRUNCATE ON review_finding_event
FOR EACH STATEMENT
EXECUTE FUNCTION reject_review_workflow_truncate();

CREATE TRIGGER review_external_link_observation_reject_truncate
BEFORE TRUNCATE ON review_external_link_observation
FOR EACH STATEMENT
EXECUTE FUNCTION reject_review_workflow_truncate();
