CREATE OR REPLACE FUNCTION guard_review_pass_change() RETURNS trigger
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
                AND (
                    (canonical_turn_state = 'active'
                     AND canonical_turn_disposition IS NULL)
                    OR canonical_turn_state = 'terminal'
                )
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
