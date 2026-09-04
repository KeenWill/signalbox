-- A release command records whether it also opened the session start gate.

ALTER TABLE session_lifecycle_command
    DROP CONSTRAINT session_lifecycle_command_result_shape,
    ADD CONSTRAINT session_lifecycle_command_result_shape CHECK (
        (result_kind = ANY (ARRAY['applied'::text, 'rejected'::text]))
        AND ((result_kind = 'rejected'::text) = (rejection_kind IS NOT NULL))
        AND ((result_kind = 'applied'::text) = (applied_effect_kind IS NOT NULL))
        AND ((applied_effect_kind IS NULL) OR (applied_effect_kind = ANY (ARRAY[
            'start_released'::text,
            'closed'::text,
            'closure_pending'::text,
            'resumed'::text,
            'ownership_changed'::text
        ])))
        AND ((applied_effect_kind IS NULL)
             OR ((operation_kind = 'release_start'::text)
                 AND (applied_effect_kind = 'start_released'::text))
             OR ((operation_kind = ANY (ARRAY[
                    'stop'::text, 'supersede'::text,
                    'abandon'::text, 'close_failed'::text
                 ])) AND (applied_effect_kind = ANY (ARRAY[
                    'closed'::text, 'closure_pending'::text
                 ])))
             OR ((operation_kind = 'resume'::text)
                 AND (applied_effect_kind = 'resumed'::text))
             OR ((operation_kind = 'adopt'::text)
                 AND (applied_effect_kind = 'ownership_changed'::text))
             OR ((operation_kind = 'release'::text)
                 AND (applied_effect_kind = ANY (ARRAY[
                    'start_released'::text, 'ownership_changed'::text
                 ]))))
        AND ((applied_effect_kind IS NOT DISTINCT FROM 'closure_pending'::text)
             = (live_turn_id IS NOT NULL))
    );
