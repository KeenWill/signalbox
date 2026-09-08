DO $$
DECLARE definition text;
BEGIN
    SELECT pg_get_constraintdef(oid) INTO STRICT definition FROM pg_constraint
        WHERE conrelid = 'replace_lost_runner_result'::regclass
          AND conname = 'replace_lost_runner_result_rejection_kind_check';
    ALTER TABLE replace_lost_runner_result DROP CONSTRAINT replace_lost_runner_result_rejection_kind_check;
    EXECUTE 'ALTER TABLE replace_lost_runner_result ADD CONSTRAINT replace_lost_runner_result_rejection_kind_check CHECK ('
        || substring(definition FROM 8 FOR char_length(definition) - 8)
        || ' OR rejection_kind = ''turn_terminalized'')';
END;
$$;

CREATE OR REPLACE FUNCTION require_runner_placement_boundary() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    record runner_session_placement_record;
    entry semantic_transcript_entry;
    last_member record;
    prior_frontier uuid;
    prior_count bigint;
    longest_prior bigint := 0;
    actual_count bigint;
BEGIN
    SELECT * INTO STRICT record FROM runner_session_placement_record
        WHERE session_id = NEW.session_id AND event_ordinal = NEW.event_ordinal;
    SELECT * INTO STRICT entry FROM semantic_transcript_entry
        WHERE source_session_id = NEW.session_id AND semantic_entry_id = NEW.semantic_entry_id;
    IF record.event_kind <> 'runner_replaced' OR record.placement_revision <> NEW.placement_revision
       OR entry.payload_kind <> 'runner_placement_changed' OR entry.runner_placement_revision <> NEW.placement_revision THEN
        RAISE EXCEPTION 'placement boundary must reference its exact successor record' USING ERRCODE = '23514';
    END IF;
    IF EXISTS (SELECT 1 FROM model_call WHERE session_id = NEW.session_id
        AND state_kind IN ('in_flight', 'cancellation_requested')) THEN
        RAISE EXCEPTION 'placement boundary precedes a model observation' USING ERRCODE = '23514';
    END IF;
    IF (EXISTS (SELECT 1 FROM turn_lifecycle WHERE session_id = NEW.session_id
            AND state_kind = 'active' AND NOT delegation_runtime_terminal AND active_tool_round_call_id IS NOT NULL)
        OR EXISTS (SELECT 1 FROM turn_lifecycle AS turn
            JOIN LATERAL resolve_context_frontier_members(turn.session_id,
                turn_lifecycle_effective_terminal_frontier(turn.session_id, turn.turn_id)) AS member ON true
            WHERE turn.session_id = NEW.session_id AND turn.state_kind = 'terminal'
                AND member.source_session_id = NEW.session_id AND member.semantic_entry_id = NEW.semantic_entry_id))
       AND NOT EXISTS (SELECT 1 FROM tool_batch_transition_outbox_event AS projected
            JOIN context_frontier AS boundary ON boundary.owning_session_id = NEW.session_id
                AND boundary.context_frontier_id = NEW.context_frontier_id
                AND boundary.prefix_context_frontier_id = projected.frontier_id
            WHERE projected.session_id = NEW.session_id AND projected.transition_kind = 'results_projected')
    THEN
        RAISE EXCEPTION 'placement boundary precedes complete batch results' USING ERRCODE = '23514';
    END IF;
    FOR prior_frontier IN
        (SELECT turn_lifecycle_effective_terminal_frontier(session_id, turn_id)
            FROM turn_lifecycle WHERE session_id = NEW.session_id AND state_kind = 'terminal'
              AND terminal_frontier_id IS NOT NULL
              AND NOT EXISTS (SELECT 1 FROM resolve_context_frontier_members(session_id,
                    turn_lifecycle_effective_terminal_frontier(session_id, turn_id)) AS member
                    WHERE member.source_session_id = NEW.session_id AND member.semantic_entry_id = NEW.semantic_entry_id)
              ORDER BY acceptance_position DESC LIMIT 1)
        UNION (SELECT context_frontier_id FROM runner_placement_boundary
            WHERE session_id = NEW.session_id AND placement_revision < NEW.placement_revision
            ORDER BY placement_revision DESC LIMIT 1)
        UNION SELECT projected.frontier_id FROM tool_batch_transition_outbox_event AS projected
            JOIN context_frontier AS boundary ON boundary.owning_session_id = NEW.session_id
                AND boundary.context_frontier_id = NEW.context_frontier_id
                AND boundary.prefix_context_frontier_id = projected.frontier_id
            WHERE projected.session_id = NEW.session_id AND projected.transition_kind = 'results_projected'
        UNION SELECT seed_context_frontier_id FROM imported_session_seed WHERE session_id = NEW.session_id
        UNION SELECT call.context_frontier_id FROM turn_lifecycle AS turn
            JOIN model_call AS call ON call.model_call_id = turn.recovery_model_call_id
                AND call.session_id = turn.session_id AND call.turn_id = turn.turn_id
            WHERE turn.session_id = NEW.session_id AND turn.state_kind = 'active'
                AND turn.active_phase_kind = 'awaiting_model_call_recovery'
                AND call.state_kind = 'terminal' AND call.terminal_disposition_kind = 'ambiguous'
        UNION SELECT compaction.result_frontier_id FROM context_compaction AS compaction
            WHERE compaction.session_id = NEW.session_id AND NOT EXISTS (SELECT 1 FROM context_compaction AS successor
                WHERE successor.predecessor_compaction_id = compaction.context_compaction_id)
    LOOP
        SELECT count(*) INTO prior_count FROM resolve_context_frontier_members(NEW.session_id, prior_frontier);
        longest_prior := greatest(longest_prior, prior_count);
        IF EXISTS (SELECT 1 FROM resolve_context_frontier_members(NEW.session_id, prior_frontier) AS prior
            LEFT JOIN resolve_context_frontier_members(NEW.session_id, NEW.context_frontier_id) AS next
                USING (member_position)
            WHERE prior.source_session_id IS DISTINCT FROM next.source_session_id
               OR prior.semantic_entry_id IS DISTINCT FROM next.semantic_entry_id) THEN
            RAISE EXCEPTION 'placement boundary must extend the authoritative prefix' USING ERRCODE = '23514';
        END IF;
    END LOOP;
    SELECT count(*) INTO actual_count FROM resolve_context_frontier_members(NEW.session_id, NEW.context_frontier_id);
    IF actual_count <> longest_prior + 1 THEN
        RAISE EXCEPTION 'placement boundary appends exactly one entry' USING ERRCODE = '23514';
    END IF;
    SELECT * INTO last_member FROM resolve_context_frontier_members(NEW.session_id, NEW.context_frontier_id)
        ORDER BY member_position DESC LIMIT 1;
    IF last_member.source_session_id IS DISTINCT FROM NEW.session_id
       OR last_member.semantic_entry_id IS DISTINCT FROM NEW.semantic_entry_id THEN
        RAISE EXCEPTION 'placement boundary must end its frontier' USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

