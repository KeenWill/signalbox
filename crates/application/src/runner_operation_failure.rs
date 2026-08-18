//! Durable admission boundary for runner-authored operation failures.

use std::{error::Error, fmt, future::Future};

use serde_json::Value;
use signalbox_domain::{
    RunnerGeneration, RunnerId, SessionId, WorkspaceManifestId, WorkspaceRepositoryKey,
};
use signalbox_runner_wire::{
    MAX_FAILURE_DETAIL_BYTES, MAX_FAILURE_DETAIL_DEPTH, MAX_FAILURE_DETAIL_MEMBERS,
    MAX_FAILURE_MESSAGE_BYTES, MAX_FAILURE_PAYLOAD_BYTES,
};

/// Why runner-authored failure detail was refused at the application boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerOperationFailureDetailFailure {
    /// The runner-specific detail code is not a portable catalog-key name.
    InvalidCode,
    /// The exact message is empty, contains NUL, or exceeds its UTF-8 bound.
    InvalidMessage,
    /// The payload is not a canonical bounded JSON object.
    InvalidPayload,
    /// The complete encoded detail exceeds the runner-protocol bound.
    DetailTooLarge,
}

/// One failed runner-operation detail construction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunnerOperationFailureDetailError {
    failure: RunnerOperationFailureDetailFailure,
}

impl RunnerOperationFailureDetailError {
    /// Returns the exact closed validation failure.
    pub const fn failure(self) -> RunnerOperationFailureDetailFailure {
        self.failure
    }
}

impl fmt::Display for RunnerOperationFailureDetailError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.failure {
            RunnerOperationFailureDetailFailure::InvalidCode => {
                "runner operation failure detail code is invalid"
            }
            RunnerOperationFailureDetailFailure::InvalidMessage => {
                "runner operation failure detail message is invalid"
            }
            RunnerOperationFailureDetailFailure::InvalidPayload => {
                "runner operation failure detail payload is invalid"
            }
            RunnerOperationFailureDetailFailure::DetailTooLarge => {
                "runner operation failure detail is too large"
            }
        })
    }
}

impl Error for RunnerOperationFailureDetailError {}

/// Complete bounded runner-specific detail retained without wire types.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerOperationFailureDetail {
    code: String,
    message: String,
    payload_json: String,
}

/// Labeled runner-authored fields checked as one operation-failure detail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerOperationFailureDetailInput {
    /// Runner-specific portable catalog-key name.
    pub code: String,
    /// Exact nonempty retained message.
    pub message: String,
    /// Canonical bounded JSON-object payload.
    pub payload_json: String,
}

impl RunnerOperationFailureDetail {
    /// Checks the exact runner-protocol spelling, recursive bounds, and bytes.
    pub fn try_new(
        input: RunnerOperationFailureDetailInput,
    ) -> Result<Self, RunnerOperationFailureDetailError> {
        let RunnerOperationFailureDetailInput {
            code,
            message,
            payload_json,
        } = input;
        WorkspaceRepositoryKey::try_new(code.clone()).map_err(|_| {
            RunnerOperationFailureDetailError {
                failure: RunnerOperationFailureDetailFailure::InvalidCode,
            }
        })?;
        if message.is_empty() || message.len() > MAX_FAILURE_MESSAGE_BYTES || message.contains('\0')
        {
            return Err(RunnerOperationFailureDetailError {
                failure: RunnerOperationFailureDetailFailure::InvalidMessage,
            });
        }
        if payload_json.len() > MAX_FAILURE_PAYLOAD_BYTES || payload_json.contains('\0') {
            return Err(RunnerOperationFailureDetailError {
                failure: RunnerOperationFailureDetailFailure::InvalidPayload,
            });
        }
        let payload: Value =
            serde_json::from_str(&payload_json).map_err(|_| RunnerOperationFailureDetailError {
                failure: RunnerOperationFailureDetailFailure::InvalidPayload,
            })?;
        if !payload.is_object()
            || serde_json::to_string(&payload).ok().as_deref() != Some(payload_json.as_str())
            || !detail_value_is_valid(&payload, 1)
        {
            return Err(RunnerOperationFailureDetailError {
                failure: RunnerOperationFailureDetailFailure::InvalidPayload,
            });
        }
        let encoded_code =
            serde_json::to_string(&code).map_err(|_| RunnerOperationFailureDetailError {
                failure: RunnerOperationFailureDetailFailure::InvalidPayload,
            })?;
        let encoded_message =
            serde_json::to_string(&message).map_err(|_| RunnerOperationFailureDetailError {
                failure: RunnerOperationFailureDetailFailure::InvalidPayload,
            })?;
        let encoded = format!(
            "{{\"code\":{encoded_code},\"message\":{encoded_message},\"payload\":{payload_json}}}"
        );
        if encoded.len() > MAX_FAILURE_DETAIL_BYTES {
            return Err(RunnerOperationFailureDetailError {
                failure: RunnerOperationFailureDetailFailure::DetailTooLarge,
            });
        }
        Ok(Self {
            code,
            message,
            payload_json,
        })
    }

