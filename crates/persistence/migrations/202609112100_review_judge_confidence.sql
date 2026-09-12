-- Review history is exported before this pre-production reset.
ALTER TABLE review_external_link DISABLE TRIGGER USER;
ALTER TABLE review_external_link_attachment DISABLE TRIGGER USER;
ALTER TABLE review_external_link_observation DISABLE TRIGGER USER;
ALTER TABLE review_external_object_identity DISABLE TRIGGER USER;
ALTER TABLE review_finding DISABLE TRIGGER USER;
ALTER TABLE review_finding_event DISABLE TRIGGER USER;
ALTER TABLE review_finding_event_head DISABLE TRIGGER USER;
ALTER TABLE review_orchestration_attempt DISABLE TRIGGER USER;
ALTER TABLE review_orchestration_command DISABLE TRIGGER USER;
ALTER TABLE review_orchestration_command_effect DISABLE TRIGGER USER;
ALTER TABLE review_orchestration_command_intent DISABLE TRIGGER USER;
ALTER TABLE review_orchestration_command_recovery DISABLE TRIGGER USER;
ALTER TABLE review_orchestration_concern DISABLE TRIGGER USER;
ALTER TABLE review_orchestration_concern_claim DISABLE TRIGGER USER;
ALTER TABLE review_orchestration_concern_finding DISABLE TRIGGER USER;
ALTER TABLE review_orchestration_fanout_member DISABLE TRIGGER USER;
ALTER TABLE review_orchestration_fanout_seal DISABLE TRIGGER USER;
ALTER TABLE review_orchestration_import DISABLE TRIGGER USER;
ALTER TABLE review_orchestration_judgment_effect DISABLE TRIGGER USER;
ALTER TABLE review_orchestration_judgment_member DISABLE TRIGGER USER;
ALTER TABLE review_orchestration_judgment_plan DISABLE TRIGGER USER;
ALTER TABLE review_orchestration_publication_inventory DISABLE TRIGGER USER;
ALTER TABLE review_orchestration_publication_inventory_seal DISABLE TRIGGER USER;
ALTER TABLE review_orchestration_publication_outcome DISABLE TRIGGER USER;
ALTER TABLE review_orchestration_publication_outcome_seal DISABLE TRIGGER USER;
ALTER TABLE review_orchestration_repair_inventory DISABLE TRIGGER USER;
ALTER TABLE review_orchestration_repair_inventory_seal DISABLE TRIGGER USER;
ALTER TABLE review_orchestration_repair_outcome DISABLE TRIGGER USER;
ALTER TABLE review_orchestration_repair_outcome_seal DISABLE TRIGGER USER;
ALTER TABLE review_pass DISABLE TRIGGER USER;
ALTER TABLE review_pass_finding_inventory_seal DISABLE TRIGGER USER;
ALTER TABLE review_pass_produced_finding DISABLE TRIGGER USER;
ALTER TABLE review_run DISABLE TRIGGER USER;
ALTER TABLE review_target DISABLE TRIGGER USER;
ALTER TABLE review_workflow_command DISABLE TRIGGER USER;
TRUNCATE TABLE
    review_external_link,
    review_external_link_attachment,
    review_external_link_observation,
    review_external_object_identity,
    review_finding,
    review_finding_event,
    review_finding_event_head,
    review_orchestration_attempt,
    review_orchestration_command,
    review_orchestration_command_effect,
    review_orchestration_command_intent,
    review_orchestration_command_recovery,
    review_orchestration_concern,
    review_orchestration_concern_claim,
    review_orchestration_concern_finding,
    review_orchestration_fanout_member,
    review_orchestration_fanout_seal,
    review_orchestration_import,
    review_orchestration_judgment_effect,
    review_orchestration_judgment_member,
    review_orchestration_judgment_plan,
    review_orchestration_publication_inventory,
    review_orchestration_publication_inventory_seal,
    review_orchestration_publication_outcome,
    review_orchestration_publication_outcome_seal,
    review_orchestration_repair_inventory,
    review_orchestration_repair_inventory_seal,
    review_orchestration_repair_outcome,
    review_orchestration_repair_outcome_seal,
    review_pass,
    review_pass_finding_inventory_seal,
    review_pass_produced_finding,
    review_run,
    review_target,
    review_workflow_command;
