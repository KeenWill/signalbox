-- Crash-safe orchestration response recovery before the owner-global receipt.

ALTER TABLE review_orchestration_command
    ADD CONSTRAINT review_orchestration_command_operation_result
        CHECK (
            (operation_kind = 'start' AND result_stage = 'started')
            OR (operation_kind = 'import' AND result_stage IN
                ('import_incomplete', 'awaiting_concerns'))
            OR (operation_kind = 'concern' AND result_stage IN
                ('awaiting_concerns', 'fanout_incomplete', 'awaiting_judgment'))
            OR (operation_kind = 'judgment_plan' AND result_stage IN
                ('awaiting_judgment_effects', 'awaiting_repair'))
            OR (operation_kind = 'judgment_effect' AND result_stage IN
                ('awaiting_judgment_effects', 'judgment_incomplete', 'awaiting_repair'))
            OR (operation_kind = 'repair' AND result_stage IN
                ('repair_incomplete', 'awaiting_publication'))
            OR (operation_kind = 'publication' AND result_stage IN
                ('publication_incomplete', 'complete'))
        ),
    ADD CONSTRAINT review_orchestration_command_result_progress_shape
        CHECK (
            (result_stage IN ('started', 'awaiting_import')
                AND NOT import_recorded AND concern_claim_count = 0
                AND NOT fanout_sealed AND NOT judgment_plan_sealed
                AND judgment_effect_count = 0 AND repair_inventory_count IS NULL
                AND NOT repair_outcomes_recorded AND publication_inventory_count IS NULL
                AND NOT publication_outcomes_recorded)
            OR (result_stage = 'import_incomplete'
                AND import_recorded AND concern_claim_count = 0
                AND NOT fanout_sealed AND NOT judgment_plan_sealed
                AND judgment_effect_count = 0 AND repair_inventory_count IS NULL
                AND NOT repair_outcomes_recorded AND publication_inventory_count IS NULL
                AND NOT publication_outcomes_recorded)
            OR (result_stage = 'awaiting_concerns'
                AND import_recorded AND NOT fanout_sealed AND NOT judgment_plan_sealed
                AND judgment_effect_count = 0 AND repair_inventory_count IS NULL
                AND NOT repair_outcomes_recorded AND publication_inventory_count IS NULL
                AND NOT publication_outcomes_recorded)
            OR (result_stage = 'fanout_incomplete'
                AND import_recorded AND concern_claim_count > 0
                AND NOT fanout_sealed AND NOT judgment_plan_sealed
                AND judgment_effect_count = 0 AND repair_inventory_count IS NULL
                AND NOT repair_outcomes_recorded AND publication_inventory_count IS NULL
                AND NOT publication_outcomes_recorded)
            OR (result_stage = 'awaiting_judgment'
                AND import_recorded AND concern_claim_count > 0
                AND fanout_sealed AND NOT judgment_plan_sealed
                AND judgment_effect_count = 0 AND repair_inventory_count IS NULL
                AND NOT repair_outcomes_recorded AND publication_inventory_count IS NULL
                AND NOT publication_outcomes_recorded)
            OR (result_stage IN ('awaiting_judgment_effects', 'judgment_incomplete')
                AND import_recorded AND concern_claim_count > 0
                AND fanout_sealed AND judgment_plan_sealed
                AND repair_inventory_count IS NULL AND NOT repair_outcomes_recorded
                AND publication_inventory_count IS NULL AND NOT publication_outcomes_recorded)
            OR (result_stage = 'awaiting_repair'
                AND import_recorded AND concern_claim_count > 0
                AND fanout_sealed AND judgment_plan_sealed
                AND repair_inventory_count IS NOT NULL AND repair_inventory_count >= 0
                AND NOT repair_outcomes_recorded AND publication_inventory_count IS NULL
                AND NOT publication_outcomes_recorded)
            OR (result_stage = 'repair_incomplete'
                AND import_recorded AND concern_claim_count > 0
                AND fanout_sealed AND judgment_plan_sealed
                AND repair_inventory_count IS NOT NULL AND repair_inventory_count >= 0
                AND repair_outcomes_recorded AND publication_inventory_count IS NULL
                AND NOT publication_outcomes_recorded)
            OR (result_stage = 'awaiting_publication'
                AND import_recorded AND concern_claim_count > 0
                AND fanout_sealed AND judgment_plan_sealed
                AND repair_inventory_count IS NOT NULL AND repair_inventory_count >= 0
                AND repair_outcomes_recorded AND publication_inventory_count IS NOT NULL
                AND publication_inventory_count >= 0 AND NOT publication_outcomes_recorded)
            OR (result_stage IN ('publication_incomplete', 'complete')
                AND import_recorded AND concern_claim_count > 0
                AND fanout_sealed AND judgment_plan_sealed
                AND repair_inventory_count IS NOT NULL AND repair_inventory_count >= 0
                AND repair_outcomes_recorded AND publication_inventory_count IS NOT NULL
                AND publication_inventory_count >= 0 AND publication_outcomes_recorded)
        );

