-- Retain the immutable review identity that created each review thread.

ALTER TABLE repo_watch_cursor
    DISABLE TRIGGER repo_watch_cursor_is_append_only;

UPDATE repo_watch_cursor AS cursor_record
   SET cursor_payload = jsonb_set(
       cursor_record.cursor_payload,
       '{state,pull_requests}',
       COALESCE(
           (
               SELECT jsonb_agg(
                   jsonb_set(
                       pull_request.value,
                       '{threads}',
                       COALESCE(
                           (
                               SELECT jsonb_agg(
                                   thread.value || jsonb_build_object(
                                       'originating_review_id', NULL
                                   )
                                   ORDER BY thread.ordinality
                               )
                                 FROM jsonb_array_elements(
                                     pull_request.value -> 'threads'
                                 ) WITH ORDINALITY AS thread(value, ordinality)
                           ),
                           '[]'::jsonb
                       )
                   )
                   ORDER BY pull_request.ordinality
               )
                 FROM jsonb_array_elements(
                     cursor_record.cursor_payload -> 'state' -> 'pull_requests'
                 ) WITH ORDINALITY AS pull_request(value, ordinality)
           ),
           '[]'::jsonb
       )
   );

ALTER TABLE repo_watch_cursor
    ENABLE TRIGGER repo_watch_cursor_is_append_only;

-- Existing cursor threads predate exact self-cause accounting. They cannot be
-- attributed safely, so retain their unknown identity until the next refresh.
-- The next committed cursor replaces these migrated nulls with provider IDs.