DO $migration$
DECLARE definition text;
BEGIN
    SELECT pg_get_functiondef('assert_model_call_steering_final_state(uuid)'::regprocedure) INTO definition;
    definition := replace(definition,
        'suffix_start_count :=
            result_boundary_count + result_request_count;',
        'suffix_start_count :=
            result_boundary_count + result_request_count;
        IF EXISTS (
            SELECT 1 FROM context_frontier_member AS member
            JOIN runner_placement_boundary AS relocation
                ON relocation.session_id = member.source_session_id
                AND relocation.semantic_entry_id = member.semantic_entry_id
            WHERE member.owning_session_id = checked_session
                AND member.context_frontier_id = checked_frontier
                AND member.member_position = suffix_start_count + 1
                AND context_frontier_preserves_prefix(checked_session, relocation.context_frontier_id, checked_frontier)
        ) THEN
            suffix_start_count := suffix_start_count + 1;
        END IF;');
    EXECUTE definition;
END;
$migration$;

DO $migration$
DECLARE
    definition text;
    prefix_end integer;
    suffix_start integer;
    result_check text;
BEGIN
    SELECT pg_get_functiondef('assert_tool_loop_turn_final_state_pre_delegation(uuid)'::regprocedure) INTO definition;
    definition := replace(definition, 'terminal_member_count numeric(20, 0);',
        'terminal_member_count numeric(20, 0); terminal_result_boundary_count numeric;');
    prefix_end := strpos(definition, '            SELECT count(*)
              INTO matching_terminal_round_count');
    suffix_start := strpos(definition, '            IF matching_terminal_round_count <> 1');
    result_check := substring(definition FROM prefix_end FOR suffix_start - prefix_end);
    definition := overlay(definition PLACING
        '            terminal_result_boundary_count := terminal_member_count;
            IF EXISTS (
                SELECT 1 FROM resolve_context_frontier_members(lifecycle.session_id,
                    lifecycle.terminal_frontier_id) AS member
                JOIN runner_placement_boundary AS relocation
                    ON relocation.session_id = member.source_session_id
                    AND relocation.semantic_entry_id = member.semantic_entry_id
                WHERE member.member_position = terminal_member_count - 1
                    AND context_frontier_preserves_prefix(lifecycle.session_id,
                        relocation.context_frontier_id, lifecycle.terminal_frontier_id)
            ) THEN
                terminal_result_boundary_count := terminal_member_count - 1;
            END IF;
' || replace(result_check, 'terminal_member_count', 'terminal_result_boundary_count')
        FROM prefix_end FOR suffix_start - prefix_end);
    EXECUTE definition;
END;
$migration$;

CREATE TRIGGER runner_recovery_observes_turn_boundary
    AFTER UPDATE OF state_kind, active_phase_kind, delegation_runtime_terminal ON turn_lifecycle
    FOR EACH ROW WHEN (OLD.state_kind IS DISTINCT FROM NEW.state_kind
        OR OLD.active_phase_kind IS DISTINCT FROM NEW.active_phase_kind
        OR OLD.delegation_runtime_terminal IS DISTINCT FROM NEW.delegation_runtime_terminal)
    EXECUTE FUNCTION notify_runner_recovery_authority_change();
