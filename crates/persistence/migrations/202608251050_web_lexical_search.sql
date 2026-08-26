-- A dedicated full-text projection keeps browser search bounded and separate
-- from canonical transcript, metadata, tool, and derived-artifact authority.
-- Product queries use plainto_tsquery through the application adapter; this
-- table's PostgreSQL representation is never part of the browser contract.
--
-- This file was renumbered above `202608251000_multipart_user_content.sql`
-- after that migration merged, so it now installs against the post-multipart
-- schema: `accepted_input.content_text` no longer exists, and an accepted
-- input's user text is the ordered `accepted_input_content_part` rows whose
-- kind is `text`. Every accepted-input and steering-input source below — the
-- two trigger bodies and the two one-time backfills — therefore reads
-- `accepted_input_projected_text` instead of that dropped column.
CREATE FUNCTION web_search_projection_chunks(source_text text)
RETURNS TABLE (ordinal integer, content_text text)
LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
SET search_path FROM CURRENT AS $$
    SELECT chunk.ordinal,
           substring(source_text FROM chunk.ordinal * 15872 + 1 FOR 16384)
      FROM generate_series(
               0, (char_length(source_text) - 1) / 15872
           ) AS chunk(ordinal)
$$;

-- The searchable user text of one accepted input, after
-- `202608251000_multipart_user_content.sql` moved that text out of
-- `accepted_input.content_text` and into ordered content parts. This is the
-- same source `append_session_timeline_input_bytes` was repaired to read in
-- that migration: the parts whose kind is `text`, in position order.
--
-- The parts join with a newline, exactly as the session-metadata projection
-- below joins a title, its tags, and its attributes, so no two parts fuse
-- into a lexeme neither of them contains. Attachment parts carry a digest and
-- bounded declarations, never projected user text, so they are excluded and
-- an input made only of attachments composes null. The chunker above is
-- STRICT, so such an input yields no chunk and contributes no projection row.
-- Every caller keeps its former shape otherwise — one `user_transcript` row
-- per chunk, keyed by the same chunk ordinal.
CREATE FUNCTION accepted_input_projected_text(checked_input uuid)
RETURNS text LANGUAGE sql STABLE STRICT PARALLEL SAFE
SET search_path FROM CURRENT AS $$
    SELECT string_agg(part.text_value, E'\n' ORDER BY part.position)
      FROM accepted_input_content_part AS part
     WHERE part.accepted_input_id = checked_input
       AND part.part_kind = 'text'
$$;

CREATE TABLE web_search_projection (
    projection_id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    item_kind text COLLATE "C" NOT NULL,
    item_id uuid NOT NULL,
    session_id uuid NOT NULL,
    event_sequence numeric(20, 0) NOT NULL,
    source_kind text COLLATE "C" NOT NULL,
    source_id uuid NOT NULL,
    turn_id uuid,
    content_class text COLLATE "C" NOT NULL,
    projection_ordinal integer NOT NULL DEFAULT 0,
    content_text text NOT NULL,
    search_vector tsvector GENERATED ALWAYS AS (
        to_tsvector('simple', content_text)
    ) STORED,

    CONSTRAINT web_search_projection_source_once
        UNIQUE (source_kind, source_id, content_class, projection_ordinal),
    CONSTRAINT web_search_projection_session_fk
        FOREIGN KEY (session_id) REFERENCES session(session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    CONSTRAINT web_search_projection_event_positive
        CHECK (event_sequence BETWEEN 1 AND 18446744073709551615),
    CONSTRAINT web_search_projection_item_closed
        CHECK (item_kind IN (
            'session', 'accepted_input', 'transcript_entry',
            'tool_request', 'tool_attempt', 'attachment', 'derived_artifact'
        )),
    CONSTRAINT web_search_projection_source_closed
        CHECK (source_kind IN (
            'accepted_input', 'steering_input', 'semantic_entry',
            'tool_request', 'tool_attempt', 'session_metadata', 'attachment',
            'derived_artifact'
        )),
    CONSTRAINT web_search_projection_item_shape
        CHECK (
            (item_kind = 'session' AND item_id = session_id AND turn_id IS NULL)
            OR (item_kind IN ('accepted_input', 'tool_request', 'tool_attempt')
                AND turn_id IS NOT NULL)
            OR item_kind IN ('transcript_entry', 'attachment', 'derived_artifact')
        ),
    CONSTRAINT web_search_projection_content_class_closed
        CHECK (content_class IN (
            'user_transcript', 'assistant_transcript', 'tool_arguments',
            'tool_result', 'session_metadata', 'attachment_filename',
            'attachment_media_metadata', 'derived_text_artifact'
        )),
    CONSTRAINT web_search_projection_ordinal_bound
        CHECK (projection_ordinal >= 0),
    CONSTRAINT web_search_projection_text_bound
        CHECK (octet_length(content_text) BETWEEN 1 AND 65536)
);

CREATE INDEX web_search_projection_vector
    ON web_search_projection USING gin(search_vector);
CREATE INDEX web_search_projection_global_page
    ON web_search_projection (event_sequence DESC, projection_id DESC);
CREATE INDEX web_search_projection_session_page
    ON web_search_projection (
        session_id, event_sequence DESC, projection_id DESC
    );

CREATE FUNCTION web_search_projection_requires_session_address()
RETURNS trigger LANGUAGE plpgsql
SET search_path FROM CURRENT AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM outbox_event
         WHERE session_id = NEW.session_id
           AND event_sequence = NEW.event_sequence
        UNION ALL
        SELECT 1
          FROM delegation_outbox_event
         WHERE session_id = NEW.session_id
           AND event_sequence = NEW.event_sequence
    ) THEN
        RAISE EXCEPTION
            'web search projection address does not belong to its session';
    END IF;
    RETURN NULL;
