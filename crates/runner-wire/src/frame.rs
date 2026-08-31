//! Closed version-two runner frame vocabulary and payload validation.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::{
    digest::{
        Advertisement, DIGEST_VERSION, LeakFact, MAX_LEAK_PAGE_FACTS, WorkspaceManifest,
        advertisement_digest, leak_page_digest, workspace_manifest_digest,
    },
    value::{
        CanonicalUuid, DetailName, Digest, EffectClass, PositiveU64, ProfileName, RepositoryKey,
        ResultBounds, SandboxProfile, TerminalResult, ValueError, WireToolName, WorkingDirectory,
    },
};

/// The only admitted runner protocol version.
pub const PROTOCOL_VERSION: u64 = 2;

/// One complete lease and physical-dispatch correlation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseCorrelation {
    /// Active connection registration revision.
    pub registration_revision: PositiveU64,
    /// Logical lease identity.
    pub lease_id: CanonicalUuid,
    /// Positive lease-lineage generation.
    pub lease_generation: PositiveU64,
    /// Selected runner.
    pub runner_id: CanonicalUuid,
    /// Exact pinned placement revision executed by this lease.
    pub placement_revision: PositiveU64,
    /// Concrete runner-interpreted directory used for execution.
    pub working_directory: WorkingDirectory,
    /// Exact pinned sandbox profile used for execution.
    pub sandbox_profile: SandboxProfile,
    /// Exact tool name.
    pub tool_name: WireToolName,
    /// Owning session.
    pub session_id: CanonicalUuid,
    /// Owning logical turn.
    pub turn_id: CanonicalUuid,
    /// Logical tool request.
    pub tool_request_id: CanonicalUuid,
    /// Physical tool attempt.
    pub tool_attempt_id: CanonicalUuid,
    /// Issuing turn attempt.
    pub issuing_turn_attempt_id: CanonicalUuid,
    /// Positive physical dispatch generation.
    pub tool_dispatch_generation: PositiveU64,
}

/// One workspace provisioning authorization correlation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisionCorrelation {
    /// Single-use authorization identity.
    pub authorization_id: CanonicalUuid,
    /// Owning session.
    pub session_id: CanonicalUuid,
    /// Positive placement revision.
    pub placement_revision: PositiveU64,
    /// Selected runner.
    pub runner_id: CanonicalUuid,
    /// Active connection registration revision.
    pub registration_revision: PositiveU64,
    /// Repository to provision; absent for a private writable root.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::deserialize_present"
    )]
    pub repository: Option<RepositoryKey>,
    /// Exact sandbox profile.
    pub sandbox_profile: SandboxProfile,
    /// Selected optional profile.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::deserialize_present"
    )]
    pub credential_profile: Option<ProfileName>,
}

impl ProvisionCorrelation {
    /// Rejects a credential profile without a repository operation.
    pub fn validate(&self) -> Result<(), ValueError> {
        if self.repository.is_none() && self.credential_profile.is_some() {
            Err(ValueError::Correlation)
        } else {
            Ok(())
        }
    }
}

/// One exact retired workspace-release correlation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseCorrelation {
    /// Retired session.
    pub session_id: CanonicalUuid,
    /// Positive retired placement revision.
    pub placement_revision: PositiveU64,
    /// Cleanup-owning runner.
    pub runner_id: CanonicalUuid,
    /// Exact protected manifest identity.
    pub manifest_id: CanonicalUuid,
}

/// One exact startup leak-page correlation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeakPageCorrelation {
    /// Active registration revision.
    pub registration_revision: PositiveU64,
    /// Complete report digest.
    pub report_digest: Digest,
    /// Positive page.
    pub page: PositiveU64,
}

/// Local fsynced lease phases admitted during heartbeat and reconnect.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeasePhaseKind {
    /// Claim acknowledgement exists and dispatch is absent.
    WaitingDispatch,
    /// Dispatch is journaled before the executor gate.
    DispatchReceived,
    /// The executor may have started.
    ExecutionMayHaveStarted,
}

