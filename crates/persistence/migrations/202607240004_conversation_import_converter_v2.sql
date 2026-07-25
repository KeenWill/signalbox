-- Admit the maximum-fidelity Claude Code converter without reinterpreting
-- stored converter-version-1 snapshots.

ALTER TABLE imported_conversation
    DROP CONSTRAINT imported_conversation_converter_version_supported;

ALTER TABLE imported_conversation
    ADD CONSTRAINT imported_conversation_converter_version_supported
        CHECK (converter_version IN (1, 2));
