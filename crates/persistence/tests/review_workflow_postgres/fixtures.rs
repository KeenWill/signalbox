//! Shared test fixtures.

use super::*;

#[path = "../../../../tooling/postgres_test_image.rs"]
mod postgres_test_image;
pub(crate) use postgres_test_image::POSTGRES_IMAGE_TAG;
pub(crate) const DATABASE_NAME: &str = "signalbox_review_workflow_integration";
pub(crate) const DATABASE_USER: &str = "signalbox";
pub(crate) const DATABASE_PASSWORD: &str = "signalbox-test-only";

pub(crate) fn test_session_credential_pin() -> signalbox_persistence::SessionCredentialPin {
    signalbox_persistence::SessionCredentialPin::try_new(vec![
        signalbox_persistence::SessionModelCredential::new(
            "test-model-family",
            "test-model-primary",
        ),
    ])
    .expect("test credential pin is valid")
}

pub(crate) async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>>
{
    migrated_postgres_with_max_connections(4).await
}

pub(crate) async fn migrated_postgres_with_max_connections(
    max_connections: u32,
) -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
    let container = Postgres::default()
        .with_db_name(DATABASE_NAME)
        .with_user(DATABASE_USER)
        .with_password(DATABASE_PASSWORD)
        .with_cmd(disposable_postgres_server_args())
        .with_mount(disposable_postgres_state_tmpfs_from_example()?)
        .with_tag(POSTGRES_IMAGE_TAG)
        .with_labels(disposable_test_container_labels())
        .start()
        .await?;
    let host = container.get_host().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let database_url =
        format!("postgres://{DATABASE_USER}:{DATABASE_PASSWORD}@{host}:{port}/{DATABASE_NAME}");
    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    migrate(&pool).await?;
    migrate(&pool).await?;
    Ok((container, pool))
}

