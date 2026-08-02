-- Retain the caller-selected descendant scope on both parent termination
-- command families. Later statements in this migration own the remaining
-- delegated-session persistence surface.

ALTER TABLE goal_command
    ADD COLUMN descendant_scope text;

UPDATE goal_command
   SET descendant_scope = 'parent_alone'
 WHERE operation_kind = 'stop';

ALTER TABLE goal_command
    ADD CONSTRAINT goal_command_descendant_scope_shape CHECK (
        (
            operation_kind = 'stop'
            AND descendant_scope IS NOT NULL
            AND descendant_scope IN ('parent_alone', 'parent_and_descendants')
        )
        OR (operation_kind <> 'stop' AND descendant_scope IS NULL)
    );

ALTER TABLE submit_input_command
    ADD COLUMN descendant_scope text;

UPDATE submit_input_command
   SET descendant_scope = 'parent_alone'
 WHERE delivery_kind = 'interrupt';

ALTER TABLE submit_input_command
    ADD CONSTRAINT submit_input_command_descendant_scope_shape CHECK (
        (
            delivery_kind = 'interrupt'
            AND descendant_scope IS NOT NULL
            AND descendant_scope IN ('parent_alone', 'parent_and_descendants')
        )
        OR (delivery_kind <> 'interrupt' AND descendant_scope IS NULL)
    );

ALTER TABLE accepted_input
    ADD COLUMN descendant_scope text;

UPDATE accepted_input
   SET descendant_scope = 'parent_alone'
 WHERE delivery_kind = 'interrupt';

ALTER TABLE accepted_input
    ADD CONSTRAINT accepted_input_descendant_scope_shape CHECK (
        (
            delivery_kind = 'interrupt'
            AND descendant_scope IS NOT NULL
            AND descendant_scope IN ('parent_alone', 'parent_and_descendants')
        )
        OR (delivery_kind <> 'interrupt' AND descendant_scope IS NULL)
    );