END
$$;

CREATE CONSTRAINT TRIGGER web_search_projection_requires_session_address
AFTER INSERT OR UPDATE OF session_id, event_sequence ON web_search_projection
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION web_search_projection_requires_session_address();

CREATE FUNCTION project_web_search_accepted_input()
RETURNS trigger LANGUAGE plpgsql
SET search_path FROM CURRENT AS $$
BEGIN
    INSERT INTO web_search_projection (
        source_kind, source_id, session_id, event_sequence,
        item_kind, item_id, turn_id, content_class,
        projection_ordinal, content_text
    )
    SELECT 'accepted_input', input.accepted_input_id, input.session_id,
           NEW.event_sequence, 'accepted_input', input.accepted_input_id,
           input.origin_turn_id, 'user_transcript',
           chunk.ordinal, chunk.content_text
      FROM accepted_input AS input
     CROSS JOIN LATERAL web_search_projection_chunks(
         accepted_input_projected_text(input.accepted_input_id)
     ) AS chunk
     WHERE input.accepted_input_id = NEW.accepted_input_id
    ON CONFLICT (
        source_kind, source_id, content_class, projection_ordinal
    ) DO NOTHING;
    RETURN NULL;
END
$$;

CREATE CONSTRAINT TRIGGER input_accepted_projects_web_search
AFTER INSERT ON input_accepted_outbox_event
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION project_web_search_accepted_input();

CREATE FUNCTION project_web_search_steering_input()
RETURNS trigger LANGUAGE plpgsql
SET search_path FROM CURRENT AS $$
BEGIN
    IF NEW.payload_kind <> 'steering_accepted_input' THEN
        RETURN NULL;
    END IF;
    INSERT INTO web_search_projection (
        source_kind, source_id, session_id, event_sequence,
        item_kind, item_id, turn_id, content_class,
        projection_ordinal, content_text
    )
    SELECT 'steering_input', input.accepted_input_id, input.session_id,
           event.event_sequence, 'accepted_input', input.accepted_input_id,
           NEW.steering_source_turn_id, 'user_transcript',
           chunk.ordinal, chunk.content_text
      FROM accepted_input AS input
      JOIN model_call_transition_outbox_event AS event
        ON event.model_call_id = input.consuming_model_call_id
       AND event.call_state_kind = 'prepared'
     CROSS JOIN LATERAL web_search_projection_chunks(
         accepted_input_projected_text(input.accepted_input_id)
     ) AS chunk
     WHERE input.accepted_input_id = NEW.origin_accepted_input_id
       AND input.session_id = NEW.source_session_id
       AND input.disposition_kind = 'consumed_as_steering'
    ON CONFLICT (
        source_kind, source_id, content_class, projection_ordinal
    ) DO NOTHING;
    RETURN NULL;
END
$$;

CREATE CONSTRAINT TRIGGER steering_input_projects_web_search
AFTER INSERT ON semantic_transcript_entry
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION project_web_search_steering_input();

