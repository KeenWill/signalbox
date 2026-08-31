//! Explicit mapping between sealed runner lease authority and version-one wire frames.

use std::{error::Error, fmt};

use signalbox_application::{RunnerLeaseClaimRequest, RunnerLeaseResultRequest};
use signalbox_domain::{
    NormalizedToolArguments, RunnerGeneration, RunnerLease, RunnerLeaseCorrelation, RunnerLeaseId,
    RunnerLeaseState, RunnerSandboxProfile, RunnerToolEffectClass, RunnerWorkingDirectory,
    SessionId, ToolArgumentsKind, ToolAttemptDispatchCorrelation,
    ToolAttemptDispatchCorrelationReconstitutionInput, ToolAttemptId, ToolAttemptObservation,
    ToolDispatchGeneration, ToolExecutionError, ToolExecutionErrorDetail, ToolExecutionErrorKind,
    ToolName, ToolRequestId, ToolResultContent, ToolResultText, TurnAttemptId, TurnId,
};
use signalbox_runner_wire::{
    CanonicalUuid, Dispatch, EffectClass, ExecutionErrorKind, LeaseClaim, LeaseClaimed,
    LeaseCorrelation, LeaseOffer, Message, PositiveU64, ProfileName, ResultBounds, ResultFrame,
    ResultRecorded, SandboxProfile, TerminalResult, ValueError, WireToolName, WorkingDirectory,
};

/// A sealed domain lease could not be represented by the closed version-one wire.
#[derive(Debug)]
pub enum RunnerDispatchWireError {
    /// The requested frame is not valid for the lease's durable lifecycle state.
    InvalidLeaseState,
    /// The normalized argument representation is not dispatchable JSON.
    ArgumentsNotJson,
    /// Canonical argument text could not be decoded into the wire value.
    ArgumentsDecode(serde_json::Error),
    /// A checked wire value or cross-member invariant was violated.
    Wire(ValueError),
    /// A wire correlation could not be reconstituted as domain facts.
    Correlation,
    /// A bounded wire result could not be reconstituted as domain evidence.
    Result,
}

impl fmt::Display for RunnerDispatchWireError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidLeaseState => "runner lease state cannot emit the requested frame",
            Self::ArgumentsNotJson => "runner lease arguments are not dispatchable JSON",
            Self::ArgumentsDecode(_) => "runner lease arguments could not be decoded",
            Self::Wire(_) => "runner lease facts violate the version-one wire",
            Self::Correlation => "runner wire correlation is not valid domain evidence",
            Self::Result => "runner wire result is not valid domain evidence",
        })
    }
}

impl Error for RunnerDispatchWireError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ArgumentsDecode(error) => Some(error),
            Self::Wire(error) => Some(error),
            Self::InvalidLeaseState | Self::ArgumentsNotJson | Self::Correlation | Self::Result => {
                None
            }
        }
    }
}

impl From<ValueError> for RunnerDispatchWireError {
    fn from(error: ValueError) -> Self {
        Self::Wire(error)
    }
}

/// Stateless boundary mapper for daemon runner-dispatch frames.
#[derive(Clone, Copy, Debug, Default)]
pub struct RunnerDispatchWireAdapter;

impl RunnerDispatchWireAdapter {
    /// Projects one durably offered lease into the complete offer frame.
    pub fn lease_offer(lease: &RunnerLease) -> Result<Message, RunnerDispatchWireError> {
        require_state(lease, RunnerLeaseState::Offered)?;
        let (credential_profile, grant_revision) = credential_wire_facts(lease)?;
        validate_message(Message::LeaseOffer(LeaseOffer {
            correlation: wire_correlation(&lease.correlation())?,
            effect_class: wire_effect(lease.effect()),
            credential_profile,
            grant_revision,
            normalized_arguments: wire_arguments(lease.arguments())?,
            result_bounds: ResultBounds::version_one(),
        }))
    }

    /// Projects the canonical claimed lease into its durable-before-ack frame.
    pub fn lease_claimed(lease: &RunnerLease) -> Result<Message, RunnerDispatchWireError> {
        require_state(lease, RunnerLeaseState::Claimed)?;
        validate_message(Message::LeaseClaimed(LeaseClaimed {
            correlation: wire_correlation(&lease.correlation())?,
        }))
    }

