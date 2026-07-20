-- Provisional owner-decided accepted-input content size bound.
--
-- The decision log entry dated 2026-07-20 records the owner's provisional
-- one-mebibyte (1,048,576 UTF-8 bytes) bound on accepted-input user text.
-- The application admission boundary rejects oversized text before typed
-- command construction. These checks independently protect the durable
-- representation. convert_to makes the measure UTF-8 bytes regardless of the
-- database's server encoding. Existing rows satisfy the bound trivially.

ALTER TABLE submit_input_command
    ADD CONSTRAINT submit_input_command_content_bounded
    CHECK (octet_length(convert_to(content_text, 'UTF8')) <= 1048576);

ALTER TABLE accepted_input
    ADD CONSTRAINT accepted_input_content_bounded
    CHECK (octet_length(convert_to(content_text, 'UTF8')) <= 1048576);
