ALTER TABLE runner_registration_tool
DROP CONSTRAINT runner_registration_tool_permission_closed;

ALTER TABLE runner_registration_tool
ADD CONSTRAINT runner_registration_tool_permission_closed
CHECK (permission_kind IN ('auto', 'confirm', 'always_confirm'));