ALTER TABLE review_external_link ENABLE TRIGGER USER;
ALTER TABLE review_external_link_attachment ENABLE TRIGGER USER;
ALTER TABLE review_external_link_observation ENABLE TRIGGER USER;
ALTER TABLE review_external_object_identity ENABLE TRIGGER USER;
ALTER TABLE review_finding ENABLE TRIGGER USER;
ALTER TABLE review_finding_event ENABLE TRIGGER USER;
ALTER TABLE review_finding_event_head ENABLE TRIGGER USER;
ALTER TABLE review_orchestration_attempt ENABLE TRIGGER USER;
ALTER TABLE review_orchestration_command ENABLE TRIGGER USER;
ALTER TABLE review_orchestration_command_effect ENABLE TRIGGER USER;
ALTER TABLE review_orchestration_command_intent ENABLE TRIGGER USER;
ALTER TABLE review_orchestration_command_recovery ENABLE TRIGGER USER;
ALTER TABLE review_orchestration_concern ENABLE TRIGGER USER;
ALTER TABLE review_orchestration_concern_claim ENABLE TRIGGER USER;
ALTER TABLE review_orchestration_concern_finding ENABLE TRIGGER USER;
ALTER TABLE review_orchestration_fanout_member ENABLE TRIGGER USER;
ALTER TABLE review_orchestration_fanout_seal ENABLE TRIGGER USER;
ALTER TABLE review_orchestration_import ENABLE TRIGGER USER;
ALTER TABLE review_orchestration_judgment_effect ENABLE TRIGGER USER;
ALTER TABLE review_orchestration_judgment_member ENABLE TRIGGER USER;
ALTER TABLE review_orchestration_judgment_plan ENABLE TRIGGER USER;
ALTER TABLE review_orchestration_publication_inventory ENABLE TRIGGER USER;
ALTER TABLE review_orchestration_publication_inventory_seal ENABLE TRIGGER USER;
ALTER TABLE review_orchestration_publication_outcome ENABLE TRIGGER USER;
ALTER TABLE review_orchestration_publication_outcome_seal ENABLE TRIGGER USER;
ALTER TABLE review_orchestration_repair_inventory ENABLE TRIGGER USER;
ALTER TABLE review_orchestration_repair_inventory_seal ENABLE TRIGGER USER;
ALTER TABLE review_orchestration_repair_outcome ENABLE TRIGGER USER;
ALTER TABLE review_orchestration_repair_outcome_seal ENABLE TRIGGER USER;
ALTER TABLE review_pass ENABLE TRIGGER USER;
ALTER TABLE review_pass_finding_inventory_seal ENABLE TRIGGER USER;
ALTER TABLE review_pass_produced_finding ENABLE TRIGGER USER;
ALTER TABLE review_run ENABLE TRIGGER USER;
ALTER TABLE review_target ENABLE TRIGGER USER;
ALTER TABLE review_workflow_command ENABLE TRIGGER USER;

ALTER TABLE review_finding_event ADD COLUMN judge_confidence smallint;
ALTER TABLE review_finding_event ADD CONSTRAINT review_finding_event_judge_confidence
    CHECK ((event_kind = 'accepted') = (judge_confidence IS NOT NULL)
           AND (judge_confidence IS NULL OR judge_confidence BETWEEN 1 AND 5));

ALTER TABLE review_pass ADD COLUMN result_judge_confidence smallint;
ALTER TABLE review_pass ADD CONSTRAINT review_pass_judge_confidence
    CHECK ((result_event_kind IS NOT DISTINCT FROM 'accepted') = (result_judge_confidence IS NOT NULL)
           AND (result_judge_confidence IS NULL OR result_judge_confidence BETWEEN 1 AND 5));

ALTER TABLE review_orchestration_judgment_member ADD COLUMN judgment jsonb NOT NULL;
ALTER TABLE review_orchestration_judgment_member
    ADD CONSTRAINT review_orchestration_judgment_shape CHECK (
        jsonb_typeof(judgment) = 'object'
        AND judgment ?& ARRAY['bar_category', 'decline_class', 'confidence', 'reason']
        AND judgment - ARRAY['bar_category', 'decline_class', 'confidence', 'reason'] = '{}'::jsonb
        AND jsonb_typeof(judgment->'bar_category') = 'string'
        AND jsonb_typeof(judgment->'confidence') = 'number'
        AND judgment->>'confidence' IN ('1', '2', '3', '4', '5')
        AND jsonb_typeof(judgment->'reason') = 'string'
        AND octet_length(judgment->>'reason') BETWEEN 1 AND 65536
        AND CASE WHEN judgment->>'bar_category' = 'none' THEN
            judgment->>'decline_class' IN (
                'hypothetical-hardening', 'inventory-restoration', 'scope-expansion',
                'pre-existing-out-of-scope', 'design-document-demand', 'style-or-prose',
                'spec-sentence-wrong', 'duplicate', 'other')
            AND jsonb_typeof(judgment->'decline_class') = 'string'
            AND disposition_kind <> 'accepted'
        ELSE
            judgment->>'bar_category' IN (
                'false-statement', 'broken-reference', 'contradiction',
                'undecided-as-committed', 'failing-gate', 'own-behavior-defect')
            AND judgment->'decline_class' = 'null'::jsonb
            AND disposition_kind = 'accepted'
        END
    );

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
           NEW.result_judge_confidence,
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
           OLD.result_judge_confidence,
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