pub(crate) fn uuid(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

pub(crate) fn key(value: &str) -> ReviewKey {
    ReviewKey::try_new(String::from(value)).expect("fixture key is admitted")
}

pub(crate) fn text(value: &str) -> ReviewText {
    ReviewText::try_new(String::from(value)).expect("fixture text is admitted")
}

#[derive(Clone, Copy)]
pub(crate) enum MaximumWidthKeyRole {
    Provider,
    Repository,
    HeadRevision,
    BaseRevision,
}

pub(crate) fn maximum_width_key(role: MaximumWidthKeyRole) -> ReviewKey {
    const KEY_BYTES: usize = 1_024;
    const HEX_CHUNK_WIDTH: usize = 16;
    const CHUNK_COUNT: usize = KEY_BYTES / HEX_CHUNK_WIDTH;
    const SPLITMIX_INCREMENT: u64 = 0x9e37_79b9_7f4a_7c15;
    const SPLITMIX_FIRST_FACTOR: u64 = 0xbf58_476d_1ce4_e5b9;
    const SPLITMIX_SECOND_FACTOR: u64 = 0x94d0_49bb_1331_11eb;
    const SPLITMIX_FIRST_SHIFT: u32 = 30;
    const SPLITMIX_SECOND_SHIFT: u32 = 27;
    const SPLITMIX_FINAL_SHIFT: u32 = 31;
    const PROVIDER_SEED: u64 = 0x243f_6a88_85a3_08d3;
    const REPOSITORY_SEED: u64 = 0x1319_8a2e_0370_7344;
    const HEAD_REVISION_SEED: u64 = 0xa409_3822_299f_31d0;
    const BASE_REVISION_SEED: u64 = 0x082e_fa98_ec4e_6c89;

    let mut state = match role {
        MaximumWidthKeyRole::Provider => PROVIDER_SEED,
        MaximumWidthKeyRole::Repository => REPOSITORY_SEED,
        MaximumWidthKeyRole::HeadRevision => HEAD_REVISION_SEED,
        MaximumWidthKeyRole::BaseRevision => BASE_REVISION_SEED,
    };
    let mut value = String::with_capacity(KEY_BYTES);
    for _ in 0..CHUNK_COUNT {
        state = state.wrapping_add(SPLITMIX_INCREMENT);
        let mut mixed = state;
        mixed = (mixed ^ (mixed >> SPLITMIX_FIRST_SHIFT)).wrapping_mul(SPLITMIX_FIRST_FACTOR);
        mixed = (mixed ^ (mixed >> SPLITMIX_SECOND_SHIFT)).wrapping_mul(SPLITMIX_SECOND_FACTOR);
        mixed ^= mixed >> SPLITMIX_FINAL_SHIFT;
        value.push_str(&format!("{mixed:0HEX_CHUNK_WIDTH$x}"));
    }
    key(&value)
}

pub(crate) fn workflow_for_pass(kind: ReviewPassKind) -> ReviewWorkflowKind {
    match kind {
        ReviewPassKind::ImportExternalContext => ReviewWorkflowKind::ImportExternalContext,
        ReviewPassKind::ReadOnlyReview => ReviewWorkflowKind::ReadOnlyReview,
        ReviewPassKind::Judge => ReviewWorkflowKind::JudgeFindings,
        ReviewPassKind::Dedupe => ReviewWorkflowKind::DedupeFindings,
        ReviewPassKind::Publish => ReviewWorkflowKind::PublishReview,
        ReviewPassKind::Fix => ReviewWorkflowKind::FixFindings,
        ReviewPassKind::PropagateStack => ReviewWorkflowKind::PropagateStack,
    }
}

pub(crate) fn pass_evidence(
    reference: ReviewPassRef,
    kind: ReviewPassKind,
    policy: ReviewPolicy,
    state: ReviewPassState,
) -> ReviewPassEvidence {
    let session = SessionId::from_uuid(uuid(0x201));
    let accepted_input = AcceptedInputId::from_uuid(uuid(0x202));
    let (origin_turn, turn_evidence) = match &state {
        ReviewPassState::Queued => (TurnId::from_uuid(uuid(0x203)), None),
        ReviewPassState::Running { turn } => (
            *turn,
            Some(ReviewPassTurnEvidence::new(
                *turn,
                session,
                accepted_input,
                ReviewPassTurnOutcome::Active,
                None,
            )),
        ),
        ReviewPassState::Succeeded {
            turn,
            output_frontier,
            ..
        } => (
            *turn,
            Some(ReviewPassTurnEvidence::new(
                *turn,
                session,
                accepted_input,
                ReviewPassTurnOutcome::Completed,
                Some(*output_frontier),
            )),
        ),
        ReviewPassState::Failed { turn } => (
            *turn,
            Some(ReviewPassTurnEvidence::new(
                *turn,
                session,
                accepted_input,
                ReviewPassTurnOutcome::Failed,
                Some(ContextFrontierId::from_uuid(uuid(0x204))),
            )),
        ),
        ReviewPassState::Blocked { turn, .. } => (
            *turn,
            Some(ReviewPassTurnEvidence::new(
                *turn,
                session,
                accepted_input,
                ReviewPassTurnOutcome::ReconciliationRequired,
                Some(ContextFrontierId::from_uuid(uuid(0x204))),
            )),
        ),
        ReviewPassState::Cancelled { turn: Some(turn) } => (
            *turn,
            Some(ReviewPassTurnEvidence::new(
                *turn,
                session,
                accepted_input,
                ReviewPassTurnOutcome::Cancelled,
                Some(ContextFrontierId::from_uuid(uuid(0x204))),
            )),
        ),
        ReviewPassState::Cancelled { turn: None } => (TurnId::from_uuid(uuid(0x203)), None),
    };
    let pass = ReviewPass::try_reconstitute(signalbox_domain::ReviewPassReconstitutionInput::new(
        reference,
        kind,
        reference.run(),
        workflow_for_pass(kind),
        session,
        accepted_input,
        ReviewPassAcceptedInputEvidence::new(accepted_input, session, Some(origin_turn)),
        state,
        turn_evidence,
    ))
    .expect("fixture pass evidence is fully authenticated");
    ReviewPassEvidence::from_pass(&pass, policy)
}

pub(crate) fn succeeded_pass(reference: ReviewPassRef, kind: ReviewPassKind) -> ReviewPassEvidence {
    pass_evidence(
        reference,
        kind,
        ReviewPolicy::version_one(),
        ReviewPassState::Succeeded {
            turn: TurnId::from_uuid(uuid(0x203)),
            output_frontier: ContextFrontierId::from_uuid(uuid(0x131)),
            result: None,
        },
    )
}

pub(crate) fn run_evidence_for_pass(pass: ReviewPassEvidence) -> ReviewRunEvidence {
    let reference = pass.reference();
    let state = match pass.state() {
        ReviewPassState::Queued => ReviewRunState::Queued,
        ReviewPassState::Running { .. } => ReviewRunState::Running {
            active_pass: reference,
        },
        ReviewPassState::Succeeded { .. } => ReviewRunState::Succeeded {
            concluding_pass: reference,
        },
        ReviewPassState::Failed { .. } => ReviewRunState::Failed {
            failed_pass: reference,
        },
        ReviewPassState::Blocked { .. } => ReviewRunState::Blocked {
            blocking_pass: reference,
        },
        ReviewPassState::Cancelled { .. } => ReviewRunState::Cancelled {
            last_pass: Some(reference),
        },
    };
    ReviewRunEvidence::new(
        reference.run(),
        workflow_for_pass(pass.kind()),
        pass.policy(),
        state,
    )
}

pub(crate) fn pass_with_finding_event(
    finding: ReviewFindingRef,
    ordinal: ReviewEventOrdinal,
    pass: ReviewPassEvidence,
    kind: ReviewFindingEventResultKind,
) -> ReviewPassEvidence {
    let result =
        ReviewPassResult::FindingEvent(ReviewFindingEventResult::new(finding, ordinal, kind));
    let state = match pass.state() {
        ReviewPassState::Succeeded {
            turn,
            output_frontier,
            ..
        } => ReviewPassState::Succeeded {
            turn: *turn,
            output_frontier: *output_frontier,
            result: Some(result),
        },
        ReviewPassState::Blocked { turn, .. } => ReviewPassState::Blocked {
            turn: *turn,
            result: Some(result),
        },
        other => other.clone(),
    };
    pass_evidence(pass.reference(), pass.kind(), pass.policy(), state)
}

pub(crate) fn pass_with_produced_findings(
    findings: Vec<ReviewFindingRef>,
    pass: ReviewPassEvidence,
) -> ReviewPassEvidence {
    let state = match pass.state() {
        ReviewPassState::Succeeded {
            turn,
            output_frontier,
            ..
        } => ReviewPassState::Succeeded {
            turn: *turn,
            output_frontier: *output_frontier,
            result: Some(ReviewPassResult::ProducedFindings(
                ReviewProducedFindings::try_new(findings)
                    .expect("fixture findings are a canonical inventory"),
            )),
        },
        state => state.clone(),
    };
    pass_evidence(pass.reference(), pass.kind(), pass.policy(), state)
}

pub(crate) fn finding_event(
    finding: ReviewFindingRef,
    ordinal: ReviewEventOrdinal,
    pass: ReviewPassEvidence,
    kind: ReviewFindingEventKind,
) -> ReviewFindingEvent {
    let result_kind = match &kind {
        ReviewFindingEventKind::Accepted => ReviewFindingEventResultKind::Accepted,
        ReviewFindingEventKind::Rejected { reason } => ReviewFindingEventResultKind::Rejected {
            reason: reason.clone(),
        },
        ReviewFindingEventKind::Duplicate { canonical } => {
            ReviewFindingEventResultKind::Duplicate {
                canonical: *canonical,
            }
        }
        ReviewFindingEventKind::Superseded { successor } => {
            ReviewFindingEventResultKind::Superseded {
                successor: *successor,
            }
        }
        ReviewFindingEventKind::Stale => ReviewFindingEventResultKind::Stale,
        ReviewFindingEventKind::Posted { link } => {
            ReviewFindingEventResultKind::Posted { link: link.link() }
        }
        ReviewFindingEventKind::Fixed => ReviewFindingEventResultKind::Fixed,
        ReviewFindingEventKind::BlockedWithReason { reason, link } => {
            ReviewFindingEventResultKind::BlockedWithReason {
                reason: reason.clone(),
                link: link.as_ref().map(|link| link.link()),
            }
        }
    };
    let pass = pass_with_finding_event(finding, ordinal, pass, result_kind);
    ReviewFindingEvent::new(
        finding,
        ordinal,
        pass.reference(),
        pass.clone(),
        run_evidence_for_pass(pass),
        kind,
    )
}

pub(crate) fn attachment(
    link: ReviewExternalLinkId,
    pass: ReviewPassEvidence,
    external_object: ReviewKey,
) -> ReviewExternalLinkAttachment {
    let state = match pass.state() {
        ReviewPassState::Succeeded {
            turn,
            output_frontier,
            result,
        } => {
            let finding_event = match result {
                Some(ReviewPassResult::FindingEvent(event))
                    if matches!(event.kind(), ReviewFindingEventResultKind::Posted { .. }) =>
                {
                    Some(event.clone())
                }
                Some(ReviewPassResult::ExternalLinkAttachment(result)) => {
                    result.finding_event().cloned()
                }
                _ => None,
            };
            ReviewPassState::Succeeded {
                turn: *turn,
                output_frontier: *output_frontier,
                result: Some(ReviewPassResult::ExternalLinkAttachment(
                    ReviewExternalLinkAttachmentResult::new(
                        link,
                        external_object.clone(),
                        finding_event,
                    ),
                )),
            }
        }
        state => state.clone(),
    };
    let pass = pass_evidence(pass.reference(), pass.kind(), pass.policy(), state);
    ReviewExternalLinkAttachment::new(
        link,
        pass.reference(),
        pass.clone(),
        run_evidence_for_pass(pass),
        external_object,
    )
}

pub(crate) fn posted_attachment(
    link: ReviewExternalLinkId,
    pass: ReviewPassEvidence,
    external_object: ReviewKey,
    finding: ReviewFindingRef,
    ordinal: ReviewEventOrdinal,
) -> ReviewExternalLinkAttachment {
    let state = match pass.state() {
        ReviewPassState::Succeeded {
            turn,
            output_frontier,
            ..
        } => ReviewPassState::Succeeded {
            turn: *turn,
            output_frontier: *output_frontier,
            result: Some(ReviewPassResult::ExternalLinkAttachment(
                ReviewExternalLinkAttachmentResult::new(
                    link,
                    external_object.clone(),
                    Some(ReviewFindingEventResult::new(
                        finding,
                        ordinal,
                        ReviewFindingEventResultKind::Posted { link },
                    )),
                ),
            )),
        },
        state => state.clone(),
    };
    let pass = pass_evidence(pass.reference(), pass.kind(), pass.policy(), state);
    ReviewExternalLinkAttachment::new(
        link,
        pass.reference(),
        pass.clone(),
        run_evidence_for_pass(pass),
        external_object,
    )
}

pub(crate) fn observation(
    link: ReviewExternalLinkId,
    ordinal: ReviewEventOrdinal,
    pass: ReviewPassEvidence,
    state: ReviewExternalObjectState,
) -> ReviewExternalLinkObservation {
    let pass_state = match pass.state() {
        ReviewPassState::Succeeded {
            turn,
            output_frontier,
            ..
        } => ReviewPassState::Succeeded {
            turn: *turn,
            output_frontier: *output_frontier,
            result: Some(ReviewPassResult::ExternalLinkObservation(
                ReviewExternalLinkObservationResult::new(link, ordinal, state),
            )),
        },
        pass_state => pass_state.clone(),
    };
    let pass = pass_evidence(pass.reference(), pass.kind(), pass.policy(), pass_state);
    ReviewExternalLinkObservation::new(
        link,
        ordinal,
        pass.reference(),
        pass.clone(),
        run_evidence_for_pass(pass),
        state,
    )
}

#[derive(Debug)]
pub(crate) struct FixedActivationIds {
    pub(crate) origin_entry: Option<SemanticTranscriptEntryId>,
    pub(crate) starting_frontier: Option<ContextFrontierId>,
    pub(crate) initial_attempt: Option<TurnAttemptId>,
}

impl StartEligibleTurnIdGenerator for FixedActivationIds {
    fn next_model_identity_entry_id(&mut self) -> SemanticTranscriptEntryId {
        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(u128::MAX))
    }

    fn next_origin_entry_id(&mut self) -> SemanticTranscriptEntryId {
        self.origin_entry
            .take()
            .expect("one activation is expected")
    }

    fn next_starting_frontier_id(&mut self) -> ContextFrontierId {
        self.starting_frontier
            .take()
            .expect("one activation is expected")
    }

    fn next_initial_attempt_id(&mut self) -> TurnAttemptId {
        self.initial_attempt
            .take()
            .expect("one activation is expected")
    }
}