    /// Projects the canonical claimed lease into its execution-capability frame.
    pub fn dispatch(lease: &RunnerLease) -> Result<Message, RunnerDispatchWireError> {
        require_state(lease, RunnerLeaseState::Claimed)?;
        validate_message(Message::Dispatch(Dispatch {
            correlation: wire_correlation(&lease.correlation())?,
            normalized_arguments: wire_arguments(lease.arguments())?,
        }))
    }

    /// Projects the canonical completed lease returned by the atomic terminal boundary.
    pub fn result_recorded(lease: &RunnerLease) -> Result<Message, RunnerDispatchWireError> {
        require_state(lease, RunnerLeaseState::Completed)?;
        validate_message(Message::ResultRecorded(ResultRecorded {
            correlation: wire_correlation(&lease.correlation())?,
        }))
    }

    /// Reconstitutes one inbound lease claim as the application transaction request.
    pub fn claim_request(
        claim: LeaseClaim,
    ) -> Result<RunnerLeaseClaimRequest, RunnerDispatchWireError> {
        Ok(RunnerLeaseClaimRequest::new(domain_correlation(
            claim.correlation,
        )?))
    }

    /// Reconstitutes one inbound terminal result as the application transaction request.
    pub fn result_request(
        result: ResultFrame,
    ) -> Result<RunnerLeaseResultRequest, RunnerDispatchWireError> {
        result
            .result
            .validate()
            .map_err(RunnerDispatchWireError::Wire)?;
        Ok(RunnerLeaseResultRequest::new(
            domain_correlation(result.correlation)?,
            domain_observation(result.result)?,
        ))
    }
}

fn require_state(
    lease: &RunnerLease,
    expected: RunnerLeaseState,
) -> Result<(), RunnerDispatchWireError> {
    if lease.state() == expected {
        Ok(())
    } else {
        Err(RunnerDispatchWireError::InvalidLeaseState)
    }
}

fn credential_wire_facts(
    lease: &RunnerLease,
) -> Result<(Option<ProfileName>, Option<PositiveU64>), RunnerDispatchWireError> {
    match lease.credential_authorization() {
        Some(authorization) => Ok((
            Some(ProfileName::try_new(
                authorization.profile.as_str().to_owned(),
            )?),
            Some(PositiveU64::try_new(authorization.grant_revision.get())?),
        )),
        None => Ok((None, None)),
    }
}

fn wire_arguments(
    arguments: &NormalizedToolArguments,
) -> Result<serde_json::Value, RunnerDispatchWireError> {
    if arguments.kind() != ToolArgumentsKind::Json {
        return Err(RunnerDispatchWireError::ArgumentsNotJson);
    }
    serde_json::from_str(arguments.as_str()).map_err(RunnerDispatchWireError::ArgumentsDecode)
}

fn wire_correlation(
    correlation: &RunnerLeaseCorrelation,
) -> Result<LeaseCorrelation, RunnerDispatchWireError> {
    Ok(LeaseCorrelation {
        registration_revision: PositiveU64::try_new(correlation.registration_revision.get())?,
        lease_id: CanonicalUuid::from_uuid(correlation.lease.into_uuid()),
        lease_generation: PositiveU64::try_new(correlation.generation.get())?,
        runner_id: CanonicalUuid::from_uuid(correlation.runner.into_uuid()),
        placement_revision: PositiveU64::try_new(correlation.placement_revision.get())?,
        working_directory: WorkingDirectory::try_new(
            correlation.working_directory.as_str().to_owned(),
        )?,
        sandbox_profile: wire_sandbox(correlation.sandbox),
        tool_name: WireToolName::try_new(correlation.tool.as_str().to_owned())?,
        session_id: CanonicalUuid::from_uuid(correlation.dispatch.session().into_uuid()),
        turn_id: CanonicalUuid::from_uuid(correlation.dispatch.turn().into_uuid()),
        tool_request_id: CanonicalUuid::from_uuid(correlation.dispatch.request().into_uuid()),
        tool_attempt_id: CanonicalUuid::from_uuid(correlation.dispatch.attempt().into_uuid()),
        issuing_turn_attempt_id: CanonicalUuid::from_uuid(
            correlation.dispatch.issuing_attempt().into_uuid(),
        ),
        tool_dispatch_generation: PositiveU64::try_new(correlation.dispatch.generation().as_u64())?,
    })
}

