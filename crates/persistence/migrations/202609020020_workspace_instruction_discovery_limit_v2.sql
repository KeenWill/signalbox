ALTER TABLE instruction_discovery
    DROP CONSTRAINT instruction_discovery_limit_version_v1,
    ADD CONSTRAINT instruction_discovery_limit_version_v2
        CHECK ((limit_set_version = 2));
