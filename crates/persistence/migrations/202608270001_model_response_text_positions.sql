-- Persist producing-call-local text byte positions so bounded timeline reads
-- seek directly to overlapping response parts instead of rescanning a whole
-- response for every continuation page.
ALTER TABLE semantic_transcript_entry
    ADD COLUMN assistant_response_text_start_bytes numeric(20, 0);

-- This migration is the sole authority permitted to enrich historical
-- append-only transcript rows. Restore the mutation guard immediately after
-- both deterministic backfills complete.
DROP TRIGGER semantic_transcript_entry_is_append_only
    ON semantic_transcript_entry;

-- These constraint triggers observe updates but own lifecycle and tool-result
-- invariants that the two position-only backfills cannot affect. Disable them
-- while historical rows are enriched so unrelated retained state is not
-- revalidated as though the transcript payload itself had changed.
ALTER TABLE semantic_transcript_entry
    DISABLE TRIGGER context_summary_requires_exact_compaction;
ALTER TABLE semantic_transcript_entry
    DISABLE TRIGGER semantic_entry_one_logical_tool_result;
ALTER TABLE semantic_transcript_entry
    DISABLE TRIGGER semantic_entry_requires_steering_final_state;
ALTER TABLE semantic_transcript_entry
    DISABLE TRIGGER semantic_entry_update_requires_matching_turn_state;

-- Completed responses historically did not need part ordinals. Their semantic
-- entries already have durable transcript order through frontier membership;
-- tool-round response entries already carry exact ordinals.
WITH completed_part AS (
    SELECT entry.source_session_id, entry.semantic_entry_id,
           row_number() OVER (
               PARTITION BY entry.producing_model_call_id
               ORDER BY member.member_position
           ) - 1 AS part_ordinal
      FROM semantic_transcript_entry AS entry
      JOIN turn_completed_outbox_event AS completed
        ON completed.session_id = entry.source_session_id
       AND completed.model_call_id = entry.producing_model_call_id
      JOIN context_frontier_member AS member
        ON member.owning_session_id = completed.session_id
       AND member.context_frontier_id = completed.terminal_frontier_id
       AND member.source_session_id = entry.source_session_id
       AND member.semantic_entry_id = entry.semantic_entry_id
     WHERE entry.payload_kind = 'assistant_text'
       AND entry.assistant_response_part_ordinal IS NULL
)
UPDATE semantic_transcript_entry AS entry
   SET assistant_response_part_ordinal = completed_part.part_ordinal
  FROM completed_part
 WHERE entry.source_session_id = completed_part.source_session_id
   AND entry.semantic_entry_id = completed_part.semantic_entry_id;

WITH positioned AS (
    SELECT source_session_id, semantic_entry_id,
           coalesce(
               sum(octet_length(assistant_text_value)) OVER (
                   PARTITION BY producing_model_call_id
                   ORDER BY assistant_response_part_ordinal
                   ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING
               ),
               0
           )::numeric AS text_start_bytes
      FROM semantic_transcript_entry
     WHERE payload_kind = 'assistant_text'
)
UPDATE semantic_transcript_entry AS entry
   SET assistant_response_text_start_bytes = positioned.text_start_bytes
  FROM positioned
 WHERE entry.source_session_id = positioned.source_session_id
   AND entry.semantic_entry_id = positioned.semantic_entry_id;

-- Foreign-key and unique-constraint triggers observe every update even though
-- neither backfill changes their key columns. Flush those queued checks before
-- the next ALTER TABLE; the unrelated lifecycle observers were disabled above
-- and therefore have no events in this transaction.
SET CONSTRAINTS ALL IMMEDIATE;

ALTER TABLE semantic_transcript_entry
    ENABLE TRIGGER context_summary_requires_exact_compaction;
ALTER TABLE semantic_transcript_entry
    ENABLE TRIGGER semantic_entry_one_logical_tool_result;
ALTER TABLE semantic_transcript_entry
    ENABLE TRIGGER semantic_entry_requires_steering_final_state;
ALTER TABLE semantic_transcript_entry
    ENABLE TRIGGER semantic_entry_update_requires_matching_turn_state;

CREATE TRIGGER semantic_transcript_entry_is_append_only
BEFORE UPDATE OR DELETE ON semantic_transcript_entry
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

ALTER TABLE semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_response_text_position_shape
        CHECK (
            (
                payload_kind = 'assistant_text'
                AND assistant_response_part_ordinal IS NOT NULL
                AND assistant_response_text_start_bytes IS NOT NULL
                AND assistant_response_text_start_bytes
                    BETWEEN 0 AND 18446744073709551615
            )
            OR (
                payload_kind <> 'assistant_text'
                AND assistant_response_text_start_bytes IS NULL
            )
        );

CREATE UNIQUE INDEX semantic_transcript_response_text_position_once
    ON semantic_transcript_entry (
        producing_model_call_id, assistant_response_text_start_bytes
    )
    WHERE payload_kind = 'assistant_text';

CREATE FUNCTION require_contiguous_assistant_response_text_positions()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    previous_end numeric;
    next_start numeric;
BEGIN
    SELECT entry.assistant_response_text_start_bytes
               + octet_length(entry.assistant_text_value)::numeric
      INTO previous_end
      FROM semantic_transcript_entry AS entry
     WHERE entry.producing_model_call_id = NEW.producing_model_call_id
       AND entry.payload_kind = 'assistant_text'
       AND entry.assistant_response_part_ordinal
           < NEW.assistant_response_part_ordinal
     ORDER BY entry.assistant_response_part_ordinal DESC
     LIMIT 1;

    IF coalesce(previous_end, 0)
        <> NEW.assistant_response_text_start_bytes THEN
        RAISE EXCEPTION
            'assistant response text positions are not contiguous for model call %',
            NEW.producing_model_call_id
            USING
                ERRCODE = '23514',
                CONSTRAINT =
                    'semantic_transcript_response_text_positions_contiguous';
    END IF;

    SELECT entry.assistant_response_text_start_bytes
      INTO next_start
      FROM semantic_transcript_entry AS entry
     WHERE entry.producing_model_call_id = NEW.producing_model_call_id
       AND entry.payload_kind = 'assistant_text'
       AND entry.assistant_response_part_ordinal
           > NEW.assistant_response_part_ordinal
     ORDER BY entry.assistant_response_part_ordinal ASC
     LIMIT 1;

    IF next_start IS NOT NULL
       AND NEW.assistant_response_text_start_bytes
               + octet_length(NEW.assistant_text_value)::numeric
           <> next_start THEN
        RAISE EXCEPTION
            'assistant response text positions are not contiguous for model call %',
            NEW.producing_model_call_id
            USING
                ERRCODE = '23514',
                CONSTRAINT =
                    'semantic_transcript_response_text_positions_contiguous';
    END IF;

    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER semantic_transcript_response_text_positions_contiguous
AFTER INSERT ON semantic_transcript_entry
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
WHEN (NEW.payload_kind = 'assistant_text')
EXECUTE FUNCTION require_contiguous_assistant_response_text_positions();