fn domain_correlation(
    correlation: LeaseCorrelation,
) -> Result<RunnerLeaseCorrelation, RunnerDispatchWireError> {
    Ok(RunnerLeaseCorrelation {
        lease: RunnerLeaseId::from_uuid(correlation.lease_id.into_uuid()),
        runner: signalbox_domain::RunnerId::from_uuid(correlation.runner_id.into_uuid()),
        registration_revision: domain_generation(correlation.registration_revision)?,
        placement_revision: domain_generation(correlation.placement_revision)?,
        working_directory: RunnerWorkingDirectory::try_new(
            correlation.working_directory.as_str().to_owned(),
        )
        .map_err(|_| RunnerDispatchWireError::Correlation)?,
        sandbox: domain_sandbox(correlation.sandbox_profile),
        tool: ToolName::try_new(correlation.tool_name.as_str().to_owned())
            .map_err(|_| RunnerDispatchWireError::Correlation)?,
        dispatch: ToolAttemptDispatchCorrelation::reconstitute(
            ToolAttemptDispatchCorrelationReconstitutionInput {
                session: SessionId::from_uuid(correlation.session_id.into_uuid()),
                turn: TurnId::from_uuid(correlation.turn_id.into_uuid()),
                issuing_attempt: TurnAttemptId::from_uuid(
                    correlation.issuing_turn_attempt_id.into_uuid(),
                ),
                request: ToolRequestId::from_uuid(correlation.tool_request_id.into_uuid()),
                attempt: ToolAttemptId::from_uuid(correlation.tool_attempt_id.into_uuid()),
                generation: ToolDispatchGeneration::try_from_u64(
                    correlation.tool_dispatch_generation.get(),
                )
                .ok_or(RunnerDispatchWireError::Correlation)?,
            },
        ),
        generation: domain_generation(correlation.lease_generation)?,
    })
}

fn domain_generation(value: PositiveU64) -> Result<RunnerGeneration, RunnerDispatchWireError> {
    RunnerGeneration::try_from_u64(value.get()).ok_or(RunnerDispatchWireError::Correlation)
}

const fn wire_sandbox(sandbox: RunnerSandboxProfile) -> SandboxProfile {
    match sandbox {
        RunnerSandboxProfile::Ambient => SandboxProfile::Ambient,
        RunnerSandboxProfile::WorkspaceRestricted => SandboxProfile::WorkspaceRestricted,
    }
}

const fn domain_sandbox(sandbox: SandboxProfile) -> RunnerSandboxProfile {
    match sandbox {
        SandboxProfile::Ambient => RunnerSandboxProfile::Ambient,
        SandboxProfile::WorkspaceRestricted => RunnerSandboxProfile::WorkspaceRestricted,
    }
}

const fn wire_effect(effect: RunnerToolEffectClass) -> EffectClass {
    match effect {
        RunnerToolEffectClass::Pure => EffectClass::Pure,
        RunnerToolEffectClass::Idempotent => EffectClass::Idempotent,
        RunnerToolEffectClass::SideEffecting => EffectClass::SideEffecting,
    }
}

fn domain_observation(
    result: TerminalResult,
) -> Result<ToolAttemptObservation, RunnerDispatchWireError> {
    match result {
        TerminalResult::Success { text } => Ok(ToolAttemptObservation::Completed {
            result: ToolResultContent::Text(
                ToolResultText::try_new(text).map_err(|_| RunnerDispatchWireError::Result)?,
            ),
        }),
        TerminalResult::KnownFailure { error_kind, detail } => {
            let detail = detail
                .map(ToolExecutionErrorDetail::try_new)
                .transpose()
                .map_err(|_| RunnerDispatchWireError::Result)?;
            Ok(ToolAttemptObservation::KnownFailed {
                error: ToolExecutionError::new(domain_error_kind(error_kind), detail),
            })
        }
        TerminalResult::Ambiguous => Ok(ToolAttemptObservation::Ambiguous),
    }
}

