//! Review-workflow PostgreSQL coverage.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

#[path = "../support/mod.rs"]
mod support;

use std::{error::Error, future::Future};

use signalbox_application::{
    AuthorizeModelCallOutcome, ModelCallCredentialReference, ReviewConcernClaim,
    ReviewConcernOutcome, ReviewConcernSpec, ReviewConcernSuccess, ReviewDurableSealOutcome,
    ReviewImportOutcome, ReviewImportedContextEvidence, ReviewJudgmentEffectId, ReviewJudgmentPlan,
    ReviewJudgmentPlanMember, ReviewOrchestrationAttempt, ReviewOrchestrationAttemptId,
    ReviewOrchestrationAttemptStore, ReviewPassCompletionStatus, ReviewPlannedDisposition,
    ReviewRepairMemberOutcome, ReviewRepairSuccess, ReviewStageTemplateDigests,
    ReviewTemplateDigest, ReviewWorkflowCommand, ReviewWorkflowCommandOutcome,
    ReviewWorkflowCommandResult, ReviewWorkflowCommandService, ReviewWorkflowOperation,
    ReviewWorkflowOperationKind, StartEligibleTurnIdGenerator, StartEligibleTurnOutcome,
    StartEligibleTurnService,
};
use signalbox_domain::{
    AcceptedInputId, AmbiguousModelCallTurnIdentities, AssistantText, AuthorizedModelCall,
    CancelledModelCallTurnIdentities, CompletedModelCallIdentities, ContextFrontierId,
    CreateSession, DeliveryRequest, DescendantTerminationScope, DirectModelSelection,
    DurableCommandId, FailedModelCallTurnIdentities, ModelCallId, ModelCallTerminalIdentities,
    ModelCallTerminalObservation, ModelCallTerminalOutcome, ModelSelectionOverride,
    ModelSelectionRequest, ModelTargetCatalog, ModelTargetDefinition, PerInputConfigurationChoices,
    ProviderModelIdentity, ResolvedProviderTarget, ReviewChangeRequestNumber, ReviewConfidence,
    ReviewEventOrdinal, ReviewExternalLink, ReviewExternalLinkAssociation,
    ReviewExternalLinkAttachment, ReviewExternalLinkAttachmentResult, ReviewExternalLinkId,
    ReviewExternalLinkNoChangeResult, ReviewExternalLinkObservation,
    ReviewExternalLinkObservationResult, ReviewExternalLinkPublicationBlockedResult,
    ReviewExternalLinkTransitionFailure, ReviewExternalObjectKind, ReviewExternalObjectState,
    ReviewFinding, ReviewFindingConfidenceAxes, ReviewFindingContent, ReviewFindingDiffSide,
    ReviewFindingEvent, ReviewFindingEventKind, ReviewFindingEventResult,
    ReviewFindingEventResultKind, ReviewFindingId, ReviewFindingLocation,
    ReviewFindingPendingExternalLinkRef, ReviewFindingProposal, ReviewFindingRef,
    ReviewFindingSeverity, ReviewFindingStatus, ReviewFindingTransitionFailure, ReviewKey,
    ReviewLineRange, ReviewPass, ReviewPassAcceptedInputEvidence, ReviewPassEvidence, ReviewPassId,
    ReviewPassKind, ReviewPassRef, ReviewPassResult, ReviewPassState, ReviewPassTransitionFailure,
    ReviewPassTurnEvidence, ReviewPassTurnOutcome, ReviewPolicy, ReviewProducedFindings,
    ReviewReferencedFindingEvidence, ReviewRun, ReviewRunEvidence, ReviewRunId, ReviewRunRef,
    ReviewRunState, ReviewTarget, ReviewTargetId, ReviewTargetSubject, ReviewText,
    ReviewWorkflowKind, SemanticTranscriptEntryId, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
    SessionId, SubmitInput, TranscriptAncestry, TurnAttemptId, TurnId, UserContent,
};
use signalbox_persistence::{
    create_session::CreateSessionRepository,
    disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
    disposable_test_container_labels, local_test_connection_options, migrate,
    model_execution::{PostgresModelCallRepository, PrepareInitialModelCallOutcome},
    review_orchestration::{
        PostgresReviewOrchestrationStore, ReviewOrchestrationCommand,
        ReviewOrchestrationCommandClaim, ReviewOrchestrationCommandGuard,
        ReviewOrchestrationCommandKind, ReviewOrchestrationCommandResult,
        ReviewOrchestrationCurrentStage, ReviewOrchestrationStage, ReviewOrchestrationStoreError,
    },
    review_workflow::{
        ReserveExternalLinkOutcome, ReviewWorkflowInsertionError, ReviewWorkflowStore,
        ReviewWorkflowStoreError, ReviewWorkflowTransitionError,
    },
    start_eligible_turn::StartEligibleTurnRepository,
    submit_input::SubmitInputRepository,
};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

use support::{blocked_backends_reached, record_empty_instruction_manifest};

mod command_receipts;
mod external_links;
mod findings;
mod fixtures;
mod loaders;
mod orchestration;
mod run_pass_lifecycle;
mod schema_constraints;
mod target_stack;

use fixtures::*;
