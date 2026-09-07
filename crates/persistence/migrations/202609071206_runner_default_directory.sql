ALTER TABLE runner_registration
    ADD COLUMN default_working_directory text CHECK (
        default_working_directory IS NULL OR default_working_directory LIKE '/%'
    );