pub(crate) async fn insert_active_turn(
    pool: &PgPool,
    session: SessionId,
    accepted_input: AcceptedInputId,
    turn: TurnId,
) {
    insert_active_turn_with_offset(pool, session, accepted_input, turn, 0).await;
}

pub(crate) async fn insert_active_turn_with_offset(
    pool: &PgPool,
    session: SessionId,
    accepted_input: AcceptedInputId,
    turn: TurnId,
    offset: u128,
) {
    let create = CreateSession::new(
        DurableCommandId::from_uuid(uuid(0x101 + offset)),
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None),
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(uuid(0x102 + offset)),
        )),
    )
    .prepare(session)
    .expect("user-created fixture session is preparable");
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(create)
        .await
        .expect("fixture session persists");

    let submit = SubmitInput::new(
        DurableCommandId::from_uuid(uuid(0x103 + offset)),
        session,
        UserContent::try_text(String::from("Perform the bounded review pass"))
            .expect("fixture content is admitted"),
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    );
    SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            submit,
            accepted_input,
            Some(turn),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0x104 + offset)),
                ContextFrontierId::from_uuid(uuid(0x105 + offset)),
            ),
            |_| TurnId::from_uuid(uuid(0x106 + offset)),
            |requests| {
                (
                    requests
                        .iter()
                        .enumerate()
                        .map(|(index, _)| {
                            SemanticTranscriptEntryId::from_uuid(uuid(
                                0x110 + offset + u128::try_from(index).expect("small fixture"),
                            ))
                        })
                        .collect(),
                    ContextFrontierId::from_uuid(uuid(0x120 + offset)),
                )
            },
        )
        .await
        .expect("fixture input and queued turn persist");

    let mut activation = StartEligibleTurnService::new(
        FixedActivationIds {
            origin_entry: Some(SemanticTranscriptEntryId::from_uuid(uuid(0x130 + offset))),
            starting_frontier: Some(ContextFrontierId::from_uuid(uuid(0x131 + offset))),
            initial_attempt: Some(TurnAttemptId::from_uuid(uuid(0x132 + offset))),
        },
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let outcome = activation
        .execute(session)
        .await
        .expect("fixture turn activates");
    assert!(matches!(outcome, StartEligibleTurnOutcome::Activated(_)));
    record_empty_instruction_manifest(pool, session)
        .await
        .expect("fixture turn records its empty instruction manifest");
}