CREATE TABLE review_orchestration_command_recovery (
    command_id uuid PRIMARY KEY,
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
    CONSTRAINT review_orchestration_command_recovery_operation_result CHECK (
        (operation_kind = 'start' AND result_stage = 'started')
        OR (operation_kind = 'import' AND result_stage IN
            ('import_incomplete', 'awaiting_concerns'))
        OR (operation_kind = 'concern' AND result_stage IN
            ('awaiting_concerns', 'fanout_incomplete', 'awaiting_judgment'))
        OR (operation_kind = 'judgment_plan' AND result_stage IN
            ('awaiting_judgment_effects', 'awaiting_repair'))
        OR (operation_kind = 'judgment_effect' AND result_stage IN
            ('awaiting_judgment_effects', 'judgment_incomplete', 'awaiting_repair'))
        OR (operation_kind = 'repair' AND result_stage IN
            ('repair_incomplete', 'awaiting_publication'))
        OR (operation_kind = 'publication' AND result_stage IN
            ('publication_incomplete', 'complete'))
    ),
    CONSTRAINT review_orchestration_command_recovery_result_progress_shape CHECK (
        (result_stage IN ('started', 'awaiting_import')
            AND NOT import_recorded AND concern_claim_count = 0
            AND NOT fanout_sealed AND NOT judgment_plan_sealed
            AND judgment_effect_count = 0 AND repair_inventory_count IS NULL
            AND NOT repair_outcomes_recorded AND publication_inventory_count IS NULL
            AND NOT publication_outcomes_recorded)
        OR (result_stage = 'import_incomplete'
            AND import_recorded AND concern_claim_count = 0
            AND NOT fanout_sealed AND NOT judgment_plan_sealed
            AND judgment_effect_count = 0 AND repair_inventory_count IS NULL
            AND NOT repair_outcomes_recorded AND publication_inventory_count IS NULL
            AND NOT publication_outcomes_recorded)
        OR (result_stage = 'awaiting_concerns'
            AND import_recorded AND NOT fanout_sealed AND NOT judgment_plan_sealed
            AND judgment_effect_count = 0 AND repair_inventory_count IS NULL
            AND NOT repair_outcomes_recorded AND publication_inventory_count IS NULL
            AND NOT publication_outcomes_recorded)
        OR (result_stage = 'fanout_incomplete'
            AND import_recorded AND concern_claim_count > 0
            AND NOT fanout_sealed AND NOT judgment_plan_sealed
            AND judgment_effect_count = 0 AND repair_inventory_count IS NULL
            AND NOT repair_outcomes_recorded AND publication_inventory_count IS NULL
            AND NOT publication_outcomes_recorded)
        OR (result_stage = 'awaiting_judgment'
            AND import_recorded AND concern_claim_count > 0
            AND fanout_sealed AND NOT judgment_plan_sealed
            AND judgment_effect_count = 0 AND repair_inventory_count IS NULL
            AND NOT repair_outcomes_recorded AND publication_inventory_count IS NULL
            AND NOT publication_outcomes_recorded)
        OR (result_stage IN ('awaiting_judgment_effects', 'judgment_incomplete')
            AND import_recorded AND concern_claim_count > 0
            AND fanout_sealed AND judgment_plan_sealed
            AND repair_inventory_count IS NULL AND NOT repair_outcomes_recorded
            AND publication_inventory_count IS NULL AND NOT publication_outcomes_recorded)
        OR (result_stage = 'awaiting_repair'
            AND import_recorded AND concern_claim_count > 0
            AND fanout_sealed AND judgment_plan_sealed
            AND repair_inventory_count IS NOT NULL AND repair_inventory_count >= 0
            AND NOT repair_outcomes_recorded AND publication_inventory_count IS NULL
            AND NOT publication_outcomes_recorded)
        OR (result_stage = 'repair_incomplete'
            AND import_recorded AND concern_claim_count > 0
            AND fanout_sealed AND judgment_plan_sealed
            AND repair_inventory_count IS NOT NULL AND repair_inventory_count >= 0
            AND repair_outcomes_recorded AND publication_inventory_count IS NULL
            AND NOT publication_outcomes_recorded)
        OR (result_stage = 'awaiting_publication'
            AND import_recorded AND concern_claim_count > 0
            AND fanout_sealed AND judgment_plan_sealed
            AND repair_inventory_count IS NOT NULL AND repair_inventory_count >= 0
            AND repair_outcomes_recorded AND publication_inventory_count IS NOT NULL
            AND publication_inventory_count >= 0 AND NOT publication_outcomes_recorded)
        OR (result_stage IN ('publication_incomplete', 'complete')
            AND import_recorded AND concern_claim_count > 0
            AND fanout_sealed AND judgment_plan_sealed
            AND repair_inventory_count IS NOT NULL AND repair_inventory_count >= 0
            AND repair_outcomes_recorded AND publication_inventory_count IS NOT NULL
            AND publication_inventory_count >= 0 AND publication_outcomes_recorded)
    )
);

CREATE TRIGGER review_orchestration_command_recovery_is_append_only
    BEFORE UPDATE OR DELETE ON review_orchestration_command_recovery
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER review_orchestration_command_recovery_reject_truncate
    BEFORE TRUNCATE ON review_orchestration_command_recovery
    FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();