    /// Returns the checked runner-specific code.
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Returns the exact retained message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the canonical bounded JSON-object payload.
    pub fn payload_json(&self) -> &str {
        &self.payload_json
    }
}

fn detail_value_is_valid(value: &Value, depth: usize) -> bool {
    match value {
        Value::Object(values) => {
            depth <= MAX_FAILURE_DETAIL_DEPTH
                && values.len() <= MAX_FAILURE_DETAIL_MEMBERS
                && values.iter().all(|(key, value)| {
                    WorkspaceRepositoryKey::try_new(key.clone()).is_ok()
                        && detail_value_is_valid(
                            value,
                            depth + usize::from(value.is_object() || value.is_array()),
                        )
                })
        }
        Value::Array(values) => {
            depth <= MAX_FAILURE_DETAIL_DEPTH
                && values.len() <= MAX_FAILURE_DETAIL_MEMBERS
                && values.iter().all(|value| {
                    detail_value_is_valid(
                        value,
                        depth + usize::from(value.is_object() || value.is_array()),
                    )
                })
        }
        Value::String(value) => value.len() <= MAX_FAILURE_MESSAGE_BYTES,
        Value::Number(value) => value.as_u64().is_some(),
        Value::Bool(_) | Value::Null => true,
    }
}

/// One exact release correlation refused because workspace cleanup failed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerWorkspaceCleanupFailure {
    session: SessionId,
    placement_revision: RunnerGeneration,
    runner: RunnerId,
    manifest: WorkspaceManifestId,
    detail: RunnerOperationFailureDetail,
}

impl RunnerWorkspaceCleanupFailure {
    /// Retains the exact release correlation and bounded runner detail.
    pub const fn new(
        session: SessionId,
        placement_revision: RunnerGeneration,
        runner: RunnerId,
        manifest: WorkspaceManifestId,
        detail: RunnerOperationFailureDetail,
    ) -> Self {
        Self {
            session,
            placement_revision,
            runner,
            manifest,
            detail,
        }
    }

    /// Returns the release's session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the exact release placement revision.
    pub const fn placement_revision(&self) -> RunnerGeneration {
        self.placement_revision
    }

    /// Returns the cleanup-owning runner.
    pub const fn runner(&self) -> RunnerId {
        self.runner
    }

    /// Returns the protected workspace manifest.
    pub const fn manifest_id(&self) -> WorkspaceManifestId {
        self.manifest
    }

    /// Returns the complete retained runner-specific detail.
    pub const fn detail(&self) -> &RunnerOperationFailureDetail {
        &self.detail
    }
}

/// Atomic durable boundary that precedes `operation_failure_recorded` delivery.
pub trait RunnerWorkspaceCleanupFailureTransaction {
    /// Adapter-specific transaction failure.
    type Error;

    /// Commits or exactly replays one authenticated cleanup failure.
    fn record_cleanup_failure(
        &mut self,
        failure: RunnerWorkspaceCleanupFailure,
    ) -> impl Future<Output = Result<RunnerWorkspaceCleanupFailure, Self::Error>> + Send;
}