CREATE FUNCTION project_web_search_assistant_text()
RETURNS trigger LANGUAGE plpgsql
SET search_path FROM CURRENT AS $$
BEGIN
    IF NEW.call_state_kind <> 'terminal' THEN
        RETURN NULL;
    END IF;
    INSERT INTO web_search_projection (
        source_kind, source_id, session_id, event_sequence,
        item_kind, item_id, turn_id, content_class,
        projection_ordinal, content_text
    )
    SELECT 'semantic_entry', entry.semantic_entry_id, entry.source_session_id,
           NEW.event_sequence, 'transcript_entry', entry.semantic_entry_id,
           NEW.turn_id, 'assistant_transcript',
           chunk.ordinal, chunk.content_text
      FROM semantic_transcript_entry AS entry
     CROSS JOIN LATERAL web_search_projection_chunks(
         entry.assistant_text_value
     ) AS chunk
     WHERE entry.producing_model_call_id = NEW.model_call_id
       AND entry.payload_kind = 'assistant_text'
    ON CONFLICT (
        source_kind, source_id, content_class, projection_ordinal
    ) DO NOTHING;
    RETURN NULL;
END
$$;

CREATE CONSTRAINT TRIGGER model_call_terminal_projects_web_search
AFTER INSERT ON model_call_transition_outbox_event
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION project_web_search_assistant_text();

CREATE FUNCTION project_web_search_tool_batch()
RETURNS trigger LANGUAGE plpgsql
SET search_path FROM CURRENT AS $$
BEGIN
    IF NEW.transition_kind = 'proposed' THEN
        INSERT INTO web_search_projection (
            source_kind, source_id, session_id, event_sequence,
            item_kind, item_id, turn_id, content_class,
            projection_ordinal, content_text
        )
        SELECT 'tool_request', request.request_id, request.session_id,
               NEW.event_sequence, 'tool_request', request.request_id,
               request.turn_id, 'tool_arguments',
               chunk.ordinal, chunk.content_text
          FROM tool_request AS request
         CROSS JOIN LATERAL web_search_projection_chunks(
             request.arguments_text
         ) AS chunk
         WHERE request.producing_model_call_id = NEW.producing_model_call_id
           AND request.arguments_text <> ''
        ON CONFLICT (
            source_kind, source_id, content_class, projection_ordinal
        ) DO NOTHING;
    ELSIF NEW.transition_kind = 'results_projected' THEN
        INSERT INTO web_search_projection (
            source_kind, source_id, session_id, event_sequence,
            item_kind, item_id, turn_id, content_class,
            projection_ordinal, content_text
        )
        SELECT 'tool_attempt', attempt.attempt_id, attempt.session_id,
               NEW.event_sequence, 'tool_attempt', attempt.attempt_id,
               attempt.turn_id, 'tool_result',
               chunk.ordinal, chunk.content_text
          FROM tool_request AS request
          JOIN tool_attempt AS attempt ON attempt.request_id = request.request_id
         CROSS JOIN LATERAL web_search_projection_chunks(
             attempt.result_text
         ) AS chunk
         WHERE request.producing_model_call_id = NEW.producing_model_call_id
           AND attempt.result_text IS NOT NULL
           AND attempt.result_text <> ''
        ON CONFLICT (
            source_kind, source_id, content_class, projection_ordinal
        ) DO NOTHING;
    END IF;
    RETURN NULL;
END
$$;

CREATE CONSTRAINT TRIGGER tool_batch_projects_web_search
AFTER INSERT ON tool_batch_transition_outbox_event
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION project_web_search_tool_batch();

CREATE FUNCTION project_web_search_context_summary()
RETURNS trigger LANGUAGE plpgsql
SET search_path FROM CURRENT AS $$
BEGIN
    INSERT INTO web_search_projection (
        source_kind, source_id, session_id, event_sequence,
        item_kind, item_id, turn_id, content_class,
        projection_ordinal, content_text
    )
    SELECT 'semantic_entry', entry.semantic_entry_id, entry.source_session_id,
           NEW.event_sequence, 'transcript_entry', entry.semantic_entry_id,
           NULL, 'derived_text_artifact', chunk.ordinal, chunk.content_text
      FROM semantic_transcript_entry AS entry
     CROSS JOIN LATERAL web_search_projection_chunks(
         entry.context_summary_value
     ) AS chunk
     WHERE entry.semantic_entry_id = NEW.summary_entry_id
       AND entry.payload_kind = 'context_summary'
    ON CONFLICT (
        source_kind, source_id, content_class, projection_ordinal
    ) DO NOTHING;
    RETURN NULL;
END
$$;