pub(crate) fn finding(
    reference: ReviewFindingRef,
    producing_pass: ReviewPassEvidence,
    target: &ReviewTarget,
) -> ReviewFinding {
    finding_with_confidence_axes_and_side(
        reference,
        producing_pass,
        target,
        FindingConfidenceAxes {
            is_real: 9_000,
            severity_label: 8_500,
        },
        Some(ReviewFindingDiffSide::Right),
    )
}

pub(crate) struct FindingConfidenceAxes {
    pub(crate) is_real: u16,
    pub(crate) severity_label: u16,
}

pub(crate) fn finding_with_confidence_axes_and_side(
    reference: ReviewFindingRef,
    producing_pass: ReviewPassEvidence,
    target: &ReviewTarget,
    confidence: FindingConfidenceAxes,
    diff_side: Option<ReviewFindingDiffSide>,
) -> ReviewFinding {
    let policy = producing_pass.policy();
    let state = match producing_pass.state() {
        ReviewPassState::Succeeded {
            turn,
            output_frontier,
            result,
        } => ReviewPassState::Succeeded {
            turn: *turn,
            output_frontier: *output_frontier,
            result: match result {
                Some(result @ ReviewPassResult::ProducedFindings(findings))
                    if !findings.findings().is_empty() =>
                {
                    Some(result.clone())
                }
                _ => Some(ReviewPassResult::ProducedFindings(
                    ReviewProducedFindings::try_new(vec![reference])
                        .expect("one fixture finding is a canonical inventory"),
                )),
            },
        },
        state => state.clone(),
    };
    let producing_pass = pass_evidence(
        producing_pass.reference(),
        producing_pass.kind(),
        policy,
        state,
    );
    ReviewFinding::new(
        ReviewFindingProposal::try_new(
            reference,
            producing_pass.clone(),
            ReviewRunEvidence::new(
                reference.run(),
                ReviewWorkflowKind::ReadOnlyReview,
                policy,
                ReviewRunState::Succeeded {
                    concluding_pass: reference.pass(),
                },
            ),
            target,
            ReviewFindingContent::new(
                ReviewFindingLocation::new(
                    key("src/review.rs"),
                    Some(ReviewLineRange::try_new(11, 14).expect("ordered fixture range")),
                    diff_side,
                ),
                text("Guard the exact evidence edge"),
                text("The transition must retain the producing turn."),
                ReviewFindingSeverity::High,
                ReviewFindingConfidenceAxes::new(
                    ReviewConfidence::try_from_basis_points(confidence.is_real)
                        .expect("fixture is-real confidence is bounded"),
                    ReviewConfidence::try_from_basis_points(confidence.severity_label)
                        .expect("fixture severity-label confidence is bounded"),
                ),
                key("correctness"),
                Some(text("Bind the transition to the complete pass reference.")),
            ),
        )
        .expect("fixture pass belongs to the finding run"),
    )
}