const fn domain_error_kind(kind: ExecutionErrorKind) -> ToolExecutionErrorKind {
    match kind {
        ExecutionErrorKind::UnknownTool => ToolExecutionErrorKind::UnknownTool,
        ExecutionErrorKind::InvalidArguments => ToolExecutionErrorKind::InvalidArguments,
        ExecutionErrorKind::ExecutionFailed => ToolExecutionErrorKind::ExecutionFailed,
        ExecutionErrorKind::ResultTooLarge => ToolExecutionErrorKind::ResultTooLarge,
        ExecutionErrorKind::CrashLost => ToolExecutionErrorKind::CrashLost,
    }
}

fn validate_message(message: Message) -> Result<Message, RunnerDispatchWireError> {
    message.validate().map_err(RunnerDispatchWireError::Wire)?;
    Ok(message)
}

#[cfg(test)]
mod tests {
    use signalbox_domain::{
        CredentialDispatchAuthorization, CredentialProfileName, CredentialProfilePolicy,
        CredentialToolApproval, NormalizedToolArguments, RunnerAdvertisement,
        RunnerAuthenticationId, RunnerCapabilityClass, RunnerCatalog, RunnerEnrollment,
        RunnerEnrollmentId, RunnerGeneration, RunnerId, RunnerLease, RunnerLeaseCorrelation,
        RunnerLeaseId, RunnerLeaseReconstitutionInput, RunnerLeaseRetryPreparation,
        RunnerLeaseState, RunnerRepositoryEntry, RunnerSandboxProfile, RunnerSelector,
        RunnerToolDeclaration, RunnerToolEffectClass, RunnerToolModelDefinition,
        RunnerWorkingDirectory, SessionId, ToolAdmissibleLoci, ToolAttemptDispatchCorrelation,
        ToolAttemptDispatchCorrelationReconstitutionInput, ToolAttemptId, ToolAttemptObservation,
        ToolDispatchGeneration, ToolExecutionError, ToolExecutionErrorDetail,
        ToolExecutionErrorKind, ToolName, ToolPermissionDefault, ToolRequestId, TurnAttemptId,
        TurnId, ValidatedRunnerRegistration, WorkspaceCapability,
    };
    use signalbox_runner_wire::{
        CanonicalUuid, Dispatch, EffectClass, LeaseClaim, LeaseClaimed, LeaseCorrelation,
        LeaseOffer, Message, PositiveU64, ResultBounds, ResultFrame, ResultRecorded,
        SandboxProfile, TerminalResult, WireToolName, WorkingDirectory,
    };
    use uuid::Uuid;

    use super::{RunnerDispatchWireAdapter, RunnerDispatchWireError};

    const ENROLLMENT: u128 = 1;
    const RUNNER: u128 = 2;
    const AUTHENTICATION: u128 = 3;
    const SESSION: u128 = 4;
    const TURN: u128 = 5;
    const REQUEST: u128 = 6;
    const ATTEMPT: u128 = 7;
    const TURN_ATTEMPT: u128 = 8;
    const LEASE: u128 = 9;

    fn id(value: u128) -> Uuid {
        Uuid::from_u128(value)
    }

    fn tool() -> ToolName {
        ToolName::try_new("sandboxed_exec".to_owned()).expect("the fixture tool name is portable")
    }

    fn class() -> RunnerCapabilityClass {
        RunnerCapabilityClass::try_new("linux.workspace".to_owned())
            .expect("the fixture capability class is portable")
    }

    fn profile() -> CredentialProfileName {
        CredentialProfileName::try_new("fixture-profile".to_owned())
            .expect("the fixture profile name is portable")
    }

    fn wire_arguments() -> serde_json::Value {
        serde_json::json!({"argv": ["printf", "runner"]})
    }

    fn arguments() -> NormalizedToolArguments {
        NormalizedToolArguments::try_from_provider_text(wire_arguments().to_string())
            .expect("the fixture arguments are canonical")
    }