/// One complete retained lease item.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeasePhase {
    /// Complete lease correlation.
    pub correlation: LeaseCorrelation,
    /// Exact fsynced phase.
    pub phase: LeasePhaseKind,
}

/// Provisioning journal phases.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvisionPhase {
    /// Provisioning is accepted or executing.
    Provisioning,
    /// Complete ready evidence awaits daemon acknowledgement.
    ReadyUnrecorded,
}

/// Release journal phases.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleasePhase {
    /// Release was journaled before deletion.
    ReleaseAccepted,
    /// Deletion completed and its acknowledgement is pending.
    ReleaseCompleted,
}

/// One retained workspace operation and its exact local phase.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkspaceOperation {
    /// Provisioning authority is live.
    Provision {
        /// Complete provisioning correlation.
        correlation: ProvisionCorrelation,
        /// Exact provisioning phase.
        phase: ProvisionPhase,
    },
    /// Release authority is live.
    Release {
        /// Complete release correlation.
        correlation: ReleaseCorrelation,
        /// Exact release phase.
        phase: ReleasePhase,
    },
}

impl WorkspaceOperation {
    fn validate(&self) -> Result<(), ValueError> {
        match self {
            Self::Provision { correlation, .. } => correlation.validate(),
            Self::Release { .. } => Ok(()),
        }
    }
}

/// Closed correlations accepted by durable operation-failure evidence.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "correlation",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum OperationCorrelation {
    /// Refused provisioning.
    Provision(ProvisionCorrelation),
    /// Refused release cleanup.
    Release(ReleaseCorrelation),
    /// Refused offer before claim.
    LeaseOffer(LeaseCorrelation),
}

impl OperationCorrelation {
    fn validate(&self) -> Result<(), ValueError> {
        match self {
            Self::Provision(correlation) => correlation.validate(),
            Self::Release(_) | Self::LeaseOffer(_) => Ok(()),
        }
    }
}

/// Closed daemon-actionable operation-failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    /// Required credentials could not be used.
    CredentialUnavailable,
    /// Repository configuration could not be used.
    RepositoryUnavailable,
    /// Requested sandbox was unavailable.
    SandboxUnavailable,
    /// Workspace facts conflicted.
    WorkspaceConflict,
    /// Release cleanup failed.
    WorkspaceCleanupFailed,
    /// Offered lease was locally inadmissible.
    LeaseAdmissionRefused,
}

/// Maximum serialized complete failure-detail bytes.
pub const MAX_FAILURE_DETAIL_BYTES: usize = 4_096;
/// Maximum failure-detail message UTF-8 bytes.
pub const MAX_FAILURE_MESSAGE_BYTES: usize = 1_024;
/// Maximum serialized failure-detail payload bytes.
pub const MAX_FAILURE_PAYLOAD_BYTES: usize = 2_048;
/// Maximum object members or array elements per failure-detail payload container.
pub const MAX_FAILURE_DETAIL_MEMBERS: usize = 64;
/// Maximum root-to-value failure-detail payload containers.
pub const MAX_FAILURE_DETAIL_DEPTH: usize = 8;

/// One bounded runner-specific structured failure detail.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailureDetail {
    /// Runner-specific checked detail code.
    pub code: DetailName,
    /// Exact nonempty retained message.
    pub message: String,
    /// Bounded structured payload, `{}` when no additional facts exist.
    pub payload: Value,
}

impl FailureDetail {
    /// Checks all exact detail and payload bounds.
    pub fn try_new(code: DetailName, message: String, payload: Value) -> Result<Self, ValueError> {
        if message.is_empty()
            || message.len() > MAX_FAILURE_MESSAGE_BYTES
            || message.contains('\0')
            || !payload.is_object()
            || serde_json::to_vec(&payload)
                .map_err(|_| ValueError::FailureDetail)?
                .len()
                > MAX_FAILURE_PAYLOAD_BYTES
        {
            return Err(ValueError::FailureDetail);
        }
        validate_detail_value(&payload, 1)?;
        let detail = Self {
            code,
            message,
            payload,
        };
        if serde_json::to_vec(&detail)
            .map_err(|_| ValueError::FailureDetail)?
            .len()
            > MAX_FAILURE_DETAIL_BYTES
        {
            return Err(ValueError::FailureDetail);
        }
        Ok(detail)
    }