pub(crate) struct PersistedReviewPassFixture {
    pub(crate) pool: PgPool,
    pub(crate) store: ReviewWorkflowStore,
    pub(crate) target: ReviewTargetId,
    pub(crate) target_snapshot: ReviewTarget,
    pub(crate) run: ReviewRunRef,
    pub(crate) pass: ReviewPassRef,
}

pub(crate) async fn insert_review_pass_fixture(pool: &PgPool) -> PersistedReviewPassFixture {
    let store = ReviewWorkflowStore::new(pool.clone());
    let session = SessionId::from_uuid(uuid(0x201));
    let accepted_input = AcceptedInputId::from_uuid(uuid(0x202));
    let turn = TurnId::from_uuid(uuid(0x203));
    insert_active_turn(pool, session, accepted_input, turn).await;

    let target = ReviewTargetId::from_uuid(uuid(0x301));
    let target_snapshot = ReviewTarget::try_new(
        target,
        key("example-code-host"),
        key("example/repository"),
        ReviewTargetSubject::Commit,
        key("0123456789abcdef"),
        Some(key("fedcba9876543210")),
        None,
    )
    .expect("fixture target topology is valid");
    store
        .insert_target(&target_snapshot)
        .await
        .expect("target persists");
    let run = ReviewRunRef::new(target, ReviewRunId::from_uuid(uuid(0x302)));
    let pass = ReviewPassRef::new(run, ReviewPassId::from_uuid(uuid(0x303)));
    let mut run_value = ReviewRun::new(
        run,
        ReviewWorkflowKind::ReadOnlyReview,
        ReviewPolicy::version_one(),
    );
    let pass_value = ReviewPass::try_new(
        pass,
        ReviewPassKind::ReadOnlyReview,
        &mut run_value,
        session,
        ReviewPassAcceptedInputEvidence::new(accepted_input, session, Some(turn)),
    )
    .expect("accepted input belongs to the fixture session");
    store
        .insert_run(&run_value)
        .await
        .expect("queued run persists");
    store
        .insert_pass(&pass_value)
        .await
        .expect("queued pass persists");

    PersistedReviewPassFixture {
        pool: pool.clone(),
        store,
        target,
        target_snapshot,
        run,
        pass,
    }
}

pub(crate) struct ReviewCommandAdmissionFixture {
    pub(crate) store: ReviewWorkflowStore,
    pub(crate) target: ReviewTargetId,
    pub(crate) run: ReviewRun,
    pub(crate) pass: ReviewPass,
    pub(crate) session: SessionId,
    pub(crate) accepted_input: AcceptedInputId,
    pub(crate) origin_turn: TurnId,
}

pub(crate) async fn review_command_admission_fixture(
    pool: &PgPool,
) -> ReviewCommandAdmissionFixture {
    let store = ReviewWorkflowStore::new(pool.clone());
    let session = SessionId::from_uuid(uuid(0x771));
    let accepted_input = AcceptedInputId::from_uuid(uuid(0x772));
    let origin_turn = TurnId::from_uuid(uuid(0x773));
    insert_active_turn(pool, session, accepted_input, origin_turn).await;
    let target = ReviewTargetId::from_uuid(uuid(0x774));
    store
        .insert_target(
            &ReviewTarget::try_new(
                target,
                key("example-code-host"),
                key("example/admission-repository"),
                ReviewTargetSubject::Commit,
                key("admission-head"),
                Some(key("admission-base")),
                None,
            )
            .expect("admission target is valid"),
        )
        .await
        .expect("admission target persists");
    let run_reference = ReviewRunRef::new(target, ReviewRunId::from_uuid(uuid(0x775)));
    let pass_reference = ReviewPassRef::new(run_reference, ReviewPassId::from_uuid(uuid(0x776)));
    let mut run = ReviewRun::new(
        run_reference,
        ReviewWorkflowKind::ReadOnlyReview,
        ReviewPolicy::version_one(),
    );
    let pass = ReviewPass::try_new(
        pass_reference,
        ReviewPassKind::ReadOnlyReview,
        &mut run,
        session,
        ReviewPassAcceptedInputEvidence::new(accepted_input, session, Some(origin_turn)),
    )
    .expect("admission pass is valid");
    ReviewCommandAdmissionFixture {
        store,
        target,
        run,
        pass,
        session,
        accepted_input,
        origin_turn,
    }
}