    fn registration() -> ValidatedRunnerRegistration {
        let declaration = RunnerToolDeclaration::new(
            tool(),
            RunnerToolModelDefinition::try_new(
                "Execute one generic sandboxed command".to_owned(),
                r#"{"type":"object"}"#.to_owned(),
            )
            .expect("the fixture model definition is valid"),
            ToolPermissionDefault::Auto,
            RunnerToolEffectClass::Pure,
            ToolAdmissibleLoci::RunnerOnly {
                selector: RunnerSelector::CapabilityClass(class()),
            },
        );
        let catalog = RunnerCatalog::try_new(
            [class()],
            [declaration],
            [CredentialProfilePolicy::try_new(
                profile(),
                [(tool(), CredentialToolApproval::Automatic)],
            )
            .expect("the fixture profile references the declared tool")],
            Vec::<WorkspaceCapability>::new(),
            [RunnerSandboxProfile::WorkspaceRestricted],
        )
        .expect("the fixture catalog is internally consistent");
        let enrollment = RunnerEnrollment::new(
            RunnerEnrollmentId::from_uuid(id(ENROLLMENT)),
            RunnerId::from_uuid(id(RUNNER)),
            RunnerAuthenticationId::from_uuid(id(AUTHENTICATION)),
            [class()],
        );
        enrollment
            .register(
                RunnerAdvertisement::new(
                    [class()],
                    [tool()],
                    [profile()],
                    [],
                    [RunnerSandboxProfile::WorkspaceRestricted],
                    Vec::<RunnerRepositoryEntry>::new(),
                ),
                &catalog,
            )
            .expect("the fixture advertisement is admitted")
    }

    fn correlation() -> RunnerLeaseCorrelation {
        RunnerLeaseCorrelation {
            lease: RunnerLeaseId::from_uuid(id(LEASE)),
            runner: RunnerId::from_uuid(id(RUNNER)),
            registration_revision: RunnerGeneration::one(),
            placement_revision: RunnerGeneration::one(),
            working_directory: RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
                .expect("the fixture working directory is exact"),
            sandbox: RunnerSandboxProfile::WorkspaceRestricted,
            tool: tool(),
            dispatch: ToolAttemptDispatchCorrelation::reconstitute(
                ToolAttemptDispatchCorrelationReconstitutionInput {
                    session: SessionId::from_uuid(id(SESSION)),
                    turn: TurnId::from_uuid(id(TURN)),
                    issuing_attempt: TurnAttemptId::from_uuid(id(TURN_ATTEMPT)),
                    request: ToolRequestId::from_uuid(id(REQUEST)),
                    attempt: ToolAttemptId::from_uuid(id(ATTEMPT)),
                    generation: ToolDispatchGeneration::first(),
                },
            ),
            generation: RunnerGeneration::one(),
        }
    }

    fn wire_correlation() -> LeaseCorrelation {
        LeaseCorrelation {
            registration_revision: PositiveU64::try_new(1)
                .expect("the fixture registration revision is positive"),
            lease_id: CanonicalUuid::from_uuid(id(LEASE)),
            lease_generation: PositiveU64::try_new(1)
                .expect("the fixture lease generation is positive"),
            runner_id: CanonicalUuid::from_uuid(id(RUNNER)),
            placement_revision: PositiveU64::try_new(1)
                .expect("the fixture placement revision is positive"),
            working_directory: WorkingDirectory::try_new("/workspace/session".to_owned())
                .expect("the fixture wire directory is exact"),
            sandbox_profile: SandboxProfile::WorkspaceRestricted,
            tool_name: WireToolName::try_new("sandboxed_exec".to_owned())
                .expect("the fixture wire tool name is portable"),
            session_id: CanonicalUuid::from_uuid(id(SESSION)),
            turn_id: CanonicalUuid::from_uuid(id(TURN)),
            tool_request_id: CanonicalUuid::from_uuid(id(REQUEST)),
            tool_attempt_id: CanonicalUuid::from_uuid(id(ATTEMPT)),
            issuing_turn_attempt_id: CanonicalUuid::from_uuid(id(TURN_ATTEMPT)),
            tool_dispatch_generation: PositiveU64::try_new(1)
                .expect("the fixture dispatch generation is positive"),
        }
    }

