-- Blobs, imports, and web projections: the content-addressed blob catalog
-- with replicas, store bindings, and derivations; the blob-read tool budget;
-- imported conversations with their raw records, transcript entries, and
-- session seeds; and the denormalized web search and usage projections.

-- Function bodies may read tables a later section or file creates;
-- 202609010000_core.sql explains why validation is deferred.
SET check_function_bodies = false;

--
-- Functions.
--

--
-- Name: assert_imported_conversation_complete(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_imported_conversation_complete(checked_imported_conversation_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    expected_raw_count numeric(20, 0);
    expected_entry_count numeric(20, 0);
    actual_raw_count numeric(20, 0);
    first_raw_position numeric(20, 0);
    last_raw_position numeric(20, 0);
    actual_entry_count numeric(20, 0);
    first_entry_position numeric(20, 0);
    last_entry_position numeric(20, 0);
BEGIN
    SELECT declared_raw_record_count, declared_entry_count
      INTO expected_raw_count, expected_entry_count
      FROM imported_conversation
     WHERE imported_conversation_id = checked_imported_conversation_id;

    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT count(*)::numeric(20, 0),
           min(raw_record_position),
           max(raw_record_position)
      INTO actual_raw_count, first_raw_position, last_raw_position
      FROM imported_conversation_raw_record
     WHERE imported_conversation_id = checked_imported_conversation_id;

    SELECT count(*)::numeric(20, 0),
           min(imported_entry_position),
           max(imported_entry_position)
      INTO actual_entry_count, first_entry_position, last_entry_position
      FROM imported_transcript_entry
     WHERE imported_conversation_id = checked_imported_conversation_id;

    IF actual_raw_count <> expected_raw_count
       OR first_raw_position <> 1
       OR last_raw_position <> expected_raw_count
       OR actual_entry_count <> expected_entry_count
       OR first_entry_position <> 1
       OR last_entry_position <> expected_entry_count
       OR EXISTS (
           SELECT 1
             FROM imported_conversation_raw_record AS raw_record
             LEFT JOIN LATERAL (
                 SELECT count(*)::numeric(20, 0) AS actual_count,
                        min(record_entry_position) AS first_position,
                        max(record_entry_position) AS last_position
                   FROM imported_transcript_entry AS entry
                  WHERE entry.imported_conversation_id =
                            raw_record.imported_conversation_id
                    AND entry.raw_record_position =
                            raw_record.raw_record_position
             ) AS membership ON true
            WHERE raw_record.imported_conversation_id =
                      checked_imported_conversation_id
              AND (
                  membership.actual_count <> raw_record.declared_entry_count
                  OR membership.first_position <> 1
                  OR membership.last_position <> raw_record.declared_entry_count
              )
       )
       OR EXISTS (
           SELECT 1
             FROM imported_transcript_entry AS entry
             JOIN (
                 SELECT raw_record_position,
                        COALESCE(
                            sum(declared_entry_count) OVER (
                                ORDER BY raw_record_position
                                ROWS BETWEEN UNBOUNDED PRECEDING
                                    AND 1 PRECEDING
                            ),
                            0
                        ) AS earlier_entry_count
                   FROM imported_conversation_raw_record
                  WHERE imported_conversation_id =
                            checked_imported_conversation_id
             ) AS raw_record_prefix
               ON raw_record_prefix.raw_record_position =
                      entry.raw_record_position
            WHERE entry.imported_conversation_id =
                      checked_imported_conversation_id
              AND entry.imported_entry_position <>
                      raw_record_prefix.earlier_entry_count
                      + entry.record_entry_position
       )
    THEN
        RAISE EXCEPTION
            'imported conversation % does not have complete contiguous membership',
            checked_imported_conversation_id
            USING ERRCODE = '23514';
    END IF;
END;
$$;


--
-- Name: assert_imported_session_seed_complete(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_imported_session_seed_complete(checked_session_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    checked_ancestry_kind text;
    checked_imported_conversation_id uuid;
    checked_frontier_entry_id uuid;
    checked_frontier_position numeric(20, 0);
    seed_frontier_id uuid;
    seed_present boolean;
    seed_member_count numeric(20, 0);
    imported_semantic_count numeric(20, 0);
BEGIN
    SELECT ancestry_kind,
           imported_conversation_id,
           imported_frontier_entry_id,
           imported_frontier_position
      INTO checked_ancestry_kind,
           checked_imported_conversation_id,
           checked_frontier_entry_id,
           checked_frontier_position
      FROM session
     WHERE session_id = checked_session_id;

    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT seed_context_frontier_id
      INTO seed_frontier_id
      FROM imported_session_seed
     WHERE session_id = checked_session_id;
    seed_present := FOUND;

    SELECT count(*)::numeric(20, 0)
      INTO imported_semantic_count
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session_id
       AND payload_kind = 'imported_entry';

    IF checked_ancestry_kind <> 'imported_conversation' THEN
        IF seed_present OR imported_semantic_count <> 0 THEN
            RAISE EXCEPTION
                'non-imported session % cannot own an imported seed',
                checked_session_id
                USING
                    ERRCODE = '23514',
                    CONSTRAINT = 'imported_session_seed_requires_imported_ancestry';
        END IF;
        RETURN;
    END IF;

    IF NOT seed_present THEN
        RAISE EXCEPTION
            'imported session % requires exactly one seed frontier',
            checked_session_id
            USING
                ERRCODE = '23503',
                CONSTRAINT = 'imported_session_requires_seed';
    END IF;

    SELECT member_count
      INTO seed_member_count
      FROM context_frontier
     WHERE owning_session_id = checked_session_id
       AND context_frontier_id = seed_frontier_id;

    IF NOT FOUND OR seed_member_count <> checked_frontier_position THEN
        RAISE EXCEPTION
            'imported session % seed has the wrong prefix length',
            checked_session_id
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'imported_session_seed_exact_prefix';
    END IF;

    PERFORM assert_context_frontier_complete_membership(
        checked_session_id,
        seed_frontier_id
    );

    IF imported_semantic_count <> checked_frontier_position
       OR EXISTS (
           SELECT 1
             FROM context_frontier_member AS member
             LEFT JOIN semantic_transcript_entry AS semantic_entry
               ON semantic_entry.source_session_id = member.source_session_id
              AND semantic_entry.semantic_entry_id = member.semantic_entry_id
             LEFT JOIN imported_transcript_entry AS imported_entry
               ON imported_entry.imported_conversation_id =
                      semantic_entry.imported_conversation_id
              AND imported_entry.imported_transcript_entry_id =
                      semantic_entry.imported_transcript_entry_id
            WHERE member.owning_session_id = checked_session_id
              AND member.context_frontier_id = seed_frontier_id
              AND (
                  member.source_session_id IS DISTINCT FROM checked_session_id
                  OR semantic_entry.payload_kind IS DISTINCT FROM 'imported_entry'
                  OR semantic_entry.imported_conversation_id
                        IS DISTINCT FROM checked_imported_conversation_id
                  OR imported_entry.imported_entry_position
                        IS DISTINCT FROM member.member_position
              )
       )
       OR EXISTS (
           SELECT 1
             FROM semantic_transcript_entry AS semantic_entry
             LEFT JOIN context_frontier_member AS member
               ON member.owning_session_id = checked_session_id
              AND member.context_frontier_id = seed_frontier_id
              AND member.source_session_id = semantic_entry.source_session_id
              AND member.semantic_entry_id = semantic_entry.semantic_entry_id
             JOIN imported_transcript_entry AS imported_entry
               ON imported_entry.imported_conversation_id =
                      semantic_entry.imported_conversation_id
              AND imported_entry.imported_transcript_entry_id =
                      semantic_entry.imported_transcript_entry_id
            WHERE semantic_entry.source_session_id = checked_session_id
              AND semantic_entry.payload_kind = 'imported_entry'
              AND (
                  member.semantic_entry_id IS NULL
                  OR member.member_position
                        IS DISTINCT FROM imported_entry.imported_entry_position
                  OR imported_entry.imported_conversation_id
                        IS DISTINCT FROM checked_imported_conversation_id
                  OR imported_entry.imported_entry_position >
                        checked_frontier_position
              )
       )
    THEN
        RAISE EXCEPTION
            'imported session % seed is not its exact ordered imported prefix',
            checked_session_id
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'imported_session_seed_exact_prefix';
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM imported_transcript_entry
         WHERE imported_conversation_id = checked_imported_conversation_id
           AND imported_transcript_entry_id = checked_frontier_entry_id
           AND imported_entry_position = checked_frontier_position
    ) THEN
        RAISE EXCEPTION
            'imported session % ancestry boundary is unavailable',
            checked_session_id
            USING
                ERRCODE = '23503',
                CONSTRAINT = 'imported_session_seed_source_boundary';
    END IF;
END;
$$;


--
-- Name: bounded_web_usage_profile(text); Type: FUNCTION; Schema: public
--

CREATE FUNCTION bounded_web_usage_profile(value text) RETURNS text
    LANGUAGE plpgsql STRICT
    AS $$
DECLARE
    lookup_digest text;
    mapped_id bigint;
BEGIN
    IF octet_length(value) <= 250 THEN
        RETURN 'exact:' || value;
    END IF;

    lookup_digest := md5(value);
    -- Serialize one bounded digest bucket while retaining exact collision
    -- resolution without indexing the unbounded canonical reference.
    PERFORM pg_advisory_xact_lock(hashtextextended(lookup_digest, 0));
    SELECT profile_id
      INTO mapped_id
      FROM web_usage_oversized_profile_identity
     WHERE reference_digest = lookup_digest
       AND exact_reference = value;
    IF mapped_id IS NULL THEN
        INSERT INTO web_usage_oversized_profile_identity (
            reference_digest, exact_reference
        ) VALUES (lookup_digest, value)
        RETURNING profile_id INTO mapped_id;
    END IF;
    RETURN 'mapped:' || mapped_id::text;
END;
$$;


--
-- Name: check_blob_derivation_complete(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION check_blob_derivation_complete() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    selected_id uuid;
    expected_inputs smallint;
    expected_outputs smallint;
    observed_inputs bigint;
    observed_outputs bigint;
    minimum_input_ordinal smallint;
    maximum_input_ordinal smallint;
    minimum_output_ordinal smallint;
    maximum_output_ordinal smallint;
BEGIN
    selected_id := NEW.derivation_id;
    SELECT input_count, output_count
      INTO expected_inputs, expected_outputs
      FROM blob_derivation
     WHERE derivation_id = selected_id;
    SELECT count(*), min(input_ordinal), max(input_ordinal)
      INTO observed_inputs, minimum_input_ordinal, maximum_input_ordinal
      FROM blob_derivation_input
     WHERE derivation_id = selected_id;
    SELECT count(*), min(output_ordinal), max(output_ordinal)
      INTO observed_outputs, minimum_output_ordinal, maximum_output_ordinal
      FROM blob_derivation_output
     WHERE derivation_id = selected_id;
    IF expected_inputs IS NULL
       OR observed_inputs <> expected_inputs
       OR observed_outputs <> expected_outputs
       OR minimum_input_ordinal <> 0
       OR maximum_input_ordinal <> expected_inputs - 1
       OR minimum_output_ordinal <> 0
       OR maximum_output_ordinal <> expected_outputs - 1 THEN
        RAISE EXCEPTION 'blob derivation record is incomplete';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: enforce_web_usage_oversized_profile_identity(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION enforce_web_usage_oversized_profile_identity() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    expected_digest text;
BEGIN
    expected_digest := md5(NEW.exact_reference);
    IF NEW.reference_digest <> expected_digest THEN
        RAISE EXCEPTION 'oversized usage profile digest must match its exact reference'
            USING ERRCODE = '23514';
    END IF;

    -- Serialize the bounded digest bucket so exact-reference uniqueness does
    -- not require an index over the unbounded canonical reference.
    PERFORM pg_advisory_xact_lock(hashtextextended(expected_digest, 0));
    IF EXISTS (
        SELECT 1
          FROM web_usage_oversized_profile_identity
         WHERE reference_digest = expected_digest
           AND exact_reference = NEW.exact_reference
    ) THEN
        RAISE EXCEPTION 'oversized usage profile reference already has an identity'
            USING ERRCODE = '23505';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: imported_content_encoding_kind(bytea); Type: FUNCTION; Schema: public
--

CREATE FUNCTION imported_content_encoding_kind(encoded bytea) RETURNS smallint
    LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE
    AS $$
DECLARE encoding_version integer; content_kind integer; next_at integer := 3;
BEGIN
    IF octet_length(encoded) < 3 THEN RAISE EXCEPTION 'truncated imported content header' USING ERRCODE = '23514'; END IF;
    encoding_version := get_byte(encoded, 0);
    IF encoding_version NOT IN (1, 2) OR get_byte(encoded, 1) <> 1 THEN
        RAISE EXCEPTION 'invalid imported content header' USING ERRCODE = '23514';
    END IF;
    content_kind := get_byte(encoded, 2);
    IF content_kind IN (0, 1, 5, 8) THEN
        next_at := imported_encoding_skip_attestation(encoded, next_at, 'text', encoding_version);
    ELSIF content_kind = 2 THEN
        next_at := imported_encoding_skip_attestation(encoded, next_at, 'text', encoding_version);
        next_at := imported_encoding_skip_attestation(encoded, next_at, 'text', encoding_version);
        next_at := imported_encoding_skip_attestation(encoded, next_at, 'structured', encoding_version);
        next_at := imported_encoding_skip_attestation(encoded, next_at, 'structured', encoding_version);
    ELSIF content_kind = 3 THEN
        next_at := imported_encoding_skip_attestation(encoded, next_at, 'text', encoding_version);
        next_at := imported_encoding_skip_attestation(encoded, next_at, 'tool_result', encoding_version);
        next_at := imported_encoding_skip_attestation(encoded, next_at, 'boolean', encoding_version);
    ELSIF content_kind = 4 THEN
        next_at := imported_encoding_skip_attestation(encoded, next_at, 'text', encoding_version);
        next_at := imported_encoding_skip_attestation(encoded, next_at, 'text', encoding_version);
    ELSIF content_kind = 6 THEN
        next_at := imported_encoding_skip_attestation(encoded, next_at, 'media_source', encoding_version);
    ELSIF content_kind = 7 THEN
        IF next_at >= octet_length(encoded) OR get_byte(encoded, next_at) NOT BETWEEN 0 AND 4 THEN
            RAISE EXCEPTION 'invalid imported message-content absence' USING ERRCODE = '23514';
        END IF; next_at := next_at + 1;
    ELSE RAISE EXCEPTION 'unsupported imported content kind %', content_kind USING ERRCODE = '23514'; END IF;
    IF next_at <> octet_length(encoded) THEN RAISE EXCEPTION 'trailing imported content bytes' USING ERRCODE = '23514'; END IF;
    RETURN content_kind::smallint;
END; $$;


--
-- Name: imported_encoding_length_at(bytea, integer); Type: FUNCTION; Schema: public
--

CREATE FUNCTION imported_encoding_length_at(encoded bytea, start_at integer) RETURNS bigint
    LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE
    AS $$
DECLARE result bigint := 0; position integer;
BEGIN
    IF start_at < 0 OR start_at + 8 > octet_length(encoded) THEN
        RAISE EXCEPTION 'truncated imported encoding length' USING ERRCODE = '23514';
    END IF;
    FOR position IN start_at..start_at + 7 LOOP
        result := result * 256 + get_byte(encoded, position);
        IF result > 2147483647 THEN
            RAISE EXCEPTION 'imported encoding length is out of range' USING ERRCODE = '23514';
        END IF;
    END LOOP;
    RETURN result;
END; $$;


--
-- Name: imported_encoding_skip_attestation(bytea, integer, text, integer); Type: FUNCTION; Schema: public
--

CREATE FUNCTION imported_encoding_skip_attestation(encoded bytea, start_at integer, value_kind text, encoding_version integer) RETURNS integer
    LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE
    AS $$
DECLARE tag integer; next_at integer;
BEGIN
    IF start_at >= octet_length(encoded) THEN
        RAISE EXCEPTION 'truncated imported source attestation' USING ERRCODE = '23514';
    END IF;
    tag := get_byte(encoded, start_at); next_at := start_at + 1;
    IF tag IN (0, 1) THEN RETURN next_at;
    ELSIF tag <> 2 THEN RAISE EXCEPTION 'invalid imported source attestation' USING ERRCODE = '23514'; END IF;
    IF value_kind = 'text' THEN RETURN imported_encoding_skip_text(encoded, next_at);
    ELSIF value_kind = 'boolean' THEN RETURN imported_encoding_skip_boolean(encoded, next_at);
    ELSIF value_kind = 'structured' THEN RETURN imported_encoding_skip_structured(encoded, next_at, 0);
    ELSIF value_kind = 'media_source' THEN RETURN imported_encoding_skip_media_source(encoded, next_at);
    ELSIF value_kind = 'tool_result' THEN RETURN imported_encoding_skip_tool_result(encoded, next_at, encoding_version);
    END IF;
    RAISE EXCEPTION 'unsupported imported attestation value kind %', value_kind USING ERRCODE = '23514';
END; $$;


--
-- Name: imported_encoding_skip_boolean(bytea, integer); Type: FUNCTION; Schema: public
--

CREATE FUNCTION imported_encoding_skip_boolean(encoded bytea, start_at integer) RETURNS integer
    LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE
    AS $$
BEGIN
    IF start_at >= octet_length(encoded) OR get_byte(encoded, start_at) NOT IN (0, 1) THEN
        RAISE EXCEPTION 'invalid imported boolean encoding' USING ERRCODE = '23514';
    END IF;
    RETURN start_at + 1;
END; $$;


--
-- Name: imported_encoding_skip_media_source(bytea, integer); Type: FUNCTION; Schema: public
--

CREATE FUNCTION imported_encoding_skip_media_source(encoded bytea, start_at integer) RETURNS integer
    LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE
    AS $$
DECLARE next_at integer := start_at; field integer; tag integer;
BEGIN
    FOR field IN 1..3 LOOP
        IF next_at >= octet_length(encoded) THEN
            RAISE EXCEPTION 'truncated imported media-source attestation' USING ERRCODE = '23514';
        END IF;
        tag := get_byte(encoded, next_at); next_at := next_at + 1;
        IF tag = 2 THEN next_at := imported_encoding_skip_text(encoded, next_at);
        ELSIF tag NOT IN (0, 1) THEN
            RAISE EXCEPTION 'invalid imported media-source attestation' USING ERRCODE = '23514';
        END IF;
    END LOOP;
    RETURN next_at;
END; $$;


--
-- Name: imported_encoding_skip_number(bytea, integer); Type: FUNCTION; Schema: public
--

CREATE FUNCTION imported_encoding_skip_number(encoded bytea, start_at integer) RETURNS integer
    LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE
    AS $_$
DECLARE payload_bytes bigint; next_at integer; value text;
BEGIN
    payload_bytes := imported_encoding_length_at(encoded, start_at);
    next_at := imported_encoding_skip_text(encoded, start_at);
    value := convert_from(
        substring(encoded FROM start_at + 9 FOR payload_bytes::integer),
        'UTF8'
    );
    IF value !~ '^-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?$' THEN
        RAISE EXCEPTION 'invalid imported JSON number' USING ERRCODE = '23514';
    END IF;
    RETURN next_at;
END; $_$;


--
-- Name: imported_encoding_skip_structured(bytea, integer, integer); Type: FUNCTION; Schema: public
--

CREATE FUNCTION imported_encoding_skip_structured(encoded bytea, start_at integer, nesting_depth integer) RETURNS integer
    LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE
    AS $$
DECLARE tag integer; item_count bigint; item bigint; next_at integer;
BEGIN
    IF start_at >= octet_length(encoded) THEN
        RAISE EXCEPTION 'invalid imported structured encoding' USING ERRCODE = '23514';
    END IF;
    tag := get_byte(encoded, start_at); next_at := start_at + 1;
    IF tag = 0 THEN RETURN next_at;
    ELSIF tag = 1 THEN RETURN imported_encoding_skip_boolean(encoded, next_at);
    ELSIF tag = 2 THEN RETURN imported_encoding_skip_number(encoded, next_at);
    ELSIF tag = 3 THEN RETURN imported_encoding_skip_text(encoded, next_at);
    ELSIF tag IN (4, 5) THEN
        IF nesting_depth >= 128 THEN
            RAISE EXCEPTION 'imported structured container depth exceeded' USING ERRCODE = '23514';
        END IF;
        item_count := imported_encoding_length_at(encoded, next_at); next_at := next_at + 8;
        IF item_count > octet_length(encoded) - next_at THEN
            RAISE EXCEPTION 'invalid imported structured item count' USING ERRCODE = '23514';
        END IF;
        IF item_count > 0 THEN
            FOR item IN 1..item_count LOOP
                IF tag = 5 THEN next_at := imported_encoding_skip_text(encoded, next_at); END IF;
                next_at := imported_encoding_skip_structured(encoded, next_at, nesting_depth + 1);
            END LOOP;
        END IF;
        RETURN next_at;
    END IF;
    RAISE EXCEPTION 'unsupported imported structured tag %', tag USING ERRCODE = '23514';
END; $$;


--
-- Name: imported_encoding_skip_text(bytea, integer); Type: FUNCTION; Schema: public
--

CREATE FUNCTION imported_encoding_skip_text(encoded bytea, start_at integer) RETURNS integer
    LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE
    AS $$
DECLARE
    payload_bytes bigint;
    next_at bigint;
    position integer;
    first_byte integer;
    second_byte integer;
BEGIN
    payload_bytes := imported_encoding_length_at(encoded, start_at);
    next_at := start_at::bigint + 8 + payload_bytes;
    IF next_at > octet_length(encoded) THEN
        RAISE EXCEPTION 'truncated imported text encoding' USING ERRCODE = '23514';
    END IF;
    position := start_at + 8;
    WHILE position < next_at LOOP
        first_byte := get_byte(encoded, position);
        IF first_byte <= 127 THEN
            position := position + 1;
        ELSIF first_byte BETWEEN 194 AND 223 THEN
            IF position + 1 >= next_at
                OR get_byte(encoded, position + 1) NOT BETWEEN 128 AND 191 THEN
                RAISE EXCEPTION 'invalid imported text UTF-8' USING ERRCODE = '23514';
            END IF;
            position := position + 2;
        ELSIF first_byte BETWEEN 224 AND 239 THEN
            IF position + 2 >= next_at THEN
                RAISE EXCEPTION 'invalid imported text UTF-8' USING ERRCODE = '23514';
            END IF;
            second_byte := get_byte(encoded, position + 1);
            IF (first_byte = 224 AND second_byte NOT BETWEEN 160 AND 191)
                OR (first_byte = 237 AND second_byte NOT BETWEEN 128 AND 159)
                OR (first_byte NOT IN (224, 237) AND second_byte NOT BETWEEN 128 AND 191)
                OR get_byte(encoded, position + 2) NOT BETWEEN 128 AND 191 THEN
                RAISE EXCEPTION 'invalid imported text UTF-8' USING ERRCODE = '23514';
            END IF;
            position := position + 3;
        ELSIF first_byte BETWEEN 240 AND 244 THEN
            IF position + 3 >= next_at THEN
                RAISE EXCEPTION 'invalid imported text UTF-8' USING ERRCODE = '23514';
            END IF;
            second_byte := get_byte(encoded, position + 1);
            IF (first_byte = 240 AND second_byte NOT BETWEEN 144 AND 191)
                OR (first_byte = 244 AND second_byte NOT BETWEEN 128 AND 143)
                OR (first_byte BETWEEN 241 AND 243 AND second_byte NOT BETWEEN 128 AND 191)
                OR get_byte(encoded, position + 2) NOT BETWEEN 128 AND 191
                OR get_byte(encoded, position + 3) NOT BETWEEN 128 AND 191 THEN
                RAISE EXCEPTION 'invalid imported text UTF-8' USING ERRCODE = '23514';
            END IF;
            position := position + 4;
        ELSE
            RAISE EXCEPTION 'invalid imported text UTF-8' USING ERRCODE = '23514';
        END IF;
    END LOOP;
    RETURN next_at::integer;
END; $$;


--
-- Name: imported_encoding_skip_tool_result(bytea, integer, integer); Type: FUNCTION; Schema: public
--

CREATE FUNCTION imported_encoding_skip_tool_result(encoded bytea, start_at integer, encoding_version integer) RETURNS integer
    LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE
    AS $$
DECLARE tag integer; block_count bigint; block bigint; block_tag integer; next_at integer;
BEGIN
    IF start_at >= octet_length(encoded) THEN
        RAISE EXCEPTION 'truncated imported tool-result encoding' USING ERRCODE = '23514';
    END IF;
    tag := get_byte(encoded, start_at); next_at := start_at + 1;
    IF tag = 0 THEN RETURN imported_encoding_skip_text(encoded, next_at);
    ELSIF tag <> 1 THEN RAISE EXCEPTION 'invalid imported tool-result value tag' USING ERRCODE = '23514'; END IF;
    block_count := imported_encoding_length_at(encoded, next_at); next_at := next_at + 8;
    IF block_count > octet_length(encoded) - next_at THEN
        RAISE EXCEPTION 'invalid imported tool-result block count' USING ERRCODE = '23514';
    END IF;
    IF block_count > 0 THEN
        FOR block IN 1..block_count LOOP
            IF next_at >= octet_length(encoded) THEN
                RAISE EXCEPTION 'truncated imported tool-result block' USING ERRCODE = '23514';
            END IF;
            block_tag := get_byte(encoded, next_at); next_at := next_at + 1;
            IF block_tag IN (0, 2) OR (block_tag = 3 AND encoding_version >= 2) THEN
                next_at := imported_encoding_skip_attestation(encoded, next_at, 'text', encoding_version);
            ELSIF block_tag = 1 THEN
                next_at := imported_encoding_skip_attestation(encoded, next_at, 'media_source', encoding_version);
            ELSE RAISE EXCEPTION 'invalid imported tool-result block tag' USING ERRCODE = '23514'; END IF;
        END LOOP;
    END IF;
    RETURN next_at;
END; $$;


--
-- Name: project_web_search_accepted_input(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION project_web_search_accepted_input() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: project_web_search_assistant_text(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION project_web_search_assistant_text() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: project_web_search_context_summary(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION project_web_search_context_summary() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: project_web_search_steering_input(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION project_web_search_steering_input() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: project_web_search_tool_batch(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION project_web_search_tool_batch() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: reject_blob_catalog_row_mutation(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_blob_catalog_row_mutation() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RAISE EXCEPTION
        'blob catalog rows are append-only'
        USING ERRCODE = '55000';
END;
$$;


--
-- Name: reject_blob_catalog_truncate(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_blob_catalog_truncate() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RAISE EXCEPTION
        'blob catalog tables cannot be truncated'
        USING ERRCODE = '55000';
END;
$$;


--
-- Name: reject_blob_derivation_mutation(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_blob_derivation_mutation() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RAISE EXCEPTION 'blob derivation records are immutable';
END;
$$;


--
-- Name: reject_imported_conversation_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_imported_conversation_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RAISE EXCEPTION 'imported_conversation is append-only'
        USING ERRCODE = '23514';
END;
$$;


--
-- Name: reject_imported_semantic_entry_after_seed(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_imported_semantic_entry_after_seed() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    checked_ancestry_kind text;
    checked_imported_conversation_id uuid;
    checked_frontier_position numeric(20, 0);
BEGIN
    IF NEW.payload_kind <> 'imported_entry' THEN
        RETURN NEW;
    END IF;

    SELECT
        ancestry_kind,
        imported_conversation_id,
        imported_frontier_position
      INTO
        checked_ancestry_kind,
        checked_imported_conversation_id,
        checked_frontier_position
      FROM session
     WHERE session_id = NEW.source_session_id;

    IF NOT FOUND OR checked_ancestry_kind <> 'imported_conversation' THEN
        RAISE EXCEPTION
            'imported semantic entry requires imported session ancestry'
            USING
                ERRCODE = '23514',
                CONSTRAINT =
                    'imported_semantic_entry_requires_imported_ancestry';
    END IF;

    -- Each selected-prefix source entry is unique per session. Restricting
    -- every imported semantic row to the selected conversation and inclusive
    -- boundary therefore prevents a same-transaction row from extending an
    -- already validated seed, even after SET CONSTRAINTS ... IMMEDIATE has
    -- discharged the one deferred full-prefix check.
    IF NEW.imported_conversation_id IS DISTINCT FROM
           checked_imported_conversation_id
       OR NOT EXISTS (
        SELECT 1
          FROM imported_transcript_entry AS imported_entry
         WHERE imported_entry.imported_conversation_id =
                   NEW.imported_conversation_id
           AND imported_entry.imported_transcript_entry_id =
                   NEW.imported_transcript_entry_id
           AND imported_entry.imported_entry_position <=
                   checked_frontier_position
    ) THEN
        RAISE EXCEPTION
            'imported semantic entry lies outside the selected prefix'
            USING
                ERRCODE = '23514',
                CONSTRAINT =
                    'imported_semantic_entry_requires_selected_prefix';
    END IF;

    IF EXISTS (
        SELECT 1
          FROM imported_session_seed AS seed
         WHERE seed.session_id = NEW.source_session_id
           AND seed.creation_transaction_id <> pg_current_xact_id()
    ) THEN
        RAISE EXCEPTION
            'imported session % semantic seed prefix is already sealed',
            NEW.source_session_id
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'imported_semantic_entry_seed_is_sealed';
    END IF;

    RETURN NEW;
END;
$$;


--
-- Name: reject_imported_table_truncate(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_imported_table_truncate() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RAISE EXCEPTION '% cannot be truncated', TG_TABLE_NAME
        USING ERRCODE = '23514';
END;
$$;


--
-- Name: reject_web_usage_projection_truncate(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_web_usage_projection_truncate() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RAISE EXCEPTION 'web usage projection cannot be truncated'
        USING ERRCODE = '23514';
END;
$$;


--
-- Name: require_blob_replica(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_blob_replica() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM blob_replica
         WHERE digest = NEW.digest
    ) THEN
        RAISE EXCEPTION
            'blob identity requires at least one verified replica'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_imported_ancestry_for_seed(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_imported_ancestry_for_seed() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    checked_ancestry_kind text;
BEGIN
    SELECT ancestry_kind
      INTO checked_ancestry_kind
      FROM session
     WHERE session_id = NEW.session_id;

    IF NOT FOUND OR checked_ancestry_kind <> 'imported_conversation' THEN
        RAISE EXCEPTION
            'imported seed requires imported session ancestry'
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'imported_session_seed_requires_imported_ancestry';
    END IF;

    RETURN NULL;
END;
$$;


--
-- Name: require_imported_conversation_complete(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_imported_conversation_complete() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM assert_imported_conversation_complete(
        CASE
            WHEN TG_OP = 'DELETE' THEN OLD.imported_conversation_id
            ELSE NEW.imported_conversation_id
        END
    );
    RETURN NULL;
END;
$$;


--
-- Name: require_imported_entry_within_declared_counts(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_imported_entry_within_declared_counts() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    conversation_count numeric(20, 0);
    raw_entry_count numeric(20, 0);
BEGIN
    SELECT declared_entry_count
      INTO conversation_count
      FROM imported_conversation
     WHERE imported_conversation_id = NEW.imported_conversation_id;

    SELECT declared_entry_count
      INTO raw_entry_count
      FROM imported_conversation_raw_record
     WHERE imported_conversation_id = NEW.imported_conversation_id
       AND raw_record_position = NEW.raw_record_position;

    IF conversation_count IS NULL
       OR raw_entry_count IS NULL
       OR NEW.imported_entry_position > conversation_count
       OR NEW.record_entry_position > raw_entry_count
    THEN
        RAISE EXCEPTION
            'imported entry position is outside its declared conversation or raw record'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: require_imported_raw_record_within_declared_count(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_imported_raw_record_within_declared_count() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    declared_count numeric(20, 0);
BEGIN
    SELECT declared_raw_record_count
      INTO declared_count
      FROM imported_conversation
     WHERE imported_conversation_id = NEW.imported_conversation_id;

    IF NOT FOUND OR NEW.raw_record_position > declared_count THEN
        RAISE EXCEPTION
            'imported raw-record position is outside its declared conversation'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: require_imported_raw_source_record_owned(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_imported_raw_source_record_owned() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM imported_conversation_raw_record
         WHERE content_hash = NEW.content_hash
    )
    THEN
        RAISE EXCEPTION
            'imported raw-source record has no conversation occurrence'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_imported_seed_for_session(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_imported_seed_for_session() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM assert_imported_session_seed_complete(
        CASE WHEN TG_OP = 'DELETE' THEN OLD.session_id ELSE NEW.session_id END
    );
    RETURN NULL;
END;
$$;


--
-- Name: require_web_usage_source_correlation(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_web_usage_source_correlation() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    identity_kind text;
    projected_identity_kind text;
    source record;
BEGIN
    SELECT call_kind INTO identity_kind
      FROM model_call_identity
     WHERE model_call_id = NEW.model_call_id;
    projected_identity_kind := identity_kind;
    IF projected_identity_kind = 'ordinary' THEN
        projected_identity_kind := 'model_call';
    END IF;
    IF NEW.call_kind IS DISTINCT FROM projected_identity_kind THEN
        RAISE EXCEPTION
            'usage projection call kind % contradicts identity kind %',
            NEW.call_kind, identity_kind
            USING ERRCODE = '23514';
    END IF;
    IF identity_kind = 'ordinary' THEN
        SELECT session_id, turn_id, resolved_provider_model_identity_id,
               bounded_web_usage_profile(credential_reference)
                   AS credential_profile_label,
               usage_provenance_kind, usage_input_includes_cache_tokens,
               usage_input_tokens AS input_tokens,
               usage_output_tokens AS output_tokens,
               usage_cache_creation_input_tokens
                   AS cache_creation_input_tokens,
               usage_cache_read_input_tokens AS cache_read_input_tokens
          INTO source
          FROM model_call
         WHERE model_call_id = NEW.model_call_id
           AND state_kind = 'terminal';
    ELSIF identity_kind = 'approval_judge' THEN
        SELECT session_id, turn_id, resolved_provider_model_identity_id,
               bounded_web_usage_profile(credential_reference)
                   AS credential_profile_label,
               usage_provenance_kind, usage_input_includes_cache_tokens,
               input_tokens, output_tokens,
               cache_creation_input_tokens, cache_read_input_tokens
          INTO source
          FROM tool_approval_judge_model_call
         WHERE model_call_id = NEW.model_call_id
           AND state_kind = 'terminal';
    ELSE
        SELECT session_id, NULL::uuid AS turn_id,
               resolved_provider_model_identity_id,
               bounded_web_usage_profile(credential_reference)
                   AS credential_profile_label,
               'reported' AS usage_provenance_kind,
               usage_input_includes_cache_tokens,
               input_tokens, output_tokens,
               cache_creation_input_tokens, cache_read_input_tokens
          INTO source
          FROM context_compaction_model_call
         WHERE model_call_id = NEW.model_call_id
           AND state_kind = 'terminal';
    END IF;
    IF NOT FOUND THEN
        RAISE EXCEPTION
            'usage projection call % has no terminal source record',
            NEW.model_call_id
            USING ERRCODE = '23514';
    END IF;
    IF NEW.session_id IS DISTINCT FROM source.session_id THEN
        RAISE EXCEPTION
            'usage projection session % contradicts source session %',
            NEW.session_id, source.session_id
            USING ERRCODE = '23514';
    END IF;
    IF NEW.turn_id IS DISTINCT FROM source.turn_id THEN
        RAISE EXCEPTION
            'usage projection turn % contradicts source turn %',
            NEW.turn_id, source.turn_id
            USING ERRCODE = '23514';
    END IF;
    IF NEW.resolved_provider_model_identity_id
           IS DISTINCT FROM source.resolved_provider_model_identity_id
       OR NEW.credential_profile_label
           IS DISTINCT FROM source.credential_profile_label
       OR NEW.usage_provenance_kind
           IS DISTINCT FROM source.usage_provenance_kind
       OR NEW.usage_input_includes_cache_tokens
           IS DISTINCT FROM source.usage_input_includes_cache_tokens
       OR NEW.input_tokens IS DISTINCT FROM source.input_tokens
       OR NEW.output_tokens IS DISTINCT FROM source.output_tokens
       OR NEW.cache_creation_input_tokens
           IS DISTINCT FROM source.cache_creation_input_tokens
       OR NEW.cache_read_input_tokens
           IS DISTINCT FROM source.cache_read_input_tokens
    THEN
        RAISE EXCEPTION
            'usage projection evidence for call % contradicts its terminal source record',
            NEW.model_call_id
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: stamp_imported_session_seed_transaction(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION stamp_imported_session_seed_transaction() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    -- pg_current_xact_id() deliberately returns the top-level transaction ID
    -- even when this INSERT runs inside a savepoint. Persisting it avoids
    -- confusing the tuple's subtransaction xmin with a previously committed
    -- seed while the rest of the prefix is assembled.
    NEW.creation_transaction_id := pg_current_xact_id();
    RETURN NEW;
END;
$$;


--
-- Name: web_search_projection_chunks(text); Type: FUNCTION; Schema: public
--

CREATE FUNCTION web_search_projection_chunks(source_text text) RETURNS TABLE(ordinal integer, content_text text)
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    AS $$
    SELECT chunk.ordinal,
           substring(source_text FROM chunk.ordinal * 15872 + 1 FOR 16384)
      FROM generate_series(
               0, (char_length(source_text) - 1) / 15872
           ) AS chunk(ordinal)
$$;


--
-- Name: web_search_projection_requires_session_address(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION web_search_projection_requires_session_address() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Tables.
--

--
-- Name: blob; Type: TABLE; Schema: public
--

CREATE TABLE blob (
    digest bytea NOT NULL,
    byte_length numeric(20,0) NOT NULL,
    created_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    CONSTRAINT blob_byte_length_positive_u64 CHECK (((byte_length >= (1)::numeric) AND (byte_length <= '18446744073709551615'::numeric))),
    CONSTRAINT blob_digest_size CHECK ((octet_length(digest) = 32))
);


--
-- Name: blob_derivation; Type: TABLE; Schema: public
--

CREATE TABLE blob_derivation (
    derivation_id uuid NOT NULL,
    deterministic_key bytea,
    transformation_name text NOT NULL COLLATE pg_catalog."C",
    transformation_version bigint NOT NULL,
    parameters_canonical text NOT NULL COLLATE pg_catalog."C",
    producer_class text NOT NULL COLLATE pg_catalog."C",
    implementation_digest bytea,
    execution_id uuid,
    model_call_id uuid,
    input_count smallint NOT NULL,
    output_count smallint NOT NULL,
    created_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    CONSTRAINT blob_derivation_counts CHECK (((input_count BETWEEN 1 AND 16) AND (output_count BETWEEN 1 AND 16))),
    CONSTRAINT blob_derivation_key_shape CHECK (((deterministic_key IS NULL) OR (octet_length(deterministic_key) = 32))),
    CONSTRAINT blob_derivation_name_shape CHECK (((octet_length(transformation_name) BETWEEN 1 AND 64) AND (transformation_name ~ '^[a-z][a-z0-9_.-]*$'::text))),
    CONSTRAINT blob_derivation_parameter_bound CHECK ((octet_length(parameters_canonical) <= 4096)),
    CONSTRAINT blob_derivation_producer_shape CHECK ((((producer_class = 'deterministic'::text) AND (deterministic_key IS NOT NULL) AND (implementation_digest IS NOT NULL) AND (octet_length(implementation_digest) = 32) AND (execution_id IS NULL) AND (model_call_id IS NULL)) OR ((producer_class = 'executed'::text) AND (deterministic_key IS NULL) AND (implementation_digest IS NOT NULL) AND (octet_length(implementation_digest) = 32) AND (execution_id IS NOT NULL) AND (model_call_id IS NULL)) OR ((producer_class = 'model_derived'::text) AND (deterministic_key IS NULL) AND (implementation_digest IS NULL) AND (execution_id IS NULL) AND (model_call_id IS NOT NULL)))),
    CONSTRAINT blob_derivation_version_shape CHECK (((transformation_version >= 1) AND (transformation_version <= '4294967295'::bigint)))
);


--
-- Name: blob_derivation_input; Type: TABLE; Schema: public
--

CREATE TABLE blob_derivation_input (
    derivation_id uuid NOT NULL,
    input_ordinal smallint NOT NULL,
    digest bytea NOT NULL,
    CONSTRAINT blob_derivation_input_digest CHECK ((octet_length(digest) = 32)),
    CONSTRAINT blob_derivation_input_ordinal CHECK (((input_ordinal >= 0) AND (input_ordinal <= 15)))
);


--
-- Name: blob_derivation_output; Type: TABLE; Schema: public
--

CREATE TABLE blob_derivation_output (
    derivation_id uuid NOT NULL,
    output_ordinal smallint NOT NULL,
    digest bytea NOT NULL,
    CONSTRAINT blob_derivation_output_digest CHECK ((octet_length(digest) = 32)),
    CONSTRAINT blob_derivation_output_ordinal CHECK (((output_ordinal >= 0) AND (output_ordinal <= 15)))
);


--
-- Name: blob_read_tool_charge; Type: TABLE; Schema: public
--

CREATE TABLE blob_read_tool_charge (
    request_id uuid NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    blob_digest bytea NOT NULL,
    decoded_byte_count numeric(20,0) NOT NULL,
    admitted boolean NOT NULL,
    rejection_reason text,
    CONSTRAINT blob_read_tool_charge_bytes_positive_u64 CHECK (((decoded_byte_count >= (1)::numeric) AND (decoded_byte_count <= '18446744073709551615'::numeric))),
    CONSTRAINT blob_read_tool_charge_rejection_shape CHECK (((admitted AND (rejection_reason IS NULL)) OR ((NOT admitted) AND (rejection_reason = ANY (ARRAY['blob_turn_byte_budget_exceeded'::text, 'blob_turn_read_count_exceeded'::text])))))
);


--
-- Name: blob_replica; Type: TABLE; Schema: public
--

CREATE TABLE blob_replica (
    digest bytea NOT NULL,
    store_name text NOT NULL COLLATE pg_catalog."C",
    object_key text NOT NULL COLLATE pg_catalog."C",
    verified_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    CONSTRAINT blob_replica_digest_size CHECK ((octet_length(digest) = 32)),
    CONSTRAINT blob_replica_object_key_bounded CHECK (((octet_length(object_key) >= 1) AND (octet_length(object_key) <= 1024))),
    CONSTRAINT blob_replica_store_name_bounded CHECK (((octet_length(store_name) BETWEEN 1 AND 64) AND (store_name ~ '^[a-z][a-z0-9_-]{0,63}$'::text)))
);


--
-- Name: blob_store_binding; Type: TABLE; Schema: public
--

CREATE TABLE blob_store_binding (
    store_name text NOT NULL COLLATE pg_catalog."C",
    namespace_id uuid NOT NULL,
    created_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    CONSTRAINT blob_store_binding_name_canonical CHECK (((octet_length(store_name) BETWEEN 1 AND 64) AND (store_name ~ '^[a-z][a-z0-9_-]{0,63}$'::text)))
);


--
-- Name: imported_conversation; Type: TABLE; Schema: public
--

CREATE TABLE imported_conversation (
    imported_conversation_id uuid NOT NULL,
    storage_version smallint NOT NULL,
    source_format text NOT NULL,
    converter_version smallint NOT NULL,
    source_digest bytea NOT NULL,
    declared_raw_record_count numeric(20,0) NOT NULL,
    declared_entry_count numeric(20,0) NOT NULL,
    source_session_id bytea,
    display_title text,
    display_title_state text NOT NULL,
    CONSTRAINT imported_conversation_converter_version_supported CHECK ((converter_version = ANY (ARRAY[1, 2]))),
    CONSTRAINT imported_conversation_display_title_presence CHECK (((display_title_state = 'derived'::text) = (display_title IS NOT NULL))),
    CONSTRAINT imported_conversation_display_title_shape CHECK (((display_title IS NULL) OR ((char_length(display_title) BETWEEN 1 AND 256) AND (POSITION((chr(10)) IN (display_title)) = 0) AND (POSITION((chr(13)) IN (display_title)) = 0) AND (btrim(display_title, ' 	'::text) = display_title)))),
    CONSTRAINT imported_conversation_display_title_state_closed CHECK ((display_title_state = ANY (ARRAY['derived'::text, 'underivable'::text]))),
    CONSTRAINT imported_conversation_entry_count_positive_u64 CHECK (((declared_entry_count >= (1)::numeric) AND (declared_entry_count <= '18446744073709551615'::numeric))),
    CONSTRAINT imported_conversation_format_version_supported CHECK ((((source_format = 'claude_code_session_jsonl'::text) AND (converter_version = ANY (ARRAY[1, 2]))) OR ((source_format = 'codex_rollout_jsonl'::text) AND (converter_version = 1)))),
    CONSTRAINT imported_conversation_raw_record_count_positive_u64 CHECK (((declared_raw_record_count >= (1)::numeric) AND (declared_raw_record_count <= '18446744073709551615'::numeric))),
    CONSTRAINT imported_conversation_source_digest_size CHECK ((octet_length(source_digest) = 32)),
    CONSTRAINT imported_conversation_source_format_closed CHECK ((source_format = ANY (ARRAY['claude_code_session_jsonl'::text, 'codex_rollout_jsonl'::text]))),
    CONSTRAINT imported_conversation_storage_version_supported CHECK ((storage_version = 1))
);


--
-- Name: imported_conversation_raw_record; Type: TABLE; Schema: public
--

CREATE TABLE imported_conversation_raw_record (
    imported_conversation_id uuid CONSTRAINT imported_conversation_raw_rec_imported_conversation_id_not_null NOT NULL,
    raw_record_position numeric(20,0) NOT NULL,
    content_hash bytea NOT NULL,
    conversion_digest bytea NOT NULL,
    normalized_value_encoding bytea CONSTRAINT imported_conversation_raw_re_normalized_value_encoding_not_null NOT NULL,
    declared_entry_count numeric(20,0) NOT NULL,
    CONSTRAINT imported_conversation_raw_record_conversion_digest_size CHECK ((octet_length(conversion_digest) = 32)),
    CONSTRAINT imported_conversation_raw_record_encoding_nonempty CHECK ((octet_length(normalized_value_encoding) >= 1)),
    CONSTRAINT imported_conversation_raw_record_entry_count_positive_u64 CHECK (((declared_entry_count >= (1)::numeric) AND (declared_entry_count <= '18446744073709551615'::numeric))),
    CONSTRAINT imported_conversation_raw_record_hash_size CHECK ((octet_length(content_hash) = 32)),
    CONSTRAINT imported_conversation_raw_record_position_positive_u64 CHECK (((raw_record_position >= (1)::numeric) AND (raw_record_position <= '18446744073709551615'::numeric)))
);


--
-- Name: imported_conversation_size_totals; Type: TABLE; Schema: public
--

CREATE TABLE imported_conversation_size_totals (
    imported_conversation_id uuid CONSTRAINT imported_conversation_size_to_imported_conversation_id_not_null NOT NULL,
    raw_source_bytes numeric(20,0) NOT NULL,
    normalized_source_record_bytes numeric(20,0) CONSTRAINT imported_conversation_size__normalized_source_record_b_not_null NOT NULL,
    normalized_entry_bytes numeric(20,0) CONSTRAINT imported_conversation_size_tota_normalized_entry_bytes_not_null NOT NULL,
    CONSTRAINT imported_conversation_size_totals_nonnegative_u64 CHECK (((raw_source_bytes BETWEEN (0)::numeric AND '18446744073709551615'::numeric) AND (normalized_source_record_bytes BETWEEN (0)::numeric AND '18446744073709551615'::numeric) AND (normalized_entry_bytes BETWEEN (0)::numeric AND '18446744073709551615'::numeric)))
);


--
-- Name: imported_raw_source_record; Type: TABLE; Schema: public
--

CREATE TABLE imported_raw_source_record (
    content_hash bytea NOT NULL,
    CONSTRAINT imported_raw_source_record_hash_size CHECK ((octet_length(content_hash) = 32))
);


--
-- Name: imported_session_seed; Type: TABLE; Schema: public
--

CREATE TABLE imported_session_seed (
    session_id uuid NOT NULL,
    seed_context_frontier_id uuid NOT NULL,
    creation_transaction_id xid8 NOT NULL
);


--
-- Name: imported_transcript_entry; Type: TABLE; Schema: public
--

CREATE TABLE imported_transcript_entry (
    imported_conversation_id uuid NOT NULL,
    imported_entry_position numeric(20,0) NOT NULL,
    imported_transcript_entry_id uuid NOT NULL,
    raw_record_position numeric(20,0) NOT NULL,
    record_entry_position numeric(20,0) NOT NULL,
    source_speaker_kind text NOT NULL,
    content_encoding bytea NOT NULL,
    source_metadata_encoding bytea NOT NULL,
    content_kind smallint GENERATED ALWAYS AS (imported_content_encoding_kind(content_encoding)) STORED NOT NULL,
    CONSTRAINT imported_transcript_entry_content_encoding_nonempty CHECK ((octet_length(content_encoding) >= 1)),
    CONSTRAINT imported_transcript_entry_position_positive_u64 CHECK (((imported_entry_position >= (1)::numeric) AND (imported_entry_position <= '18446744073709551615'::numeric))),
    CONSTRAINT imported_transcript_entry_raw_position_positive_u64 CHECK (((raw_record_position >= (1)::numeric) AND (raw_record_position <= '18446744073709551615'::numeric))),
    CONSTRAINT imported_transcript_entry_record_position_positive_u64 CHECK (((record_entry_position >= (1)::numeric) AND (record_entry_position <= '18446744073709551615'::numeric))),
    CONSTRAINT imported_transcript_entry_source_encoding_nonempty CHECK ((octet_length(source_metadata_encoding) >= 1)),
    CONSTRAINT imported_transcript_entry_source_speaker_closed CHECK ((source_speaker_kind = ANY (ARRAY['not_attested'::text, 'attested_absent'::text, 'attested_user'::text, 'attested_assistant'::text])))
);


--
-- Name: web_search_projection; Type: TABLE; Schema: public
--

CREATE TABLE web_search_projection (
    projection_id bigint NOT NULL,
    item_kind text NOT NULL COLLATE pg_catalog."C",
    item_id uuid NOT NULL,
    session_id uuid NOT NULL,
    event_sequence numeric(20,0) NOT NULL,
    source_kind text NOT NULL COLLATE pg_catalog."C",
    source_id uuid NOT NULL,
    turn_id uuid,
    content_class text NOT NULL COLLATE pg_catalog."C",
    projection_ordinal integer DEFAULT 0 NOT NULL,
    content_text text NOT NULL,
    search_vector tsvector GENERATED ALWAYS AS (to_tsvector('simple'::regconfig, content_text)) STORED,
    CONSTRAINT web_search_projection_content_class_closed CHECK ((content_class = ANY (ARRAY['user_transcript'::text, 'assistant_transcript'::text, 'tool_arguments'::text, 'tool_result'::text, 'session_metadata'::text, 'attachment_filename'::text, 'attachment_media_metadata'::text, 'derived_text_artifact'::text]))),
    CONSTRAINT web_search_projection_event_positive CHECK (((event_sequence >= (1)::numeric) AND (event_sequence <= '18446744073709551615'::numeric))),
    CONSTRAINT web_search_projection_item_closed CHECK ((item_kind = ANY (ARRAY['session'::text, 'accepted_input'::text, 'transcript_entry'::text, 'tool_request'::text, 'tool_attempt'::text, 'attachment'::text, 'derived_artifact'::text]))),
    CONSTRAINT web_search_projection_item_shape CHECK ((((item_kind = 'session'::text) AND (item_id = session_id) AND (turn_id IS NULL)) OR ((item_kind = ANY (ARRAY['accepted_input'::text, 'tool_request'::text, 'tool_attempt'::text])) AND (turn_id IS NOT NULL)) OR (item_kind = ANY (ARRAY['transcript_entry'::text, 'attachment'::text, 'derived_artifact'::text])))),
    CONSTRAINT web_search_projection_ordinal_bound CHECK ((projection_ordinal >= 0)),
    CONSTRAINT web_search_projection_source_closed CHECK ((source_kind = ANY (ARRAY['accepted_input'::text, 'steering_input'::text, 'semantic_entry'::text, 'tool_request'::text, 'tool_attempt'::text, 'session_metadata'::text, 'attachment'::text, 'derived_artifact'::text]))),
    CONSTRAINT web_search_projection_text_bound CHECK (((octet_length(content_text) >= 1) AND (octet_length(content_text) <= 65536)))
);


--
-- Name: web_search_projection_projection_id_seq; Type: SEQUENCE; Schema: public
--

ALTER TABLE web_search_projection ALTER COLUMN projection_id ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME web_search_projection_projection_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: web_usage_call_projection; Type: TABLE; Schema: public
--

CREATE TABLE web_usage_call_projection (
    model_call_id uuid NOT NULL,
    call_kind text NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid,
    resolved_provider_model_identity_id uuid CONSTRAINT web_usage_call_projection_resolved_provider_model_iden_not_null NOT NULL,
    credential_profile_label text NOT NULL,
    usage_provenance_kind text NOT NULL,
    usage_input_includes_cache_tokens boolean,
    input_tokens numeric,
    output_tokens numeric,
    cache_creation_input_tokens numeric,
    cache_read_input_tokens numeric,
    recorded_at timestamp with time zone DEFAULT statement_timestamp() NOT NULL,
    CONSTRAINT web_usage_call_kind_closed CHECK ((call_kind = ANY (ARRAY['model_call'::text, 'approval_judge'::text, 'context_compaction'::text]))),
    CONSTRAINT web_usage_credential_profile_label_bounded CHECK (((char_length(credential_profile_label) > 0) AND (octet_length(credential_profile_label) <= 256) AND ((("left"(credential_profile_label, 6) = 'exact:'::text) AND (octet_length(credential_profile_label) > 6)) OR (("left"(credential_profile_label, 7) = 'mapped:'::text) AND (octet_length(credential_profile_label) > 7))))),
    CONSTRAINT web_usage_provenance_closed CHECK ((usage_provenance_kind = ANY (ARRAY['reported'::text, 'estimated'::text]))),
    CONSTRAINT web_usage_recorded_at_representable CHECK (((recorded_at >= '1970-01-01 00:00:00+00'::timestamp with time zone) AND (recorded_at <= '9999-12-31 23:59:59.999999+00'::timestamp with time zone))),
    CONSTRAINT web_usage_token_axes_u64 CHECK ((((input_tokens IS NULL) OR ((input_tokens = trunc(input_tokens)) AND ((input_tokens >= (0)::numeric) AND (input_tokens <= '18446744073709551615'::numeric)))) AND ((output_tokens IS NULL) OR ((output_tokens = trunc(output_tokens)) AND ((output_tokens >= (0)::numeric) AND (output_tokens <= '18446744073709551615'::numeric)))) AND ((cache_creation_input_tokens IS NULL) OR ((cache_creation_input_tokens = trunc(cache_creation_input_tokens)) AND ((cache_creation_input_tokens >= (0)::numeric) AND (cache_creation_input_tokens <= '18446744073709551615'::numeric)))) AND ((cache_read_input_tokens IS NULL) OR ((cache_read_input_tokens = trunc(cache_read_input_tokens)) AND ((cache_read_input_tokens >= (0)::numeric) AND (cache_read_input_tokens <= '18446744073709551615'::numeric)))))),
    CONSTRAINT web_usage_turn_shape CHECK (((call_kind = 'context_compaction'::text) = (turn_id IS NULL)))
);


--
-- Name: web_usage_oversized_profile_identity; Type: TABLE; Schema: public
--

CREATE TABLE web_usage_oversized_profile_identity (
    profile_id bigint NOT NULL,
    reference_digest text NOT NULL,
    exact_reference text NOT NULL,
    CONSTRAINT web_usage_oversized_profile_digest_shape CHECK ((reference_digest ~ '^[0-9a-f]{32}$'::text)),
    CONSTRAINT web_usage_oversized_profile_reference CHECK ((octet_length(exact_reference) > 250))
);


--
-- Name: web_usage_oversized_profile_identity_profile_id_seq; Type: SEQUENCE; Schema: public
--

ALTER TABLE web_usage_oversized_profile_identity ALTER COLUMN profile_id ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME web_usage_oversized_profile_identity_profile_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Constraints.
--

--
-- Name: blob_derivation blob_derivation_deterministic_key_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY blob_derivation
    ADD CONSTRAINT blob_derivation_deterministic_key_key UNIQUE (deterministic_key);


--
-- Name: blob_derivation_input blob_derivation_input_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY blob_derivation_input
    ADD CONSTRAINT blob_derivation_input_pk PRIMARY KEY (derivation_id, input_ordinal);


--
-- Name: blob_derivation_output blob_derivation_output_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY blob_derivation_output
    ADD CONSTRAINT blob_derivation_output_pk PRIMARY KEY (derivation_id, output_ordinal);


--
-- Name: blob_derivation blob_derivation_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY blob_derivation
    ADD CONSTRAINT blob_derivation_pkey PRIMARY KEY (derivation_id);


--
-- Name: blob blob_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY blob
    ADD CONSTRAINT blob_pkey PRIMARY KEY (digest);


--
-- Name: blob_read_tool_charge blob_read_tool_charge_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY blob_read_tool_charge
    ADD CONSTRAINT blob_read_tool_charge_pkey PRIMARY KEY (request_id);


--
-- Name: blob_replica blob_replica_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY blob_replica
    ADD CONSTRAINT blob_replica_pk PRIMARY KEY (digest, store_name);


--
-- Name: blob_replica blob_replica_store_object_unique; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY blob_replica
    ADD CONSTRAINT blob_replica_store_object_unique UNIQUE (store_name, object_key);


--
-- Name: blob_store_binding blob_store_binding_namespace_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY blob_store_binding
    ADD CONSTRAINT blob_store_binding_namespace_id_key UNIQUE (namespace_id);


--
-- Name: blob_store_binding blob_store_binding_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY blob_store_binding
    ADD CONSTRAINT blob_store_binding_pkey PRIMARY KEY (store_name);


--
-- Name: imported_conversation imported_conversation_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY imported_conversation
    ADD CONSTRAINT imported_conversation_pkey PRIMARY KEY (imported_conversation_id);


--
-- Name: imported_conversation_raw_record imported_conversation_raw_record_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY imported_conversation_raw_record
    ADD CONSTRAINT imported_conversation_raw_record_pk PRIMARY KEY (imported_conversation_id, raw_record_position);


--
-- Name: imported_conversation_size_totals imported_conversation_size_totals_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY imported_conversation_size_totals
    ADD CONSTRAINT imported_conversation_size_totals_pkey PRIMARY KEY (imported_conversation_id);


--
-- Name: imported_conversation imported_conversation_source_identity; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY imported_conversation
    ADD CONSTRAINT imported_conversation_source_identity UNIQUE (source_format, converter_version, source_digest);


--
-- Name: imported_raw_source_record imported_raw_source_record_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY imported_raw_source_record
    ADD CONSTRAINT imported_raw_source_record_pkey PRIMARY KEY (content_hash);


--
-- Name: imported_session_seed imported_session_seed_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY imported_session_seed
    ADD CONSTRAINT imported_session_seed_pkey PRIMARY KEY (session_id);


--
-- Name: imported_session_seed imported_session_seed_seed_context_frontier_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY imported_session_seed
    ADD CONSTRAINT imported_session_seed_seed_context_frontier_id_key UNIQUE (seed_context_frontier_id);


--
-- Name: imported_transcript_entry imported_transcript_entry_frontier_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY imported_transcript_entry
    ADD CONSTRAINT imported_transcript_entry_frontier_key UNIQUE (imported_conversation_id, imported_transcript_entry_id, imported_entry_position);


--
-- Name: imported_transcript_entry imported_transcript_entry_identity_unique; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY imported_transcript_entry
    ADD CONSTRAINT imported_transcript_entry_identity_unique UNIQUE (imported_transcript_entry_id);


--
-- Name: imported_transcript_entry imported_transcript_entry_owner_identity_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY imported_transcript_entry
    ADD CONSTRAINT imported_transcript_entry_owner_identity_key UNIQUE (imported_conversation_id, imported_transcript_entry_id);


--
-- Name: imported_transcript_entry imported_transcript_entry_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY imported_transcript_entry
    ADD CONSTRAINT imported_transcript_entry_pk PRIMARY KEY (imported_conversation_id, imported_entry_position);


--
-- Name: imported_transcript_entry imported_transcript_entry_within_record_unique; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY imported_transcript_entry
    ADD CONSTRAINT imported_transcript_entry_within_record_unique UNIQUE (imported_conversation_id, raw_record_position, record_entry_position);


--
-- Name: web_search_projection web_search_projection_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY web_search_projection
    ADD CONSTRAINT web_search_projection_pkey PRIMARY KEY (projection_id);


--
-- Name: web_search_projection web_search_projection_source_once; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY web_search_projection
    ADD CONSTRAINT web_search_projection_source_once UNIQUE (source_kind, source_id, content_class, projection_ordinal);


--
-- Name: web_usage_call_projection web_usage_call_projection_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY web_usage_call_projection
    ADD CONSTRAINT web_usage_call_projection_pkey PRIMARY KEY (model_call_id);


--
-- Name: web_usage_oversized_profile_identity web_usage_oversized_profile_identity_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY web_usage_oversized_profile_identity
    ADD CONSTRAINT web_usage_oversized_profile_identity_pkey PRIMARY KEY (profile_id);


--
-- Indexes.
--

--
-- Name: blob_read_tool_charge_admitted_turn_idx; Type: INDEX; Schema: public
--

CREATE INDEX blob_read_tool_charge_admitted_turn_idx ON blob_read_tool_charge USING btree (turn_id) WHERE admitted;


--
-- Name: imported_conversation_format_catalog_idx; Type: INDEX; Schema: public
--

CREATE INDEX imported_conversation_format_catalog_idx ON imported_conversation USING btree (source_format, converter_version, imported_conversation_id);


--
-- Name: imported_conversation_raw_record_content_hash_idx; Type: INDEX; Schema: public
--

CREATE INDEX imported_conversation_raw_record_content_hash_idx ON imported_conversation_raw_record USING btree (content_hash);


--
-- Name: imported_conversation_source_session_catalog_idx; Type: INDEX; Schema: public
--

CREATE INDEX imported_conversation_source_session_catalog_idx ON imported_conversation USING btree (sha256(source_session_id), imported_conversation_id) WHERE (source_session_id IS NOT NULL);


--
-- Name: imported_conversation_source_session_id_idx; Type: INDEX; Schema: public
--

CREATE INDEX imported_conversation_source_session_id_idx ON imported_conversation USING hash (source_session_id) WHERE (source_session_id IS NOT NULL);


--
-- Name: web_search_projection_global_page; Type: INDEX; Schema: public
--

CREATE INDEX web_search_projection_global_page ON web_search_projection USING btree (event_sequence DESC, projection_id DESC);


--
-- Name: web_search_projection_session_page; Type: INDEX; Schema: public
--

CREATE INDEX web_search_projection_session_page ON web_search_projection USING btree (session_id, event_sequence DESC, projection_id DESC);


--
-- Name: web_search_projection_vector; Type: INDEX; Schema: public
--

CREATE INDEX web_search_projection_vector ON web_search_projection USING gin (search_vector);


--
-- Name: web_usage_by_kind_recorded_call; Type: INDEX; Schema: public
--

CREATE INDEX web_usage_by_kind_recorded_call ON web_usage_call_projection USING btree (call_kind, recorded_at DESC, model_call_id DESC);


--
-- Name: web_usage_by_model_kind_recorded_call; Type: INDEX; Schema: public
--

CREATE INDEX web_usage_by_model_kind_recorded_call ON web_usage_call_projection USING btree (resolved_provider_model_identity_id, call_kind, recorded_at DESC, model_call_id DESC);


--
-- Name: web_usage_by_model_provenance_kind_recorded_call; Type: INDEX; Schema: public
--

CREATE INDEX web_usage_by_model_provenance_kind_recorded_call ON web_usage_call_projection USING btree (resolved_provider_model_identity_id, usage_provenance_kind, call_kind, recorded_at DESC, model_call_id DESC);


--
-- Name: web_usage_by_model_provenance_recorded_call; Type: INDEX; Schema: public
--

CREATE INDEX web_usage_by_model_provenance_recorded_call ON web_usage_call_projection USING btree (resolved_provider_model_identity_id, usage_provenance_kind, recorded_at DESC, model_call_id DESC);


--
-- Name: web_usage_by_model_recorded_call; Type: INDEX; Schema: public
--

CREATE INDEX web_usage_by_model_recorded_call ON web_usage_call_projection USING btree (resolved_provider_model_identity_id, recorded_at DESC, model_call_id DESC);


--
-- Name: web_usage_by_provenance_kind_recorded_call; Type: INDEX; Schema: public
--

CREATE INDEX web_usage_by_provenance_kind_recorded_call ON web_usage_call_projection USING btree (usage_provenance_kind, call_kind, recorded_at DESC, model_call_id DESC);


--
-- Name: web_usage_by_provenance_recorded_call; Type: INDEX; Schema: public
--

CREATE INDEX web_usage_by_provenance_recorded_call ON web_usage_call_projection USING btree (usage_provenance_kind, recorded_at DESC, model_call_id DESC);


--
-- Name: web_usage_by_recorded_call; Type: INDEX; Schema: public
--

CREATE INDEX web_usage_by_recorded_call ON web_usage_call_projection USING btree (recorded_at DESC, model_call_id DESC);


--
-- Name: web_usage_by_session_kind_recorded_call; Type: INDEX; Schema: public
--

CREATE INDEX web_usage_by_session_kind_recorded_call ON web_usage_call_projection USING btree (session_id, call_kind, recorded_at DESC, model_call_id DESC);


--
-- Name: web_usage_by_session_model_kind_recorded_call; Type: INDEX; Schema: public
--

CREATE INDEX web_usage_by_session_model_kind_recorded_call ON web_usage_call_projection USING btree (session_id, resolved_provider_model_identity_id, call_kind, recorded_at DESC, model_call_id DESC);


--
-- Name: web_usage_by_session_model_provenance_kind_recorded_call; Type: INDEX; Schema: public
--

CREATE INDEX web_usage_by_session_model_provenance_kind_recorded_call ON web_usage_call_projection USING btree (session_id, resolved_provider_model_identity_id, usage_provenance_kind, call_kind, recorded_at DESC, model_call_id DESC);


--
-- Name: web_usage_by_session_model_provenance_recorded_call; Type: INDEX; Schema: public
--

CREATE INDEX web_usage_by_session_model_provenance_recorded_call ON web_usage_call_projection USING btree (session_id, resolved_provider_model_identity_id, usage_provenance_kind, recorded_at DESC, model_call_id DESC);


--
-- Name: web_usage_by_session_model_recorded_call; Type: INDEX; Schema: public
--

CREATE INDEX web_usage_by_session_model_recorded_call ON web_usage_call_projection USING btree (session_id, resolved_provider_model_identity_id, recorded_at DESC, model_call_id DESC);


--
-- Name: web_usage_by_session_provenance_kind_recorded_call; Type: INDEX; Schema: public
--

CREATE INDEX web_usage_by_session_provenance_kind_recorded_call ON web_usage_call_projection USING btree (session_id, usage_provenance_kind, call_kind, recorded_at DESC, model_call_id DESC);


--
-- Name: web_usage_by_session_provenance_recorded_call; Type: INDEX; Schema: public
--

CREATE INDEX web_usage_by_session_provenance_recorded_call ON web_usage_call_projection USING btree (session_id, usage_provenance_kind, recorded_at DESC, model_call_id DESC);


--
-- Name: web_usage_by_session_recorded_call; Type: INDEX; Schema: public
--

CREATE INDEX web_usage_by_session_recorded_call ON web_usage_call_projection USING btree (session_id, recorded_at DESC, model_call_id DESC);


--
-- Name: web_usage_by_turn_kind_recorded_call; Type: INDEX; Schema: public
--

CREATE INDEX web_usage_by_turn_kind_recorded_call ON web_usage_call_projection USING btree (turn_id, call_kind, recorded_at DESC, model_call_id DESC);


--
-- Name: web_usage_by_turn_model_kind_recorded_call; Type: INDEX; Schema: public
--

CREATE INDEX web_usage_by_turn_model_kind_recorded_call ON web_usage_call_projection USING btree (turn_id, resolved_provider_model_identity_id, call_kind, recorded_at DESC, model_call_id DESC);


--
-- Name: web_usage_by_turn_model_provenance_kind_recorded_call; Type: INDEX; Schema: public
--

CREATE INDEX web_usage_by_turn_model_provenance_kind_recorded_call ON web_usage_call_projection USING btree (turn_id, resolved_provider_model_identity_id, usage_provenance_kind, call_kind, recorded_at DESC, model_call_id DESC);


--
-- Name: web_usage_by_turn_model_provenance_recorded_call; Type: INDEX; Schema: public
--

CREATE INDEX web_usage_by_turn_model_provenance_recorded_call ON web_usage_call_projection USING btree (turn_id, resolved_provider_model_identity_id, usage_provenance_kind, recorded_at DESC, model_call_id DESC);


--
-- Name: web_usage_by_turn_model_recorded_call; Type: INDEX; Schema: public
--

CREATE INDEX web_usage_by_turn_model_recorded_call ON web_usage_call_projection USING btree (turn_id, resolved_provider_model_identity_id, recorded_at DESC, model_call_id DESC);


--
-- Name: web_usage_by_turn_provenance_kind_recorded_call; Type: INDEX; Schema: public
--

CREATE INDEX web_usage_by_turn_provenance_kind_recorded_call ON web_usage_call_projection USING btree (turn_id, usage_provenance_kind, call_kind, recorded_at DESC, model_call_id DESC);


--
-- Name: web_usage_by_turn_provenance_recorded_call; Type: INDEX; Schema: public
--

CREATE INDEX web_usage_by_turn_provenance_recorded_call ON web_usage_call_projection USING btree (turn_id, usage_provenance_kind, recorded_at DESC, model_call_id DESC);


--
-- Name: web_usage_by_turn_recorded_call; Type: INDEX; Schema: public
--

CREATE INDEX web_usage_by_turn_recorded_call ON web_usage_call_projection USING btree (turn_id, recorded_at DESC, model_call_id DESC);


--
-- Name: web_usage_oversized_profile_by_digest; Type: INDEX; Schema: public
--

CREATE INDEX web_usage_oversized_profile_by_digest ON web_usage_oversized_profile_identity USING btree (reference_digest);


--
-- Triggers.
--

--
-- Name: blob blob_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER blob_cannot_be_truncated BEFORE TRUNCATE ON blob FOR EACH STATEMENT EXECUTE FUNCTION reject_blob_catalog_truncate();


--
-- Name: blob_derivation blob_derivation_immutable; Type: TRIGGER; Schema: public
--

CREATE TRIGGER blob_derivation_immutable BEFORE DELETE OR UPDATE ON blob_derivation FOR EACH ROW EXECUTE FUNCTION reject_blob_derivation_mutation();


--
-- Name: blob_derivation_input blob_derivation_input_complete; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER blob_derivation_input_complete AFTER INSERT ON blob_derivation_input DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION check_blob_derivation_complete();


--
-- Name: blob_derivation_input blob_derivation_input_immutable; Type: TRIGGER; Schema: public
--

CREATE TRIGGER blob_derivation_input_immutable BEFORE DELETE OR UPDATE ON blob_derivation_input FOR EACH ROW EXECUTE FUNCTION reject_blob_derivation_mutation();


--
-- Name: blob_derivation_input blob_derivation_input_no_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER blob_derivation_input_no_truncate BEFORE TRUNCATE ON blob_derivation_input FOR EACH STATEMENT EXECUTE FUNCTION reject_blob_derivation_mutation();


--
-- Name: blob_derivation blob_derivation_no_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER blob_derivation_no_truncate BEFORE TRUNCATE ON blob_derivation FOR EACH STATEMENT EXECUTE FUNCTION reject_blob_derivation_mutation();


--
-- Name: blob_derivation_output blob_derivation_output_complete; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER blob_derivation_output_complete AFTER INSERT ON blob_derivation_output DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION check_blob_derivation_complete();


--
-- Name: blob_derivation_output blob_derivation_output_immutable; Type: TRIGGER; Schema: public
--

CREATE TRIGGER blob_derivation_output_immutable BEFORE DELETE OR UPDATE ON blob_derivation_output FOR EACH ROW EXECUTE FUNCTION reject_blob_derivation_mutation();


--
-- Name: blob_derivation_output blob_derivation_output_no_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER blob_derivation_output_no_truncate BEFORE TRUNCATE ON blob_derivation_output FOR EACH STATEMENT EXECUTE FUNCTION reject_blob_derivation_mutation();


--
-- Name: blob_derivation blob_derivation_root_complete; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER blob_derivation_root_complete AFTER INSERT ON blob_derivation DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION check_blob_derivation_complete();


--
-- Name: blob blob_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER blob_is_append_only BEFORE DELETE OR UPDATE ON blob FOR EACH ROW EXECUTE FUNCTION reject_blob_catalog_row_mutation();


--
-- Name: blob_read_tool_charge blob_read_tool_charge_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER blob_read_tool_charge_cannot_be_truncated BEFORE TRUNCATE ON blob_read_tool_charge FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: blob_read_tool_charge blob_read_tool_charge_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER blob_read_tool_charge_is_append_only BEFORE DELETE OR UPDATE ON blob_read_tool_charge FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: blob_replica blob_replica_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER blob_replica_cannot_be_truncated BEFORE TRUNCATE ON blob_replica FOR EACH STATEMENT EXECUTE FUNCTION reject_blob_catalog_truncate();


--
-- Name: blob_replica blob_replica_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER blob_replica_is_append_only BEFORE DELETE OR UPDATE ON blob_replica FOR EACH ROW EXECUTE FUNCTION reject_blob_catalog_row_mutation();


--
-- Name: blob blob_requires_replica; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER blob_requires_replica AFTER INSERT ON blob DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_blob_replica();


--
-- Name: blob_store_binding blob_store_binding_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER blob_store_binding_cannot_be_truncated BEFORE TRUNCATE ON blob_store_binding FOR EACH STATEMENT EXECUTE FUNCTION reject_blob_catalog_truncate();


--
-- Name: blob_store_binding blob_store_binding_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER blob_store_binding_is_append_only BEFORE DELETE OR UPDATE ON blob_store_binding FOR EACH ROW EXECUTE FUNCTION reject_blob_catalog_row_mutation();


--
-- Name: context_compacted_outbox_event context_compacted_projects_web_search; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER context_compacted_projects_web_search AFTER INSERT ON context_compacted_outbox_event DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION project_web_search_context_summary();


--
-- Name: context_frontier_delta context_frontier_member_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER context_frontier_member_cannot_be_truncated BEFORE TRUNCATE ON context_frontier_delta FOR EACH STATEMENT EXECUTE FUNCTION reject_imported_table_truncate();


--
-- Name: create_session_command create_session_command_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER create_session_command_cannot_be_truncated BEFORE TRUNCATE ON create_session_command FOR EACH STATEMENT EXECUTE FUNCTION reject_imported_table_truncate();


--
-- Name: imported_conversation imported_conversation_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER imported_conversation_cannot_be_truncated BEFORE TRUNCATE ON imported_conversation FOR EACH STATEMENT EXECUTE FUNCTION reject_imported_table_truncate();


--
-- Name: imported_conversation imported_conversation_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER imported_conversation_is_append_only BEFORE DELETE OR UPDATE ON imported_conversation FOR EACH ROW EXECUTE FUNCTION reject_imported_conversation_change();


--
-- Name: imported_conversation_raw_record imported_conversation_raw_record_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER imported_conversation_raw_record_cannot_be_truncated BEFORE TRUNCATE ON imported_conversation_raw_record FOR EACH STATEMENT EXECUTE FUNCTION reject_imported_table_truncate();


--
-- Name: imported_conversation_raw_record imported_conversation_raw_record_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER imported_conversation_raw_record_is_append_only BEFORE DELETE OR UPDATE ON imported_conversation_raw_record FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: imported_conversation imported_conversation_requires_complete_membership; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER imported_conversation_requires_complete_membership AFTER INSERT OR DELETE OR UPDATE ON imported_conversation DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_imported_conversation_complete();


--
-- Name: imported_conversation_size_totals imported_conversation_size_totals_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER imported_conversation_size_totals_cannot_be_truncated BEFORE TRUNCATE ON imported_conversation_size_totals FOR EACH STATEMENT EXECUTE FUNCTION reject_imported_table_truncate();


--
-- Name: imported_conversation_size_totals imported_conversation_size_totals_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER imported_conversation_size_totals_is_append_only BEFORE DELETE OR UPDATE ON imported_conversation_size_totals FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: imported_transcript_entry imported_entry_stays_within_declared_counts; Type: TRIGGER; Schema: public
--

CREATE TRIGGER imported_entry_stays_within_declared_counts BEFORE INSERT ON imported_transcript_entry FOR EACH ROW EXECUTE FUNCTION require_imported_entry_within_declared_counts();


--
-- Name: create_session_from_imported_frontier_command imported_frontier_command_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER imported_frontier_command_cannot_be_truncated BEFORE TRUNCATE ON create_session_from_imported_frontier_command FOR EACH STATEMENT EXECUTE FUNCTION reject_imported_table_truncate();


--
-- Name: imported_conversation_raw_record imported_raw_record_stays_within_declared_count; Type: TRIGGER; Schema: public
--

CREATE TRIGGER imported_raw_record_stays_within_declared_count BEFORE INSERT ON imported_conversation_raw_record FOR EACH ROW EXECUTE FUNCTION require_imported_raw_record_within_declared_count();


--
-- Name: imported_raw_source_record imported_raw_source_record_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER imported_raw_source_record_cannot_be_truncated BEFORE TRUNCATE ON imported_raw_source_record FOR EACH STATEMENT EXECUTE FUNCTION reject_imported_table_truncate();


--
-- Name: imported_raw_source_record imported_raw_source_record_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER imported_raw_source_record_is_append_only BEFORE DELETE OR UPDATE ON imported_raw_source_record FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: imported_raw_source_record imported_raw_source_record_requires_occurrence; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER imported_raw_source_record_requires_occurrence AFTER INSERT ON imported_raw_source_record DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_imported_raw_source_record_owned();


--
-- Name: imported_session_seed imported_seed_requires_imported_ancestry; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER imported_seed_requires_imported_ancestry AFTER INSERT ON imported_session_seed DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_imported_ancestry_for_seed();


--
-- Name: semantic_transcript_entry imported_semantic_entry_seed_is_sealed; Type: TRIGGER; Schema: public
--

CREATE TRIGGER imported_semantic_entry_seed_is_sealed AFTER INSERT ON semantic_transcript_entry FOR EACH ROW EXECUTE FUNCTION reject_imported_semantic_entry_after_seed();


--
-- Name: imported_session_seed imported_session_seed_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER imported_session_seed_cannot_be_truncated BEFORE TRUNCATE ON imported_session_seed FOR EACH STATEMENT EXECUTE FUNCTION reject_imported_table_truncate();


--
-- Name: imported_session_seed imported_session_seed_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER imported_session_seed_is_append_only BEFORE DELETE OR UPDATE ON imported_session_seed FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: imported_session_seed imported_session_seed_records_creation_transaction; Type: TRIGGER; Schema: public
--

CREATE TRIGGER imported_session_seed_records_creation_transaction BEFORE INSERT ON imported_session_seed FOR EACH ROW EXECUTE FUNCTION stamp_imported_session_seed_transaction();


--
-- Name: imported_transcript_entry imported_transcript_entry_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER imported_transcript_entry_cannot_be_truncated BEFORE TRUNCATE ON imported_transcript_entry FOR EACH STATEMENT EXECUTE FUNCTION reject_imported_table_truncate();


--
-- Name: imported_transcript_entry imported_transcript_entry_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER imported_transcript_entry_is_append_only BEFORE DELETE OR UPDATE ON imported_transcript_entry FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: input_accepted_outbox_event input_accepted_projects_web_search; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER input_accepted_projects_web_search AFTER INSERT ON input_accepted_outbox_event DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION project_web_search_accepted_input();


--
-- Name: model_call_transition_outbox_event model_call_terminal_projects_web_search; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER model_call_terminal_projects_web_search AFTER INSERT ON model_call_transition_outbox_event DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION project_web_search_assistant_text();


--
-- Name: session session_requires_imported_seed; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_requires_imported_seed AFTER INSERT ON session DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_imported_seed_for_session();


--
-- Name: semantic_transcript_entry steering_input_projects_web_search; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER steering_input_projects_web_search AFTER INSERT ON semantic_transcript_entry DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION project_web_search_steering_input();


--
-- Name: tool_batch_transition_outbox_event tool_batch_projects_web_search; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER tool_batch_projects_web_search AFTER INSERT ON tool_batch_transition_outbox_event DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION project_web_search_tool_batch();


--
-- Name: web_search_projection web_search_projection_requires_session_address; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER web_search_projection_requires_session_address AFTER INSERT OR UPDATE OF session_id, event_sequence ON web_search_projection DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION web_search_projection_requires_session_address();


--
-- Name: web_usage_call_projection web_usage_call_projection_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER web_usage_call_projection_cannot_be_truncated BEFORE TRUNCATE ON web_usage_call_projection FOR EACH STATEMENT EXECUTE FUNCTION reject_web_usage_projection_truncate();


--
-- Name: web_usage_call_projection web_usage_call_projection_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER web_usage_call_projection_is_append_only BEFORE DELETE OR UPDATE ON web_usage_call_projection FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: web_usage_oversized_profile_identity web_usage_oversized_profile_identity_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER web_usage_oversized_profile_identity_cannot_be_truncated BEFORE TRUNCATE ON web_usage_oversized_profile_identity FOR EACH STATEMENT EXECUTE FUNCTION reject_web_usage_projection_truncate();


--
-- Name: web_usage_oversized_profile_identity web_usage_oversized_profile_identity_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER web_usage_oversized_profile_identity_is_append_only BEFORE DELETE OR UPDATE ON web_usage_oversized_profile_identity FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: web_usage_oversized_profile_identity web_usage_oversized_profile_identity_is_consistent; Type: TRIGGER; Schema: public
--

CREATE TRIGGER web_usage_oversized_profile_identity_is_consistent BEFORE INSERT ON web_usage_oversized_profile_identity FOR EACH ROW EXECUTE FUNCTION enforce_web_usage_oversized_profile_identity();


--
-- Name: web_usage_call_projection web_usage_projection_matches_its_source; Type: TRIGGER; Schema: public
--

CREATE TRIGGER web_usage_projection_matches_its_source BEFORE INSERT ON web_usage_call_projection FOR EACH ROW EXECUTE FUNCTION require_web_usage_source_correlation();


--
-- Foreign keys.
--

--
-- Name: blob_derivation_input blob_derivation_input_blob_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY blob_derivation_input
    ADD CONSTRAINT blob_derivation_input_blob_fk FOREIGN KEY (digest) REFERENCES blob(digest) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: blob_derivation_input blob_derivation_input_root_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY blob_derivation_input
    ADD CONSTRAINT blob_derivation_input_root_fk FOREIGN KEY (derivation_id) REFERENCES blob_derivation(derivation_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: blob_derivation blob_derivation_model_call_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY blob_derivation
    ADD CONSTRAINT blob_derivation_model_call_fk FOREIGN KEY (model_call_id) REFERENCES model_call(model_call_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: blob_derivation_output blob_derivation_output_blob_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY blob_derivation_output
    ADD CONSTRAINT blob_derivation_output_blob_fk FOREIGN KEY (digest) REFERENCES blob(digest) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: blob_derivation_output blob_derivation_output_root_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY blob_derivation_output
    ADD CONSTRAINT blob_derivation_output_root_fk FOREIGN KEY (derivation_id) REFERENCES blob_derivation(derivation_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: blob_read_tool_charge blob_read_tool_charge_blob_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY blob_read_tool_charge
    ADD CONSTRAINT blob_read_tool_charge_blob_fk FOREIGN KEY (blob_digest) REFERENCES blob(digest) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: blob_read_tool_charge blob_read_tool_charge_request_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY blob_read_tool_charge
    ADD CONSTRAINT blob_read_tool_charge_request_fk FOREIGN KEY (request_id, turn_id, session_id) REFERENCES tool_request(request_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: blob_replica blob_replica_blob_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY blob_replica
    ADD CONSTRAINT blob_replica_blob_fk FOREIGN KEY (digest) REFERENCES blob(digest) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: blob_replica blob_replica_store_binding_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY blob_replica
    ADD CONSTRAINT blob_replica_store_binding_fk FOREIGN KEY (store_name) REFERENCES blob_store_binding(store_name) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: imported_conversation_raw_record imported_conversation_raw_record_blob_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY imported_conversation_raw_record
    ADD CONSTRAINT imported_conversation_raw_record_blob_fk FOREIGN KEY (content_hash) REFERENCES imported_raw_source_record(content_hash) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: imported_conversation_raw_record imported_conversation_raw_record_owner_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY imported_conversation_raw_record
    ADD CONSTRAINT imported_conversation_raw_record_owner_fk FOREIGN KEY (imported_conversation_id) REFERENCES imported_conversation(imported_conversation_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: imported_conversation_size_totals imported_conversation_size_totals_import_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY imported_conversation_size_totals
    ADD CONSTRAINT imported_conversation_size_totals_import_fk FOREIGN KEY (imported_conversation_id) REFERENCES imported_conversation(imported_conversation_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: imported_raw_source_record imported_raw_source_record_blob_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY imported_raw_source_record
    ADD CONSTRAINT imported_raw_source_record_blob_fk FOREIGN KEY (content_hash) REFERENCES blob(digest);


--
-- Name: imported_session_seed imported_session_seed_frontier_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY imported_session_seed
    ADD CONSTRAINT imported_session_seed_frontier_fk FOREIGN KEY (session_id, seed_context_frontier_id) REFERENCES context_frontier(owning_session_id, context_frontier_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: imported_transcript_entry imported_transcript_entry_owner_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY imported_transcript_entry
    ADD CONSTRAINT imported_transcript_entry_owner_fk FOREIGN KEY (imported_conversation_id) REFERENCES imported_conversation(imported_conversation_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: imported_transcript_entry imported_transcript_entry_raw_record_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY imported_transcript_entry
    ADD CONSTRAINT imported_transcript_entry_raw_record_fk FOREIGN KEY (imported_conversation_id, raw_record_position) REFERENCES imported_conversation_raw_record(imported_conversation_id, raw_record_position) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: web_search_projection web_search_projection_session_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY web_search_projection
    ADD CONSTRAINT web_search_projection_session_fk FOREIGN KEY (session_id) REFERENCES session(session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: web_usage_call_projection web_usage_call_identity_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY web_usage_call_projection
    ADD CONSTRAINT web_usage_call_identity_fk FOREIGN KEY (model_call_id) REFERENCES model_call_identity(model_call_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: web_usage_call_projection web_usage_turn_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY web_usage_call_projection
    ADD CONSTRAINT web_usage_turn_fk FOREIGN KEY (turn_id, session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Search-path pins for this file's constraint-reachable functions.
--
-- The pin has to name the schema the migration selected rather than a
-- literal, so it is applied here through current_schema instead of inline
-- in each CREATE FUNCTION (the full rationale is in 202609010000_core.sql;
-- crates/persistence/tests/search_path_postgres.rs is the guard).
--

DO $search_path_pins$
DECLARE
    signature text;
BEGIN
    -- the server default captured at creation time by SET search_path FROM CURRENT
    FOREACH signature IN ARRAY ARRAY[
        'project_web_search_accepted_input()',
        'project_web_search_assistant_text()',
        'project_web_search_context_summary()',
        'project_web_search_steering_input()',
        'project_web_search_tool_batch()',
        'web_search_projection_chunks(text)',
        'web_search_projection_requires_session_address()'
    ] LOOP
        EXECUTE format('ALTER FUNCTION %s SET search_path TO "$user", %I',
                   signature, current_schema);
    END LOOP;
    -- the canonical restore-safe pin: the migration-selected schema, then pg_catalog, then pg_temp
    FOREACH signature IN ARRAY ARRAY[
        'imported_content_encoding_kind(bytea)',
        'imported_encoding_length_at(bytea, integer)',
        'imported_encoding_skip_attestation(bytea, integer, text, integer)',
        'imported_encoding_skip_boolean(bytea, integer)',
        'imported_encoding_skip_media_source(bytea, integer)',
        'imported_encoding_skip_number(bytea, integer)',
        'imported_encoding_skip_structured(bytea, integer, integer)',
        'imported_encoding_skip_text(bytea, integer)',
        'imported_encoding_skip_tool_result(bytea, integer, integer)'
    ] LOOP
        EXECUTE format('ALTER FUNCTION %s SET search_path TO %I, pg_catalog, pg_temp',
                   signature, current_schema);
    END LOOP;
END
$search_path_pins$;


--
-- Restore body validation for anything applied after this file.
--

RESET check_function_bodies;