CREATE CONSTRAINT TRIGGER context_compacted_projects_web_search
AFTER INSERT ON context_compacted_outbox_event
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION project_web_search_context_summary();

CREATE FUNCTION project_web_search_session_metadata()
RETURNS trigger LANGUAGE plpgsql
SET search_path FROM CURRENT AS $$
DECLARE
    projected_text text;
    anchor_sequence numeric(20, 0);
BEGIN
    SELECT concat_ws(
               E'\n', metadata.title,
               (SELECT string_agg(tag.tag, E'\n' ORDER BY tag.tag)
                  FROM session_metadata_tag AS tag
                 WHERE tag.session_id = metadata.session_id),
               (SELECT string_agg(
                           attribute.attribute_key || E'\n' || attribute.attribute_value,
                           E'\n' ORDER BY attribute.attribute_key
                       )
                  FROM session_metadata_attribute AS attribute
                 WHERE attribute.session_id = metadata.session_id)
           ), created.event_sequence
      INTO projected_text, anchor_sequence
      FROM session_metadata AS metadata
      JOIN session_created_outbox_event AS created
        ON created.session_id = metadata.session_id
     WHERE metadata.session_id = NEW.session_id;
    IF projected_text IS NULL OR projected_text = '' THEN
        DELETE FROM web_search_projection
         WHERE source_kind = 'session_metadata'
           AND source_id = NEW.session_id
           AND content_class = 'session_metadata';
        RETURN NULL;
    END IF;
    INSERT INTO web_search_projection (
        source_kind, source_id, session_id, event_sequence,
        item_kind, item_id, turn_id, content_class,
        projection_ordinal, content_text
    ) SELECT
        'session_metadata', NEW.session_id, NEW.session_id, anchor_sequence,
        'session', NEW.session_id, NULL, 'session_metadata',
        chunk.ordinal, chunk.content_text
      FROM web_search_projection_chunks(projected_text) AS chunk
    ON CONFLICT (
        source_kind, source_id, content_class, projection_ordinal
    ) DO UPDATE
       SET content_text = EXCLUDED.content_text;
    DELETE FROM web_search_projection
     WHERE source_kind = 'session_metadata'
       AND source_id = NEW.session_id
       AND content_class = 'session_metadata'
       AND projection_ordinal >= (
           SELECT count(*)
             FROM web_search_projection_chunks(projected_text)
       );
    RETURN NULL;
END
$$;

CREATE CONSTRAINT TRIGGER session_metadata_installation_projects_web_search
AFTER INSERT ON session_metadata_installation
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION project_web_search_session_metadata();

-- Existing canonical state is projected once by the migration. Subsequent
-- maintenance is event-driven by the exact durable event that supplies each
-- result's stable reveal address.
INSERT INTO web_search_projection (
    source_kind, source_id, session_id, event_sequence,
    item_kind, item_id, turn_id, content_class,
    projection_ordinal, content_text
)
SELECT 'accepted_input', input.accepted_input_id, input.session_id,
       event.event_sequence, 'accepted_input', input.accepted_input_id,
       input.origin_turn_id, 'user_transcript',
       chunk.ordinal, chunk.content_text
  FROM accepted_input AS input
  JOIN input_accepted_outbox_event AS event
    ON event.accepted_input_id = input.accepted_input_id
 CROSS JOIN LATERAL web_search_projection_chunks(
     accepted_input_projected_text(input.accepted_input_id)
 ) AS chunk;

INSERT INTO web_search_projection (
    source_kind, source_id, session_id, event_sequence,
    item_kind, item_id, turn_id, content_class,
    projection_ordinal, content_text
)
SELECT 'steering_input', input.accepted_input_id, input.session_id,
       event.event_sequence, 'accepted_input', input.accepted_input_id,
       entry.steering_source_turn_id, 'user_transcript',
       chunk.ordinal, chunk.content_text
  FROM semantic_transcript_entry AS entry
  JOIN accepted_input AS input
    ON input.accepted_input_id = entry.origin_accepted_input_id
   AND input.session_id = entry.source_session_id
  JOIN model_call_transition_outbox_event AS event
    ON event.model_call_id = input.consuming_model_call_id
   AND event.call_state_kind = 'prepared'
 CROSS JOIN LATERAL web_search_projection_chunks(
     accepted_input_projected_text(input.accepted_input_id)
 ) AS chunk
 WHERE entry.payload_kind = 'steering_accepted_input'
   AND input.disposition_kind = 'consumed_as_steering';