CREATE OR REPLACE FUNCTION require_review_finding_event_sequence() RETURNS trigger
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
    event_pass_result_judge_confidence smallint;
    event_pass_result_referenced_finding uuid;
    event_pass_result_referenced_run uuid;
    event_pass_result_referenced_target uuid;
    event_pass_result_referenced_pass uuid;
    event_pass_result_referenced_status text;
    event_pass_result_external_link uuid;
    event_policy_version bigint;
    event_judge_confidence integer;
    event_publication_confidence integer;
    finding_policy_version bigint;
    finding_judge_confidence integer;
    finding_publication_confidence integer;
    finding_producing_pass uuid;
    referenced_pass_kind text;
    referenced_pass_state text;
    referenced_pass_result_kind text;
    referenced_run_state text;
    referenced_run_state_pass uuid;
    referenced_policy_version bigint;
    referenced_judge_confidence integer;
    referenced_publication_confidence integer;
    referenced_seal_count integer;
BEGIN
    PERFORM finding_id
      FROM review_finding
     WHERE finding_id IN (
         NEW.finding_id,
         NEW.referenced_finding_id
     )
     ORDER BY finding_id
     FOR NO KEY UPDATE;

    SELECT finding.producing_pass_id,
           producing_run.policy_version,
           producing_run.minimum_judge_confidence,
           producing_run.minimum_publication_confidence
      INTO finding_producing_pass,
           finding_policy_version,
           finding_judge_confidence,
           finding_publication_confidence
      FROM review_finding AS finding
      JOIN review_run AS producing_run
        ON producing_run.run_id = finding.run_id
       AND producing_run.target_id = finding.target_id
     WHERE finding.finding_id = NEW.finding_id
       AND finding.run_id = NEW.finding_run_id
       AND finding.target_id = NEW.target_id;

    SELECT pass.pass_kind, pass.state_kind,
           pass.result_kind,
           pass.result_finding_id,
           pass.result_finding_run_id,
           pass.result_finding_pass_id,
           pass.result_event_ordinal,
           pass.result_event_kind,
           pass.result_reason,
           pass.result_judge_confidence,
           pass.result_referenced_finding_id,
           pass.result_referenced_finding_run_id,
           pass.result_referenced_finding_target_id,
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
           event_pass_result_judge_confidence,
           event_pass_result_referenced_finding,
           event_pass_result_referenced_run,
           event_pass_result_referenced_target,
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

    IF NEW.referenced_finding_id IS NOT NULL THEN
        SELECT referenced_pass.pass_kind,
               referenced_pass.state_kind,
               referenced_pass.result_kind,
               referenced_run.state_kind,
               referenced_run.state_pass_id,
               referenced_run.policy_version,
               referenced_run.minimum_judge_confidence,
               referenced_run.minimum_publication_confidence,
               seal.finding_count
          INTO referenced_pass_kind,
               referenced_pass_state,
               referenced_pass_result_kind,
               referenced_run_state,
               referenced_run_state_pass,
               referenced_policy_version,
               referenced_judge_confidence,
               referenced_publication_confidence,
               referenced_seal_count
          FROM review_finding AS referenced
          JOIN review_pass AS referenced_pass
            ON referenced_pass.pass_id =
                referenced.producing_pass_id
           AND referenced_pass.run_id = referenced.run_id
           AND referenced_pass.target_id = referenced.target_id
          JOIN review_run AS referenced_run
            ON referenced_run.run_id = referenced.run_id
           AND referenced_run.target_id = referenced.target_id
          LEFT JOIN review_pass_finding_inventory_seal AS seal
            ON seal.pass_id = referenced.producing_pass_id
         WHERE referenced.finding_id = NEW.referenced_finding_id
           AND referenced.run_id = NEW.referenced_finding_run_id
           AND referenced.target_id = NEW.referenced_finding_target_id
           AND referenced.producing_pass_id =
                NEW.referenced_finding_pass_id;

        IF NEW.referenced_finding_target_id
                IS DISTINCT FROM NEW.target_id
           OR referenced_pass_kind
                IS DISTINCT FROM 'read_only_review'
           OR referenced_pass_state IS DISTINCT FROM 'succeeded'
           OR referenced_pass_result_kind
                IS DISTINCT FROM 'produced_findings'
           OR referenced_run_state IS DISTINCT FROM 'succeeded'
           OR referenced_run_state_pass
                IS DISTINCT FROM NEW.referenced_finding_pass_id
           OR referenced_seal_count IS NULL
           OR NOT EXISTS (
               SELECT 1
                 FROM review_pass_produced_finding
                WHERE finding_id = NEW.referenced_finding_id
                  AND finding_run_id =
                        NEW.referenced_finding_run_id
                  AND target_id =
                        NEW.referenced_finding_target_id
                  AND finding_pass_id =
                        NEW.referenced_finding_pass_id
                  AND pass_id =
                        NEW.referenced_finding_pass_id
           )
        THEN
            RAISE EXCEPTION
                'referenced finding producer or sealed inventory is invalid'
                USING ERRCODE = '23514';
        END IF;

        IF referenced_policy_version
                IS DISTINCT FROM finding_policy_version
           OR referenced_judge_confidence
                IS DISTINCT FROM finding_judge_confidence
           OR referenced_publication_confidence
                IS DISTINCT FROM finding_publication_confidence
        THEN
            RAISE EXCEPTION
                'referenced finding policy differs from finding policy'
                USING ERRCODE = '23514';
        END IF;
    END IF;

    IF NEW.event_kind = 'accepted'
       AND NEW.judge_confidence * 2000 < finding_judge_confidence
    THEN
        RAISE EXCEPTION
            'independent judge confidence is below the judge threshold'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.event_kind = 'posted'
       AND NOT EXISTS (
           SELECT 1 FROM review_finding_event AS accepted
            WHERE accepted.finding_id = NEW.finding_id
              AND accepted.event_kind = 'accepted'
              AND accepted.judge_confidence * 2000 >= finding_publication_confidence
       )
    THEN
        RAISE EXCEPTION
            'independent judge confidence is below the publication threshold'
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
              AND run_id = NEW.finding_run_id
              AND target_id = NEW.target_id
       )
       OR event_pass_result_ordinal IS DISTINCT FROM NEW.event_ordinal
       OR event_pass_result_event_kind IS DISTINCT FROM NEW.event_kind
       OR event_pass_result_reason IS DISTINCT FROM NEW.reason
       OR event_pass_result_judge_confidence IS DISTINCT FROM NEW.judge_confidence
       OR event_pass_result_referenced_finding
            IS DISTINCT FROM NEW.referenced_finding_id
       OR event_pass_result_referenced_run
            IS DISTINCT FROM NEW.referenced_finding_run_id
       OR event_pass_result_referenced_target
            IS DISTINCT FROM NEW.referenced_finding_target_id
       OR event_pass_result_referenced_pass
            IS DISTINCT FROM NEW.referenced_finding_pass_id
       OR event_pass_result_referenced_status
            IS DISTINCT FROM NEW.referenced_finding_status
       OR event_pass_result_external_link
            IS DISTINCT FROM NEW.external_link_id
    THEN
        RAISE EXCEPTION
            'finding event is not the exact result committed by its pass'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.referenced_finding_id IS NOT NULL THEN
        IF EXISTS (
            WITH RECURSIVE referenced_ancestry(
                finding_id,
                run_id,
                target_id,
                pass_id
            ) AS (
                SELECT NEW.referenced_finding_id,
                       NEW.referenced_finding_run_id,
                       NEW.referenced_finding_target_id,
                       NEW.referenced_finding_pass_id
                UNION
                SELECT latest.referenced_finding_id,
                       latest.referenced_finding_run_id,
                       latest.referenced_finding_target_id,
                       latest.referenced_finding_pass_id
                  FROM referenced_ancestry AS ancestry
                  JOIN LATERAL (
                      SELECT referenced_finding_id,
                             referenced_finding_run_id,
                             referenced_finding_target_id,
                             referenced_finding_pass_id
                        FROM review_finding_event
                       WHERE finding_id = ancestry.finding_id
                         AND finding_run_id = ancestry.run_id
                         AND target_id = ancestry.target_id
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
               AND run_id = NEW.finding_run_id
               AND target_id = NEW.target_id
               AND pass_id = finding_producing_pass
        )
        THEN
            RAISE EXCEPTION
                'finding reference would create a cycle'
                USING ERRCODE = '23514';
        END IF;
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