    /// Borrows the checked JSON payload.
    pub const fn payload(&self) -> &Value {
        &self.payload
    }

    fn validate(&self) -> Result<(), ValueError> {
        Self::try_new(
            self.code.clone(),
            self.message.clone(),
            self.payload.clone(),
        )
        .map(|_| ())
    }
}

fn validate_detail_value(value: &Value, depth: usize) -> Result<(), ValueError> {
    match value {
        Value::Object(values) => {
            if depth > MAX_FAILURE_DETAIL_DEPTH || values.len() > MAX_FAILURE_DETAIL_MEMBERS {
                return Err(ValueError::FailureDetail);
            }
            for (key, value) in values {
                DetailName::try_new(key.clone()).map_err(|_| ValueError::FailureDetail)?;
                validate_detail_value(
                    value,
                    depth + usize::from(value.is_object() || value.is_array()),
                )?;
            }
            Ok(())
        }
        Value::Array(values) => {
            if depth > MAX_FAILURE_DETAIL_DEPTH || values.len() > MAX_FAILURE_DETAIL_MEMBERS {
                return Err(ValueError::FailureDetail);
            }
            values.iter().try_for_each(|value| {
                validate_detail_value(
                    value,
                    depth + usize::from(value.is_object() || value.is_array()),
                )
            })
        }
        Value::String(value) if value.len() <= MAX_FAILURE_MESSAGE_BYTES => Ok(()),
        Value::Number(value) if value.as_u64().is_some() => Ok(()),
        Value::Bool(_) | Value::Null => Ok(()),
        Value::String(_) | Value::Number(_) => Err(ValueError::FailureDetail),
    }
}

/// One exact unacknowledged operation failure.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationFailure {
    /// Refused-operation authority.
    pub correlation: OperationCorrelation,
    /// Closed daemon-actionable category.
    pub category: FailureCategory,
    /// Bounded structured runner detail.
    pub detail: FailureDetail,
}

impl OperationFailure {
    /// Checks category/correlation admissibility and detail bounds.
    pub fn validate(&self) -> Result<(), ValueError> {
        let admitted = matches!(
            (&self.correlation, self.category),
            (
                OperationCorrelation::Provision(_),
                FailureCategory::CredentialUnavailable
                    | FailureCategory::RepositoryUnavailable
                    | FailureCategory::SandboxUnavailable
                    | FailureCategory::WorkspaceConflict,
            ) | (
                OperationCorrelation::Release(_),
                FailureCategory::WorkspaceCleanupFailed,
            ) | (
                OperationCorrelation::LeaseOffer(_),
                FailureCategory::CredentialUnavailable
                    | FailureCategory::RepositoryUnavailable
                    | FailureCategory::SandboxUnavailable
                    | FailureCategory::WorkspaceConflict
                    | FailureCategory::LeaseAdmissionRefused,
            )
        );
        if !admitted {
            return Err(ValueError::Correlation);
        }
        self.correlation.validate()?;
        self.detail.validate()
    }
}

/// One exact retained terminal result item.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedResult {
    /// Complete claimed lease correlation.
    pub correlation: LeaseCorrelation,
    /// Complete terminal envelope.
    pub result: TerminalResult,
}

/// One exact retained leak page.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeakPage {
    /// Complete page correlation.
    pub correlation: LeakPageCorrelation,
    /// Prior page digest; explicitly null only on page one.
    #[serde(deserialize_with = "crate::deserialize_required_nullable")]
    pub prior_page_digest: Option<Digest>,
    /// Whether this is the report's final page.
    pub final_page: bool,
    /// At most 64 sorted facts.
    pub facts: Vec<LeakFact>,
    /// Digest of this complete page.
    pub page_digest: Digest,
}