    fn lease_with(
        state: RunnerLeaseState,
        arguments: NormalizedToolArguments,
        credential_authorization: Option<CredentialDispatchAuthorization>,
    ) -> RunnerLease {
        let registration = registration();
        let correlation = correlation();
        RunnerLease::reconstitute(
            RunnerLeaseReconstitutionInput {
                lease: correlation.lease,
                dispatch: correlation.dispatch,
                runner: correlation.runner,
                registration_revision: correlation.registration_revision,
                placement_revision: correlation.placement_revision,
                working_directory: correlation.working_directory.clone(),
                sandbox: correlation.sandbox,
                tool: correlation.tool.clone(),
                arguments: arguments.clone(),
                effect: RunnerToolEffectClass::Pure,
                credential_authorization: credential_authorization.clone(),
                generation: correlation.generation,
                state,
                recorded_correlation: correlation,
                recorded_session: SessionId::from_uuid(id(SESSION)),
                recorded_effect: RunnerToolEffectClass::Pure,
                recorded_arguments: arguments,
                recorded_credential_authorization: credential_authorization,
                recorded_state: state,
                retry_preparation: RunnerLeaseRetryPreparation::Available,
            },
            &registration,
        )
        .expect("the fixture lease facts are self-consistent")
    }

    fn lease(state: RunnerLeaseState) -> RunnerLease {
        lease_with(state, arguments(), None)
    }

    #[test]
    fn s16_inv043_offered_lease_projects_the_complete_version_one_frame() {
        let lease = lease(RunnerLeaseState::Offered);
        let expected_correlation = lease.correlation();
        let message = RunnerDispatchWireAdapter::lease_offer(&lease)
            .expect("the sealed offered lease projects");
        let wire_correlation = wire_correlation();

        assert_eq!(
            message,
            Message::LeaseOffer(LeaseOffer {
                correlation: wire_correlation.clone(),
                effect_class: EffectClass::Pure,
                credential_profile: None,
                grant_revision: None,
                normalized_arguments: wire_arguments(),
                result_bounds: ResultBounds::version_one(),
            })
        );
        assert_eq!(
            RunnerDispatchWireAdapter::claim_request(LeaseClaim {
                correlation: wire_correlation,
            })
            .expect("the projected correlation reconstitutes")
            .into_correlation(),
            expected_correlation
        );
    }

    #[test]
    fn s16_inv043_claimed_lease_projects_acknowledgement_and_dispatch_from_the_same_authority() {
        let claimed = lease(RunnerLeaseState::Claimed);
        let acknowledgement = RunnerDispatchWireAdapter::lease_claimed(&claimed)
            .expect("the claimed lease projects its acknowledgement");
        let dispatch = RunnerDispatchWireAdapter::dispatch(&claimed)
            .expect("the claimed lease projects its dispatch");

        assert_eq!(
            acknowledgement,
            Message::LeaseClaimed(LeaseClaimed {
                correlation: wire_correlation(),
            })
        );
        assert_eq!(
            dispatch,
            Message::Dispatch(Dispatch {
                correlation: wire_correlation(),
                normalized_arguments: wire_arguments(),
            })
        );
    }

    #[test]
    fn s16_inv043_offered_lease_projects_credential_profile_and_grant_as_one_pair() {
        let offered = lease_with(
            RunnerLeaseState::Offered,
            arguments(),
            Some(CredentialDispatchAuthorization {
                session: SessionId::from_uuid(id(SESSION)),
                runner: RunnerId::from_uuid(id(RUNNER)),
                grant_revision: RunnerGeneration::one(),
                profile: profile(),
                tool: tool(),
                approval: CredentialToolApproval::Automatic,
            }),
        );
        let expected_correlation = wire_correlation();

        assert_eq!(
            RunnerDispatchWireAdapter::lease_offer(&offered)
                .expect("the credential-bound lease projects"),
            Message::LeaseOffer(LeaseOffer {
                correlation: expected_correlation,
                effect_class: EffectClass::Pure,
                credential_profile: Some(
                    signalbox_runner_wire::ProfileName::try_new(profile().as_str().to_owned())
                        .expect("the fixture profile maps"),
                ),
                grant_revision: Some(
                    signalbox_runner_wire::PositiveU64::try_new(1)
                        .expect("the fixture grant revision is positive"),
                ),
                normalized_arguments: wire_arguments(),
                result_bounds: ResultBounds::version_one(),
            })
        );
    }

