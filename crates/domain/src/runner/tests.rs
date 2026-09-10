//! Runner tests for `docs/spec/runner-protocol.md`.

use super::catalog::{PERMISSION_OVERRIDE_MAX_ENTRIES, tool_effect_class};
use super::names::NAME_MAX_BYTES;
use super::placement::{WorkspaceRevisionMatch, validate_placement};
use crate::{
    NormalizedToolArguments, RunnerId, ToolAttemptId, ToolBatch, ToolBatchExecutionFailure,
    ToolEffectClass, ToolName, ToolPermissionDefault, WorkspaceManifestId,
};
use std::{collections::BTreeSet, sync::Arc, sync::atomic::AtomicBool};

use super::*;
use crate::{
    ApprovedToolRequest, DangerousToolAutoApproval, DecideToolRequest, DurableCommandId,
    ReconstitutedToolAttempt, ResolvedContextFrontierSnapshot, ToolApprovalDecision,
    ToolApprovalPosture, ToolApprovalResolutionReconstitutionInput, ToolAttemptEnd,
    ToolBatchPhaseReconstitutionInput, ToolBatchReconstitutionInput, ToolExecutionErrorKind,
    ToolRequest, ToolRequestId, ToolRequestOrdinal, ToolRequestReconstitutionInput,
    test_support::{
        context_frontier_id, model_call_id, runner_authentication_id, runner_enrollment_id,
        runner_id, runner_lease_id, session_id, tool_attempt_id, tool_request_id, turn_attempt_id,
        turn_id,
    },
};

const ENROLLMENT: u128 = 0x7100;
const RUNNER: u128 = 0x7200;
const REPLACEMENT_RUNNER: u128 = 0x7201;
const THIRD_RUNNER: u128 = 0x7202;
const AUTHENTICATION: u128 = 0x7300;
const LEASE: u128 = 0x7400;
const ATTEMPT: u128 = 0x7500;
const RETRY_ATTEMPT: u128 = 0x7501;
const SESSION: u128 = 0x7600;
/// Arbitrary empty context-frontier identity for complete batch fixtures.
const YIELDED_FRONTIER: u128 = 0x7a00;

fn class() -> RunnerCapabilityClass {
    RunnerCapabilityClass::try_new("linux.workspace".to_owned())
        .expect("the canonical class name is valid")
}

fn profile(name: &str) -> CredentialProfileName {
    CredentialProfileName::try_new(name.to_owned()).expect("fixture profile names are valid")
}

fn tool(name: &str) -> ToolName {
    ToolName::try_new(name.to_owned()).expect("fixture tool names are valid")
}

fn repository_key() -> WorkspaceRepositoryKey {
    WorkspaceRepositoryKey::try_new("signalbox".to_owned())
        .expect("the fixture repository key is valid")
}

fn model_definition(name: &str) -> RunnerToolModelDefinition {
    RunnerToolModelDefinition::try_new(
        format!("Run the {name} fixture operation"),
        r#"{"type":"object"}"#.to_owned(),
    )
    .expect("fixture model definitions are valid")
}

fn sandbox_profiles() -> [RunnerSandboxProfile; 2] {
    [
        RunnerSandboxProfile::Ambient,
        RunnerSandboxProfile::WorkspaceRestricted,
    ]
}

fn no_permission_overrides() -> RunnerToolPermissionOverrides {
    RunnerToolPermissionOverrides::try_new([])
        .expect("the empty permission override fixture is valid")
}

fn catalog() -> RunnerCatalog {
    let inspect = RunnerToolDeclaration::new(
        tool("inspect"),
        model_definition("inspect"),
        ToolPermissionDefault::Auto,
        RunnerToolEffectClass::Pure,
        ToolAdmissibleLoci::DaemonOrRunner {
            selector: RunnerSelector::CapabilityClass(class()),
        },
    );
    let deploy = RunnerToolDeclaration::new(
        tool("deploy"),
        model_definition("deploy"),
        ToolPermissionDefault::Confirm,
        RunnerToolEffectClass::SideEffecting,
        ToolAdmissibleLoci::RunnerOnly {
            selector: RunnerSelector::CapabilityClass(class()),
        },
    );
    let sync = RunnerToolDeclaration::new(
        tool("sync"),
        model_definition("sync"),
        ToolPermissionDefault::Confirm,
        RunnerToolEffectClass::Idempotent,
        ToolAdmissibleLoci::RunnerOnly {
            selector: RunnerSelector::CapabilityClass(class()),
        },
    );
    let readonly = CredentialProfilePolicy::try_new(
        profile("readonly"),
        [
            (tool("inspect"), CredentialToolApproval::Automatic),
            (tool("sync"), CredentialToolApproval::SessionPolicy),
        ],
    )
    .expect("the profile references a declared fixture tool");
    let admin = CredentialProfilePolicy::try_new(
        profile("admin"),
        [(tool("deploy"), CredentialToolApproval::SessionPolicy)],
    )
    .expect("the profile references a declared fixture tool");
    RunnerCatalog::try_new(
        [class()],
        [inspect, deploy, sync],
        [readonly, admin],
        [WorkspaceCapability::WorktreePerSession],
        sandbox_profiles(),
    )
    .expect("the canonical catalog is internally consistent")
}

fn enrollment_for(runner: RunnerId) -> RunnerEnrollment {
    RunnerEnrollment::new(
        runner_enrollment_id(ENROLLMENT),
        runner,
        runner_authentication_id(AUTHENTICATION),
        [class()],
    )
}

fn enrollment() -> RunnerEnrollment {
    enrollment_for(runner_id(RUNNER))
}

fn enrollment_for_registration(registration: &ValidatedRunnerRegistration) -> RunnerEnrollment {
    RunnerEnrollment {
        enrollment: registration.enrollment,
        runner: registration.runner,
        authentication: registration.authentication,
        allowed_classes: BTreeSet::from([class()]),
        state: RunnerEnrollmentState::Active,
        registration_revision: Arc::clone(&registration.current_revision),
        registration_active: Arc::clone(&registration.enrollment_active),
        registration_preparation: Arc::new(AtomicBool::new(false)),
    }
}

fn advertisement() -> RunnerAdvertisement {
    RunnerAdvertisement::new(
        [class()],
        [tool("inspect"), tool("deploy"), tool("sync")],
        [profile("readonly"), profile("admin")],
        [WorkspaceCapability::WorktreePerSession],
        sandbox_profiles(),
        [RunnerRepositoryEntry::new(repository_key(), None)],
    )
}

fn registration_for(runner: RunnerId) -> ValidatedRunnerRegistration {
    enrollment_for(runner)
        .register(advertisement(), &catalog())
        .expect("the advertisement is a subset of daemon policy")
}

fn registration() -> ValidatedRunnerRegistration {
    registration_for(runner_id(RUNNER))
}

fn placement_request(profile: CredentialProfileName) -> SessionRunnerPlacementRequest {
    SessionRunnerPlacementRequest {
        selector: RunnerSelector::CapabilityClass(class()),
        working_directory: WorkingDirectorySelection::RunnerDefault,
        credential_profile: Some(profile),
        workspace: WorkspaceRequirement::None,
        sandbox: RunnerSandboxProfile::Ambient,
        permission_overrides: no_permission_overrides(),
    }
}

fn profileless_placement_request() -> SessionRunnerPlacementRequest {
    SessionRunnerPlacementRequest {
        selector: RunnerSelector::CapabilityClass(class()),
        working_directory: WorkingDirectorySelection::RunnerDefault,
        credential_profile: None,
        workspace: WorkspaceRequirement::None,
        sandbox: RunnerSandboxProfile::Ambient,
        permission_overrides: no_permission_overrides(),
    }
}

fn exact_placement_request(runner: RunnerId) -> SessionRunnerPlacementRequest {
    SessionRunnerPlacementRequest {
        selector: RunnerSelector::Identity(runner),
        working_directory: WorkingDirectorySelection::Exact(directory("/workspace/session")),
        credential_profile: None,
        workspace: WorkspaceRequirement::None,
        sandbox: RunnerSandboxProfile::WorkspaceRestricted,
        permission_overrides: no_permission_overrides(),
    }
}

fn directory(value: &str) -> RunnerWorkingDirectory {
    RunnerWorkingDirectory::try_new(value.to_owned())
        .expect("fixture working directories are valid")
}

fn lease_offer_request(tool_name: &str) -> RunnerLeaseOfferRequest {
    RunnerLeaseOfferRequest {
        lease: runner_lease_id(LEASE),
        tool: tool(tool_name),
    }
}

fn request(tool_name: &str) -> ToolRequest {
    let request_seed = match tool_name {
        "inspect" => 0x7700,
        "sync" => 0x7701,
        "deploy" => 0x7702,
        _ => panic!("the fixture tool must be declared"),
    };
    ToolRequestReconstitutionInput::new(
        tool_request_id(request_seed),
        session_id(SESSION),
        turn_id(0x7800),
        model_call_id(0x7900),
        ToolRequestOrdinal::from_u32(0),
        tool(tool_name),
        NormalizedToolArguments::try_from_provider_text(String::from("{}"))
            .expect("fixture arguments are canonical"),
    )
    .into_request()
}

fn approved_request(tool_name: &str) -> ApprovedToolRequest {
    let request = request(tool_name);
    let request_seed = request.id().as_uuid().as_u128();
    let command = DecideToolRequest::new(
        DurableCommandId::from_uuid(uuid::Uuid::from_u128(request_seed + 0x300)),
        request.id(),
        ToolApprovalDecision::Approve,
    );
    let prepared = command
        .prepare_applied(&request)
        .expect("the fixture request and decision correlate");
    let crate::DecideToolRequestResult::Applied(applied) = prepared.result() else {
        panic!("the approving fixture decision applies")
    };
    ApprovedToolRequest::try_from_resolution(request, applied.resolution().clone())
        .expect("the fixture approval matches its request")
}

fn claimed_batch(tool_name: &str, effect: RunnerToolEffectClass) -> ToolBatch {
    claimed_batch_with_issuing_attempt(tool_name, effect, turn_attempt_id(0x7b00))
}

fn claimed_batch_with_issuing_attempt(
    tool_name: &str,
    effect: RunnerToolEffectClass,
    issuing_attempt: crate::TurnAttemptId,
) -> ToolBatch {
    let approved = approved_request(tool_name);
    let current = approved
        .prepare_attempt(
            tool_attempt_id(ATTEMPT),
            issuing_attempt,
            tool_effect_class(effect),
        )
        .authorize()
        .expect("the claimed fixture attempt authorizes once")
        .into_parts()
        .0;
    ToolBatchReconstitutionInput::new(
        session_id(SESSION),
        turn_id(0x7800),
        model_call_id(0x7900),
        ResolvedContextFrontierSnapshot::try_from_candidate(
            session_id(SESSION),
            context_frontier_id(YIELDED_FRONTIER),
            Vec::new(),
        )
        .expect("an empty fixture snapshot is valid"),
        vec![approved.request().clone()],
        vec![approved.approval().clone()],
        vec![ReconstitutedToolAttempt::Current(current)],
        ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: issuing_attempt,
        },
    )
    .reconstitute()
    .expect("the claimed fixture batch is complete")
}

fn current_attempt_id(batch: &ToolBatch, request: ToolRequestId) -> Option<ToolAttemptId> {
    batch.attempt(request).map(|attempt| match attempt {
        ReconstitutedToolAttempt::Current(current) => current.attempt(),
        ReconstitutedToolAttempt::Ended(ended) => ended.attempt(),
    })
}

fn current_attempt_effect_class(
    batch: &ToolBatch,
    request: ToolRequestId,
) -> Option<ToolEffectClass> {
    batch.attempt(request).map(|attempt| match attempt {
        ReconstitutedToolAttempt::Current(current) => current.effect_class(),
        ReconstitutedToolAttempt::Ended(ended) => ended.effect_class(),
    })
}

fn placement_grant_lineage(
    placement: &SessionRunnerPlacement,
) -> Option<RunnerCredentialGrantLineage> {
    match placement.state() {
        SessionRunnerPlacementState::Pinned(pinned) => pinned.grant_lineage,
        SessionRunnerPlacementState::RunnerLost(lost) => lost.pinned.grant_lineage,
        SessionRunnerPlacementState::RunnerAbandoned(AbandonedRunnerPlacement::Pinned(lost)) => {
            lost.pinned.grant_lineage
        }
        SessionRunnerPlacementState::Unpinned
        | SessionRunnerPlacementState::RunnerLostBeforePin(_)
        | SessionRunnerPlacementState::RunnerAbandoned(AbandonedRunnerPlacement::BeforePin(_)) => {
            None
        }
    }
}

fn automatically_approved_request(tool_name: &str) -> ApprovedToolRequest {
    let request = request(tool_name);
    let approval = ToolApprovalResolutionReconstitutionInput::policy_auto(request.id())
        .reconstitute()
        .expect("the fixture registry policy approves");
    ApprovedToolRequest::try_from_resolution(request, approval)
        .expect("the fixture approval matches its request")
}

fn blanket_approved_request(tool_name: &str) -> ApprovedToolRequest {
    let request = request(tool_name);
    let approval = ToolApprovalResolutionReconstitutionInput::session_blanket(
        request.id(),
        DangerousToolAutoApproval::ApproveAll,
    )
    .reconstitute()
    .expect("the fixture session blanket approves");
    ApprovedToolRequest::try_from_resolution(request, approval)
        .expect("the fixture approval matches its request")
}

fn authorized(
    tool_name: &str,
    attempt: ToolAttemptId,
    effect: RunnerToolEffectClass,
) -> RunnerToolAttemptAuthorization {
    let effect = match effect {
        RunnerToolEffectClass::Pure => ToolEffectClass::EffectFree,
        RunnerToolEffectClass::Idempotent | RunnerToolEffectClass::SideEffecting => {
            ToolEffectClass::ExternalEffect
        }
    };
    let approved = approved_request(tool_name);
    let authorized = approved
        .prepare_attempt(attempt, turn_attempt_id(0x7b00), effect)
        .authorize()
        .expect("the prepared fixture attempt authorizes once");
    RunnerToolAttemptAuthorization::try_new(approved, authorized)
        .expect("the approved request binds the authorized attempt")
}

fn automatically_authorized(
    tool_name: &str,
    attempt: ToolAttemptId,
    effect: RunnerToolEffectClass,
) -> RunnerToolAttemptAuthorization {
    let approved = automatically_approved_request(tool_name);
    let authorized = approved
        .prepare_attempt(attempt, turn_attempt_id(0x7b00), tool_effect_class(effect))
        .authorize()
        .expect("the prepared fixture attempt authorizes once");
    RunnerToolAttemptAuthorization::try_new(approved, authorized)
        .expect("the approved request binds the authorized attempt")
}

fn blanket_authorized(
    tool_name: &str,
    attempt: ToolAttemptId,
    effect: RunnerToolEffectClass,
) -> RunnerToolAttemptAuthorization {
    let approved = blanket_approved_request(tool_name);
    let authorized = approved
        .prepare_attempt(attempt, turn_attempt_id(0x7b00), tool_effect_class(effect))
        .authorize()
        .expect("the prepared fixture attempt authorizes once");
    RunnerToolAttemptAuthorization::try_new(approved, authorized)
        .expect("the approved request binds the authorized attempt")
}

