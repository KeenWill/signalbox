ALTER TABLE tool_attempt ADD COLUMN context_error_detail text;

ALTER TABLE tool_attempt DISABLE TRIGGER tool_attempt_changes_are_guarded;
UPDATE tool_attempt SET context_error_detail = error_detail;
SET CONSTRAINTS ALL IMMEDIATE;
ALTER TABLE tool_attempt ENABLE TRIGGER tool_attempt_changes_are_guarded;

ALTER TABLE tool_attempt ADD CONSTRAINT tool_attempt_context_error_shape CHECK (
    (error_detail IS NULL) = (context_error_detail IS NULL)
);
