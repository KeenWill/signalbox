ALTER TABLE program_registration
    ALTER COLUMN source_digest DROP NOT NULL,
    ALTER COLUMN artifact_digest DROP NOT NULL,
    ALTER COLUMN artifact DROP NOT NULL,
    ADD COLUMN executable_kind text NOT NULL,
    ADD COLUMN native_entry text,
    ADD COLUMN native_revision text,
    ADD COLUMN binary_digest bytea,
    ADD CONSTRAINT program_registration_executable_shape CHECK (
        (executable_kind = 'javascript'
         AND source_digest IS NOT NULL AND artifact_digest IS NOT NULL AND artifact IS NOT NULL
         AND native_entry IS NULL AND native_revision IS NULL AND binary_digest IS NULL)
        OR
        (executable_kind = 'native'
         AND source_digest IS NULL AND artifact_digest IS NULL AND artifact IS NULL
         AND native_entry IS NOT NULL AND native_revision IS NOT NULL AND binary_digest IS NOT NULL
         AND octet_length(binary_digest) = 32)
    );