fn user_override_approved_request(tool_name: &str) -> ApprovedToolRequest {
    const OVERRIDE_COMMAND: u128 = 0x7c00;
    const DENIED_REQUEST: u128 = 0x7c01;

    let request = request(tool_name);
    let approval = ToolApprovalResolutionReconstitutionInput::user_override(
        request.id(),
        DurableCommandId::from_uuid(uuid::Uuid::from_u128(OVERRIDE_COMMAND)),
        tool_request_id(DENIED_REQUEST),
        ToolApprovalPosture::Delegated,
    )
    .reconstitute()
    .expect("the fixture override consumes under the frozen delegated posture");
    ApprovedToolRequest::try_from_resolution(request, approval)
        .expect("the fixture approval matches its request")
}

fn user_override_authorized(
    tool_name: &str,
    attempt: ToolAttemptId,
    effect: RunnerToolEffectClass,
) -> RunnerToolAttemptAuthorization {
    let approved = user_override_approved_request(tool_name);
    let authorized = approved
        .prepare_attempt(attempt, turn_attempt_id(0x7b00), tool_effect_class(effect))
        .authorize()
        .expect("the prepared fixture attempt authorizes once");
    RunnerToolAttemptAuthorization::try_new(approved, authorized)
        .expect("the approved request binds the authorized attempt")
}

fn declared_effect(tool_name: &str) -> RunnerToolEffectClass {
    match tool_name {
        "inspect" => RunnerToolEffectClass::Pure,
        "sync" => RunnerToolEffectClass::Idempotent,
        "deploy" => RunnerToolEffectClass::SideEffecting,
        _ => panic!("the fixture tool must have a declared effect"),
    }
}

fn pinned(profile_name: &str) -> (ValidatedRunnerRegistration, SessionRunnerPin) {
    let registration = registration();
    let pin = SessionRunnerPlacement::new(
        session_id(SESSION),
        placement_request(profile(profile_name)),
    )
    .pin_and_offer_lease(
        &enrollment_for_registration(&registration),
        &registration,
        directory("/workspace/session"),
        None,
        authorized(
            "inspect",
            tool_attempt_id(ATTEMPT),
            RunnerToolEffectClass::Pure,
        ),
        lease_offer_request("inspect"),
    )
    .expect("the registration and authorized attempt satisfy placement");
    (registration, pin)
}

