use super::{error::HubModelConfigurationError, toml_scalars::reject_unknown_fields};
use std::{collections::HashMap, time::Duration};
use toml_edit::Item;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NumericBoundKind {
    Integer,
    Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum NumericBoundValue {
    Integer(u64),
    Duration(Duration),
    Unbounded,
}

/// Validated deployment policy for every non-structural numeric bound.
#[derive(Clone, Debug)]
pub struct NumericBoundsConfiguration {
    pub(super) values: HashMap<&'static str, NumericBoundValue>,
}

const REQUIRED_NUMERIC_BOUNDS: &[(&str, NumericBoundKind)] = &[
    ("client_frame_deadline", NumericBoundKind::Duration),
    ("client_write_progress_deadline", NumericBoundKind::Duration),
    (
        "repository_watch_webhook_retention",
        NumericBoundKind::Duration,
    ),
    ("fenced_pool_min_connections", NumericBoundKind::Integer),
    (
        "fenced_pool_floor_reconciliation_interval",
        NumericBoundKind::Duration,
    ),
    (
        "fenced_pool_floor_reconciliation_attempt_bound",
        NumericBoundKind::Duration,
    ),
    ("max_concurrent_snapshot_readers", NumericBoundKind::Integer),
    ("max_blob_replica_count", NumericBoundKind::Integer),
    ("max_session_metadata_tags", NumericBoundKind::Integer),
    ("max_session_metadata_attributes", NumericBoundKind::Integer),
    (
        "max_session_metadata_required_tags",
        NumericBoundKind::Integer,
    ),
    ("max_system_prompt_utf8_bytes", NumericBoundKind::Integer),
    (
        "max_imported_text_preview_utf8_bytes",
        NumericBoundKind::Integer,
    ),
    (
        "max_review_orchestration_concerns",
        NumericBoundKind::Integer,
    ),
    (
        "max_imported_conversation_display_title_scalars",
        NumericBoundKind::Integer,
    ),
    (
        "graceful_shutdown_cleanup_window",
        NumericBoundKind::Duration,
    ),
    ("model_exchange_timeout", NumericBoundKind::Duration),
    ("codex_cli_version_probe_bound", NumericBoundKind::Duration),
    ("expired_pass_recovery_attempts", NumericBoundKind::Integer),
    (
        "expired_pass_recovery_attempt_bound",
        NumericBoundKind::Duration,
    ),
    (
        "expired_pass_recovery_lock_retry_delay",
        NumericBoundKind::Duration,
    ),
    (
        "expired_pass_recovery_conservative_retry_delay",
        NumericBoundKind::Duration,
    ),
    (
        "convergence_sweep_request_timeout",
        NumericBoundKind::Duration,
    ),
    (
        "max_convergence_sweep_connection_pages",
        NumericBoundKind::Integer,
    ),
    (
        "max_concurrent_convergence_sweep_targets",
        NumericBoundKind::Integer,
    ),
    (
        "max_convergence_sweep_request_attempts",
        NumericBoundKind::Integer,
    ),
    (
        "convergence_sweep_request_retry_delay",
        NumericBoundKind::Duration,
    ),
    (
        "convergence_sweep_retry_backoff_base",
        NumericBoundKind::Duration,
    ),
    (
        "convergence_sweep_retry_backoff_cap",
        NumericBoundKind::Duration,
    ),
    (
        "terminalizations_per_liveness_scan",
        NumericBoundKind::Integer,
    ),
    (
        "turn_liveness_recovery_attempt_bound",
        NumericBoundKind::Duration,
    ),
    (
        "automatic_reconciliations_per_liveness_scan",
        NumericBoundKind::Integer,
    ),
    (
        "automatic_reconciliation_attempt_bound",
        NumericBoundKind::Duration,
    ),
    ("max_convergence_sweep_targets", NumericBoundKind::Integer),
    ("max_convergence_sweep_interval", NumericBoundKind::Duration),
    ("max_convergence_sweep_cool_off", NumericBoundKind::Duration),
    ("automatic_resume_base_backoff", NumericBoundKind::Duration),
    ("automatic_resume_backoff_cap", NumericBoundKind::Duration),
    ("automatic_resume_attempt_budget", NumericBoundKind::Integer),
    (
        "automatic_resume_attempt_ceiling",
        NumericBoundKind::Integer,
    ),
    (
        "automatic_resume_startup_retry_delay",
        NumericBoundKind::Duration,
    ),
    ("post_kill_reap_bound", NumericBoundKind::Duration),
    ("stale_active_turn_bound", NumericBoundKind::Duration),
    ("turn_liveness_scan_interval", NumericBoundKind::Duration),
    (
        "automatic_reconciliation_base_backoff",
        NumericBoundKind::Duration,
    ),
    (
        "automatic_reconciliation_backoff_cap",
        NumericBoundKind::Duration,
    ),
    (
        "automatic_reconciliation_attempt_budget",
        NumericBoundKind::Integer,
    ),
    ("terminal_input_channel_capacity", NumericBoundKind::Integer),
    ("max_message_utf8_bytes", NumericBoundKind::Integer),
    ("min_metadata_page_size", NumericBoundKind::Integer),
    ("max_metadata_page_size", NumericBoundKind::Integer),
    ("max_review_findings_per_run", NumericBoundKind::Integer),
    (
        "max_automatic_tool_rounds_per_turn",
        NumericBoundKind::Integer,
    ),
    (
        "max_same_credential_attempts_per_turn",
        NumericBoundKind::Integer,
    ),
    ("max_required_tags", NumericBoundKind::Integer),
    ("reconciliation_sweep_interval", NumericBoundKind::Duration),
    ("nudge_buffer_capacity", NumericBoundKind::Integer),
    ("scheduler_pass_admission_cap", NumericBoundKind::Integer),
    ("scheduler_pass_occupancy_bound", NumericBoundKind::Duration),
    ("max_native_message_bytes", NumericBoundKind::Integer),
    ("terminalization_lock_wait", NumericBoundKind::Duration),
    ("terminalization_acquire_wait", NumericBoundKind::Duration),
    (
        "terminalization_write_lock_wait",
        NumericBoundKind::Duration,
    ),
    (
        "disposable_postgres_state_ceiling_bytes",
        NumericBoundKind::Integer,
    ),
    ("diagnostic_model_identity_limit", NumericBoundKind::Integer),
    ("code_host_request_timeout", NumericBoundKind::Duration),
    ("max_job_log_bytes", NumericBoundKind::Integer),
    ("max_stack_comparisons_in_flight", NumericBoundKind::Integer),
    ("max_code_host_result_text_bytes", NumericBoundKind::Integer),
    ("max_code_host_result_items", NumericBoundKind::Integer),
    (
        "max_repository_file_content_bytes",
        NumericBoundKind::Integer,
    ),
    ("session_admission_deadline", NumericBoundKind::Duration),
    ("session_active_stall_deadline", NumericBoundKind::Duration),
    ("session_waiting_deadline", NumericBoundKind::Duration),
    (
        "session_lifecycle_metric_scan_interval",
        NumericBoundKind::Duration,
    ),
];

impl NumericBoundsConfiguration {
    pub(super) fn parse(item: Option<&Item>) -> Result<Self, HubModelConfigurationError> {
        let table = item.and_then(Item::as_table);
        let missing = REQUIRED_NUMERIC_BOUNDS
            .iter()
            .filter_map(|(name, _)| {
                table
                    .is_none_or(|table| !table.contains_key(name))
                    .then_some(*name)
            })
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(HubModelConfigurationError::MissingNumericBounds { fields: missing });
        }
        let table = table.ok_or_else(|| HubModelConfigurationError::MissingNumericBounds {
            fields: REQUIRED_NUMERIC_BOUNDS
                .iter()
                .map(|(name, _)| *name)
                .collect(),
        })?;
        let allowed_fields = REQUIRED_NUMERIC_BOUNDS
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>();
        reject_unknown_fields(table, &allowed_fields)?;
        let mut values = HashMap::with_capacity(REQUIRED_NUMERIC_BOUNDS.len());
        for (name, kind) in REQUIRED_NUMERIC_BOUNDS {
            let item = table.get(name).ok_or_else(|| {
                HubModelConfigurationError::MissingNumericBounds {
                    fields: vec![*name],
                }
            })?;
            let value = if item.as_str() == Some("none") {
                NumericBoundValue::Unbounded
            } else {
                match kind {
                    NumericBoundKind::Integer => item
                        .as_integer()
                        .and_then(|value| u64::try_from(value).ok())
                        .filter(|value| usize::try_from(*value).is_ok())
                        .map(NumericBoundValue::Integer),
                    NumericBoundKind::Duration => item
                        .as_str()
                        .and_then(parse_numeric_bound_duration)
                        .map(NumericBoundValue::Duration),
                }
                .ok_or(HubModelConfigurationError::InvalidNumericBound { field: name })?
            };
            values.insert(*name, value);
        }
        let configuration = Self { values };
        if configuration
            .duration("repository_watch_webhook_retention")
            .flatten()
            .is_none_or(|duration| duration.is_zero())
        {
            return Err(HubModelConfigurationError::InvalidNumericBound {
                field: "repository_watch_webhook_retention",
            });
        }
        let field = "max_review_findings_per_run";
        if configuration
            .integer(field)
            .flatten()
            .is_none_or(|value| value > signalbox_domain::ReviewProducedFindings::MAXIMUM as u64)
        {
            return Err(HubModelConfigurationError::InvalidNumericBound { field });
        }
        // Replica registration already bounds distinct stores globally. A smaller
        // read policy cannot be maintained by that durable write boundary.
        if configuration
            .integer("max_blob_replica_count")
            .flatten()
            .is_some_and(|value| value < signalbox_blob_store::MAX_BLOB_STORES as u64)
        {
            return Err(HubModelConfigurationError::InvalidNumericBound {
                field: "max_blob_replica_count",
            });
        }
        if configuration
            .duration("reconciliation_sweep_interval")
            .flatten()
            .is_none()
            && configuration
                .integer("nudge_buffer_capacity")
                .flatten()
                .is_some()
        {
            return Err(HubModelConfigurationError::InvalidNumericBound {
                field: "nudge_buffer_capacity",
            });
        }
        Ok(configuration)
    }

    /// Returns one integer policy, with inner `None` denoting configured `"none"`.
    ///
    /// Callers use field names from the checked-in schema inventory.
    pub fn integer(&self, field: &'static str) -> Option<Option<u64>> {
        match self.values.get(field) {
            Some(NumericBoundValue::Integer(value)) => Some(Some(*value)),
            Some(NumericBoundValue::Unbounded) => Some(None),
            _ => None,
        }
    }

    /// Returns one duration policy, with inner `None` denoting configured `"none"`.
    ///
    /// Callers use field names from the checked-in schema inventory.
    pub fn duration(&self, field: &'static str) -> Option<Option<Duration>> {
        match self.values.get(field) {
            Some(NumericBoundValue::Duration(value)) => Some(Some(*value)),
            Some(NumericBoundValue::Unbounded) => Some(None),
            _ => None,
        }
    }
}

pub(super) fn parse_numeric_bound_duration(value: &str) -> Option<Duration> {
    jiff::fmt::friendly::SpanParser::new()
        .parse_unsigned_duration(value)
        .ok()
}