impl LeakPage {
    /// Checks page-one prior correlation, fact shape/order, and exact digest.
    pub fn validate(&self) -> Result<(), ValueError> {
        if self.facts.len() > MAX_LEAK_PAGE_FACTS
            || (self.correlation.page.get() == 1) != self.prior_page_digest.is_none()
        {
            return Err(ValueError::Correlation);
        }
        let expected = leak_page_digest(crate::LeakPageDigestInput {
            registration_revision: self.correlation.registration_revision,
            report_digest: &self.correlation.report_digest,
            page: self.correlation.page,
            prior_page_digest: self.prior_page_digest.as_ref(),
            final_page: self.final_page,
            facts: &self.facts,
        })?;
        if expected == self.page_digest {
            Ok(())
        } else {
            Err(ValueError::Digest)
        }
    }
}

/// Complete bounded resume inventory.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconnectInventory {
    /// At most one outstanding lease.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::deserialize_present"
    )]
    pub lease: Option<LeasePhase>,
    /// At most one terminal result.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::deserialize_present"
    )]
    pub result: Option<RetainedResult>,
    /// At most one workspace operation.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::deserialize_present"
    )]
    pub workspace_operation: Option<WorkspaceOperation>,
    /// At most one operation failure.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::deserialize_present"
    )]
    pub operation_failure: Option<OperationFailure>,
    /// At most one leak page.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::deserialize_present"
    )]
    pub leak_page: Option<LeakPage>,
}

impl ReconnectInventory {
    /// Validates every bounded retained item before durable journaling or send.
    pub fn validate(&self) -> Result<(), ValueError> {
        if let Some(result) = &self.result {
            result.result.validate()?;
        }
        if let Some(operation) = &self.workspace_operation {
            operation.validate()?;
        }
        if let Some(failure) = &self.operation_failure {
            failure.validate()?;
        }
        if let Some(page) = &self.leak_page {
            page.validate()?;
        }
        Ok(())
    }
}

/// Closed resume directive actions.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectiveAction {
    /// Resend the retained item.
    Resend,
    /// Await its durable predecessor or next state.
    Await,
    /// Discard evidence already durably recorded.
    DiscardAsRecorded,
    /// Fail the stale retained item.
    FailStale,
}

/// One directive bound to the inventoried item's exact correlation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Directive<T> {
    /// Exact correlation repeated from the inventory.
    pub correlation: T,
    /// Durable-state-derived action.
    pub action: DirectiveAction,
}

/// Resume directives with the inventory's exact presence set.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconnectDirectives {
    /// Lease directive.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::deserialize_present"
    )]
    pub lease: Option<Directive<LeaseCorrelation>>,
    /// Terminal-result directive.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::deserialize_present"
    )]
    pub result: Option<Directive<LeaseCorrelation>>,
    /// Workspace-operation directive.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::deserialize_present"
    )]
    pub workspace_operation: Option<Directive<OperationCorrelation>>,
    /// Operation-failure directive.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::deserialize_present"
    )]
    pub operation_failure: Option<Directive<OperationCorrelation>>,
    /// Leak-page directive.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::deserialize_present"
    )]
    pub leak_page: Option<Directive<LeakPageCorrelation>>,
}

impl ReconnectDirectives {
    fn validate(&self) -> Result<(), ValueError> {
        if let Some(directive) = &self.workspace_operation {
            match &directive.correlation {
                OperationCorrelation::Provision(correlation) => correlation.validate()?,
                OperationCorrelation::Release(_) => {}
                OperationCorrelation::LeaseOffer(_) => {
                    return Err(ValueError::Correlation);
                }
            }
        }
        if let Some(directive) = &self.operation_failure {
            directive.correlation.validate()?;
        }
        Ok(())
    }

