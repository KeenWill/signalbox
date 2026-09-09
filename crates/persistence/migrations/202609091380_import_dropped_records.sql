ALTER TABLE imported_conversation
    ADD COLUMN dropped_record_count numeric(20,0) NOT NULL,
    ADD COLUMN first_dropped_record_position numeric(20,0),
    ADD CONSTRAINT imported_conversation_dropped_record_count_u64
        CHECK (dropped_record_count BETWEEN 0 AND 18446744073709551615),
    ADD CONSTRAINT imported_conversation_first_dropped_position_positive_u64
        CHECK (first_dropped_record_position BETWEEN 1 AND 18446744073709551615),
    ADD CONSTRAINT imported_conversation_dropped_record_facts_complete
        CHECK ((dropped_record_count = 0) = (first_dropped_record_position IS NULL));
