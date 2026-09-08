//! Feature-gated PostgreSQL coverage for runner enrollment, leases, placement, and grants.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::{error::Error, num::NonZeroU64, time::Duration};

use rust_decimal::Decimal;
use signalbox_domain::{
    AcceptedInputId, AcceptedInputTurnActivationIdentities, ApprovedToolRequest,
    CancelledModelCallTurnIdentities, CanonicalCloneUrlDigest, ContextFrontierId, CreateSession,
    CredentialProfileGrant, CredentialProfileGrantReconstitutionInput, CredentialProfileName,
    CredentialProfilePolicy, CredentialToolApproval, DecideToolRequest, DeliveryRequest,
    DescendantTerminationScope, DirectModelSelection, DurableCommandId, EndedToolAttempt,
    ModelCallId, ModelSelectionOverride, ModelSelectionRequest, NormalizedToolArguments,
    PerInputConfigurationChoices, ProvisionedWorkspace, ResolvedContextFrontierReconstitutionInput,
    RunnerAdvertisement, RunnerAuthenticationId, RunnerCapabilityClass, RunnerCatalog,
    RunnerDomainError, RunnerEnrollment, RunnerEnrollmentId, RunnerGeneration, RunnerId,
    RunnerLease, RunnerLeaseCorrelation, RunnerLeaseId, RunnerLeaseOfferRequest,
    RunnerLeaseReconstitutionInput, RunnerLeaseRetryPreparation, RunnerLostBeforePin,
    RunnerPlacementReconstitutionHistory, RunnerRepositoryEntry, RunnerSandboxProfile,
    RunnerSelector, RunnerToolAttemptAuthorization, RunnerToolDeclaration, RunnerToolEffectClass,
    RunnerToolModelDefinition, RunnerToolPermissionOverride, RunnerToolPermissionOverrides,
    RunnerWorkingDirectory, SemanticTranscriptEntryId, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
    SessionId, SessionRunnerPin, SessionRunnerPlacement, SessionRunnerPlacementReconstitutionInput,
    SessionRunnerPlacementRequest, SessionRunnerPlacementState, SubmitInput, ToolAdmissibleLoci,
    ToolApprovalDecision, ToolApprovalResolutionReconstitutionInput,
    ToolAttemptDispatchCorrelation, ToolAttemptDispatchCorrelationReconstitutionInput,
    ToolAttemptId, ToolAttemptReconstitutionInput, ToolAttemptReconstitutionState, ToolBatch,
    ToolBatchPhaseReconstitutionInput, ToolBatchReconstitutionInput, ToolDispatchGeneration,
    ToolEffectClass, ToolName, ToolPermissionDefault, ToolRequestId, ToolRequestOrdinal,
    ToolRequestReconstitutionInput, TranscriptAncestry, TurnAttemptId, TurnId,
    TurnInstructionManifest, TurnInstructionManifestId, UserContent, ValidatedRunnerRegistration,
    WorkingDirectorySelection, WorkspaceCapability, WorkspaceManifestId, WorkspaceRecovery,
    WorkspaceRelativePath, WorkspaceRepositoryKey, WorkspaceRequirement, WorkspaceRevision,
};
use signalbox_persistence::{
    create_session::CreateSessionRepository,
    disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
    disposable_test_container_labels, local_test_connection_options, migrate,
    outbox::{
        DispatchedOutboxEvent, DispatchedOutboxEventKind, DispatchedRunnerState, OutboxCorruption,
        OutboxDeliveryDecision, OutboxDispatchError, OutboxDispatchOutcome, OutboxDispatcher,
        RunnerStateTransitionOutboxTestEvent, RunnerStateTransitionOutboxTestSource,
        append_runner_state_transition_for_test,
    },
    process_read::{
        ProcessReadRepository, ProcessRunnerConnectionHealth, ProcessRunnerProjectionState,
    },
    runner_protocol::{
        RunnerConnectionCause, RunnerConnectionEpoch, RunnerConnectionLossSessionDisposition,
        RunnerConnectionState, RunnerConnectionTransition, RunnerProtocolCorruption,
        RunnerProtocolStore, RunnerProtocolStoreError, StoredValidatedRunnerRegistration,
    },
    session_credentials::{SessionCredentialPin, SessionModelCredential},
    start_eligible_turn::StartEligibleTurnRepository,
    submit_input::SubmitInputRepository,
};
use sqlx::{PgConnection, PgPool, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

#[path = "../support/mod.rs"]
mod support;

use support::blocked_backends_reached;

const LOCK_WAIT_PROBE: Duration = Duration::from_millis(100);
const LOCK_COMPLETION_TIMEOUT: Duration = Duration::from_secs(30);
const SERIALIZATION_TEST_TIMEOUT: Duration = Duration::from_secs(90);

mod connection_epoch;
mod fixtures;
mod grants;
mod lease;
mod loss_history;
mod loss_propagation;
mod outbox;
mod placement;
mod placement_loss;
mod recovery_commands;
mod recovery_provisioning;
mod runner_recovery;
mod store_load;

use fixtures::*;
use lease::*;
use loss_history::*;
use placement::*;
use runner_recovery::*;

mod status;