    /// Checks exact inventory presence and repeats every inventoried correlation.
    pub fn validate_against(&self, inventory: &ReconnectInventory) -> Result<(), ValueError> {
        self.validate()?;
        inventory.validate()?;
        let workspace_correlation = inventory
            .workspace_operation
            .as_ref()
            .map(|item| match item {
                WorkspaceOperation::Provision { correlation, .. } => {
                    OperationCorrelation::Provision(correlation.clone())
                }
                WorkspaceOperation::Release { correlation, .. } => {
                    OperationCorrelation::Release(correlation.clone())
                }
            });
        if directive_matches(
            self.lease.as_ref(),
            inventory.lease.as_ref().map(|item| &item.correlation),
        ) && directive_matches(
            self.result.as_ref(),
            inventory.result.as_ref().map(|item| &item.correlation),
        ) && directive_matches(
            self.workspace_operation.as_ref(),
            workspace_correlation.as_ref(),
        ) && directive_matches(
            self.operation_failure.as_ref(),
            inventory
                .operation_failure
                .as_ref()
                .map(|item| &item.correlation),
        ) && directive_matches(
            self.leak_page.as_ref(),
            inventory.leak_page.as_ref().map(|item| &item.correlation),
        ) {
            Ok(())
        } else {
            Err(ValueError::Correlation)
        }
    }
}

fn directive_matches<T: PartialEq>(directive: Option<&Directive<T>>, item: Option<&T>) -> bool {
    matches!((directive, item), (None, None))
        || matches!((directive, item), (Some(directive), Some(item)) if &directive.correlation == item)
}
/// Closed workspace-operation correlations admitted by heartbeat failure state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "correlation",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum WorkspaceFailureCorrelation {
    /// Provisioning failed before durable acknowledgement.
    Provision(ProvisionCorrelation),
    /// Release cleanup failed before durable acknowledgement.
    Release(ReleaseCorrelation),
}

impl WorkspaceFailureCorrelation {
    fn validate(&self) -> Result<(), ValueError> {
        match self {
            Self::Provision(correlation) => correlation.validate(),
            Self::Release(_) => Ok(()),
        }
    }
}

/// Workspace phase admitted in a heartbeat acknowledgement.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub enum HeartbeatWorkspacePhase {
    /// Provisioning is active.
    Provisioning { correlation: ProvisionCorrelation },
    /// Ready evidence is unrecorded.
    ReadyUnrecorded { correlation: ProvisionCorrelation },
    /// Release is accepted.
    ReleaseAccepted { correlation: ReleaseCorrelation },
    /// Release completion is unrecorded.
    ReleaseCompleted { correlation: ReleaseCorrelation },
    /// Workspace operation failure is unrecorded.
    FailureUnrecorded {
        correlation: WorkspaceFailureCorrelation,
    },
}

/// Available correlation returned in a rejected frame.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "correlation",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum AvailableCorrelation {
    /// Decode failed before a complete correlation was available.
    None,
    /// Enrollment request identity.
    Enrollment(CanonicalUuid),
    /// Registration revision.
    Registration(PositiveU64),
    /// Connection epoch named by one lifecycle frame.
    ConnectionEpoch(PositiveU64),
    /// Complete lease correlation.
    Lease(LeaseCorrelation),
    /// Complete provision correlation.
    Provision(ProvisionCorrelation),
    /// Complete release correlation.
    Release(ReleaseCorrelation),
    /// Complete leak-page correlation.
    LeakPage(LeakPageCorrelation),
    /// Exact refused-operation correlation.
    OperationFailure(OperationCorrelation),
}

impl AvailableCorrelation {
    fn validate(&self) -> Result<(), ValueError> {
        match self {
            Self::Provision(correlation) => correlation.validate(),
            Self::OperationFailure(correlation) => correlation.validate(),
            Self::None
            | Self::Enrollment(_)
            | Self::Registration(_)
            | Self::ConnectionEpoch(_)
            | Self::Lease(_)
            | Self::Release(_)
            | Self::LeakPage(_) => Ok(()),
        }
    }
}