INSERT INTO web_search_projection (
    source_kind, source_id, session_id, event_sequence,
    item_kind, item_id, turn_id, content_class,
    projection_ordinal, content_text
)
SELECT 'semantic_entry', entry.semantic_entry_id, entry.source_session_id,
       event.event_sequence, 'transcript_entry', entry.semantic_entry_id,
       call.turn_id, 'assistant_transcript',
       chunk.ordinal, chunk.content_text
  FROM semantic_transcript_entry AS entry
  JOIN model_call AS call ON call.model_call_id = entry.producing_model_call_id
  JOIN model_call_transition_outbox_event AS event
    ON event.model_call_id = call.model_call_id
   AND event.call_state_kind = 'terminal'
 CROSS JOIN LATERAL web_search_projection_chunks(
     entry.assistant_text_value
 ) AS chunk
 WHERE entry.payload_kind = 'assistant_text';

INSERT INTO web_search_projection (
    source_kind, source_id, session_id, event_sequence,
    item_kind, item_id, turn_id, content_class,
    projection_ordinal, content_text
)
SELECT 'tool_request', request.request_id, request.session_id,
       event.event_sequence, 'tool_request', request.request_id,
       request.turn_id, 'tool_arguments',
       chunk.ordinal, chunk.content_text
  FROM tool_request AS request
  JOIN tool_batch_transition_outbox_event AS event
    ON event.producing_model_call_id = request.producing_model_call_id
   AND event.transition_kind = 'proposed'
 CROSS JOIN LATERAL web_search_projection_chunks(
     request.arguments_text
 ) AS chunk
 WHERE request.arguments_text <> '';

INSERT INTO web_search_projection (
    source_kind, source_id, session_id, event_sequence,
    item_kind, item_id, turn_id, content_class,
    projection_ordinal, content_text
)
SELECT 'tool_attempt', attempt.attempt_id, attempt.session_id,
       event.event_sequence, 'tool_attempt', attempt.attempt_id,
       attempt.turn_id, 'tool_result', chunk.ordinal, chunk.content_text
  FROM tool_attempt AS attempt
  JOIN tool_request AS request ON request.request_id = attempt.request_id
  JOIN tool_batch_transition_outbox_event AS event
    ON event.producing_model_call_id = request.producing_model_call_id
   AND event.transition_kind = 'results_projected'
 CROSS JOIN LATERAL web_search_projection_chunks(attempt.result_text) AS chunk
 WHERE attempt.result_text IS NOT NULL AND attempt.result_text <> '';

INSERT INTO web_search_projection (
    source_kind, source_id, session_id, event_sequence,
    item_kind, item_id, turn_id, content_class,
    projection_ordinal, content_text
)
SELECT 'semantic_entry', entry.semantic_entry_id, entry.source_session_id,
       event.event_sequence, 'transcript_entry', entry.semantic_entry_id,
       NULL, 'derived_text_artifact', chunk.ordinal, chunk.content_text
  FROM semantic_transcript_entry AS entry
  JOIN context_compacted_outbox_event AS event
    ON event.summary_entry_id = entry.semantic_entry_id
 CROSS JOIN LATERAL web_search_projection_chunks(
     entry.context_summary_value
 ) AS chunk
 WHERE entry.payload_kind = 'context_summary';

INSERT INTO web_search_projection (
    source_kind, source_id, session_id, event_sequence,
    item_kind, item_id, turn_id, content_class,
    projection_ordinal, content_text
)
SELECT 'session_metadata', metadata.session_id, metadata.session_id,
       created.event_sequence, 'session', metadata.session_id,
       NULL, 'session_metadata', chunk.ordinal, chunk.content_text
  FROM session_metadata AS metadata
  JOIN session_created_outbox_event AS created
    ON created.session_id = metadata.session_id
 CROSS JOIN LATERAL (
     SELECT concat_ws(
                E'\n', metadata.title,
                (SELECT string_agg(tag.tag, E'\n' ORDER BY tag.tag)
                   FROM session_metadata_tag AS tag
                  WHERE tag.session_id = metadata.session_id),
                (SELECT string_agg(
                            attribute.attribute_key || E'\n' || attribute.attribute_value,
                            E'\n' ORDER BY attribute.attribute_key
                        )
                   FROM session_metadata_attribute AS attribute
                  WHERE attribute.session_id = metadata.session_id)
            ) AS content_text
 ) AS projected
 CROSS JOIN LATERAL web_search_projection_chunks(
     projected.content_text
 ) AS chunk
 WHERE projected.content_text <> '';
