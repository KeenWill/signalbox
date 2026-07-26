//! Reviewed SQL statements that acquire explicit persistence row locks.

pub(crate) const START_ELIGIBLE_TURN: &str = "SELECT
            EXISTS (
                SELECT 1
                  FROM session
                 WHERE session_id = $1
            ),
            (
                SELECT session_id
                  FROM session_scheduler
                 WHERE session_id = $1
                 FOR UPDATE
            )";

pub(crate) const STARTUP_RECOVERY: &str = "SELECT
            EXISTS (
                SELECT 1
                  FROM session
                 WHERE session_id = $1
            ),
            (
                SELECT session_id
                  FROM session_scheduler
                 WHERE session_id = $1
                 FOR UPDATE
            ),
            (
                SELECT turn_id
                  FROM turn_lifecycle
                 WHERE session_id = $1
                   AND state_kind = 'active'
            )";

pub(crate) const SUBMIT_INPUT_SESSION: &str =
    "SELECT session_id FROM session WHERE session_id = $1 FOR NO KEY UPDATE";

pub(crate) const SUBMIT_INPUT_SCHEDULER: &str = "SELECT session_id
           FROM session_scheduler
          WHERE session_id = $1
          FOR UPDATE";

pub(crate) const SUBMIT_INPUT_DEFAULTS: &str = "SELECT current_version
           FROM session_current_defaults
          WHERE session_id = $1
          FOR UPDATE";

pub(crate) const REPLACE_SESSION_METADATA: &str =
    "SELECT session_id FROM session WHERE session_id = $1 FOR NO KEY UPDATE";

pub(crate) const OUTBOX_DELIVERY: &str = "SELECT delivered_through
           FROM outbox_delivery_state
          WHERE singleton
          FOR UPDATE";

pub(crate) const HUB_FENCE_GENERATION: &str = "SELECT generation
           FROM hub_fence_state
          WHERE singleton
          FOR UPDATE";

pub(crate) const RUNNER_ENROLLMENT: &str = "SELECT enrollment_id
               FROM runner_enrollment
              WHERE enrollment_id = $1
              FOR UPDATE";

pub(crate) const RUNNER_GRANT: &str = "SELECT credential_profile_name
               FROM runner_credential_grant
              WHERE session_id = $1
                AND runner_id = $2
                AND grant_revision = $3
              FOR UPDATE";

pub(crate) const RUNNER_LEASE_ENROLLMENT_AUTHORITY: &str = "SELECT state_kind
               FROM runner_enrollment
              WHERE enrollment_id = $1
              FOR SHARE";

pub(crate) const RUNNER_LEASE_GRANT_AUTHORITY: &str = "SELECT credential_profile_name
               FROM runner_credential_grant
              WHERE session_id = $1
                AND runner_id = $2
                AND grant_revision = $3
              FOR SHARE";

pub(crate) const RUNNER_REGISTRATION_HEAD: &str = "SELECT registration_revision
               FROM runner_current_registration
              WHERE enrollment_id = $1
              FOR UPDATE";

pub(crate) const RUNNER_PLACEMENT_HEAD: &str =
    "SELECT record.event_ordinal, record.placement_revision,
                    record.state_kind
               FROM runner_current_session_placement AS current_placement
               JOIN runner_session_placement_record AS record
                 ON record.session_id = current_placement.session_id
                AND record.event_ordinal = current_placement.event_ordinal
              WHERE current_placement.session_id = $1
              FOR UPDATE OF current_placement";

pub(crate) const RUNNER_LEASE_HEAD: &str = "SELECT current_event.event_ordinal, event.state_kind,
                    lease_generation.attempt_id,
                    lease_generation.session_id,
                    lease_generation.runner_id,
                    lease_generation.tool_name,
                    lease_generation.effect_class,
                    lease_generation.credential_profile_name,
                    lease_generation.credential_grant_revision,
                    lease_generation.credential_approval_kind
               FROM runner_current_lease_event AS current_event
               JOIN runner_lease_event AS event
                 ON event.lease_id = current_event.lease_id
                AND event.generation = current_event.generation
                AND event.event_ordinal = current_event.event_ordinal
               JOIN runner_lease_generation AS lease_generation
                 ON lease_generation.lease_id = current_event.lease_id
                AND lease_generation.generation = current_event.generation
              WHERE current_event.lease_id = $1
                AND current_event.generation = $2
              FOR UPDATE OF current_event";

pub(crate) const RUNNER_LEASE_PLACEMENT: &str = "SELECT record.*
           FROM runner_current_session_placement AS current_placement
           JOIN runner_session_placement_record AS record
             ON record.session_id = current_placement.session_id
            AND record.event_ordinal = current_placement.event_ordinal
          WHERE current_placement.session_id = $1
          FOR UPDATE OF current_placement";