/// Closed fatal/nonfatal rejection codes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionCode {
    UnsupportedVersion,
    UnsupportedDigestVersion,
    MalformedFrame,
    EnrollmentConflict,
    EnrollmentRevoked,
    RegistrationRejected,
    StaleConnection,
    CorrelationMismatch,
    PolicyRejected,
    WorkspaceConflict,
    RunnerLost,
    Unavailable,
    ShuttingDown,
}

/// Closed shutdown reasons.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownReason {
    DaemonShutdown,
    RunnerShutdown,
}

/// One successfully recorded workspace manifest and its exact digest.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReadyManifest {
    /// Complete ready manifest facts.
    manifest: WorkspaceManifest,
    /// Exact content digest of these lifecycle-specific facts.
    manifest_digest: Digest,
    /// Absolute runner-authored directory selected for later execution.
    execution_directory: WorkingDirectory,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReadyManifest {
    manifest: WorkspaceManifest,
    manifest_digest: Digest,
    execution_directory: WorkingDirectory,
}

impl<'de> Deserialize<'de> for ReadyManifest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawReadyManifest::deserialize(deserializer)?;
        Self::try_new(raw.manifest, raw.manifest_digest, raw.execution_directory)
            .map_err(serde::de::Error::custom)
    }
}

impl ReadyManifest {
    /// Constructs ready evidence only when its digest and execution directory are valid.
    pub fn try_new(
        manifest: WorkspaceManifest,
        manifest_digest: Digest,
        execution_directory: WorkingDirectory,
    ) -> Result<Self, ValueError> {
        let ready = Self {
            manifest,
            manifest_digest,
            execution_directory,
        };
        ready.validate()?;
        Ok(ready)
    }

    /// Borrows the complete ready manifest facts.
    pub const fn manifest(&self) -> &WorkspaceManifest {
        &self.manifest
    }

    /// Borrows the exact content digest of the ready manifest.
    pub const fn manifest_digest(&self) -> &Digest {
        &self.manifest_digest
    }

    /// Borrows the absolute runner-authored execution directory.
    pub const fn execution_directory(&self) -> &WorkingDirectory {
        &self.execution_directory
    }

    fn validate(&self) -> Result<(), ValueError> {
        let expected = workspace_manifest_digest(&self.manifest)?;
        if expected != self.manifest_digest {
            return Err(ValueError::Digest);
        }
        self.execution_directory.validate_absolute()
    }
}

fn validate_ready_correlation(
    correlation: &ProvisionCorrelation,
    ready: &ReadyManifest,
) -> Result<(), ValueError> {
    ready.validate()?;
    let manifest = &ready.manifest;
    let terminal = if manifest.repository.is_some() {
        "repo"
    } else {
        "work"
    };
    let expected_relative_path = format!(
        "sessions/{}/{}/{}",
        correlation.session_id,
        correlation.placement_revision.get(),
        terminal
    );
    if manifest.lifecycle == crate::ManifestLifecycle::Ready
        && manifest.session == correlation.session_id
        && manifest.placement_revision == correlation.placement_revision
        && manifest.runner == correlation.runner_id
        && manifest.repository == correlation.repository
        && manifest.sandbox_profile == correlation.sandbox_profile
        && manifest.credential_profile == correlation.credential_profile
        && manifest.relative_path.as_str() == expected_relative_path
    {
        Ok(())
    } else {
        Err(ValueError::Correlation)
    }
}