pub(crate) async fn insert_fixture_pass(
    fixture: &PersistedReviewPassFixture,
    identity: u128,
    kind: ReviewPassKind,
) -> ReviewPassRef {
    insert_isolated_pass_for_target(
        &fixture.pool,
        &fixture.store,
        fixture.target,
        identity,
        kind,
    )
    .await
    .0
}

pub(crate) async fn insert_isolated_pass_for_target(
    pool: &PgPool,
    store: &ReviewWorkflowStore,
    target: ReviewTargetId,
    identity: u128,
    kind: ReviewPassKind,
) -> (ReviewPassRef, TurnId) {
    let session = SessionId::from_uuid(uuid(0x10_0000 + identity));
    let accepted_input = AcceptedInputId::from_uuid(uuid(0x20_0000 + identity));
    let turn = TurnId::from_uuid(uuid(0x20_0001 + identity));
    insert_active_turn_with_offset(
        pool,
        session,
        accepted_input,
        turn,
        0x40_0000 + identity * 0x100,
    )
    .await;
    (
        insert_pass_for_target(store, target, identity, kind, session, accepted_input).await,
        turn,
    )
}

pub(crate) async fn insert_pass_for_target(
    store: &ReviewWorkflowStore,
    target: ReviewTargetId,
    identity: u128,
    kind: ReviewPassKind,
    session: SessionId,
    accepted_input: AcceptedInputId,
) -> ReviewPassRef {
    insert_pass_for_target_with_policy(
        store,
        target,
        identity,
        kind,
        session,
        accepted_input,
        ReviewPolicy::version_one(),
    )
    .await
}

pub(crate) async fn insert_pass_for_target_with_policy(
    store: &ReviewWorkflowStore,
    target: ReviewTargetId,
    identity: u128,
    kind: ReviewPassKind,
    session: SessionId,
    accepted_input: AcceptedInputId,
    policy: ReviewPolicy,
) -> ReviewPassRef {
    let run = ReviewRunRef::new(target, ReviewRunId::from_uuid(uuid(identity + 0x1000)));
    let pass = ReviewPassRef::new(run, ReviewPassId::from_uuid(uuid(identity)));
    let mut run_value = ReviewRun::new(run, workflow_for_pass(kind), policy);
    let origin_turn = TurnId::from_uuid(Uuid::from_u128(
        accepted_input
            .into_uuid()
            .as_u128()
            .checked_add(1)
            .expect("fixture input identity has a successor"),
    ));
    let pass_value = ReviewPass::try_new(
        pass,
        kind,
        &mut run_value,
        session,
        ReviewPassAcceptedInputEvidence::new(accepted_input, session, Some(origin_turn)),
    )
    .expect("fixture input belongs to its session");
    store
        .insert_run(&run_value)
        .await
        .expect("additional fixture run persists");
    store
        .insert_pass(&pass_value)
        .await
        .expect("additional fixture pass persists");
    pass
}

pub(crate) async fn start_review_pass(
    store: &ReviewWorkflowStore,
    reference: ReviewPassRef,
) -> (ReviewPass, TurnId) {
    let turn = store
        .load_pass(reference.pass())
        .await
        .expect("pass origin loads")
        .expect("pass exists")
        .origin_turn();
    let (_, pass) = store
        .transition_run_and_pass(
            reference.run().run(),
            reference.pass(),
            ReviewRunState::Running {
                active_pass: reference,
            },
            ReviewPassState::Running { turn },
        )
        .await
        .expect("run/pass activation persists")
        .expect("fixture run and pass exist");
    (pass, turn)
}

pub(crate) const REVIEW_TURN_IDENTITY_NAMESPACE: u128 = 0xfeed_f00d_dead_beef_0000_0000_0000_0000;
pub(crate) const ARBITRARY_REVIEW_CREDENTIAL_REFERENCE: &str = "review-fixture-primary";
pub(crate) const ARBITRARY_REVIEW_RESPONSE: &str = "Bounded review fixture response";
pub(crate) struct ReviewTurnTransitionIdentities {
    pub(crate) provider: ProviderModelIdentity,
    pub(crate) call: ModelCallId,
    pub(crate) resume_candidate_call: ModelCallId,
    pub(crate) initial_failure_entry: SemanticTranscriptEntryId,
    pub(crate) initial_failure_frontier: ContextFrontierId,
    pub(crate) initial_steering_frontier: ContextFrontierId,
    pub(crate) resume_failure_entry: SemanticTranscriptEntryId,
    pub(crate) resume_failure_frontier: ContextFrontierId,
    pub(crate) resume_steering_frontier: ContextFrontierId,
    pub(crate) assistant_entry: SemanticTranscriptEntryId,
    pub(crate) completion_entry: SemanticTranscriptEntryId,
    pub(crate) terminal_entry: SemanticTranscriptEntryId,
    pub(crate) terminal_frontier: ContextFrontierId,
    pub(crate) interrupt_command: DurableCommandId,
    pub(crate) interrupt_input: AcceptedInputId,
    pub(crate) interrupt_successor: TurnId,
    pub(crate) interrupt_cancellation_entry: SemanticTranscriptEntryId,
    pub(crate) interrupt_cancellation_frontier: ContextFrontierId,
}

