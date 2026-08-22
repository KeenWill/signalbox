-- Cursor-compatible support for bounded catalog pages filtered by exact source
-- format and converter interpretation.

CREATE INDEX imported_conversation_format_catalog_idx
    ON imported_conversation (
        source_format,
        converter_version,
        imported_conversation_id
    );