fn pinned_with_confirm_override(
    profile_name: &str,
) -> (ValidatedRunnerRegistration, SessionRunnerPin) {
    let registration = registration();
    let mut request = placement_request(profile(profile_name));
    request.permission_overrides = RunnerToolPermissionOverrides::try_new([(
        tool("inspect"),
        RunnerToolPermissionOverride::Confirm,
    )])
    .expect("the exact confirmation override is valid");
    let pin = SessionRunnerPlacement::new(session_id(SESSION), request)
        .pin_and_offer_lease(
            &enrollment_for_registration(&registration),
            &registration,
            directory("/workspace/session"),
            None,
            authorized(
                "inspect",
                tool_attempt_id(ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        )
        .expect("the confirmation override accepts an exact user decision");
    (registration, pin)
}

fn omit_runner_required_tool(placement: &mut SessionRunnerPlacement, omitted: &ToolName) {
    let SessionRunnerPlacementState::Pinned(stored) = &mut placement.state else {
        panic!("the fixture placement is pinned")
    };
    stored.runner_required_tools.remove(omitted);
}

fn offered(
    tool_name: &str,
    attempt: ToolAttemptId,
) -> (
    ValidatedRunnerRegistration,
    SessionRunnerPlacement,
    Option<CredentialProfileGrant>,
    RunnerLease,
) {
    let registration = registration();
    let pin =
        SessionRunnerPlacement::new(session_id(SESSION), placement_request(profile("readonly")))
            .pin_and_offer_lease(
                &enrollment_for_registration(&registration),
                &registration,
                directory("/workspace/session"),
                None,
                authorized(tool_name, attempt, declared_effect(tool_name)),
                lease_offer_request(tool_name),
            )
            .expect("the first authorized lease pins the fixture placement");
    (registration, pin.placement, pin.grant, pin.lease)
}

fn offered_from_batch(
    tool_name: &str,
) -> (
    ValidatedRunnerRegistration,
    SessionRunnerPlacement,
    Option<CredentialProfileGrant>,
    ToolBatch,
    RunnerLease,
) {
    let registration = registration();
    let batch = claimed_batch(tool_name, declared_effect(tool_name));
    let authorization = batch
        .resume_runner_attempt(tool_attempt_id(ATTEMPT))
        .expect("the owning batch issues the fixture runner authority once");
    let pin =
        SessionRunnerPlacement::new(session_id(SESSION), placement_request(profile("readonly")))
            .pin_and_offer_lease(
                &enrollment_for_registration(&registration),
                &registration,
                directory("/workspace/session"),
                None,
                authorization,
                lease_offer_request(tool_name),
            )
            .expect("batch-issued authority pins the fixture placement");
    (registration, pin.placement, pin.grant, batch, pin.lease)
}

fn placement_without_required_tool(
    mut placement: SessionRunnerPlacement,
    missing: &ToolName,
) -> SessionRunnerPlacement {
    let SessionRunnerPlacementState::Pinned(pinned) = &mut placement.state else {
        panic!("the fixture placement must be pinned")
    };
    pinned.runner_required_tools.remove(missing);
    placement
}

fn placement_without_grant_lineage(
    mut placement: SessionRunnerPlacement,
) -> SessionRunnerPlacement {
    let SessionRunnerPlacementState::Pinned(pinned) = &mut placement.state else {
        panic!("the fixture placement must be pinned")
    };
    pinned.grant_lineage = None;
    placement
}

fn placement_with_grant_lineage(
    mut placement: SessionRunnerPlacement,
    lineage: RunnerCredentialGrantLineage,
) -> SessionRunnerPlacement {
    let SessionRunnerPlacementState::Pinned(pinned) = &mut placement.state else {
        panic!("the fixture placement must be pinned")
    };
    pinned.grant_lineage = Some(lineage);
    placement
}

fn enrollment_reconstitution_input() -> RunnerEnrollmentReconstitutionInput {
    RunnerEnrollmentReconstitutionInput {
        enrollment: runner_enrollment_id(ENROLLMENT),
        recorded_enrollment: runner_enrollment_id(ENROLLMENT),
        runner: runner_id(RUNNER),
        recorded_runner: runner_id(RUNNER),
        authentication: runner_authentication_id(AUTHENTICATION),
        recorded_authentication: runner_authentication_id(AUTHENTICATION),
        allowed_classes: BTreeSet::from([class()]),
        recorded_allowed_classes: BTreeSet::from([class()]),
        registration_revision: None,
        recorded_registration_revision: None,
        state: RunnerEnrollmentState::Active,
        recorded_state: RunnerEnrollmentState::Active,
    }
}

fn lease_reconstitution_input(lease: RunnerLease) -> RunnerLeaseReconstitutionInput {
    RunnerLeaseReconstitutionInput {
        lease: lease.lease,
        dispatch: lease.dispatch,
        runner: lease.runner,
        tool: lease.tool.clone(),
        effect: lease.effect,
        credential_authorization: lease.credential_authorization.clone(),
        generation: lease.generation,
        state: lease.state,
        recorded_correlation: lease.correlation(),
        recorded_session: lease.dispatch.session(),
        recorded_effect: lease.effect,
        recorded_credential_authorization: lease.credential_authorization.clone(),
        recorded_state: lease.state,
        retry_preparation: RunnerLeaseRetryPreparation::Available,
    }
}

fn borrowed_lease_reconstitution_input(lease: &RunnerLease) -> RunnerLeaseReconstitutionInput {
    RunnerLeaseReconstitutionInput {
        lease: lease.lease,
        dispatch: lease.dispatch,
        runner: lease.runner,
        tool: lease.tool.clone(),
        effect: lease.effect,
        credential_authorization: lease.credential_authorization.clone(),
        generation: lease.generation,
        state: lease.state,
        recorded_correlation: lease.correlation(),
        recorded_session: lease.dispatch.session(),
        recorded_effect: lease.effect,
        recorded_credential_authorization: lease.credential_authorization.clone(),
        recorded_state: lease.state,
        retry_preparation: RunnerLeaseRetryPreparation::Available,
    }
}

fn no_execution_proof(lease: &RunnerLease) -> RunnerLeaseNoExecutionProof {
    RunnerLeaseNoExecutionProof {
        correlation: lease.correlation(),
    }
}

fn placement_reconstitution_input(
    placement: SessionRunnerPlacement,
) -> SessionRunnerPlacementReconstitutionInput {
    SessionRunnerPlacementReconstitutionInput {
        session: placement.session,
        revision: placement.revision,
        request: placement.request,
        state: placement.state,
        history: RunnerPlacementReconstitutionHistory::Initial,
    }
}

fn grant_reconstitution_input(
    grant: CredentialProfileGrant,
) -> CredentialProfileGrantReconstitutionInput {
    CredentialProfileGrantReconstitutionInput {
        session: grant.session,
        runner: grant.runner,
        revision: grant.revision,
        profile: grant.profile,
        tools: grant.tools,
        approvals: grant.approvals,
        state: grant.state,
    }
}

#[test]
fn runner_catalog_names_are_portable_and_bounded() {
    assert_eq!(
        RunnerCapabilityClass::try_new("-leading".to_owned()),
        Err(RunnerDomainError::InvalidName)
    );
    assert_eq!(
        CredentialProfileName::try_new("contains space".to_owned()),
        Err(RunnerDomainError::InvalidName)
    );
    assert_eq!(
        RunnerCapabilityClass::try_new("x".repeat(NAME_MAX_BYTES + 1)),
        Err(RunnerDomainError::TooLong)
    );
}

#[test]
fn clone_url_digest_rejects_nonhex_text() {
    assert_eq!(
        CanonicalCloneUrlDigest::try_new("g".repeat(64)),
        Err(RunnerDomainError::InvalidHex)
    );
}

#[test]
fn clone_url_digest_rejects_wrong_length() {
    assert_eq!(
        CanonicalCloneUrlDigest::try_new("a".repeat(65)),
        Err(RunnerDomainError::InvalidHex)
    );
}

#[test]
fn workspace_revision_rejects_abbreviated_or_overlong_object_ids() {
    assert_eq!(
        WorkspaceRevision::try_new("a".repeat(39)),
        Err(RunnerDomainError::InvalidHex)
    );
    assert_eq!(
        WorkspaceRevision::try_new("a".repeat(41)),
        Err(RunnerDomainError::InvalidHex)
    );
}

#[test]
fn workspace_revision_rejects_uppercase_object_id() {
    assert_eq!(
        WorkspaceRevision::try_new("A".repeat(40)),
        Err(RunnerDomainError::InvalidHex)
    );
}

#[test]
fn workspace_branch_rejects_dot_dot() {
    assert_eq!(
        WorkspaceBranchName::try_new("bad..branch".to_owned()),
        Err(RunnerDomainError::InvalidBranchName)
    );
}

#[test]
fn workspace_branch_rejects_lock_suffix() {
    assert_eq!(
        WorkspaceBranchName::try_new("component.lock".to_owned()),
        Err(RunnerDomainError::InvalidBranchName)
    );
}

#[test]
fn workspace_branch_rejects_reflog_syntax() {
    assert_eq!(
        WorkspaceBranchName::try_new("bad@{branch".to_owned()),
        Err(RunnerDomainError::InvalidBranchName)
    );
}

#[test]
fn workspace_branch_rejects_single_at() {
    assert!(matches!(
        WorkspaceBranchName::try_new("@".to_owned()),
        Err(RunnerDomainError::InvalidBranchName)
    ));
}

#[test]
fn workspace_branch_accepts_closing_bracket() {
    assert!(WorkspaceBranchName::try_new("topic]ok".to_owned()).is_ok());
}

#[test]
fn workspace_relative_path_rejects_absolute_value() {
    assert_eq!(
        WorkspaceRelativePath::try_new("/sessions/one".to_owned()),
        Err(RunnerDomainError::InvalidRelativePath)
    );
}

#[test]
fn workspace_relative_path_rejects_parent_traversal() {
    assert_eq!(
        WorkspaceRelativePath::try_new("sessions/../one".to_owned()),
        Err(RunnerDomainError::InvalidRelativePath)
    );
}

#[test]
fn workspace_relative_path_rejects_empty_component() {
    assert_eq!(
        WorkspaceRelativePath::try_new("sessions//one".to_owned()),
        Err(RunnerDomainError::InvalidRelativePath)
    );
}

#[test]
fn permission_overrides_reject_duplicate_tools() {
    assert_eq!(
        RunnerToolPermissionOverrides::try_new([
            (tool("inspect"), RunnerToolPermissionOverride::Auto),
            (tool("inspect"), RunnerToolPermissionOverride::Confirm),
        ]),
        Err(RunnerDomainError::DuplicateTool(tool("inspect")))
    );
}

#[test]
fn permission_overrides_reject_more_than_sixty_four_tools() {
    assert_eq!(
        RunnerToolPermissionOverrides::try_new((0..=PERMISSION_OVERRIDE_MAX_ENTRIES).map(
            |index| (
                tool(&format!("tool_{index}")),
                RunnerToolPermissionOverride::Auto,
            )
        ),),
        Err(RunnerDomainError::TooManyPermissionOverrides)
    );
}

#[test]
fn runner_tool_model_definition_requires_a_json_object_schema() {
    assert_eq!(
        RunnerToolModelDefinition::try_new("Inspect the workspace".to_owned(), "[]".to_owned()),
        Err(RunnerDomainError::InvalidToolInputSchema)
    );
}

#[test]
fn workspace_repository_keys_use_the_catalog_name_contract() {
    let accepted = WorkspaceRepositoryKey::try_new("r".repeat(NAME_MAX_BYTES))
        .expect("the catalog-name maximum is accepted");

    assert_eq!(accepted.as_str().len(), NAME_MAX_BYTES);
    assert_eq!(
        WorkspaceRepositoryKey::try_new("r".repeat(NAME_MAX_BYTES + 1)),
        Err(RunnerDomainError::TooLong)
    );
    assert_eq!(
        WorkspaceRepositoryKey::try_new("contains space".to_owned()),
        Err(RunnerDomainError::InvalidName)
    );
}

#[test]
fn catalog_rejects_duplicate_capability_class() {
    assert_eq!(
        RunnerCatalog::try_new([class(), class()], [], [], [], []),
        Err(RunnerDomainError::DuplicateCapabilityClass(class()))
    );
}

#[test]
fn catalog_rejects_duplicate_sandbox_profile() {
    assert_eq!(
        RunnerCatalog::try_new(
            [],
            [],
            [],
            [],
            [RunnerSandboxProfile::Ambient, RunnerSandboxProfile::Ambient],
        ),
        Err(RunnerDomainError::DuplicateSandboxProfile(
            RunnerSandboxProfile::Ambient
        ))
    );
}

#[test]
fn catalog_rejects_duplicate_workspace_capability() {
    assert_eq!(
        RunnerCatalog::try_new(
            [],
            [],
            [],
            [
                WorkspaceCapability::WorktreePerSession,
                WorkspaceCapability::WorktreePerSession,
            ],
            [],
        ),
        Err(RunnerDomainError::DuplicateWorkspaceCapability(
            WorkspaceCapability::WorktreePerSession
        ))
    );
}

#[test]
fn unknown_advertised_tool_rejects_the_complete_registration() {
    let enrollment = RunnerEnrollment::new(
        runner_enrollment_id(ENROLLMENT),
        runner_id(RUNNER),
        runner_authentication_id(AUTHENTICATION),
        [class()],
    );
    let advertisement = RunnerAdvertisement::new([class()], [tool("unknown")], [], [], [], []);

    assert_eq!(
        enrollment.register(advertisement, &catalog()),
        Err(RunnerDomainError::ToolUndeclared(tool("unknown")))
    );
}

#[test]
fn catalog_rejects_tool_selector_for_undeclared_class() {
    let declaration = RunnerToolDeclaration::new(
        tool("specialized"),
        model_definition("specialized"),
        ToolPermissionDefault::Auto,
        RunnerToolEffectClass::Pure,
        ToolAdmissibleLoci::RunnerOnly {
            selector: RunnerSelector::CapabilityClass(class()),
        },
    );

    assert_eq!(
        RunnerCatalog::try_new([], [declaration], [], [], []),
        Err(RunnerDomainError::CapabilityClassNotAllowed(class()))
    );
}

#[test]
fn catalog_rejects_idempotent_tool_with_daemon_locus() {
    let declaration = RunnerToolDeclaration::new(
        tool("sync"),
        model_definition("sync"),
        ToolPermissionDefault::Confirm,
        RunnerToolEffectClass::Idempotent,
        ToolAdmissibleLoci::DaemonOrRunner {
            selector: RunnerSelector::CapabilityClass(class()),
        },
    );

    assert_eq!(
        RunnerCatalog::try_new([class()], [declaration], [], [], []),
        Err(RunnerDomainError::UnsupportedDaemonIdempotency(tool(
            "sync"
        )))
    );
}

#[test]
fn advertised_class_requires_enrollment_and_catalog_authority() {
    let enrollment = RunnerEnrollment::new(
        runner_enrollment_id(ENROLLMENT),
        runner_id(RUNNER),
        runner_authentication_id(AUTHENTICATION),
        [class()],
    );
    let catalog = RunnerCatalog::try_new([], [], [], [], [])
        .expect("the empty catalog is internally consistent");
    let advertisement = RunnerAdvertisement::new([class()], [], [], [], [], []);

    assert_eq!(
        enrollment.register(advertisement, &catalog),
        Err(RunnerDomainError::CapabilityClassNotAllowed(class()))
    );
}

#[test]
fn daemon_only_tool_rejects_the_complete_registration() {
    let enrollment = RunnerEnrollment::new(
        runner_enrollment_id(ENROLLMENT),
        runner_id(RUNNER),
        runner_authentication_id(AUTHENTICATION),
        [class()],
    );
    let daemon_only = RunnerToolDeclaration::new(
        tool("daemon"),
        model_definition("daemon"),
        ToolPermissionDefault::Auto,
        RunnerToolEffectClass::Pure,
        ToolAdmissibleLoci::DaemonOnly,
    );
    let catalog = RunnerCatalog::try_new([], [daemon_only], [], [], [])
        .expect("the daemon-only declaration is internally consistent");
    let advertisement = RunnerAdvertisement::new([], [tool("daemon")], [], [], [], []);

    assert_eq!(
        enrollment.register(advertisement, &catalog),
        Err(RunnerDomainError::ToolLocusNotAllowed(tool("daemon")))
    );
}

#[test]
fn tool_selector_must_match_advertised_runner_capability() {
    let enrollment = RunnerEnrollment::new(
        runner_enrollment_id(ENROLLMENT),
        runner_id(RUNNER),
        runner_authentication_id(AUTHENTICATION),
        [class()],
    );
    let declaration = RunnerToolDeclaration::new(
        tool("specialized"),
        model_definition("specialized"),
        ToolPermissionDefault::Auto,
        RunnerToolEffectClass::Pure,
        ToolAdmissibleLoci::RunnerOnly {
            selector: RunnerSelector::Identity(runner_id(REPLACEMENT_RUNNER)),
        },
    );
    let catalog = RunnerCatalog::try_new([], [declaration], [], [], [])
        .expect("the identity-targeted declaration is internally consistent");
    let advertisement = RunnerAdvertisement::new([], [tool("specialized")], [], [], [], []);

    assert_eq!(
        enrollment.register(advertisement, &catalog),
        Err(RunnerDomainError::ToolLocusNotAllowed(tool("specialized")))
    );
}

#[test]
fn registration_rejects_unadvertised_sandbox_profile() {
    let advertisement =
        RunnerAdvertisement::new([class()], [], [], [], [RunnerSandboxProfile::Ambient], []);
    let restricted_catalog = RunnerCatalog::try_new(
        [class()],
        [],
        [],
        [],
        [RunnerSandboxProfile::WorkspaceRestricted],
    )
    .expect("the restricted catalog is internally consistent");

    assert_eq!(
        enrollment().register(advertisement, &restricted_catalog),
        Err(RunnerDomainError::SandboxProfileNotAllowed(
            RunnerSandboxProfile::Ambient
        ))
    );
}

#[test]
fn registration_rejects_repository_profile_outside_advertisement() {
    let advertisement = RunnerAdvertisement::new(
        [class()],
        [],
        [],
        [],
        [],
        [RunnerRepositoryEntry::new(
            repository_key(),
            Some(profile("readonly")),
        )],
    );

    assert_eq!(
        enrollment().register(advertisement, &catalog()),
        Err(RunnerDomainError::RepositoryProfileUnavailable(profile(
            "readonly"
        )))
    );
}

#[test]
fn registration_rejects_oversized_repository_inventory() {
    let repositories = (0..=RunnerAdvertisement::MAX_REPOSITORIES).map(|index| {
        RunnerRepositoryEntry::new(
            WorkspaceRepositoryKey::try_new(format!("repository_{index}"))
                .expect("the generated repository key is valid"),
            None,
        )
    });
    let advertisement = RunnerAdvertisement::new([class()], [], [], [], [], repositories);

    assert_eq!(
        enrollment().register(advertisement, &catalog()),
        Err(RunnerDomainError::TooManyAdvertisedRepositories)
    );
}

#[test]
fn revoked_enrollment_cannot_register() {
    let enrollment = RunnerEnrollment::new(
        runner_enrollment_id(ENROLLMENT),
        runner_id(RUNNER),
        runner_authentication_id(AUTHENTICATION),
        [class()],
    )
    .revoke()
    .expect("an active enrollment can be revoked");

    assert_eq!(
        enrollment.register(RunnerAdvertisement::new([], [], [], [], [], []), &catalog()),
        Err(RunnerDomainError::EnrollmentRevoked)
    );
}

#[test]
fn outstanding_preparation_excludes_concurrent_registration() {
    let enrollment = enrollment();
    let outstanding = enrollment
        .prepare_registration(advertisement(), &catalog())
        .expect("the pristine enrollment prepares its first registration");

    assert_eq!(
        enrollment.register(advertisement(), &catalog()),
        Err(RunnerDomainError::RegistrationInProgress)
    );
    drop(outstanding);
    let registration = enrollment
        .register(advertisement(), &catalog())
        .expect("an abandoned preparation releases the exclusive fence");
    assert_eq!(registration.revision(), RunnerGeneration::one());
}

#[test]
fn committed_preparation_releases_the_exclusive_fence() {
    let enrollment = enrollment();
    let first = enrollment
        .prepare_registration(advertisement(), &catalog())
        .expect("the pristine enrollment prepares its first registration")
        .commit()
        .expect("the sole outstanding preparation commits");

    let second = enrollment
        .register(advertisement(), &catalog())
        .expect("a committed preparation releases the exclusive fence");
    assert_eq!(Some(second.revision()), first.revision().checked_next());
}

#[test]
fn enrollment_reports_its_last_issued_registration_revision() {
    let enrollment = enrollment();
    assert_eq!(enrollment.last_issued_registration_revision(), None);

    let registration = enrollment
        .register(advertisement(), &catalog())
        .expect("the pristine enrollment issues its first registration");
    assert_eq!(
        enrollment.last_issued_registration_revision(),
        Some(registration.revision())
    );
}

#[test]
fn revoked_enrollment_cannot_authorize_a_later_lease() {
    let (registration, pin) = pinned("readonly");
    let revoked = enrollment()
        .revoke()
        .expect("an active enrollment can be revoked");

    assert_eq!(
        pin.placement.offer_lease(
            &revoked,
            &registration,
            pin.grant.as_ref(),
            authorized(
                "inspect",
                tool_attempt_id(ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        ),
        Err(RunnerDomainError::EnrollmentRevoked)
    );
}

#[test]
fn revocation_invalidates_registration_for_grant_transition() {
    let enrollment = enrollment();
    let registration = enrollment
        .register(advertisement(), &catalog())
        .expect("the active enrollment issues a registration");
    let mut pin =
        SessionRunnerPlacement::new(session_id(SESSION), placement_request(profile("readonly")))
            .pin_and_offer_lease(
                &enrollment,
                &registration,
                directory("/workspace/session"),
                None,
                authorized(
                    "inspect",
                    tool_attempt_id(ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            )
            .expect("the active registration pins its runner");
    let grant = pin.grant.take().expect("profile selection creates a grant");
    let _revoked = enrollment
        .revoke()
        .expect("revocation invalidates retained registration authority");

    assert_eq!(
        pin.placement.replace_credential_profile(
            grant,
            &registration,
            profile("admin"),
            BTreeSet::from([tool("deploy")]),
        ),
        Err(RunnerDomainError::RegistrationChanged)
    );
}

#[test]
fn lease_rejects_a_foreign_active_enrollment() {
    let (registration, pin) = pinned("readonly");
    let foreign = enrollment_for(runner_id(REPLACEMENT_RUNNER));

    assert_eq!(
        pin.placement.offer_lease(
            &foreign,
            &registration,
            pin.grant.as_ref(),
            authorized(
                "inspect",
                tool_attempt_id(ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        ),
        Err(RunnerDomainError::CorrelationMismatch)
    );
}

#[test]
fn registration_attaches_daemon_policy_not_runner_policy() {
    let registration = registration();
    let declaration = registration
        .tool(&tool("deploy"))
        .expect("the advertised tool is validated");

    assert_eq!(declaration.permission(), ToolPermissionDefault::Confirm);
    assert_eq!(declaration.effect(), RunnerToolEffectClass::SideEffecting);
}

#[test]
fn reregistration_retires_prior_registration_authority() {
    let enrollment = enrollment();
    let initial = enrollment
        .register(advertisement(), &catalog())
        .expect("the initial advertisement is valid");
    let retained = initial.clone();
    let pin =
        SessionRunnerPlacement::new(session_id(SESSION), placement_request(profile("readonly")))
            .pin_and_offer_lease(
                &enrollment,
                &initial,
                directory("/workspace/session"),
                None,
                authorized(
                    "inspect",
                    tool_attempt_id(ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            )
            .expect("the initial registration pins the placement");
    let current = enrollment
        .register(advertisement(), &catalog())
        .expect("the replacement advertisement is valid");

    assert_eq!(
        pin.placement.offer_lease(
            &enrollment,
            &retained,
            pin.grant.as_ref(),
            authorized(
                "inspect",
                tool_attempt_id(RETRY_ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        ),
        Err(RunnerDomainError::RegistrationChanged)
    );
    assert_ne!(retained.revision(), current.revision());
}

#[test]
fn enrollment_reconstitution_rejects_cross_wired_runner() {
    let mut input = enrollment_reconstitution_input();
    input.recorded_runner = runner_id(REPLACEMENT_RUNNER);

    assert_eq!(
        RunnerEnrollment::reconstitute(input),
        Err(RunnerDomainError::CorruptStoredFacts)
    );
}

#[test]
fn enrollment_reconstitution_rejects_cross_wired_class_inventory() {
    let mut input = enrollment_reconstitution_input();
    input.recorded_allowed_classes = BTreeSet::new();

    assert_eq!(
        RunnerEnrollment::reconstitute(input),
        Err(RunnerDomainError::CorruptStoredFacts)
    );
}

#[test]
fn enrollment_reconstitution_rejects_cross_wired_state() {
    let mut input = enrollment_reconstitution_input();
    input.recorded_state = RunnerEnrollmentState::Revoked;

    assert_eq!(
        RunnerEnrollment::reconstitute(input),
        Err(RunnerDomainError::CorruptStoredFacts)
    );
}

#[test]
fn enrollment_reconstitution_restores_registration_revision() {
    let mut input = enrollment_reconstitution_input();
    let restored_revision = RunnerGeneration::try_from_u64(2).expect("two is positive");
    input.registration_revision = Some(restored_revision);
    input.recorded_registration_revision = Some(restored_revision);
    let enrollment = RunnerEnrollment::reconstitute(input)
        .expect("the complete enrollment facts restore the registration counter");

    let registration = enrollment
        .register(advertisement(), &catalog())
        .expect("the next registration advances the restored counter");

    assert_eq!(
        registration.revision(),
        RunnerGeneration::try_from_u64(3).expect("three is positive")
    );
}

#[test]
fn enrollment_reconstitution_rejects_cross_wired_registration_revision() {
    let mut input = enrollment_reconstitution_input();
    input.registration_revision = Some(RunnerGeneration::one());
    input.recorded_registration_revision =
        Some(RunnerGeneration::try_from_u64(2).expect("two is positive"));

    assert_eq!(
        RunnerEnrollment::reconstitute(input),
        Err(RunnerDomainError::CorruptStoredFacts)
    );
}

#[test]
fn unclaimed_side_effecting_loss_is_releasable() {
    let attempt = tool_attempt_id(ATTEMPT);
    let (registration, placement, grant, batch, lease) = offered_from_batch("deploy");
    let preexisting_clone = batch.clone();
    let proof = no_execution_proof(&lease);
    let loss = lease
        .lose_unclaimed(&proof)
        .expect("proof-backed unclaimed loss is checked");
    let prepared = loss
        .retry()
        .expect("unclaimed loss carries retry authority")
        .prepare_unclaimed_attempt(batch)
        .expect("the owning batch reauthorizes the never-executed attempt");
    let (retry_batch, authorization) = prepared.into_parts();
    let replacement = placement
        .offer_retry(
            &enrollment_for_registration(&registration),
            &registration,
            grant.as_ref(),
            loss,
            authorization,
        )
        .expect("unclaimed loss retains its never-executed attempt");
    let duplicate = retry_batch
        .resume_runner_attempt(attempt)
        .expect_err("the reissued runner authority remains single-use");
    let clone_duplicate = preexisting_clone
        .resume_runner_attempt(attempt)
        .expect_err("a preexisting clone shares the reissued single-use fence");
    let retained_authorized_attempts = preexisting_clone
        .runner_authorized_attempts()
        .collect::<Vec<_>>();

    assert_eq!(
        replacement.generation(),
        RunnerGeneration::try_from_u64(2).expect("two is positive")
    );
    assert_eq!(replacement.attempt(), attempt);
    assert_eq!(retained_authorized_attempts, vec![attempt]);
    assert_eq!(
        duplicate.failure(),
        ToolBatchExecutionFailure::AttemptStageMismatch
    );
    assert_eq!(
        clone_duplicate.failure(),
        ToolBatchExecutionFailure::AttemptStageMismatch
    );
}

#[test]
fn unclaimed_retry_preparation_is_single_use_across_batch_copies() {
    let (_, _, _, batch, lease) = offered_from_batch("deploy");
    let retained_batch = batch.clone();
    let proof = no_execution_proof(&lease);
    let loss = lease
        .lose_unclaimed(&proof)
        .expect("proof-backed unclaimed loss is checked");
    let _prepared = loss
        .retry()
        .expect("unclaimed loss carries retry authority")
        .prepare_unclaimed_attempt(batch)
        .expect("the first batch copy consumes retry preparation authority");

    assert_eq!(
        loss.retry()
            .expect("the loss still exposes its checked lineage")
            .prepare_unclaimed_attempt(retained_batch),
        Err(RunnerDomainError::InvalidState)
    );
}

#[test]
fn unclaimed_retry_authority_is_rejected_by_ordinary_offer() {
    let (registration, placement, grant, batch, lease) = offered_from_batch("deploy");
    let proof = no_execution_proof(&lease);
    let loss = lease
        .lose_unclaimed(&proof)
        .expect("proof-backed unclaimed loss is checked");
    let prepared = loss
        .retry()
        .expect("unclaimed loss carries retry authority")
        .prepare_unclaimed_attempt(batch)
        .expect("the owning batch reauthorizes the never-executed attempt");
    let (_, authorization) = prepared.into_parts();

    assert_eq!(
        placement.offer_lease(
            &enrollment_for_registration(&registration),
            &registration,
            grant.as_ref(),
            authorization,
            lease_offer_request("deploy"),
        ),
        Err(RunnerDomainError::CorrelationMismatch)
    );
}

#[test]
fn offered_side_effecting_loss_without_proof_is_ambiguous() {
    let (_, _, _, offered) = offered("deploy", tool_attempt_id(ATTEMPT));
    let expected_attempt = offered.attempt();

    let loss = offered
        .lose()
        .expect("loss without no-execution proof stays ambiguous");

    assert_eq!(loss.retry(), None);
    assert_eq!(loss.crash_attempt(), Some(expected_attempt));
}

#[test]
fn claimed_pure_retry_requires_fresh_physical_attempt() {
    let expected_tool = tool("inspect");
    let retry_attempt = tool_attempt_id(RETRY_ATTEMPT);
    let (registration, placement, grant, offered) = offered("inspect", tool_attempt_id(ATTEMPT));
    let offered_attempt = offered.attempt();
    let correlation = offered.correlation();
    let claimed = offered
        .claim(correlation)
        .expect("the exact fence claims the offered lease");

    let loss = claimed.lose().expect("a claimed lease can be lost");
    let prepared = loss
        .retry()
        .expect("claimed pure work carries retry authority")
        .prepare_claimed_attempt(
            claimed_batch("inspect", RunnerToolEffectClass::Pure),
            retry_attempt,
        )
        .expect("retry authority produces a fresh physical attempt");
    let (retry_batch, retired_attempt, authorization) = prepared.into_parts();
    let replacement = placement
        .offer_retry(
            &enrollment_for_registration(&registration),
            &registration,
            grant.as_ref(),
            loss,
            authorization,
        )
        .expect("pure claimed work permits a fresh physical attempt");
    let duplicate_local_authority = retry_batch
        .resume_in_flight_attempt(retry_attempt)
        .expect("the replacement remains locally resumable");
    let duplicate_approved = approved_request("inspect");

    assert_eq!(
        RunnerToolAttemptAuthorization::try_new(duplicate_approved, duplicate_local_authority),
        Err(RunnerDomainError::InvalidState)
    );
    assert_eq!(retired_attempt.attempt(), offered_attempt);
    assert_eq!(
        retired_attempt.end(),
        &ToolAttemptEnd::KnownFailed {
            error: crate::ToolExecutionError::new(ToolExecutionErrorKind::CrashLost, None),
        }
    );
    assert_eq!(
        current_attempt_id(&retry_batch, request("inspect").id()),
        Some(retry_attempt)
    );
    assert_eq!(replacement.attempt(), retry_attempt);
    assert_eq!(replacement.tool(), &expected_tool);
}

#[test]
fn claimed_retry_rejects_cross_wired_lost_lease_correlation() {
    let (registration, placement, grant, offered) = offered("inspect", tool_attempt_id(ATTEMPT));
    let correlation = offered.correlation();
    let claimed = offered
        .claim(correlation)
        .expect("the exact first lease is claimed");
    let source_loss = claimed.lose().expect("the claimed pure lease may be lost");
    let prepared = source_loss
        .retry()
        .expect("the source loss carries retry authority")
        .prepare_claimed_attempt(
            claimed_batch("inspect", RunnerToolEffectClass::Pure),
            tool_attempt_id(RETRY_ATTEMPT),
        )
        .expect("the source loss prepares its exact replacement");
    let (_, _, authorization) = prepared.into_parts();
    let mut cross_wired_input = borrowed_lease_reconstitution_input(source_loss.lost());
    cross_wired_input.lease = runner_lease_id(LEASE + 1);
    cross_wired_input.recorded_correlation.lease = runner_lease_id(LEASE + 1);
    let cross_wired_loss = RunnerLease::reconstitute_loss(cross_wired_input, &registration, None)
        .expect("the distinct complete loss is internally consistent");

    assert_eq!(
        placement.offer_retry(
            &enrollment_for_registration(&registration),
            &registration,
            grant.as_ref(),
            cross_wired_loss,
            authorization,
        ),
        Err(RunnerDomainError::CorrelationMismatch)
    );
}

#[test]
fn later_retry_rejects_retired_attempt_identity() {
    let (registration, placement, grant, offered) = offered("inspect", tool_attempt_id(ATTEMPT));
    let retired_identity = offered.attempt();
    let first_correlation = offered.correlation();
    let first_claimed = offered
        .claim(first_correlation)
        .expect("the exact first fence claims");
    let first_loss = first_claimed.lose().expect("the first claim may be lost");
    let prepared = first_loss
        .retry()
        .expect("the first loss carries retry authority")
        .prepare_claimed_attempt(
            claimed_batch("inspect", RunnerToolEffectClass::Pure),
            tool_attempt_id(RETRY_ATTEMPT),
        )
        .expect("the first retry replaces the claimed physical attempt");
    let (retry_batch, _, authorization) = prepared.into_parts();
    let retry_lease = placement
        .offer_retry(
            &enrollment_for_registration(&registration),
            &registration,
            grant.as_ref(),
            first_loss,
            authorization,
        )
        .expect("the checked first replacement offers its successor lease");
    let retry_correlation = retry_lease.correlation();
    let retry_claimed = retry_lease
        .claim(retry_correlation)
        .expect("the exact retry fence claims");
    let retry_loss = retry_claimed
        .lose()
        .expect("the claimed retry may itself be lost");

    assert_eq!(
        retry_loss
            .retry()
            .expect("the later loss carries retry authority")
            .prepare_claimed_attempt(retry_batch, retired_identity),
        Err(RunnerDomainError::AttemptIdentityReuse),
    );
}

#[test]
fn claimed_retry_rejects_attempt_identity_reuse() {
    let (_, _, _, offered) = offered("sync", tool_attempt_id(ATTEMPT));
    let correlation = offered.correlation();
    let claimed = offered
        .claim(correlation)
        .expect("the exact fence claims the offered lease");

    let loss = claimed.lose().expect("a claimed lease can be lost");

    assert_eq!(
        loss.retry()
            .expect("claimed idempotent work carries retry authority")
            .prepare_claimed_attempt(
                claimed_batch("sync", RunnerToolEffectClass::Idempotent),
                tool_attempt_id(ATTEMPT),
            ),
        Err(RunnerDomainError::AttemptIdentityReuse)
    );
}

#[test]
fn claimed_retry_rejects_a_different_request() {
    let (_, _, _, offered) = offered("sync", tool_attempt_id(ATTEMPT));
    let correlation = offered.correlation();
    let claimed = offered
        .claim(correlation)
        .expect("the exact fence claims the offered lease");
    let loss = claimed.lose().expect("a claimed lease can be lost");

    assert_eq!(
        loss.retry()
            .expect("claimed idempotent work carries retry authority")
            .prepare_claimed_attempt(
                claimed_batch("inspect", RunnerToolEffectClass::Pure),
                tool_attempt_id(RETRY_ATTEMPT),
            ),
        Err(RunnerDomainError::CorrelationMismatch)
    );
}

#[test]
fn claimed_retry_rejects_cross_wired_issuing_attempt() {
    let (_, _, _, offered) = offered("sync", tool_attempt_id(ATTEMPT));
    let correlation = offered.correlation();
    let claimed = offered
        .claim(correlation)
        .expect("the exact fence claims the offered lease");
    let loss = claimed.lose().expect("a claimed lease can be lost");
    let cross_wired = claimed_batch_with_issuing_attempt(
        "sync",
        RunnerToolEffectClass::Idempotent,
        turn_attempt_id(0x7b01),
    );

    assert_eq!(
        loss.retry()
            .expect("claimed idempotent work carries retry authority")
            .prepare_claimed_attempt(cross_wired, tool_attempt_id(RETRY_ATTEMPT)),
        Err(RunnerDomainError::CorrelationMismatch)
    );
}

#[test]
fn unclaimed_retry_cannot_mint_a_fresh_attempt() {
    let (_, _, _, offered) = offered("inspect", tool_attempt_id(ATTEMPT));
    let proof = no_execution_proof(&offered);
    let loss = offered
        .lose_unclaimed(&proof)
        .expect("proof-backed unclaimed loss is checked");

    assert_eq!(
        loss.retry()
            .expect("unclaimed pure work carries retry authority")
            .prepare_claimed_attempt(
                claimed_batch("inspect", RunnerToolEffectClass::Pure),
                tool_attempt_id(RETRY_ATTEMPT),
            ),
        Err(RunnerDomainError::InvalidState)
    );
}

#[test]
fn claimed_retry_authority_preserves_effect_class() {
    let (_, _, _, offered) = offered("sync", tool_attempt_id(ATTEMPT));
    let correlation = offered.correlation();
    let claimed = offered
        .claim(correlation)
        .expect("the exact fence claims the offered lease");
    let loss = claimed.lose().expect("a claimed lease can be lost");
    let source_batch = claimed_batch("sync", RunnerToolEffectClass::Idempotent);
    let source_effect = current_attempt_effect_class(&source_batch, request("sync").id())
        .expect("the source batch carries its current attempt effect");
    let prepared = loss
        .retry()
        .expect("claimed idempotent work carries retry authority")
        .prepare_claimed_attempt(source_batch, tool_attempt_id(RETRY_ATTEMPT))
        .expect("retry authority preserves the source effect");
    let (_, retired_attempt, authorization) = prepared.into_parts();
    let (attempt, _) = authorization.authorized.into_parts();

    assert_eq!(retired_attempt.end(), &ToolAttemptEnd::Ambiguous);
    assert_eq!(attempt.effect_class(), source_effect);
}

#[test]
fn claimed_retry_rejects_standalone_same_request_authority() {
    let (registration, placement, grant, offered) = offered("inspect", tool_attempt_id(ATTEMPT));
    let correlation = offered.correlation();
    let claimed = offered
        .claim(correlation)
        .expect("the exact fence claims the first lease");
    let loss = claimed
        .lose()
        .expect("claimed pure work permits checked retry");

    assert_eq!(
        placement.offer_retry(
            &enrollment_for_registration(&registration),
            &registration,
            grant.as_ref(),
            loss,
            authorized(
                "inspect",
                tool_attempt_id(RETRY_ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
        ),
        Err(RunnerDomainError::CorrelationMismatch)
    );
}

#[test]
fn claimed_side_effecting_loss_requires_crash_classification() {
    let (_, _, _, offered) = offered("deploy", tool_attempt_id(ATTEMPT));
    let correlation = offered.correlation();
    let expected_attempt = correlation.dispatch.attempt();
    let claimed = offered
        .claim(correlation)
        .expect("the exact fence claims the offered lease");

    let loss = claimed.lose().expect("a claimed lease can be lost");

    assert_eq!(loss.retry(), None);
    assert_eq!(loss.crash_attempt(), Some(expected_attempt));
}

#[test]
fn stale_generation_cannot_claim() {
    let (_, _, _, offered) = offered("inspect", tool_attempt_id(ATTEMPT));
    let stale = RunnerLeaseCorrelation {
        generation: RunnerGeneration::try_from_u64(2).expect("two is positive"),
        ..offered.correlation()
    };

    assert_eq!(
        offered.claim(stale),
        Err(RunnerDomainError::CorrelationMismatch)
    );
}

#[test]
fn cross_wired_attempt_dispatch_cannot_claim() {
    let (_, _, _, offered) = offered("inspect", tool_attempt_id(ATTEMPT));
    let stale = RunnerLeaseCorrelation {
        dispatch: authorized(
            "inspect",
            tool_attempt_id(RETRY_ATTEMPT),
            RunnerToolEffectClass::Pure,
        )
        .authorized
        .correlation(),
        ..offered.correlation()
    };

    assert_eq!(
        offered.claim(stale),
        Err(RunnerDomainError::CorrelationMismatch)
    );
}

#[test]
fn lease_requires_matching_authorized_attempt_effect() {
    let (registration, pin) = pinned("readonly");

    assert_eq!(
        pin.placement.offer_lease(
            &enrollment_for_registration(&registration),
            &registration,
            pin.grant.as_ref(),
            authorized(
                "inspect",
                tool_attempt_id(RETRY_ATTEMPT),
                RunnerToolEffectClass::SideEffecting,
            ),
            lease_offer_request("inspect"),
        ),
        Err(RunnerDomainError::CorrelationMismatch)
    );
}

#[test]
fn lease_requires_the_authorized_request_tool() {
    let (registration, pin) = pinned("readonly");

    assert_eq!(
        pin.placement.offer_lease(
            &enrollment_for_registration(&registration),
            &registration,
            pin.grant.as_ref(),
            authorized(
                "deploy",
                tool_attempt_id(RETRY_ATTEMPT),
                RunnerToolEffectClass::SideEffecting,
            ),
            lease_offer_request("inspect"),
        ),
        Err(RunnerDomainError::CorrelationMismatch)
    );
}

#[test]
fn attempt_authorization_rejects_a_cross_wired_request() {
    let approved = approved_request("inspect");
    let deploy = approved_request("deploy");
    let authorized = deploy
        .prepare_attempt(
            tool_attempt_id(RETRY_ATTEMPT),
            turn_attempt_id(0x7b00),
            ToolEffectClass::ExternalEffect,
        )
        .authorize()
        .expect("the prepared fixture attempt authorizes once");

    assert_eq!(
        RunnerToolAttemptAuthorization::try_new(approved, authorized),
        Err(RunnerDomainError::CorrelationMismatch)
    );
}

#[test]
fn lease_reconstitution_rejects_cross_wired_fence() {
    let (registration, _, _, lease) = offered("inspect", tool_attempt_id(ATTEMPT));
    let mut input = lease_reconstitution_input(lease);
    input.recorded_correlation = RunnerLeaseCorrelation {
        runner: runner_id(REPLACEMENT_RUNNER),
        ..input.recorded_correlation
    };

    assert_eq!(
        RunnerLease::reconstitute(input, &registration),
        Err(RunnerDomainError::CorruptStoredFacts)
    );
}

#[test]
fn lease_reconstitution_rejects_cross_wired_effect() {
    let (registration, _, _, lease) = offered("inspect", tool_attempt_id(ATTEMPT));
    let mut input = lease_reconstitution_input(lease);
    input.recorded_effect = RunnerToolEffectClass::SideEffecting;

    assert_eq!(
        RunnerLease::reconstitute(input, &registration),
        Err(RunnerDomainError::CorruptStoredFacts)
    );
}

#[test]
fn lease_reconstitution_binds_effect_to_registration_declaration() {
    let (registration, _, _, offered) = offered("deploy", tool_attempt_id(ATTEMPT));
    let correlation = offered.correlation();
    let claimed = offered
        .claim(correlation)
        .expect("the exact side-effecting lease is claimed");
    let loss = claimed.lose().expect("the claimed lease may be lost");
    let mut input = borrowed_lease_reconstitution_input(loss.lost());
    input.effect = RunnerToolEffectClass::Idempotent;
    input.recorded_effect = RunnerToolEffectClass::Idempotent;

    assert_eq!(
        RunnerLease::reconstitute_loss(input, &registration, None),
        Err(RunnerDomainError::CorruptStoredFacts)
    );
}

#[test]
fn lease_reconstitution_rejects_cross_wired_authorization() {
    let (registration, _, _, lease) = offered("inspect", tool_attempt_id(ATTEMPT));
    let mut input = lease_reconstitution_input(lease);
    input.recorded_credential_authorization = None;

    assert_eq!(
        RunnerLease::reconstitute(input, &registration),
        Err(RunnerDomainError::CorruptStoredFacts)
    );
}

#[test]
fn lease_reconstitution_rejects_foreign_credential_session() {
    let (registration, _, _, lease) = offered("inspect", tool_attempt_id(ATTEMPT));
    let mut input = lease_reconstitution_input(lease);
    let authorization = input
        .credential_authorization
        .as_mut()
        .expect("the fixture lease carries credential authorization");
    authorization.session = session_id(SESSION + 1);
    input.recorded_credential_authorization = input.credential_authorization.clone();

    assert_eq!(
        RunnerLease::reconstitute(input, &registration),
        Err(RunnerDomainError::CorruptStoredFacts)
    );
}

#[test]
fn lease_reconstitution_rejects_foreign_credential_runner() {
    let (registration, _, _, lease) = offered("inspect", tool_attempt_id(ATTEMPT));
    let mut input = lease_reconstitution_input(lease);
    let authorization = input
        .credential_authorization
        .as_mut()
        .expect("the fixture lease carries credential authorization");
    authorization.runner = runner_id(REPLACEMENT_RUNNER);
    input.recorded_credential_authorization = input.credential_authorization.clone();

    assert_eq!(
        RunnerLease::reconstitute(input, &registration),
        Err(RunnerDomainError::CorruptStoredFacts)
    );
}

#[test]
fn lease_reconstitution_rejects_foreign_credential_tool() {
    let (registration, _, _, lease) = offered("inspect", tool_attempt_id(ATTEMPT));
    let mut input = lease_reconstitution_input(lease);
    let authorization = input
        .credential_authorization
        .as_mut()
        .expect("the fixture lease carries credential authorization");
    authorization.tool = tool("deploy");
    input.recorded_credential_authorization = input.credential_authorization.clone();
    assert_eq!(
        RunnerLease::reconstitute(input, &registration),
        Err(RunnerDomainError::CorruptStoredFacts)
    );
}

#[test]
fn lease_reconstitution_rejects_cross_wired_session() {
    let (registration, _, _, lease) = offered("inspect", tool_attempt_id(ATTEMPT));
    let mut input = lease_reconstitution_input(lease);
    input.recorded_session = session_id(SESSION + 1);

    assert_eq!(
        RunnerLease::reconstitute(input, &registration),
        Err(RunnerDomainError::CorruptStoredFacts)
    );
}

#[test]
fn lease_reconstitution_rejects_cross_wired_state() {
    let (registration, _, _, lease) = offered("inspect", tool_attempt_id(ATTEMPT));
    let mut input = lease_reconstitution_input(lease);
    input.recorded_state = RunnerLeaseState::Claimed;

    assert_eq!(
        RunnerLease::reconstitute(input, &registration),
        Err(RunnerDomainError::CorruptStoredFacts)
    );
}

#[test]
fn first_execution_pins_the_exact_runner() {
    let registration = registration();
    let placement =
        SessionRunnerPlacement::new(session_id(SESSION), placement_request(profile("readonly")));

    let pinned = placement
        .pin_and_offer_lease(
            &enrollment_for_registration(&registration),
            &registration,
            directory("/workspace/session"),
            None,
            authorized(
                "inspect",
                tool_attempt_id(ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        )
        .expect("the first authorized lease satisfies every requested axis");

    let expected_grant = pinned
        .grant
        .as_ref()
        .expect("profile selection creates a grant");
    let expected = SessionRunnerPlacementState::Pinned(PinnedRunnerPlacement {
        runner: runner_id(RUNNER),
        working_directory: directory("/workspace/session"),
        credential_profile: Some(profile("readonly")),
        grant_lineage: Some(RunnerCredentialGrantLineage {
            runner: expected_grant.runner,
            revision: expected_grant.revision(),
        }),
        tools: BTreeSet::from([tool("deploy"), tool("inspect"), tool("sync")]),
        runner_required_tools: BTreeSet::from([tool("deploy"), tool("sync")]),
        workspace: None,
        sandbox: RunnerSandboxProfile::Ambient,
        permission_overrides: no_permission_overrides(),
    });
    assert_eq!(pinned.placement.state(), &expected);
    assert_eq!(expected_grant.profile(), &profile("readonly"));
}

#[test]
fn placement_reconstitution_accepts_raw_pinned_facts() {
    let (registration, pin) = pinned("readonly");
    let expected_state = pin.placement.state().clone();
    let input = placement_reconstitution_input(pin.placement);

    let reconstituted =
        SessionRunnerPlacement::reconstitute(input, session_id(SESSION), Some(&registration), None)
            .expect("complete pinned facts reconstitute");

    assert_eq!(reconstituted.state(), &expected_state);
}

#[test]
fn placement_reconstitution_rejects_missing_grant_lineage() {
    let (registration, pin) = pinned("readonly");
    let corrupted = placement_without_grant_lineage(pin.placement);
    let input = placement_reconstitution_input(corrupted);

    assert_eq!(
        SessionRunnerPlacement::reconstitute(input, session_id(SESSION), Some(&registration), None,),
        Err(RunnerDomainError::CorruptStoredFacts),
    );
}

#[test]
fn generation_one_profiled_placement_rejects_later_grant_revision() {
    let (registration, pin) = pinned("readonly");
    let corrupted = placement_with_grant_lineage(
        pin.placement,
        RunnerCredentialGrantLineage {
            runner: registration.runner(),
            revision: RunnerGeneration::try_from_u64(2).expect("two is positive"),
        },
    );
    let input = placement_reconstitution_input(corrupted);

    assert_eq!(
        SessionRunnerPlacement::reconstitute(input, session_id(SESSION), Some(&registration), None,),
        Err(RunnerDomainError::CorruptStoredFacts)
    );
}

#[test]
fn placement_rejects_grant_lineage_newer_than_its_revision() {
    let (registration, pin) = pinned("readonly");
    let mut placement = pin.placement;
    placement.revision = RunnerGeneration::try_from_u64(2).expect("two is positive");
    let corrupted = placement_with_grant_lineage(
        placement,
        RunnerCredentialGrantLineage {
            runner: registration.runner(),
            revision: RunnerGeneration::try_from_u64(3).expect("three is positive"),
        },
    );
    let input = placement_reconstitution_input(corrupted);

    assert_eq!(
        SessionRunnerPlacement::reconstitute(input, session_id(SESSION), Some(&registration), None,),
        Err(RunnerDomainError::CorruptStoredFacts)
    );
}
#[test]
fn generation_one_profileless_placement_rejects_grant_lineage() {
    let registration = registration();
    let pin = SessionRunnerPlacement::new(session_id(SESSION), profileless_placement_request())
        .pin_and_offer_lease(
            &enrollment_for_registration(&registration),
            &registration,
            directory("/workspace/session"),
            None,
            automatically_authorized(
                "inspect",
                tool_attempt_id(ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        )
        .expect("profileless execution pins without a grant");
    let corrupted = placement_with_grant_lineage(
        pin.placement,
        RunnerCredentialGrantLineage {
            runner: registration.runner(),
            revision: RunnerGeneration::one(),
        },
    );
    let input = placement_reconstitution_input(corrupted);

    assert_eq!(
        SessionRunnerPlacement::reconstitute(input, session_id(SESSION), Some(&registration), None,),
        Err(RunnerDomainError::CorruptStoredFacts)
    );
}

#[test]
fn placement_reconstitution_rejects_cross_wired_session() {
    let (registration, pin) = pinned("readonly");
    let input = placement_reconstitution_input(pin.placement);

    assert_eq!(
        SessionRunnerPlacement::reconstitute(
            input,
            session_id(SESSION + 1),
            Some(&registration),
            None,
        ),
        Err(RunnerDomainError::CorruptStoredFacts)
    );
}

#[test]
fn placement_reconstitution_rejects_missing_required_tool() {
    let (registration, pin) = pinned("readonly");
    let corrupted = placement_without_required_tool(pin.placement, &tool("deploy"));
    let input = placement_reconstitution_input(corrupted);

    assert_eq!(
        SessionRunnerPlacement::reconstitute(input, session_id(SESSION), Some(&registration), None,),
        Err(RunnerDomainError::CorruptStoredFacts)
    );
}

#[test]
fn placement_reconstitution_requires_complete_runner_only_set() {
    let (registration, mut pin) = pinned("readonly");
    omit_runner_required_tool(&mut pin.placement, &tool("deploy"));
    let input = placement_reconstitution_input(pin.placement);

    assert_eq!(
        SessionRunnerPlacement::reconstitute(input, session_id(SESSION), Some(&registration), None,),
        Err(RunnerDomainError::CorruptStoredFacts)
    );
}

#[test]
fn reregistration_additions_do_not_widen_a_pinned_snapshot() {
    let narrow_registration = enrollment()
        .register(
            RunnerAdvertisement::new(
                [class()],
                [tool("inspect")],
                [profile("readonly")],
                [WorkspaceCapability::WorktreePerSession],
                sandbox_profiles(),
                [],
            ),
            &catalog(),
        )
        .expect("the narrow advertisement is allowed");
    let pin =
        SessionRunnerPlacement::new(session_id(SESSION), placement_request(profile("readonly")))
            .pin_and_offer_lease(
                &enrollment_for_registration(&narrow_registration),
                &narrow_registration,
                directory("/workspace/session"),
                None,
                authorized(
                    "inspect",
                    tool_attempt_id(ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            )
            .expect("the narrow registration and first lease satisfy placement");
    let expanded_registration = registration();
    let reconciled = pin
        .placement
        .reconcile_registration(&expanded_registration)
        .expect("an expanded registration preserves the pin");

    assert_eq!(
        reconciled.offer_lease(
            &enrollment_for_registration(&expanded_registration),
            &expanded_registration,
            pin.grant.as_ref(),
            authorized(
                "deploy",
                tool_attempt_id(RETRY_ATTEMPT),
                RunnerToolEffectClass::SideEffecting,
            ),
            lease_offer_request("deploy"),
        ),
        Err(RunnerDomainError::ToolUnavailable)
    );
}

#[test]
fn reregistration_omission_reconciles_to_runner_loss() {
    let (_, pin_for_offer) = pinned("readonly");
    let (_, pin_for_reconciliation) = pinned("readonly");
    let (_, pin_for_expected_state) = pinned("readonly");
    let narrowed_registration = enrollment()
        .register(
            RunnerAdvertisement::new(
                [class()],
                [tool("inspect"), tool("deploy")],
                [profile("readonly")],
                [WorkspaceCapability::WorktreePerSession],
                sandbox_profiles(),
                [],
            ),
            &catalog(),
        )
        .expect("the narrowed advertisement remains allowed");
    let expected = pin_for_expected_state
        .placement
        .reconcile_registration(&narrowed_registration)
        .expect("registration narrowing is explicit runner loss");

    assert_eq!(
        pin_for_offer.placement.offer_lease(
            &enrollment_for_registration(&narrowed_registration),
            &narrowed_registration,
            pin_for_offer.grant.as_ref(),
            authorized(
                "inspect",
                tool_attempt_id(RETRY_ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        ),
        Err(RunnerDomainError::RegistrationChanged)
    );
    assert_eq!(
        expected,
        pin_for_reconciliation
            .placement
            .reconcile_registration(&narrowed_registration)
            .expect("registration narrowing is explicit runner loss")
    );
}

#[test]
fn reconciliation_rejects_a_stale_registration() {
    let enrollment = enrollment();
    let retained = enrollment
        .register(advertisement(), &catalog())
        .expect("the first registration pins the complete runner snapshot");
    let pin =
        SessionRunnerPlacement::new(session_id(SESSION), placement_request(profile("readonly")))
            .pin_and_offer_lease(
                &enrollment,
                &retained,
                directory("/workspace/session"),
                None,
                authorized(
                    "inspect",
                    tool_attempt_id(ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            )
            .expect("the first registration pins the runner");
    let current = enrollment
        .register(
            RunnerAdvertisement::new(
                [class()],
                [tool("inspect")],
                [profile("readonly")],
                [WorkspaceCapability::WorktreePerSession],
                sandbox_profiles(),
                [],
            ),
            &catalog(),
        )
        .expect("the narrowed successor registration is current");

    assert_eq!(
        pin.placement.reconcile_registration(&retained),
        Err(RunnerDomainError::RegistrationChanged)
    );
    assert_ne!(retained.revision(), current.revision());
}

#[test]
fn reconciliation_rejects_a_foreign_runner_registration() {
    let (_, pin) = pinned("readonly");
    let foreign = registration_for(runner_id(REPLACEMENT_RUNNER));

    assert_eq!(
        pin.placement.reconcile_registration(&foreign),
        Err(RunnerDomainError::CorrelationMismatch)
    );
}

#[test]
fn combined_tool_omission_retains_daemon_fallback() {
    let (_, pin) = pinned("readonly");
    let expected_state = pin.placement.state().clone();
    let expected_revision = pin.placement.revision();
    let narrowed_registration = enrollment()
        .register(
            RunnerAdvertisement::new(
                [class()],
                [tool("deploy"), tool("sync")],
                [profile("readonly")],
                [WorkspaceCapability::WorktreePerSession],
                sandbox_profiles(),
                [],
            ),
            &catalog(),
        )
        .expect("omitting the combined tool remains a valid registration");
    let reconciled = pin
        .placement
        .reconcile_registration(&narrowed_registration)
        .expect("combined-tool omission retains pinned placement");

    assert_eq!(reconciled.state(), &expected_state);
    assert_eq!(reconciled.revision(), expected_revision);
    assert_eq!(
        reconciled.offer_lease(
            &enrollment_for_registration(&narrowed_registration),
            &narrowed_registration,
            pin.grant.as_ref(),
            authorized(
                "inspect",
                tool_attempt_id(RETRY_ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        ),
        Err(RunnerDomainError::ToolUnavailable)
    );
}

#[test]
fn combined_tool_override_does_not_require_runner_advertisement() {
    let enrollment = enrollment();
    let registration = enrollment
        .register(
            RunnerAdvertisement::new(
                [class()],
                [tool("deploy"), tool("sync")],
                [],
                [WorkspaceCapability::WorktreePerSession],
                sandbox_profiles(),
                [],
            ),
            &catalog(),
        )
        .expect("the runner may omit the combined-locus tool");
    let mut request = profileless_placement_request();
    request.permission_overrides = RunnerToolPermissionOverrides::try_new([(
        tool("inspect"),
        RunnerToolPermissionOverride::Confirm,
    )])
    .expect("the daemon-declared combined-tool override is valid");
    let pin = SessionRunnerPlacement::new(session_id(SESSION), request)
        .pin_and_offer_lease(
            &enrollment,
            &registration,
            directory("/workspace/session"),
            None,
            authorized(
                "deploy",
                tool_attempt_id(ATTEMPT),
                RunnerToolEffectClass::SideEffecting,
            ),
            lease_offer_request("deploy"),
        )
        .expect("the override remains session policy while another tool dispatches");
    let unavailable = pin.placement.offer_lease(
        &enrollment,
        &registration,
        None,
        authorized(
            "inspect",
            tool_attempt_id(RETRY_ATTEMPT),
            RunnerToolEffectClass::Pure,
        ),
        lease_offer_request("inspect"),
    );

    assert_eq!(unavailable, Err(RunnerDomainError::ToolUnavailable));
}

#[test]
fn permission_override_rejects_tool_absent_from_daemon_catalog() {
    let enrollment = enrollment();
    let registration = enrollment
        .register(advertisement(), &catalog())
        .expect("the canonical registration is valid");
    let mut request = profileless_placement_request();
    request.permission_overrides = RunnerToolPermissionOverrides::try_new([(
        tool("future"),
        RunnerToolPermissionOverride::Confirm,
    )])
    .expect("the override map is structurally valid before catalog validation");
    let rejected = SessionRunnerPlacement::new(session_id(SESSION), request)
        .pin_and_offer_lease(
            &enrollment,
            &registration,
            directory("/workspace/session"),
            None,
            authorized(
                "inspect",
                tool_attempt_id(ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        )
        .expect_err("a tool outside daemon policy must fail closed");

    assert_eq!(rejected, RunnerDomainError::ToolUndeclared(tool("future")));
}

#[test]
fn combined_tool_override_omission_retains_daemon_fallback() {
    let registration = registration();
    let mut request = placement_request(profile("readonly"));
    request.permission_overrides = RunnerToolPermissionOverrides::try_new([(
        tool("inspect"),
        RunnerToolPermissionOverride::Confirm,
    )])
    .expect("the exact combined-tool override is valid");
    let pin = SessionRunnerPlacement::new(session_id(SESSION), request)
        .pin_and_offer_lease(
            &enrollment_for_registration(&registration),
            &registration,
            directory("/workspace/session"),
            None,
            authorized(
                "inspect",
                tool_attempt_id(ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        )
        .expect("the combined tool pins with its exact override");
    let expected_state = pin.placement.state().clone();
    let narrowed_registration = enrollment_for_registration(&registration)
        .register(
            RunnerAdvertisement::new(
                [class()],
                [tool("deploy"), tool("sync")],
                [profile("readonly")],
                [WorkspaceCapability::WorktreePerSession],
                sandbox_profiles(),
                [],
            ),
            &catalog(),
        )
        .expect("omitting the combined tool remains a valid registration");
    let reconciled = pin
        .placement
        .reconcile_registration(&narrowed_registration)
        .expect("the immutable override does not turn fallback into runner affinity");

    assert_eq!(reconciled.state(), &expected_state);
}

#[test]
fn lost_placement_cannot_offer_another_lease() {
    let (registration, mut pin) = pinned("readonly");
    let grant = pin.grant.take().expect("profile selection creates a grant");
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the pinned runner can be marked lost");

    assert_eq!(
        lost.offer_lease(
            &enrollment_for_registration(&registration),
            &registration,
            Some(&grant),
            authorized(
                "inspect",
                tool_attempt_id(RETRY_ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        ),
        Err(RunnerDomainError::InvalidState)
    );
}

#[test]
fn replacement_is_explicit_and_advances_revision() {
    let initial = registration();
    let replacement = registration_for(runner_id(REPLACEMENT_RUNNER));
    let mut pin =
        SessionRunnerPlacement::new(session_id(SESSION), placement_request(profile("readonly")))
            .pin_and_offer_lease(
                &enrollment_for_registration(&initial),
                &initial,
                directory("/workspace/old"),
                None,
                authorized(
                    "inspect",
                    tool_attempt_id(ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            )
            .expect("the initial registration and lease satisfy placement");
    let prior_grant = pin
        .grant
        .take()
        .expect("the selected profile creates a prior grant");
    let placement = pin
        .placement
        .mark_runner_lost()
        .expect("the pinned runner can be marked lost");

    let replaced = placement
        .replace_lost_runner(
            placement_request(profile("admin")),
            &replacement,
            directory("/workspace/new"),
            None,
            Some(prior_grant),
        )
        .expect("explicit replacement validates every new axis");

    assert_eq!(
        replaced.placement.revision(),
        RunnerGeneration::try_from_u64(2).expect("two is positive")
    );
    assert_eq!(replaced.change.before.runner, initial.runner());
    assert_eq!(replaced.change.after.runner, replacement.runner());
}

#[test]
fn lost_before_pin_requires_the_exact_selected_runner() {
    let selected = runner_id(RUNNER);
    let foreign = runner_id(REPLACEMENT_RUNNER);
    let capability_selected =
        SessionRunnerPlacement::new(session_id(SESSION), profileless_placement_request());
    let exact_selected =
        SessionRunnerPlacement::new(session_id(SESSION), exact_placement_request(selected));

    assert_eq!(
        capability_selected.mark_runner_lost_before_pin(selected),
        Err(RunnerDomainError::InvalidState),
    );
    assert_eq!(
        exact_selected.mark_runner_lost_before_pin(foreign),
        Err(RunnerDomainError::InvalidState),
    );
}

#[test]
fn pre_pin_replacement_advances_unpinned_without_pinned_facts() {
    let selected = runner_id(RUNNER);
    let replacement_registration = registration_for(runner_id(REPLACEMENT_RUNNER));
    let initial_request = exact_placement_request(selected);
    let replacement_request = exact_placement_request(replacement_registration.runner());
    let lost = SessionRunnerPlacement::new(session_id(SESSION), initial_request.clone())
        .mark_runner_lost_before_pin(selected)
        .expect("the exact selection may be lost before pinning");
    let expected_revision =
        RunnerGeneration::try_from_u64(2).expect("the fixture states revision two");
    let replacement = lost
        .replace_lost_runner_before_pin(replacement_request.clone(), &replacement_registration)
        .expect("a distinct current runner installs a successor request");

    assert_eq!(replacement.placement.revision(), expected_revision);
    assert_eq!(
        replacement.placement.state(),
        &SessionRunnerPlacementState::Unpinned,
    );
    assert_eq!(replacement.before.runner(), selected);
    assert_eq!(replacement.prior_request, initial_request);
    assert_eq!(replacement.replacement_request, replacement_request);
}

#[test]
fn pre_pin_replacement_requires_repository_workspace_capability() {
    let selected = runner_id(RUNNER);
    let replacement_runner = runner_id(REPLACEMENT_RUNNER);
    let replacement_registration = enrollment_for(replacement_runner)
        .register(
            RunnerAdvertisement::new(
                [class()],
                [tool("inspect"), tool("deploy"), tool("sync")],
                [profile("readonly"), profile("admin")],
                [],
                sandbox_profiles(),
                [RunnerRepositoryEntry::new(repository_key(), None)],
            ),
            &catalog(),
        )
        .expect("the repository inventory is valid without workspace capability");
    let lost = SessionRunnerPlacement::new(session_id(SESSION), exact_placement_request(selected))
        .mark_runner_lost_before_pin(selected)
        .expect("the exact selection may be lost before pinning");
    let replacement_request = SessionRunnerPlacementRequest {
        selector: RunnerSelector::Identity(replacement_runner),
        working_directory: WorkingDirectorySelection::RunnerDefault,
        credential_profile: None,
        workspace: WorkspaceRequirement::RepositoryWorktree {
            repository: repository_key(),
        },
        sandbox: RunnerSandboxProfile::WorkspaceRestricted,
        permission_overrides: no_permission_overrides(),
    };

    assert_eq!(
        lost.replace_lost_runner_before_pin(replacement_request, &replacement_registration,),
        Err(RunnerDomainError::WorkspaceCapabilityUnavailable),
    );
}

#[test]
fn connection_loss_rejects_same_runner_replacement() {
    let (registration, mut pin) = pinned("readonly");
    let prior_grant = pin.grant.take().expect("the pin carries its grant");
    let request = pin.placement.request().clone();
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the pinned runner may be lost through its connection");

    assert_eq!(
        lost.replace_lost_runner(
            request,
            &registration,
            directory("/workspace/session"),
            None,
            Some(prior_grant),
        ),
        Err(RunnerDomainError::CorrelationMismatch),
    );
}

#[test]
fn registration_loss_revalidates_a_same_runner_replacement() {
    let (registration, mut pin) = pinned("readonly");
    let prior_grant = pin.grant.take().expect("the pin carries its grant");
    let request = pin.placement.request().clone();
    let mut pinned = validate_placement(
        pin.placement.session(),
        pin.placement.revision(),
        &request,
        &registration,
        directory("/workspace/session"),
        None,
        WorkspaceRevisionMatch::Exact,
    )
    .expect("the fixture registration validates the pinned facts");
    pinned.grant_lineage = Some(prior_grant.lineage());
    let lost = SessionRunnerPlacement::reconstitute(
        SessionRunnerPlacementReconstitutionInput {
            session: pin.placement.session(),
            revision: pin.placement.revision(),
            request: request.clone(),
            state: SessionRunnerPlacementState::RunnerLost(LostPinnedRunnerPlacement::from_stored(
                pinned,
                RunnerPlacementLossSource::Registration,
            )),
            history: RunnerPlacementReconstitutionHistory::Initial,
        },
        pin.placement.session(),
        Some(&registration),
        None,
    )
    .expect("complete stored loss facts reconstitute");

    let replacement = lost
        .replace_lost_runner(
            request,
            &registration,
            directory("/workspace/session"),
            None,
            Some(prior_grant),
        )
        .expect("current registration permits same-runner registration-loss recovery");
    assert_eq!(
        replacement.change.before.runner,
        replacement.change.after.runner
    );
    assert_eq!(
        replacement.placement.revision(),
        replacement.change.prior_revision.checked_next().unwrap()
    );
}

#[test]
fn abandonment_retires_the_exact_lost_pre_pin_state() {
    let selected = runner_id(RUNNER);
    let lost = SessionRunnerPlacement::new(session_id(SESSION), exact_placement_request(selected))
        .mark_runner_lost_before_pin(selected)
        .expect("the exact selection may be lost before pinning");
    let abandoned = lost
        .abandon_lost_runner()
        .expect("the lost pre-pin selection may be abandoned");

    assert_eq!(
        abandoned.state(),
        &SessionRunnerPlacementState::RunnerAbandoned(AbandonedRunnerPlacement::BeforePin(
            RunnerLostBeforePin::from_stored(selected)
        ),),
    );
}

#[test]
fn unpinned_successor_reconstitution_requires_pre_pin_history() {
    let selected = runner_id(RUNNER);
    let replacement_registration = registration_for(runner_id(REPLACEMENT_RUNNER));
    let lost = SessionRunnerPlacement::new(session_id(SESSION), exact_placement_request(selected))
        .mark_runner_lost_before_pin(selected)
        .expect("the exact selection may be lost before pinning");
    let prior_revision = lost.revision();
    let prior_request = lost.request().clone();
    let replacement = lost
        .replace_lost_runner_before_pin(
            exact_placement_request(replacement_registration.runner()),
            &replacement_registration,
        )
        .expect("a distinct current runner installs a successor request");
    let initial_history = placement_reconstitution_input(replacement.placement);
    let mut replacement_history = initial_history.clone();
    replacement_history.history = RunnerPlacementReconstitutionHistory::PrePinReplacements(vec![
        RunnerPrePinReplacementHistory {
            prior_revision,
            lost_runner: selected,
            prior_request,
            replacement_request: initial_history.request.clone(),
        },
    ]);

    assert_eq!(
        SessionRunnerPlacement::reconstitute(initial_history, session_id(SESSION), None, None,),
        Err(RunnerDomainError::CorruptStoredFacts),
    );
    let restored =
        SessionRunnerPlacement::reconstitute(replacement_history, session_id(SESSION), None, None)
            .expect("append-only pre-pin replacement history authenticates revision two");
    assert_eq!(restored.state(), &SessionRunnerPlacementState::Unpinned);
}

#[test]
fn pre_pin_reconstitution_rejects_a_truncated_predecessor_chain() {
    let second_runner = runner_id(REPLACEMENT_RUNNER);
    let third_runner = runner_id(THIRD_RUNNER);
    let second_request = exact_placement_request(second_runner);
    let third_request = exact_placement_request(third_runner);
    let input = SessionRunnerPlacementReconstitutionInput {
        session: session_id(SESSION),
        revision: RunnerGeneration::try_from_u64(3).expect("three is a positive generation"),
        request: third_request.clone(),
        state: SessionRunnerPlacementState::Unpinned,
        history: RunnerPlacementReconstitutionHistory::PrePinReplacements(vec![
            RunnerPrePinReplacementHistory {
                prior_revision: RunnerGeneration::try_from_u64(2)
                    .expect("two is a positive generation"),
                lost_runner: second_runner,
                prior_request: second_request,
                replacement_request: third_request,
            },
        ]),
    };

    assert_eq!(
        SessionRunnerPlacement::reconstitute(input, session_id(SESSION), None, None),
        Err(RunnerDomainError::CorruptStoredFacts),
    );
}

#[test]
fn lost_before_pin_reconstitution_requires_an_identity_selector() {
    let selected = runner_id(RUNNER);
    let input = SessionRunnerPlacementReconstitutionInput {
        session: session_id(SESSION),
        revision: RunnerGeneration::one(),
        request: profileless_placement_request(),
        state: SessionRunnerPlacementState::RunnerLostBeforePin(RunnerLostBeforePin::from_stored(
            selected,
        )),
        history: RunnerPlacementReconstitutionHistory::Initial,
    };

    assert_eq!(
        SessionRunnerPlacement::reconstitute(input, session_id(SESSION), None, None),
        Err(RunnerDomainError::CorruptStoredFacts),
    );
}

#[test]
fn replacement_advances_a_revoked_grant_revision() {
    let replacement = registration_for(runner_id(REPLACEMENT_RUNNER));
    let mut pin = pinned("readonly").1;
    let prior_grant = pin
        .grant
        .take()
        .expect("the selected profile creates a prior grant")
        .revoke()
        .expect("the active prior grant can be revoked");
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the pinned placement can be marked lost");

    let replaced = lost
        .replace_lost_runner(
            placement_request(profile("readonly")),
            &replacement,
            directory("/workspace/session"),
            None,
            Some(prior_grant),
        )
        .expect("explicit replacement creates a checked successor grant");
    let replacement_grant = replaced
        .grant
        .expect("the replaced profiled placement creates a grant");

    assert_eq!(
        replacement_grant.revision(),
        RunnerGeneration::try_from_u64(2).expect("two is positive")
    );
    assert_eq!(
        replacement_grant.state(),
        CredentialProfileGrantState::Active
    );
}

#[test]
fn replacement_change_retains_policy_only_request_change() {
    let registration = registration();
    let replacement = registration_for(runner_id(REPLACEMENT_RUNNER));
    let before_request = placement_request(profile("readonly"));
    let after_request = SessionRunnerPlacementRequest {
        selector: RunnerSelector::Identity(replacement.runner()),
        ..before_request.clone()
    };
    let mut pin = SessionRunnerPlacement::new(session_id(SESSION), before_request.clone())
        .pin_and_offer_lease(
            &enrollment_for_registration(&registration),
            &registration,
            directory("/workspace/session"),
            None,
            authorized(
                "inspect",
                tool_attempt_id(ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        )
        .expect("the initial request pins the selected runner");
    let prior_grant = pin
        .grant
        .take()
        .expect("profile selection creates a prior grant");
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the pinned runner can be marked lost");

    let replaced = lost
        .replace_lost_runner(
            after_request.clone(),
            &replacement,
            directory("/workspace/session"),
            None,
            Some(prior_grant),
        )
        .expect("the exact-runner request selects the replacement runner");

    assert_eq!(
        replaced.change.after.grant_lineage,
        replaced.grant.as_ref().map(CredentialProfileGrant::lineage),
    );
    assert_eq!(replaced.change.before_request, before_request);
    assert_eq!(replaced.change.after_request, after_request);
}

#[test]
fn profileless_replacement_retains_grant_lineage() {
    let registration = registration();
    let replacement = registration_for(runner_id(REPLACEMENT_RUNNER));
    let mut pin = pinned("readonly").1;
    let prior_grant = pin
        .grant
        .take()
        .expect("profile selection creates a prior grant");
    let first_lost = pin
        .placement
        .mark_runner_lost()
        .expect("the profiled placement can be marked lost");
    let profileless = first_lost
        .replace_lost_runner(
            profileless_placement_request(),
            &replacement,
            directory("/workspace/session"),
            None,
            Some(prior_grant),
        )
        .expect("profileless replacement retains a terminal grant lineage");
    let tombstone = profileless
        .grant
        .expect("the prior grant lineage remains as a tombstone");
    let second_lost = profileless
        .placement
        .mark_runner_lost()
        .expect("the profileless placement can be marked lost");

    let restored = second_lost
        .replace_lost_runner(
            placement_request(profile("readonly")),
            &registration,
            directory("/workspace/session"),
            None,
            Some(tombstone),
        )
        .expect("restoring the prior profile advances its retained lineage");
    let restored_grant = restored
        .grant
        .expect("the restored profile creates an active successor grant");

    assert_eq!(
        restored_grant.revision(),
        RunnerGeneration::try_from_u64(3).expect("three is positive")
    );
    assert_eq!(restored_grant.state(), CredentialProfileGrantState::Active);
}

#[test]
fn profileless_placement_reconstitutes_with_exact_tombstone() {
    let replacement = registration_for(runner_id(REPLACEMENT_RUNNER));
    let mut pin = pinned("readonly").1;
    let prior_grant = pin
        .grant
        .take()
        .expect("profile selection creates a prior grant");
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the profiled placement can be marked lost");
    let profileless = lost
        .replace_lost_runner(
            profileless_placement_request(),
            &replacement,
            directory("/workspace/session"),
            None,
            Some(prior_grant),
        )
        .expect("profileless replacement creates its terminal tombstone");
    let tombstone = profileless
        .grant
        .expect("the checked replacement returns the tombstone");
    let expected_state = profileless.placement.state().clone();
    let input = placement_reconstitution_input(profileless.placement);

    assert_eq!(
        SessionRunnerPlacement::reconstitute(
            input.clone(),
            session_id(SESSION),
            Some(&replacement),
            None,
        ),
        Err(RunnerDomainError::CorruptStoredFacts)
    );
    let restored = SessionRunnerPlacement::reconstitute(
        input,
        session_id(SESSION),
        Some(&replacement),
        Some(&tombstone),
    )
    .expect("the exact revoked tombstone authenticates retained profileless lineage");
    assert_eq!(restored.state(), &expected_state);
}

#[test]
fn cross_runner_profileless_placement_reconstitutes_with_tombstone() {
    let initial = registration();
    let replacement = registration_for(runner_id(REPLACEMENT_RUNNER));
    let mut pin =
        SessionRunnerPlacement::new(session_id(SESSION), placement_request(profile("readonly")))
            .pin_and_offer_lease(
                &enrollment_for_registration(&initial),
                &initial,
                directory("/workspace/old"),
                None,
                authorized(
                    "inspect",
                    tool_attempt_id(ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            )
            .expect("the initial registration and lease satisfy placement");
    let prior_grant = pin
        .grant
        .take()
        .expect("the selected profile creates a prior grant");
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the profiled placement can be marked lost");
    let profileless = lost
        .replace_lost_runner(
            profileless_placement_request(),
            &replacement,
            directory("/workspace/new"),
            None,
            Some(prior_grant),
        )
        .expect("the replacement runner preserves the retired grant lineage");
    let tombstone = profileless
        .grant
        .expect("the checked replacement returns the prior runner tombstone");
    let expected_state = profileless.placement.state().clone();
    let input = placement_reconstitution_input(profileless.placement);

    let restored = SessionRunnerPlacement::reconstitute(
        input,
        session_id(SESSION),
        Some(&replacement),
        Some(&tombstone),
    )
    .expect("the prior runner tombstone authenticates the retained lineage");

    assert_eq!(restored.state(), &expected_state);
}

#[test]
fn profileless_lineage_rejects_an_omitted_tombstone() {
    let registration = registration();
    let replacement = registration_for(runner_id(REPLACEMENT_RUNNER));
    let mut pin = pinned("readonly").1;
    let prior_grant = pin
        .grant
        .take()
        .expect("profile selection creates a prior grant");
    let first_lost = pin
        .placement
        .mark_runner_lost()
        .expect("the profiled placement can be marked lost");
    let profileless = first_lost
        .replace_lost_runner(
            profileless_placement_request(),
            &replacement,
            directory("/workspace/session"),
            None,
            Some(prior_grant),
        )
        .expect("profileless replacement creates structural lineage evidence");
    let expected_lineage = profileless
        .grant
        .as_ref()
        .map(CredentialProfileGrant::lineage);
    let _omitted_tombstone = profileless
        .grant
        .expect("the profileless successor carries its tombstone");
    let second_lost = profileless
        .placement
        .mark_runner_lost()
        .expect("the profileless placement can be marked lost");

    assert_eq!(placement_grant_lineage(&second_lost), expected_lineage);
    assert_eq!(
        second_lost.replace_lost_runner(
            placement_request(profile("readonly")),
            &registration,
            directory("/workspace/session"),
            None,
            None,
        ),
        Err(RunnerDomainError::CorrelationMismatch),
    );
}

#[test]
fn workspace_cannot_cross_runner_ownership() {
    let registration = registration();
    let request = SessionRunnerPlacementRequest {
        selector: RunnerSelector::CapabilityClass(class()),
        working_directory: WorkingDirectorySelection::RunnerDefault,
        credential_profile: None,
        workspace: WorkspaceRequirement::RepositoryWorktree {
            repository: repository_key(),
        },
        sandbox: RunnerSandboxProfile::Ambient,
        permission_overrides: no_permission_overrides(),
    };
    let foreign_workspace = ProvisionedWorkspace {
        session: session_id(SESSION),
        placement_revision: RunnerGeneration::one(),
        runner: runner_id(REPLACEMENT_RUNNER),
        repository: Some(repository_key()),
        canonical_clone_url_digest: Some(
            CanonicalCloneUrlDigest::try_new("b".repeat(64))
                .expect("the fixture clone URL digest is canonical"),
        ),
        credential_profile: None,
        sandbox: RunnerSandboxProfile::Ambient,
        working_directory: directory("/workspace/session"),
        relative_path: WorkspaceRelativePath::try_new(format!(
            "sessions/{}/1/repo",
            session_id(SESSION).as_uuid()
        ))
        .expect("the fixture relative path is valid"),
        manifest_id: WorkspaceManifestId::from_uuid(uuid::Uuid::from_u128(0x7b00)),
        recovery: Some(WorkspaceRecovery::Commit {
            revision: WorkspaceRevision::try_new("c".repeat(40))
                .expect("the fixture recovery revision is canonical"),
        }),
    };

    assert_eq!(
        SessionRunnerPlacement::new(session_id(SESSION), request).pin_and_offer_lease(
            &enrollment_for_registration(&registration),
            &registration,
            directory("/workspace/session"),
            Some(foreign_workspace),
            authorized(
                "inspect",
                tool_attempt_id(ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        ),
        Err(RunnerDomainError::WorkspaceMismatch)
    );
}

#[test]
fn profile_pair_resolves_automatic_without_a_value() {
    let (_, _, _, lease) = offered("inspect", tool_attempt_id(ATTEMPT));
    let authorization = lease
        .credential_authorization()
        .expect("the selected profile authorizes the exact pair");

    assert_eq!(authorization.approval, CredentialToolApproval::Automatic);
    assert_eq!(authorization.profile, profile("readonly"));
}

#[test]
fn workspace_rejects_nondeterministic_relative_path() {
    let registration = registration();
    let request = SessionRunnerPlacementRequest {
        selector: RunnerSelector::CapabilityClass(class()),
        working_directory: WorkingDirectorySelection::RunnerDefault,
        credential_profile: None,
        workspace: WorkspaceRequirement::None,
        sandbox: RunnerSandboxProfile::WorkspaceRestricted,
        permission_overrides: no_permission_overrides(),
    };
    let private_root = ProvisionedWorkspace {
        session: session_id(SESSION),
        placement_revision: RunnerGeneration::one(),
        runner: registration.runner(),
        repository: None,
        canonical_clone_url_digest: None,
        credential_profile: None,
        sandbox: RunnerSandboxProfile::WorkspaceRestricted,
        working_directory: directory("/workspace/session"),
        relative_path: WorkspaceRelativePath::try_new(format!(
            "sessions/{}/1/alternate",
            session_id(SESSION).as_uuid()
        ))
        .expect("the mismatched path remains structurally safe"),
        manifest_id: WorkspaceManifestId::from_uuid(uuid::Uuid::from_u128(0x7b01)),
        recovery: None,
    };

    assert_eq!(
        SessionRunnerPlacement::new(session_id(SESSION), request).pin_and_offer_lease(
            &enrollment_for_registration(&registration),
            &registration,
            directory("/workspace/session"),
            Some(private_root),
            authorized(
                "inspect",
                tool_attempt_id(ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        ),
        Err(RunnerDomainError::WorkspaceMismatch)
    );
}

#[test]
fn ambient_runner_default_rejects_a_managed_private_root() {
    let registration = registration();
    let request = SessionRunnerPlacementRequest {
        selector: RunnerSelector::CapabilityClass(class()),
        working_directory: WorkingDirectorySelection::RunnerDefault,
        credential_profile: None,
        workspace: WorkspaceRequirement::None,
        sandbox: RunnerSandboxProfile::Ambient,
        permission_overrides: no_permission_overrides(),
    };
    let private_root = ProvisionedWorkspace {
        session: session_id(SESSION),
        placement_revision: RunnerGeneration::one(),
        runner: registration.runner(),
        repository: None,
        canonical_clone_url_digest: None,
        credential_profile: None,
        sandbox: RunnerSandboxProfile::Ambient,
        working_directory: directory("/workspace/session"),
        relative_path: WorkspaceRelativePath::try_new(format!(
            "sessions/{}/1/work",
            session_id(SESSION).as_uuid()
        ))
        .expect("the fixture relative path is valid"),
        manifest_id: WorkspaceManifestId::from_uuid(uuid::Uuid::from_u128(0x7b01)),
        recovery: None,
    };

    assert_eq!(
        SessionRunnerPlacement::new(session_id(SESSION), request).pin_and_offer_lease(
            &enrollment_for_registration(&registration),
            &registration,
            directory("/workspace/session"),
            Some(private_root),
            authorized(
                "inspect",
                tool_attempt_id(ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        ),
        Err(RunnerDomainError::WorkspaceMismatch)
    );
}

#[test]
fn exact_confirm_override_precedes_ambient_pure_auto() {
    let (registration, pin) = pinned_with_confirm_override("admin");
    let lease = pin
        .placement
        .offer_lease(
            &enrollment_for_registration(&registration),
            &registration,
            pin.grant.as_ref(),
            authorized(
                "inspect",
                tool_attempt_id(RETRY_ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        )
        .expect("the selected profile advertises the tool");
    let authorization = lease
        .credential_authorization()
        .expect("profile selection records pair posture");

    assert_eq!(
        authorization.approval,
        CredentialToolApproval::SessionPolicy
    );
}

#[test]
fn exact_confirm_override_rejects_automatic_approval() {
    let (registration, pin) = pinned_with_confirm_override("admin");

    assert_eq!(
        pin.placement.offer_lease(
            &enrollment_for_registration(&registration),
            &registration,
            pin.grant.as_ref(),
            automatically_authorized(
                "inspect",
                tool_attempt_id(RETRY_ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        ),
        Err(RunnerDomainError::CorrelationMismatch)
    );
}

#[test]
fn pair_automatic_accepts_tool_policy_approval() {
    let (registration, pin) = pinned("readonly");
    let lease = pin
        .placement
        .offer_lease(
            &enrollment_for_registration(&registration),
            &registration,
            pin.grant.as_ref(),
            automatically_authorized(
                "inspect",
                tool_attempt_id(RETRY_ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        )
        .expect("the pair-specific automatic posture permits policy approval");

    assert_eq!(
        lease
            .credential_authorization()
            .expect("the lease freezes pair authorization")
            .approval,
        catalog().profiles[&profile("readonly")].approval_for(&tool("inspect"))
    );
}

#[test]
fn exact_confirm_override_rejects_session_blanket_approval() {
    let (registration, pin) = pinned_with_confirm_override("admin");

    assert_eq!(
        pin.placement.offer_lease(
            &enrollment_for_registration(&registration),
            &registration,
            pin.grant.as_ref(),
            blanket_authorized(
                "inspect",
                tool_attempt_id(RETRY_ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        ),
        Err(RunnerDomainError::CorrelationMismatch)
    );
}

#[test]
fn exact_confirm_override_accepts_user_override_approval() {
    let (registration, pin) = pinned_with_confirm_override("admin");
    let lease = pin
        .placement
        .offer_lease(
            &enrollment_for_registration(&registration),
            &registration,
            pin.grant.as_ref(),
            user_override_authorized(
                "inspect",
                tool_attempt_id(RETRY_ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        )
        .expect("a one-shot user override confirms the session-policy pair");

    assert_eq!(
        lease
            .credential_authorization()
            .expect("profile selection records pair posture")
            .approval,
        CredentialToolApproval::SessionPolicy
    );
}

#[test]
fn revocation_does_not_rewrite_an_already_offered_lease() {
    let (_, _, mut grant, offered) = offered("inspect", tool_attempt_id(ATTEMPT));
    let correlation = offered.correlation();
    let revoked = grant
        .take()
        .expect("profile selection creates a grant")
        .revoke()
        .expect("an active grant can be revoked");

    let claimed = offered
        .claim(correlation)
        .expect("an already offered lease retains its fence");

    assert_eq!(revoked.state(), CredentialProfileGrantState::Revoked);
    assert_eq!(claimed.state(), RunnerLeaseState::Claimed);
}

#[test]
fn revocation_gates_later_lease_creation() {
    let (registration, mut pin) = pinned("readonly");
    let revoked = pin
        .grant
        .take()
        .expect("profile selection creates a grant")
        .revoke()
        .expect("an active grant can be revoked");

    assert_eq!(
        pin.placement.offer_lease(
            &enrollment_for_registration(&registration),
            &registration,
            Some(&revoked),
            authorized(
                "inspect",
                tool_attempt_id(RETRY_ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        ),
        Err(RunnerDomainError::GrantRevoked)
    );
}

#[test]
fn repository_profile_replacement_requires_reprovisioning() {
    let expected_enrollment = enrollment();
    let registration = expected_enrollment
        .register(
            RunnerAdvertisement::new(
                [class()],
                [tool("inspect"), tool("deploy"), tool("sync")],
                [profile("readonly"), profile("admin")],
                [WorkspaceCapability::WorktreePerSession],
                sandbox_profiles(),
                [RunnerRepositoryEntry::new(
                    repository_key(),
                    Some(profile("readonly")),
                )],
            ),
            &catalog(),
        )
        .expect("the repository entry binds its configured clone profile");
    let request = SessionRunnerPlacementRequest {
        selector: RunnerSelector::CapabilityClass(class()),
        working_directory: WorkingDirectorySelection::RunnerDefault,
        credential_profile: Some(profile("readonly")),
        workspace: WorkspaceRequirement::RepositoryWorktree {
            repository: repository_key(),
        },
        sandbox: RunnerSandboxProfile::Ambient,
        permission_overrides: no_permission_overrides(),
    };
    let workspace = ProvisionedWorkspace {
        session: session_id(SESSION),
        placement_revision: RunnerGeneration::one(),
        runner: registration.runner(),
        repository: Some(repository_key()),
        canonical_clone_url_digest: Some(
            CanonicalCloneUrlDigest::try_new("b".repeat(64))
                .expect("the fixture clone URL digest is canonical"),
        ),
        credential_profile: Some(profile("readonly")),
        sandbox: RunnerSandboxProfile::Ambient,
        working_directory: directory("/workspace/session"),
        relative_path: WorkspaceRelativePath::try_new(format!(
            "sessions/{}/1/repo",
            session_id(SESSION).as_uuid()
        ))
        .expect("the fixture relative path is valid"),
        manifest_id: WorkspaceManifestId::from_uuid(uuid::Uuid::from_u128(0x7b02)),
        recovery: Some(WorkspaceRecovery::Commit {
            revision: WorkspaceRevision::try_new("c".repeat(40))
                .expect("the fixture recovery revision is canonical"),
        }),
    };
    let mut pin = SessionRunnerPlacement::new(session_id(SESSION), request)
        .pin_and_offer_lease(
            &expected_enrollment,
            &registration,
            directory("/workspace/session"),
            Some(workspace),
            authorized(
                "inspect",
                tool_attempt_id(ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        )
        .expect("the configured profile provisions the repository");
    let grant = pin
        .grant
        .take()
        .expect("the configured profile creates a grant");

    assert_eq!(
        pin.placement.replace_credential_profile(
            grant,
            &registration,
            profile("admin"),
            [tool("deploy")],
        ),
        Err(RunnerDomainError::CredentialProfileUnavailable)
    );
}

#[test]
fn replacement_binds_profile_grant_to_placement() {
    let (registration, mut pin) = pinned("readonly");
    let grant = pin.grant.take().expect("profile selection creates a grant");
    let expected_before_tools = grant.tools.clone();
    let replacement_tools = BTreeSet::from([tool("deploy")]);

    let replaced = pin
        .placement
        .replace_credential_profile(
            grant,
            &registration,
            profile("admin"),
            replacement_tools.clone(),
        )
        .expect("the explicit replacement binds placement and grant");

    assert_eq!(replaced.grant.grant.profile(), &profile("admin"));
    assert_eq!(
        replaced.grant.grant.revision(),
        RunnerGeneration::try_from_u64(2).expect("two is positive")
    );
    assert_eq!(replaced.grant.change.before_tools, expected_before_tools);
    assert_eq!(replaced.grant.change.after_tools, replacement_tools);
    assert_eq!(
        replaced.placement_change.after.credential_profile,
        Some(profile("admin"))
    );
}

#[test]
fn profile_replacement_rejects_stale_registration() {
    let (registration, mut pin) = pinned("readonly");
    let retained = registration.clone();
    let current = enrollment_for_registration(&registration)
        .register(advertisement(), &catalog())
        .expect("the later registration retires the retained snapshot");
    let grant = pin.grant.take().expect("profile selection creates a grant");

    assert_eq!(
        pin.placement.replace_credential_profile(
            grant,
            &retained,
            profile("admin"),
            BTreeSet::from([tool("deploy")]),
        ),
        Err(RunnerDomainError::RegistrationChanged)
    );
    assert_ne!(retained.revision(), current.revision());
}

#[test]
fn profile_replacement_rejects_runner_only_omission() {
    let (_, mut pin) = pinned("readonly");
    let grant = pin.grant.take().expect("profile selection creates a grant");
    let narrowed_registration = enrollment()
        .register(
            RunnerAdvertisement::new(
                [class()],
                [tool("inspect"), tool("deploy")],
                [profile("readonly"), profile("admin")],
                [WorkspaceCapability::WorktreePerSession],
                sandbox_profiles(),
                [],
            ),
            &catalog(),
        )
        .expect("the narrowed advertisement remains catalog-valid");

    assert_eq!(
        pin.placement.replace_credential_profile(
            grant,
            &narrowed_registration,
            profile("admin"),
            [tool("deploy")],
        ),
        Err(RunnerDomainError::RegistrationChanged)
    );
}

#[test]
fn grant_reconstitution_accepts_raw_active_facts() {
    let (registration, mut pin) = pinned("readonly");
    let grant = pin.grant.take().expect("profile selection creates a grant");
    let expected_profile = grant.profile().clone();
    let input = grant_reconstitution_input(grant);

    let reconstituted = CredentialProfileGrant::reconstitute(
        input,
        session_id(SESSION),
        &registration,
        RunnerSandboxProfile::Ambient,
        &no_permission_overrides(),
    )
    .expect("complete active grant facts reconstitute");

    assert_eq!(reconstituted.profile(), &expected_profile);
}

#[test]
fn grant_reconstitution_rejects_changed_pair_policy() {
    let (registration, mut pin) = pinned("readonly");
    let grant = pin.grant.take().expect("profile selection creates a grant");
    let mut input = grant_reconstitution_input(grant);
    input
        .approvals
        .insert(tool("inspect"), CredentialToolApproval::SessionPolicy);

    assert_eq!(
        CredentialProfileGrant::reconstitute(
            input,
            session_id(SESSION),
            &registration,
            RunnerSandboxProfile::Ambient,
            &no_permission_overrides(),
        ),
        Err(RunnerDomainError::CorruptStoredFacts)
    );
}

#[test]
fn grant_reconstitution_rejects_cross_wired_session() {
    let (registration, mut pin) = pinned("readonly");
    let grant = pin.grant.take().expect("profile selection creates a grant");
    let input = grant_reconstitution_input(grant);

    assert_eq!(
        CredentialProfileGrant::reconstitute(
            input,
            session_id(SESSION + 1),
            &registration,
            RunnerSandboxProfile::Ambient,
            &no_permission_overrides(),
        ),
        Err(RunnerDomainError::CorruptStoredFacts)
    );
}
#[test]
fn profileless_confirm_rejects_policy_auto_authorization() {
    let registration = registration();
    let pin = SessionRunnerPlacement::new(session_id(SESSION), profileless_placement_request())
        .pin_and_offer_lease(
            &enrollment_for_registration(&registration),
            &registration,
            directory("/workspace/session"),
            None,
            automatically_authorized(
                "inspect",
                tool_attempt_id(ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        )
        .expect("automatic profileless work can pin the runner");

    assert_eq!(
        pin.placement.offer_lease(
            &enrollment_for_registration(&registration),
            &registration,
            None,
            automatically_authorized(
                "sync",
                tool_attempt_id(RETRY_ATTEMPT),
                RunnerToolEffectClass::Idempotent,
            ),
            lease_offer_request("sync"),
        ),
        Err(RunnerDomainError::CorrelationMismatch)
    );
}

#[test]
fn profileless_confirm_rejects_session_blanket_authorization() {
    let registration = registration();
    let pin = SessionRunnerPlacement::new(session_id(SESSION), profileless_placement_request())
        .pin_and_offer_lease(
            &enrollment_for_registration(&registration),
            &registration,
            directory("/workspace/session"),
            None,
            automatically_authorized(
                "inspect",
                tool_attempt_id(ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        )
        .expect("automatic profileless work can pin the runner");

    assert_eq!(
        pin.placement.offer_lease(
            &enrollment_for_registration(&registration),
            &registration,
            None,
            blanket_authorized(
                "sync",
                tool_attempt_id(RETRY_ATTEMPT),
                RunnerToolEffectClass::Idempotent,
            ),
            lease_offer_request("sync"),
        ),
        Err(RunnerDomainError::CorrelationMismatch)
    );
}

#[test]
fn lost_unclaimed_lease_reconstitutes_retry_authority() {
    let (registration, _, _, offered) = offered("inspect", tool_attempt_id(ATTEMPT));
    let proof = no_execution_proof(&offered);
    let loss = offered
        .lose_unclaimed(&proof)
        .expect("proof-backed unclaimed loss is checked");
    let expected_generation = loss
        .retry()
        .expect("unclaimed pure loss carries retry authority")
        .generation();
    let input = borrowed_lease_reconstitution_input(loss.lost());

    let restored =
        RunnerLease::reconstitute_loss(input, &registration, Some(proof.correlation().clone()))
            .expect("complete lost facts and proof restore the checked consequence");

    assert_eq!(
        restored
            .retry()
            .expect("restored unclaimed pure loss carries retry authority")
            .generation(),
        expected_generation
    );
    assert_eq!(restored.crash_attempt(), None);
}

#[test]
fn loss_reconstitution_restores_consumed_retry_preparation() {
    let (registration, _, _, offered) = offered("inspect", tool_attempt_id(ATTEMPT));
    let loss = offered
        .lose()
        .expect("an execution-possible pure lease carries retry authority");
    let mut input = borrowed_lease_reconstitution_input(loss.lost());
    input.retry_preparation = RunnerLeaseRetryPreparation::Prepared;
    let restored = RunnerLease::reconstitute_loss(input, &registration, None)
        .expect("the durable consumed preparation state reconstitutes");

    assert_eq!(
        restored
            .retry()
            .expect("the restored loss retains its durable identity")
            .prepare_claimed_attempt(
                claimed_batch("inspect", RunnerToolEffectClass::Pure),
                tool_attempt_id(RETRY_ATTEMPT),
            ),
        Err(RunnerDomainError::InvalidState)
    );
}

#[test]
fn lost_unclaimed_reconstitution_requires_no_execution_proof() {
    let (registration, _, _, offered) = offered("inspect", tool_attempt_id(ATTEMPT));
    let proof = no_execution_proof(&offered);
    let loss = offered
        .lose_unclaimed(&proof)
        .expect("proof-backed unclaimed loss is checked");
    let input = borrowed_lease_reconstitution_input(loss.lost());

    assert_eq!(
        RunnerLease::reconstitute_loss(input, &registration, None),
        Err(RunnerDomainError::InvalidState)
    );
}

#[test]
fn lost_claimed_side_effect_reconstitutes_crash_authority() {
    let (registration, _, _, offered) = offered("deploy", tool_attempt_id(ATTEMPT));
    let correlation = offered.correlation();
    let expected_attempt = offered.attempt();
    let claimed = offered
        .claim(correlation)
        .expect("the exact fence claims the offered lease");
    let loss = claimed.lose().expect("a claimed lease can be lost");
    let input = borrowed_lease_reconstitution_input(loss.lost());

    let restored = RunnerLease::reconstitute_loss(input, &registration, None)
        .expect("complete lost facts restore crash classification authority");

    assert_eq!(restored.retry(), None);
    assert_eq!(restored.crash_attempt(), Some(expected_attempt));
}

#[test]
fn nonlost_lease_cannot_reconstitute_a_loss_consequence() {
    let (registration, _, _, offered) = offered("inspect", tool_attempt_id(ATTEMPT));
    let input = lease_reconstitution_input(offered);

    assert_eq!(
        RunnerLease::reconstitute_loss(input, &registration, None),
        Err(RunnerDomainError::InvalidState)
    );
}

#[test]
fn runner_replacement_reports_complete_grant_change() {
    let (registration, mut pin) = pinned("readonly");
    let initial_grant = pin.grant.take().expect("profile selection creates a grant");
    let narrowed = pin
        .placement
        .replace_credential_profile(
            initial_grant,
            &registration,
            profile("readonly"),
            [tool("inspect")],
        )
        .expect("the explicit profile replacement narrows the grant");
    let expected_before_tools = narrowed.grant.change.after_tools.clone();
    let lost = narrowed
        .placement
        .mark_runner_lost()
        .expect("the narrowed placement can be marked lost");
    let replacement = registration_for(runner_id(REPLACEMENT_RUNNER));
    let replaced = lost
        .replace_lost_runner(
            placement_request(profile("readonly")),
            &replacement,
            directory("/workspace/session"),
            None,
            Some(narrowed.grant.grant),
        )
        .expect("runner replacement advances the narrowed grant");
    let expected_after_tools = replaced
        .grant
        .as_ref()
        .map(|grant| grant.tools.clone())
        .expect("the replacement carries its successor grant inventory");
    let change = replaced
        .grant_change
        .expect("credential-bearing replacement reports grant change facts");

    assert_eq!(
        change
            .before
            .expect("a prior grant supplies before facts")
            .tools,
        expected_before_tools
    );
    assert_eq!(
        change
            .after
            .expect("a successor grant supplies after facts")
            .tools,
        expected_after_tools
    );
}

#[test]
fn runner_tool_model_definition_rejects_a_schema_exceeding_the_storage_bound() {
    let schema = serde_json::json!({"description": "x".repeat(1024 * 1024)}).to_string();
    assert_eq!(
        RunnerToolModelDefinition::try_new("Inspect the workspace".to_owned(), schema),
        Err(RunnerDomainError::InvalidToolInputSchema),
    );
}