    #[test]
    fn s16_inv043_undecodable_arguments_cannot_enter_a_lease_offer_frame() {
        let offered = lease_with(
            RunnerLeaseState::Offered,
            NormalizedToolArguments::try_from_provider_text("not-json".to_owned())
                .expect("the exact undecodable fixture text is bounded"),
            None,
        );

        assert!(matches!(
            RunnerDispatchWireAdapter::lease_offer(&offered),
            Err(RunnerDispatchWireError::ArgumentsNotJson)
        ));
    }

    #[test]
    fn s16_inv043_nonobject_json_arguments_cannot_enter_a_lease_offer_frame() {
        let offered = lease_with(
            RunnerLeaseState::Offered,
            NormalizedToolArguments::try_from_provider_text("[]".to_owned())
                .expect("the exact JSON array fixture is canonical"),
            None,
        );

        assert!(matches!(
            RunnerDispatchWireAdapter::lease_offer(&offered),
            Err(RunnerDispatchWireError::Wire(
                signalbox_runner_wire::ValueError::Correlation
            ))
        ));
    }

    #[test]
    fn s12_inv043_result_frame_reconstitutes_bounded_terminal_evidence() {
        let request = RunnerDispatchWireAdapter::result_request(ResultFrame {
            correlation: wire_correlation(),
            result: TerminalResult::KnownFailure {
                error_kind: signalbox_runner_wire::ExecutionErrorKind::ExecutionFailed,
                detail: Some("synthetic failure".to_owned()),
            },
        })
        .expect("the bounded result reconstitutes");

        assert_eq!(
            request.observation(),
            &ToolAttemptObservation::KnownFailed {
                error: ToolExecutionError::new(
                    ToolExecutionErrorKind::ExecutionFailed,
                    Some(
                        ToolExecutionErrorDetail::try_new("synthetic failure".to_owned())
                            .expect("the expected detail is bounded"),
                    ),
                ),
            }
        );
    }

    #[test]
    fn s12_inv043_invalid_result_detail_fails_before_transaction_admission() {
        let rejected = RunnerDispatchWireAdapter::result_request(ResultFrame {
            correlation: wire_correlation(),
            result: TerminalResult::KnownFailure {
                error_kind: signalbox_runner_wire::ExecutionErrorKind::ExecutionFailed,
                detail: Some(" surrounding whitespace ".to_owned()),
            },
        });

        assert!(matches!(
            rejected,
            Err(RunnerDispatchWireError::Wire(
                signalbox_runner_wire::ValueError::Result
            ))
        ));
    }

    #[test]
    fn s12_inv043_success_result_frame_reconstitutes_exact_text() {
        let request = RunnerDispatchWireAdapter::result_request(ResultFrame {
            correlation: wire_correlation(),
            result: TerminalResult::Success {
                text: "runner output".to_owned(),
            },
        })
        .expect("the bounded success reconstitutes");

        assert_eq!(
            request.observation(),
            &ToolAttemptObservation::Completed {
                result: signalbox_domain::ToolResultContent::Text(
                    signalbox_domain::ToolResultText::try_new("runner output".to_owned())
                        .expect("the expected output is bounded"),
                ),
            }
        );
    }

    #[test]
    fn s12_inv043_ambiguous_result_frame_reconstitutes_without_inventing_detail() {
        let request = RunnerDispatchWireAdapter::result_request(ResultFrame {
            correlation: wire_correlation(),
            result: TerminalResult::Ambiguous,
        })
        .expect("the ambiguous result reconstitutes");

        assert_eq!(request.observation(), &ToolAttemptObservation::Ambiguous);
    }

    #[test]
    fn s12_inv043_result_acknowledgement_requires_the_atomic_completed_lease() {
        let completed = lease(RunnerLeaseState::Completed);

        assert_eq!(
            RunnerDispatchWireAdapter::result_recorded(&completed)
                .expect("the completed lease projects its acknowledgement"),
            Message::ResultRecorded(ResultRecorded {
                correlation: wire_correlation(),
            })
        );
    }

    #[test]
    fn s16_inv043_offered_lease_cannot_project_claimed_only_frames() {
        let offered = lease(RunnerLeaseState::Offered);

        assert!(matches!(
            RunnerDispatchWireAdapter::dispatch(&offered),
            Err(RunnerDispatchWireError::InvalidLeaseState)
        ));
    }
}