/// Coordinates one exact workspace-cleanup failure admission.
#[derive(Debug)]
pub struct RunnerWorkspaceCleanupFailureService<Transaction> {
    transaction: Transaction,
}

impl<Transaction> RunnerWorkspaceCleanupFailureService<Transaction> {
    /// Uses the supplied durable operation-failure boundary.
    pub const fn new(transaction: Transaction) -> Self {
        Self { transaction }
    }
}

impl<Transaction> RunnerWorkspaceCleanupFailureService<Transaction>
where
    Transaction: RunnerWorkspaceCleanupFailureTransaction,
{
    /// Commits the exact failure before its acknowledgement is emitted.
    pub async fn execute(
        &mut self,
        failure: RunnerWorkspaceCleanupFailure,
    ) -> Result<RunnerWorkspaceCleanupFailure, Transaction::Error> {
        self.transaction.record_cleanup_failure(failure).await
    }
}

#[cfg(test)]
mod tests {
    use std::future::ready;

    use signalbox_domain::{RunnerGeneration, RunnerId, SessionId, WorkspaceManifestId};
    use uuid::Uuid;

    use super::{
        RunnerOperationFailureDetail, RunnerOperationFailureDetailFailure,
        RunnerOperationFailureDetailInput, RunnerWorkspaceCleanupFailure,
        RunnerWorkspaceCleanupFailureService, RunnerWorkspaceCleanupFailureTransaction,
    };

    const SESSION: u128 = 1;
    const RUNNER: u128 = 2;
    const MANIFEST: u128 = 3;
    const CODE: &str = "cleanup.io";
    const MESSAGE: &str = "workspace removal failed";
    const PAYLOAD: &str = r#"{"attempt":1}"#;

    fn cleanup_failure() -> RunnerWorkspaceCleanupFailure {
        RunnerWorkspaceCleanupFailure::new(
            SessionId::from_uuid(Uuid::from_u128(SESSION)),
            RunnerGeneration::one(),
            RunnerId::from_uuid(Uuid::from_u128(RUNNER)),
            WorkspaceManifestId::from_uuid(Uuid::from_u128(MANIFEST)),
            RunnerOperationFailureDetail::try_new(RunnerOperationFailureDetailInput {
                code: String::from(CODE),
                message: String::from(MESSAGE),
                payload_json: String::from(PAYLOAD),
            })
            .expect("the fixture detail is valid"),
        )
    }

    struct RecordingTransaction {
        expected: RunnerWorkspaceCleanupFailure,
    }

    impl RunnerWorkspaceCleanupFailureTransaction for RecordingTransaction {
        type Error = ();

        fn record_cleanup_failure(
            &mut self,
            failure: RunnerWorkspaceCleanupFailure,
        ) -> impl Future<Output = Result<RunnerWorkspaceCleanupFailure, Self::Error>> + Send
        {
            assert_eq!(failure, self.expected);
            ready(Ok(failure))
        }
    }

    #[test]
    fn detail_retains_checked_fields() {
        let failure = cleanup_failure();

        assert_eq!(failure.detail().code(), CODE);
        assert_eq!(failure.detail().message(), MESSAGE);
        assert_eq!(failure.detail().payload_json(), PAYLOAD);
    }

    #[test]
    fn detail_rejects_noncanonical_payload_json() {
        let error = RunnerOperationFailureDetail::try_new(RunnerOperationFailureDetailInput {
            code: String::from(CODE),
            message: String::from(MESSAGE),
            payload_json: String::from("{ \"attempt\": 1 }"),
        })
        .expect_err("noncanonical payload text is refused");

        assert_eq!(
            error.failure(),
            RunnerOperationFailureDetailFailure::InvalidPayload
        );
    }

    #[tokio::test]
    async fn service_passes_the_exact_failure_to_one_transaction() {
        let expected = cleanup_failure();
        let transaction = RecordingTransaction {
            expected: expected.clone(),
        };
        let mut service = RunnerWorkspaceCleanupFailureService::new(transaction);

        let recorded = service
            .execute(expected.clone())
            .await
            .expect("the recording transaction succeeds");

        assert_eq!(recorded, expected);
    }
}
