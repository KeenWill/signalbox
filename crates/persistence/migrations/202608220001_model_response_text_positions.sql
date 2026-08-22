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
BEGIN
    IF EXISTS (
        SELECT 1
          FROM (
              SELECT entry.assistant_response_text_start_bytes AS actual_start,
                     coalesce(
                         sum(octet_length(entry.assistant_text_value)) OVER (
                             ORDER BY entry.assistant_response_part_ordinal
                             ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING
                         ),
                         0
                     )::numeric AS expected_start
                FROM semantic_transcript_entry AS entry
               WHERE entry.producing_model_call_id =
                     NEW.producing_model_call_id
                 AND entry.payload_kind = 'assistant_text'
          ) AS positioned
         WHERE positioned.actual_start <> positioned.expected_start
    ) THEN
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
