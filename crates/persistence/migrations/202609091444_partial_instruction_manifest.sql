CREATE OR REPLACE FUNCTION validate_turn_instruction_manifest_discovery() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM instruction_discovery AS discovery
         WHERE discovery.instruction_discovery_id = NEW.instruction_discovery_id
           AND discovery.session_id = NEW.session_id
           AND discovery.turn_id = NEW.turn_id
    ) THEN
        RAISE EXCEPTION 'turn instruction manifest requires matching discovery evidence'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'turn_instruction_manifest_discovery_exact';
    END IF;
    IF NEW.eligibility_hash <> sha256(
        convert_to('signalbox-instruction-eligibility-v1', 'UTF8')
    ) OR NEW.admitted_set_hash <> sha256(
        convert_to('signalbox-instruction-admitted-set-v1', 'UTF8')
        || '\x0000000000000000'::bytea
    ) OR NEW.manifest_hash <> sha256(
        convert_to('signalbox-turn-instruction-manifest-v1', 'UTF8')
        || uuid_send(NEW.session_id)
        || uuid_send(NEW.turn_id)
        || NEW.eligibility_hash
        || NEW.admitted_set_hash
        || convert_to(NEW.boundary_kind, 'UTF8')
    ) THEN
        RAISE EXCEPTION 'turn instruction manifest hashes are not canonical'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'turn_instruction_manifest_hash_shape';
    END IF;
    RETURN NEW;
END;
$$;

ALTER TABLE instruction_discovery
    DROP CONSTRAINT instruction_discovery_entry_count_bounded,
    DROP CONSTRAINT instruction_discovery_finding_count_bounded,
    DROP CONSTRAINT instruction_discovery_source_bytes_bounded,
    ADD CONSTRAINT instruction_discovery_entry_count_nonnegative
        CHECK (classified_entry_count >= 0),
    ADD CONSTRAINT instruction_discovery_finding_count_nonnegative
        CHECK (finding_count >= 0),
    ADD CONSTRAINT instruction_discovery_source_bytes_nonnegative
        CHECK (candidate_source_byte_count >= 0);
