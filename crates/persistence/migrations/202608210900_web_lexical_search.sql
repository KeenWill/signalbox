-- A dedicated full-text projection keeps browser search bounded and separate
-- from canonical transcript, metadata, tool, and derived-artifact authority.
-- Product queries use plainto_tsquery through the application adapter; this
-- table's PostgreSQL representation is never part of the browser contract.
CREATE TABLE web_search_projection (
    projection_id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    source_kind text COLLATE "C" NOT NULL,
    source_id uuid NOT NULL,
    session_id uuid NOT NULL,
    event_sequence numeric(20, 0) NOT NULL,
    owner_kind text COLLATE "C" NOT NULL,
    owner_id uuid NOT NULL,
    turn_id uuid,
    content_class text COLLATE "C" NOT NULL,
    content_text text NOT NULL,
    search_vector tsvector GENERATED ALWAYS AS (
        to_tsvector('simple', content_text)
    ) STORED,

    CONSTRAINT web_search_projection_source_once
        UNIQUE (source_kind, source_id, content_class),
    CONSTRAINT web_search_projection_session_fk
        FOREIGN KEY (session_id) REFERENCES session(session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    CONSTRAINT web_search_projection_event_positive
        CHECK (event_sequence BETWEEN 1 AND 18446744073709551615),
    CONSTRAINT web_search_projection_source_closed
        CHECK (source_kind IN (
            'accepted_input', 'semantic_entry', 'tool_request', 'tool_attempt',
            'session_metadata', 'attachment', 'derived_artifact'
        )),
    CONSTRAINT web_search_projection_owner_closed
        CHECK (owner_kind IN (
            'session', 'accepted_input', 'transcript_entry',
            'tool_request', 'tool_attempt', 'attachment', 'derived_artifact'
        )),
    CONSTRAINT web_search_projection_owner_shape
        CHECK (
            (owner_kind = 'session' AND owner_id = session_id AND turn_id IS NULL)
            OR (owner_kind IN ('accepted_input', 'tool_request', 'tool_attempt')
                AND turn_id IS NOT NULL)
            OR owner_kind IN ('transcript_entry', 'attachment', 'derived_artifact')
        ),
    CONSTRAINT web_search_projection_content_class_closed
        CHECK (content_class IN (
            'user_transcript', 'assistant_transcript', 'tool_arguments',
            'tool_result', 'session_metadata', 'attachment_filename',
            'attachment_media_metadata', 'derived_text_artifact'
        )),
    CONSTRAINT web_search_projection_text_bound
        CHECK (octet_length(content_text) BETWEEN 1 AND 1048576)
);

CREATE INDEX web_search_projection_vector
    ON web_search_projection USING gin(search_vector);
CREATE INDEX web_search_projection_global_page
    ON web_search_projection (event_sequence DESC, projection_id DESC);
CREATE INDEX web_search_projection_session_page
    ON web_search_projection (
        session_id, event_sequence DESC, projection_id DESC
    );

CREATE FUNCTION project_web_search_accepted_input()
RETURNS trigger LANGUAGE plpgsql
SET search_path FROM CURRENT AS $$
BEGIN
    INSERT INTO web_search_projection (
        source_kind, source_id, session_id, event_sequence,
        owner_kind, owner_id, turn_id, content_class, content_text
    )
    SELECT 'accepted_input', input.accepted_input_id, input.session_id,
           NEW.event_sequence, 'accepted_input', input.accepted_input_id,
           input.origin_turn_id, 'user_transcript', input.content_text
      FROM accepted_input AS input
     WHERE input.accepted_input_id = NEW.accepted_input_id
    ON CONFLICT (source_kind, source_id, content_class) DO NOTHING;
    RETURN NULL;
END
$$;

CREATE CONSTRAINT TRIGGER input_accepted_projects_web_search
AFTER INSERT ON input_accepted_outbox_event
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION project_web_search_accepted_input();

CREATE FUNCTION project_web_search_assistant_text()
RETURNS trigger LANGUAGE plpgsql
SET search_path FROM CURRENT AS $$
BEGIN
    IF NEW.call_state_kind <> 'terminal' THEN
        RETURN NULL;
    END IF;
    INSERT INTO web_search_projection (
        source_kind, source_id, session_id, event_sequence,
        owner_kind, owner_id, turn_id, content_class, content_text
    )
    SELECT 'semantic_entry', entry.semantic_entry_id, entry.source_session_id,
           NEW.event_sequence, 'transcript_entry', entry.semantic_entry_id,
           NEW.turn_id, 'assistant_transcript', entry.assistant_text_value
      FROM semantic_transcript_entry AS entry
     WHERE entry.producing_model_call_id = NEW.model_call_id
       AND entry.payload_kind = 'assistant_text'
    ON CONFLICT (source_kind, source_id, content_class) DO NOTHING;
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
            owner_kind, owner_id, turn_id, content_class, content_text
        )
        SELECT 'tool_request', request.request_id, request.session_id,
               NEW.event_sequence, 'tool_request', request.request_id,
               request.turn_id, 'tool_arguments', request.arguments_text
          FROM tool_request AS request
         WHERE request.producing_model_call_id = NEW.producing_model_call_id
        ON CONFLICT (source_kind, source_id, content_class) DO NOTHING;
    ELSIF NEW.transition_kind = 'results_projected' THEN
        INSERT INTO web_search_projection (
            source_kind, source_id, session_id, event_sequence,
            owner_kind, owner_id, turn_id, content_class, content_text
        )
        SELECT 'tool_attempt', attempt.attempt_id, attempt.session_id,
               NEW.event_sequence, 'tool_attempt', attempt.attempt_id,
               attempt.turn_id, 'tool_result', attempt.result_text
          FROM tool_request AS request
          JOIN tool_attempt AS attempt ON attempt.request_id = request.request_id
         WHERE request.producing_model_call_id = NEW.producing_model_call_id
           AND attempt.result_text IS NOT NULL
           AND attempt.result_text <> ''
        ON CONFLICT (source_kind, source_id, content_class) DO NOTHING;
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
        owner_kind, owner_id, turn_id, content_class, content_text
    )
    SELECT 'semantic_entry', entry.semantic_entry_id, entry.source_session_id,
           NEW.event_sequence, 'transcript_entry', entry.semantic_entry_id,
           NULL, 'derived_text_artifact', entry.context_summary_value
      FROM semantic_transcript_entry AS entry
     WHERE entry.semantic_entry_id = NEW.summary_entry_id
       AND entry.payload_kind = 'context_summary'
    ON CONFLICT (source_kind, source_id, content_class) DO NOTHING;
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
        owner_kind, owner_id, turn_id, content_class, content_text
    ) VALUES (
        'session_metadata', NEW.session_id, NEW.session_id, anchor_sequence,
        'session', NEW.session_id, NULL, 'session_metadata', projected_text
    )
    ON CONFLICT (source_kind, source_id, content_class) DO UPDATE
       SET content_text = EXCLUDED.content_text;
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
    owner_kind, owner_id, turn_id, content_class, content_text
)
SELECT 'accepted_input', input.accepted_input_id, input.session_id,
       event.event_sequence, 'accepted_input', input.accepted_input_id,
       input.origin_turn_id, 'user_transcript', input.content_text
  FROM accepted_input AS input
  JOIN input_accepted_outbox_event AS event
    ON event.accepted_input_id = input.accepted_input_id;