macro_rules! payload {
    ($name:ident { $($(#[$meta:meta])* $field:ident : $ty:ty),* $(,)? }) => {
        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct $name {
            $($(#[$meta])* pub $field: $ty,)*
        }
    };
}

payload!(Enroll {
    request_id: CanonicalUuid,
    digest_version: u64,
    advertisement: Advertisement
});
payload!(Enrolled {
    request_id: CanonicalUuid,
    enrollment_id: CanonicalUuid,
    runner_id: CanonicalUuid,
    authentication_id: CanonicalUuid,
    registration_revision: PositiveU64,
    connection_epoch: PositiveU64,
    advertisement_digest: Digest
});
payload!(Resume {
    request_id: CanonicalUuid,
    digest_version: u64,
    enrollment_id: CanonicalUuid,
    runner_id: CanonicalUuid,
    authentication_id: CanonicalUuid,
    advertisement: Advertisement,
    prior_registration_revision: PositiveU64,
    inventory: ReconnectInventory
});
payload!(Resumed {
    registration_revision: PositiveU64,
    connection_epoch: PositiveU64,
    directives: ReconnectDirectives
});
payload!(ReplacementPending {
    request_id: CanonicalUuid,
    enrollment_id: CanonicalUuid,
    runner_id: CanonicalUuid,
    authentication_id: CanonicalUuid,
    registration_revision: PositiveU64,
    connection_epoch: PositiveU64,
    advertisement_digest: Digest
});
payload!(Advertise {
    enrollment_id: CanonicalUuid,
    runner_id: CanonicalUuid,
    authentication_id: CanonicalUuid,
    registration_revision: PositiveU64,
    advertisement: Advertisement
});
payload!(Registered {
    registration_revision: PositiveU64,
    advertisement_digest: Digest
});
payload!(Heartbeat {
    sequence: PositiveU64,
    last_accepted_peer_sequence: u64
});
payload!(HeartbeatAck {
    challenge_sequence: PositiveU64,
    runner_sequence: PositiveU64,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::deserialize_present")] lease_phase: Option<LeasePhase>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::deserialize_present")] workspace_phase: Option<HeartbeatWorkspacePhase>
});
payload!(WorkspaceLeakPage { page: LeakPage });
payload!(WorkspaceLeakRecorded {
    correlation: LeakPageCorrelation,
    page_digest: Digest
});
payload!(WorkspaceProvision {
    correlation: ProvisionCorrelation
});
payload!(WorkspaceReady {
    correlation: ProvisionCorrelation,
    ready: ReadyManifest
});
payload!(WorkspaceRecorded {
    correlation: ProvisionCorrelation,
    manifest_id: CanonicalUuid,
    manifest_digest: Digest
});
payload!(WorkspaceRelease {
    correlation: ReleaseCorrelation
});
payload!(WorkspaceReleased {
    correlation: ReleaseCorrelation
});
payload!(WorkspaceReleaseRecorded {
    correlation: ReleaseCorrelation
});
payload!(LeaseOffer {
    correlation: LeaseCorrelation,
    effect_class: EffectClass,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::deserialize_present")] credential_profile: Option<ProfileName>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::deserialize_present")] grant_revision: Option<PositiveU64>,
    normalized_arguments: Value,
    result_bounds: ResultBounds
});
payload!(LeaseClaim {
    correlation: LeaseCorrelation
});
payload!(LeaseClaimed {
    correlation: LeaseCorrelation
});
payload!(Dispatch {
    correlation: LeaseCorrelation,
    normalized_arguments: Value
});
payload!(ResultFrame {
    correlation: LeaseCorrelation,
    result: TerminalResult
});
payload!(ResultRecorded {
    correlation: LeaseCorrelation
});
payload!(OperationFailed {
    failure: OperationFailure
});
payload!(OperationFailureRecorded {
    correlation: OperationCorrelation
});
payload!(Shutdown {
    connection_epoch: PositiveU64,
    reason: ShutdownReason
});
payload!(Rejected {
    offending_kind: String,
    available_correlation: AvailableCorrelation,
    code: RejectionCode
});

/// Complete closed version-two message vocabulary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum Message {
    Enroll(Enroll),
    Enrolled(Enrolled),
    Resume(Box<Resume>),
    Resumed(Box<Resumed>),
    ReplacementPending(ReplacementPending),
    Advertise(Advertise),
    Registered(Registered),
    Heartbeat(Heartbeat),
    HeartbeatAck(HeartbeatAck),
    WorkspaceLeakPage(WorkspaceLeakPage),
    WorkspaceLeakRecorded(WorkspaceLeakRecorded),
    WorkspaceProvision(WorkspaceProvision),
    WorkspaceReady(WorkspaceReady),
    WorkspaceRecorded(WorkspaceRecorded),
    WorkspaceRelease(WorkspaceRelease),
    WorkspaceReleased(WorkspaceReleased),
    WorkspaceReleaseRecorded(WorkspaceReleaseRecorded),
    LeaseOffer(LeaseOffer),
    LeaseClaim(LeaseClaim),
    LeaseClaimed(LeaseClaimed),
    Dispatch(Dispatch),
    Result(ResultFrame),
    ResultRecorded(ResultRecorded),
    OperationFailed(OperationFailed),
    OperationFailureRecorded(OperationFailureRecorded),
    Shutdown(Shutdown),
    Rejected(Rejected),
}