impl ReviewTurnTransitionIdentities {
    pub(crate) fn for_turn(turn: TurnId) -> Self {
        let mut next_value =
            REVIEW_TURN_IDENTITY_NAMESPACE ^ turn.into_uuid().as_u128().rotate_left(u128::BITS / 2);
        let mut next_uuid = || {
            next_value = next_value
                .checked_add(1)
                .expect("review fixture identity namespace is not exhausted");
            Uuid::from_u128(next_value)
        };
        Self {
            provider: ProviderModelIdentity::from_uuid(next_uuid()),
            call: ModelCallId::from_uuid(next_uuid()),
            resume_candidate_call: ModelCallId::from_uuid(next_uuid()),
            initial_failure_entry: SemanticTranscriptEntryId::from_uuid(next_uuid()),
            initial_failure_frontier: ContextFrontierId::from_uuid(next_uuid()),
            initial_steering_frontier: ContextFrontierId::from_uuid(next_uuid()),
            resume_failure_entry: SemanticTranscriptEntryId::from_uuid(next_uuid()),
            resume_failure_frontier: ContextFrontierId::from_uuid(next_uuid()),
            resume_steering_frontier: ContextFrontierId::from_uuid(next_uuid()),
            assistant_entry: SemanticTranscriptEntryId::from_uuid(next_uuid()),
            completion_entry: SemanticTranscriptEntryId::from_uuid(next_uuid()),
            terminal_entry: SemanticTranscriptEntryId::from_uuid(next_uuid()),
            terminal_frontier: ContextFrontierId::from_uuid(next_uuid()),
            interrupt_command: DurableCommandId::from_uuid(next_uuid()),
            interrupt_input: AcceptedInputId::from_uuid(next_uuid()),
            interrupt_successor: TurnId::from_uuid(next_uuid()),
            interrupt_cancellation_entry: SemanticTranscriptEntryId::from_uuid(next_uuid()),
            interrupt_cancellation_frontier: ContextFrontierId::from_uuid(next_uuid()),
        }
    }
}

pub(crate) struct PreparedReviewTurnCall {
    pub(crate) session: SessionId,
    pub(crate) repository: PostgresModelCallRepository,
    pub(crate) authorized: AuthorizedModelCall,
    pub(crate) identities: ReviewTurnTransitionIdentities,
}

pub(crate) async fn prepare_review_turn_call(
    pool: &PgPool,
    turn: TurnId,
) -> PreparedReviewTurnCall {
    #[derive(sqlx::FromRow)]
    struct StoredReviewTurnModelFacts {
        session_id: Uuid,
        direct_selection_id: Uuid,
    }

    let stored = sqlx::query_as::<_, StoredReviewTurnModelFacts>(
        "SELECT lifecycle.session_id,
                COALESCE(
                    origin.frozen_direct_model_selection_id,
                    origin.frozen_alias_selected_direct_id
                ) AS direct_selection_id
           FROM turn_lifecycle AS lifecycle
           JOIN queued_input_origin AS origin
             ON origin.turn_id = lifecycle.turn_id
            AND origin.session_id = lifecycle.session_id
          WHERE lifecycle.turn_id = $1",
    )
    .bind(turn.into_uuid())
    .fetch_one(pool)
    .await
    .expect("active review fixture turn has frozen model facts");
    let session = SessionId::from_uuid(stored.session_id);
    let selection = DirectModelSelection::from_uuid(stored.direct_selection_id);
    let identities = ReviewTurnTransitionIdentities::for_turn(turn);
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(identities.provider),
    )])
    .expect("one review fixture target forms a catalog");
    let repository = PostgresModelCallRepository::new(
        pool.clone(),
        targets,
        ModelCallCredentialReference::new(ARBITRARY_REVIEW_CREDENTIAL_REFERENCE),
    );
    let checkpointed = repository
        .prepare_initial_call(
            session,
            identities.call,
            FailedModelCallTurnIdentities::new(
                identities.initial_failure_entry,
                identities.initial_failure_frontier,
            ),
            identities.initial_steering_frontier,
            |_| panic!("review fixture has no pending steering"),
        )
        .await
        .expect("review fixture model call checkpoints");
    assert!(matches!(
        checkpointed,
        PrepareInitialModelCallOutcome::Checkpointed(call) if call == identities.call
    ));
    let resumed = repository
        .prepare_initial_call(
            session,
            identities.resume_candidate_call,
            FailedModelCallTurnIdentities::new(
                identities.resume_failure_entry,
                identities.resume_failure_frontier,
            ),
            identities.resume_steering_frontier,
            |_| panic!("review fixture has no pending steering"),
        )
        .await
        .expect("review fixture model call resumes");
    assert!(matches!(
        resumed,
        PrepareInitialModelCallOutcome::Ready { .. }
    ));
    let AuthorizeModelCallOutcome::Authorized(authorized) = repository
        .authorize_send(session, identities.call)
        .await
        .expect("review fixture model call authorizes")
    else {
        panic!("review fixture model call must be ready to authorize");
    };
    PreparedReviewTurnCall {
        session,
        repository,
        authorized: *authorized,
        identities,
    }
}

pub(crate) async fn complete_review_turn(pool: &PgPool, turn: TurnId) -> ContextFrontierId {
    let prepared = prepare_review_turn_call(pool, turn).await;
    let terminal = prepared
        .repository
        .apply_terminal_observation(
            prepared.session,
            prepared
                .authorized
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::Completed {
                    assistant_text: vec![
                        AssistantText::try_new(String::from(ARBITRARY_REVIEW_RESPONSE))
                            .expect("review fixture assistant text is admitted"),
                    ],
                }),
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![prepared.identities.assistant_entry],
                prepared.identities.completion_entry,
                prepared.identities.terminal_frontier,
            )),
            |_| panic!("review fixture terminalization has no pending steering"),
        )
        .await
        .expect("review fixture model call completes");
    assert!(matches!(terminal, ModelCallTerminalOutcome::Completed(_)));
    prepared.identities.terminal_frontier
}