INSERT INTO web_search_projection (
    source_kind, source_id, session_id, event_sequence,
    owner_kind, owner_id, turn_id, content_class, content_text
)
SELECT 'semantic_entry', entry.semantic_entry_id, entry.source_session_id,
       event.event_sequence, 'transcript_entry', entry.semantic_entry_id,
       call.turn_id, 'assistant_transcript', entry.assistant_text_value
  FROM semantic_transcript_entry AS entry
  JOIN model_call AS call ON call.model_call_id = entry.producing_model_call_id
  JOIN model_call_transition_outbox_event AS event
    ON event.model_call_id = call.model_call_id
   AND event.call_state_kind = 'terminal'
 WHERE entry.payload_kind = 'assistant_text';

INSERT INTO web_search_projection (
    source_kind, source_id, session_id, event_sequence,
    owner_kind, owner_id, turn_id, content_class, content_text
)
SELECT 'tool_request', request.request_id, request.session_id,
       event.event_sequence, 'tool_request', request.request_id,
       request.turn_id, 'tool_arguments', request.arguments_text
  FROM tool_request AS request
  JOIN tool_batch_transition_outbox_event AS event
    ON event.producing_model_call_id = request.producing_model_call_id
   AND event.transition_kind = 'proposed';

INSERT INTO web_search_projection (
    source_kind, source_id, session_id, event_sequence,
    owner_kind, owner_id, turn_id, content_class, content_text
)
SELECT 'tool_attempt', attempt.attempt_id, attempt.session_id,
       event.event_sequence, 'tool_attempt', attempt.attempt_id,
       attempt.turn_id, 'tool_result', attempt.result_text
  FROM tool_attempt AS attempt
  JOIN tool_request AS request ON request.request_id = attempt.request_id
  JOIN tool_batch_transition_outbox_event AS event
    ON event.producing_model_call_id = request.producing_model_call_id
   AND event.transition_kind = 'results_projected'
 WHERE attempt.result_text IS NOT NULL AND attempt.result_text <> '';

INSERT INTO web_search_projection (
    source_kind, source_id, session_id, event_sequence,
    owner_kind, owner_id, turn_id, content_class, content_text
)
SELECT 'semantic_entry', entry.semantic_entry_id, entry.source_session_id,
       event.event_sequence, 'transcript_entry', entry.semantic_entry_id,
       NULL, 'derived_text_artifact', entry.context_summary_value
  FROM semantic_transcript_entry AS entry
  JOIN context_compacted_outbox_event AS event
    ON event.summary_entry_id = entry.semantic_entry_id
 WHERE entry.payload_kind = 'context_summary';

INSERT INTO web_search_projection (
    source_kind, source_id, session_id, event_sequence,
    owner_kind, owner_id, turn_id, content_class, content_text
)
SELECT 'session_metadata', metadata.session_id, metadata.session_id,
       created.event_sequence, 'session', metadata.session_id,
       NULL, 'session_metadata', projected.content_text
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
 WHERE projected.content_text <> '';