impl Message {
    /// Validates every cross-member invariant before encoding or after decoding.
    pub fn validate(&self) -> Result<(), ValueError> {
        match self {
            Self::Enroll(value) => {
                validate_advertisement(value.digest_version, &value.advertisement)
            }
            Self::Resume(value) => {
                validate_advertisement(value.digest_version, &value.advertisement)?;
                value.inventory.validate()
            }
            Self::Resumed(value) => value.directives.validate(),
            Self::Advertise(value) => value.advertisement.validate(),
            Self::HeartbeatAck(value) => {
                if let Some(phase) = &value.workspace_phase {
                    match phase {
                        HeartbeatWorkspacePhase::Provisioning { correlation }
                        | HeartbeatWorkspacePhase::ReadyUnrecorded { correlation } => {
                            correlation.validate()?
                        }
                        HeartbeatWorkspacePhase::FailureUnrecorded { correlation } => {
                            correlation.validate()?
                        }
                        HeartbeatWorkspacePhase::ReleaseAccepted { .. }
                        | HeartbeatWorkspacePhase::ReleaseCompleted { .. } => {}
                    }
                }
                Ok(())
            }
            Self::WorkspaceLeakPage(value) => value.page.validate(),
            Self::WorkspaceProvision(value) => value.correlation.validate(),
            Self::WorkspaceReady(value) => {
                value.correlation.validate()?;
                validate_ready_correlation(&value.correlation, &value.ready)
            }
            Self::WorkspaceRecorded(value) => value.correlation.validate(),
            Self::LeaseOffer(value) => {
                value.result_bounds.validate()?;
                if !value.normalized_arguments.is_object()
                    || value.credential_profile.is_some() != value.grant_revision.is_some()
                {
                    return Err(ValueError::Correlation);
                }
                Ok(())
            }
            Self::Dispatch(value) if !value.normalized_arguments.is_object() => {
                Err(ValueError::Result)
            }
            Self::Result(value) => value.result.validate(),
            Self::OperationFailed(value) => value.failure.validate(),
            Self::OperationFailureRecorded(value) => value.correlation.validate(),
            Self::Enrolled(_)
            | Self::ReplacementPending(_)
            | Self::Registered(_)
            | Self::Heartbeat(_)
            | Self::WorkspaceLeakRecorded(_)
            | Self::WorkspaceRelease(_)
            | Self::WorkspaceReleased(_)
            | Self::WorkspaceReleaseRecorded(_)
            | Self::LeaseClaim(_)
            | Self::LeaseClaimed(_)
            | Self::Dispatch(_)
            | Self::ResultRecorded(_)
            | Self::Shutdown(_) => Ok(()),
            Self::Rejected(value) => {
                DetailName::try_new(value.offending_kind.clone())?;
                value.available_correlation.validate()
            }
        }
    }
}

fn validate_advertisement(version: u64, advertisement: &Advertisement) -> Result<(), ValueError> {
    advertisement.validate()?;
    if version == DIGEST_VERSION {
        let _ = advertisement_digest(advertisement)?;
    }
    Ok(())
}