pub(crate) async fn fail_review_turn(pool: &PgPool, turn: TurnId) -> ContextFrontierId {
    let prepared = prepare_review_turn_call(pool, turn).await;
    let terminal = prepared
        .repository
        .apply_terminal_observation(
            prepared.session,
            prepared
                .authorized
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::KnownFailed),
            ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                prepared.identities.terminal_entry,
                prepared.identities.terminal_frontier,
            )),
            |_| panic!("review fixture terminalization has no pending steering"),
        )
        .await
        .expect("review fixture model call fails");
    assert!(matches!(terminal, ModelCallTerminalOutcome::Failed(_)));
    prepared.identities.terminal_frontier
}

pub(crate) async fn conclude_review_pass(
    store: &ReviewWorkflowStore,
    reference: ReviewPassRef,
    next_pass: ReviewPassState,
) -> ReviewPassEvidence {
    let policy = store
        .load_run(reference.run().run())
        .await
        .expect("fixture run loads")
        .expect("fixture run exists")
        .policy();
    let next_run = match next_pass {
        ReviewPassState::Succeeded { .. } => ReviewRunState::Succeeded {
            concluding_pass: reference,
        },
        ReviewPassState::Failed { .. } => ReviewRunState::Failed {
            failed_pass: reference,
        },
        ReviewPassState::Blocked { .. } => ReviewRunState::Blocked {
            blocking_pass: reference,
        },
        ReviewPassState::Cancelled { turn: Some(_) } => ReviewRunState::Cancelled {
            last_pass: Some(reference),
        },
        ReviewPassState::Cancelled { turn: None } => ReviewRunState::Cancelled { last_pass: None },
        ReviewPassState::Queued | ReviewPassState::Running { .. } => {
            panic!("fixture helper accepts only terminal outcomes")
        }
    };
    let (_, pass) = store
        .transition_run_and_pass(reference.run().run(), reference.pass(), next_run, next_pass)
        .await
        .expect("run/pass conclusion persists")
        .expect("fixture run and pass exist");
    pass_evidence(pass.reference(), pass.kind(), policy, pass.state().clone())
}
pub(crate) async fn propose_read_only_success(
    store: &ReviewWorkflowStore,
    pass: ReviewPass,
    output_frontier: ContextFrontierId,
) -> ReviewPassEvidence {
    let reference = pass.reference();
    let policy = store
        .load_run(reference.run().run())
        .await
        .expect("fixture run loads")
        .expect("fixture run exists")
        .policy();
    let ReviewPassState::Running { turn } = pass.state() else {
        panic!("read-only success proposal requires a running pass");
    };
    let turn = *turn;
    let state = ReviewPassState::Succeeded {
        turn,
        output_frontier,
        result: Some(ReviewPassResult::ProducedFindings(
            ReviewProducedFindings::try_new(Vec::new())
                .expect("empty fixture inventory is canonical"),
        )),
    };
    let turn_evidence = ReviewPassTurnEvidence::new(
        turn,
        pass.session(),
        pass.accepted_input(),
        ReviewPassTurnOutcome::Completed,
        Some(output_frontier),
    );
    let terminal_pass = pass
        .transition(state, Some(turn_evidence))
        .expect("read-only fixture proposes its atomic inventory");
    ReviewPassEvidence::from_pass(&terminal_pass, policy)
}

pub(crate) async fn succeed_fixture_passes(
    pool: &PgPool,
    store: &ReviewWorkflowStore,
    references: &[ReviewPassRef],
) -> Vec<ReviewPassEvidence> {
    let mut terminal = Vec::with_capacity(references.len());
    for reference in references {
        let (pass, turn) = start_review_pass(store, *reference).await;
        let output_frontier = complete_review_turn(pool, turn).await;
        terminal.push((pass, turn, output_frontier));
    }
    let mut evidence = Vec::with_capacity(terminal.len());
    for (pass, turn, output_frontier) in terminal {
        let reference = pass.reference();
        if pass.kind() == ReviewPassKind::ReadOnlyReview {
            let proposed = propose_read_only_success(store, pass, output_frontier).await;
            evidence.push(proposed);
        } else {
            evidence.push(
                conclude_review_pass(
                    store,
                    reference,
                    ReviewPassState::Succeeded {
                        turn,
                        output_frontier,
                        result: None,
                    },
                )
                .await,
            );
        }
    }
    evidence
}

pub(crate) fn assert_sqlstate(error: &sqlx::Error, expected: &str) {
    assert_eq!(
        error
            .as_database_error()
            .and_then(|database| database.code())
            .as_deref(),
        Some(expected)
    );
}

#[track_caller]
pub(crate) fn assert_external_link_no_change_result(
    state: &ReviewPassState,
    expected: ReviewExternalLinkNoChangeResult,
) {
    let actual = match state {
        ReviewPassState::Succeeded {
            result: Some(ReviewPassResult::ExternalLinkNoChange(result)),
            ..
        } => *result,
        other => panic!("expected an external-link no-change result, got {other:?}"),
    };
    assert_eq!(actual, expected);
}
