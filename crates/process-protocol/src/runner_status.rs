//! Paged runner placement and retained diagnostic projections.

use crate::{
    CanonicalDigest, CanonicalUuid, FrameValidationError, PositiveCanonicalU64,
    RunnerConnectionHealth, RunnerCredentialProfileName, RunnerProjection, RunnerRepositoryKey,
    RunnerSandboxProfile,
};
use serde::{Deserialize, Serialize};

/// Exclusive key identifying the last fact emitted by a page.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerStatusCursor {
    /// Current enrollment identity.
    Enrollment { runner_id: CanonicalUuid },
    /// Current session placement identity.
    Placement { session_id: CanonicalUuid },
    /// Immutable replacement-provisioning refusal.
    OperationFailure { authorization_id: CanonicalUuid },
    /// Retained workspace leak identity.
    WorkspaceLeak {
        runner_id: CanonicalUuid,
        locator: String,
        entry_digest: CanonicalDigest,
    },
}

/// Current daemon-issued enrollment authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerAuthorityState {
    /// Provisioning-only successor awaiting explicit promotion.
    Pending,
    /// Enrollment may authorize execution.
    Active,
    /// Enrollment has been revoked.
    Revoked,
}

/// One current enrollment or session placement fact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerStatusFact {
    /// Current enrollment, including its operator-addressable request identity.
    Enrollment {
        runner_id: CanonicalUuid,
        enrollment_request_id: CanonicalUuid,
        authority: RunnerAuthorityState,
        #[serde(deserialize_with = "crate::deserialize_required_nullable")]
        connection_health: Option<RunnerConnectionHealth>,
    },
    /// Current session placement, using the snapshot's complete runner object.
    Placement {
        session_id: CanonicalUuid,
        runner: RunnerProjection,
    },
}

/// Complete refused provisioning correlation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerProvisionFailureCorrelation {
    pub authorization_id: CanonicalUuid,
    pub session_id: CanonicalUuid,
    pub placement_revision: PositiveCanonicalU64,
    pub runner_id: CanonicalUuid,
    pub registration_revision: PositiveCanonicalU64,
    #[serde(deserialize_with = "crate::deserialize_required_nullable")]
    pub repository: Option<RunnerRepositoryKey>,
    pub sandbox_profile: RunnerSandboxProfile,
    #[serde(deserialize_with = "crate::deserialize_required_nullable")]
    pub credential_profile: Option<RunnerCredentialProfileName>,
}

/// Closed daemon-actionable categories carried by the runner wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerFailureCategory {
    CredentialUnavailable,
    RepositoryUnavailable,
    SandboxUnavailable,
    WorkspaceConflict,
    WorkspaceCleanupFailed,
    LeaseAdmissionRefused,
}

/// Diagnostic view of bounded runner-authored detail.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerFailureDetail {
    pub code: String,
    pub message: String,
    pub payload: serde_json::Value,
}

/// The operation kind selects exactly one complete correlation arm.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation_kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerOperationFailure {
    Provision {
        correlation: RunnerProvisionFailureCorrelation,
        category: RunnerFailureCategory,
        detail: RunnerFailureDetail,
    },
}

/// Closed workspace-leak classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerWorkspaceLeakKind {
    UnknownManifest,
    RetiredPresent,
    ManifestConflict,
    CleanupFailed,
    Unreconciled,
}

/// One diagnostic workspace-leak projection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerWorkspaceLeak {
    pub runner_id: CanonicalUuid,
    pub kind: RunnerWorkspaceLeakKind,
    pub locator: String,
    pub entry_digest: CanonicalDigest,
    #[serde(deserialize_with = "crate::deserialize_required_nullable")]
    pub session_id: Option<CanonicalUuid>,
    #[serde(deserialize_with = "crate::deserialize_required_nullable")]
    pub placement_revision: Option<PositiveCanonicalU64>,
}

impl RunnerStatusCursor {
    pub(crate) fn validate(&self) -> Result<(), FrameValidationError> {
        if let Self::WorkspaceLeak { locator, .. } = self {
            validate_locator(locator)?;
        }
        Ok(())
    }
}

fn validate_locator(locator: &str) -> Result<(), FrameValidationError> {
    signalbox_domain::WorkspaceRelativePath::try_new(locator.to_owned())
        .map(|_| ())
        .map_err(|_| FrameValidationError::RunnerStatusShape)
}

impl RunnerWorkspaceLeak {
    pub(crate) fn validate(&self) -> Result<(), FrameValidationError> {
        validate_locator(&self.locator)
    }
}

impl RunnerOperationFailure {
    pub(crate) fn validate(&self) -> Result<(), FrameValidationError> {
        use RunnerFailureCategory as C;
        let (detail, admitted) = match self {
            Self::Provision {
                correlation,
                category,
                detail,
            } => (
                detail,
                (correlation.repository.is_some() || correlation.credential_profile.is_none())
                    && matches!(
                        category,
                        C::CredentialUnavailable
                            | C::RepositoryUnavailable
                            | C::SandboxUnavailable
                            | C::WorkspaceConflict
                    ),
            ),
        };
        if !admitted
            || signalbox_domain::WorkspaceRepositoryKey::try_new(detail.code.clone()).is_err()
            || detail.message.is_empty()
            || detail.message.len() > 1024
            || detail.message.contains('\0')
            || !detail.payload.is_object()
            || serde_json::to_vec(&detail.payload)
                .map_err(|_| FrameValidationError::RunnerStatusShape)?
                .len()
                > 2048
            || serde_json::to_vec(detail)
                .map_err(|_| FrameValidationError::RunnerStatusShape)?
                .len()
                > 4096
        {
            return Err(FrameValidationError::RunnerStatusShape);
        }
        validate_payload(&detail.payload, 1)
    }
}

fn validate_payload(value: &serde_json::Value, depth: usize) -> Result<(), FrameValidationError> {
    match value {
        serde_json::Value::Object(items) => {
            if depth > 8 || items.len() > 64 {
                return Err(FrameValidationError::RunnerStatusShape);
            }
            for (key, value) in items {
                if signalbox_domain::WorkspaceRepositoryKey::try_new(key.clone()).is_err() {
                    return Err(FrameValidationError::RunnerStatusShape);
                }
                validate_payload(value, depth + 1)?;
            }
        }
        serde_json::Value::Array(items) => {
            if depth > 8 || items.len() > 64 {
                return Err(FrameValidationError::RunnerStatusShape);
            }
            for value in items {
                validate_payload(value, depth + 1)?;
            }
        }
        serde_json::Value::String(text) if text.len() > 1024 => {
            return Err(FrameValidationError::RunnerStatusShape);
        }
        serde_json::Value::Number(number) if number.as_u64().is_none() => {
            return Err(FrameValidationError::RunnerStatusShape);
        }
        _ => {}
    }
    Ok(())
}
