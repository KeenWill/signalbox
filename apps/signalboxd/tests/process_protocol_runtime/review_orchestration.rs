//! Review orchestration coverage.

use super::*;

pub(crate) const REVIEW_IMPORT_TEMPLATE: &str = "review-import";
pub(crate) const REVIEW_JUDGMENT_TEMPLATE: &str = "review-judgment";
pub(crate) const REVIEW_REPAIR_TEMPLATE: &str = "review-repair";
pub(crate) const REVIEW_PUBLICATION_TEMPLATE: &str = "review-publication";
pub(crate) const REVIEW_CONCERN_SET_VERSION: &str = "initial-five-v1";

pub(crate) fn review_identity(value: u128) -> CanonicalUuid {
    CanonicalUuid::from_uuid(Uuid::from_u128(value))
}

pub(crate) fn review_concern_inputs() -> Vec<ReviewOrchestrationConcernInput> {
    vec![
        ReviewOrchestrationConcernInput {
            key: String::from("correctness"),
            template_name: String::from("review-concern-correctness"),
        },
        ReviewOrchestrationConcernInput {
            key: String::from("interface-and-type-design"),
            template_name: String::from("review-concern-interface-and-type-design"),
        },
        ReviewOrchestrationConcernInput {
            key: String::from("test-quality"),
            template_name: String::from("review-concern-test-quality"),
        },
        ReviewOrchestrationConcernInput {
            key: String::from("security"),
            template_name: String::from("review-concern-security"),
        },
        ReviewOrchestrationConcernInput {
            key: String::from("documentation-code-drift"),
            template_name: String::from("review-concern-documentation-code-drift"),
        },
    ]
}

#[derive(Clone, Copy)]
pub(crate) struct ReviewPassFixture {
    pub(crate) run: CanonicalUuid,
    pub(crate) pass: CanonicalUuid,
    pub(crate) turn: CanonicalUuid,
    pub(crate) frontier: CanonicalUuid,
}

#[derive(Clone, Copy)]
pub(crate) struct ReviewFindingFixtures {
    pub(crate) accepted_and_fixed: CanonicalUuid,
    pub(crate) duplicate: CanonicalUuid,
    pub(crate) accepted_and_published: CanonicalUuid,
}

pub(crate) struct ReviewConcernEvidence {
    pub(crate) key: String,
    pub(crate) pass: CanonicalUuid,
}

pub(crate) struct ReviewRuntimeDriver {
    pub(crate) connection: Connection,
    pub(crate) pool: PgPool,
    pub(crate) target: CanonicalUuid,
    pub(crate) next_request: u64,
}

impl ReviewRuntimeDriver {
    pub(crate) async fn connect(
        runtime: &RunningRuntime,
        target: CanonicalUuid,
    ) -> Result<Self, Box<dyn Error>> {
        sqlx::query(
            "CREATE TABLE test_rejected_review_orchestration_receipt (command_id uuid PRIMARY KEY)",
        )
        .execute(&runtime.pool)
        .await?;
        sqlx::query(
            "CREATE FUNCTION test_review_orchestration_receipt_allowed(candidate uuid)
             RETURNS boolean LANGUAGE sql
             RETURN NOT EXISTS (
                 SELECT 1 FROM test_rejected_review_orchestration_receipt
                  WHERE command_id = candidate
             )",
        )
        .execute(&runtime.pool)
        .await?;
        sqlx::query(
            "ALTER TABLE review_orchestration_command
             ADD CONSTRAINT test_reject_orchestration_receipt
             CHECK (test_review_orchestration_receipt_allowed(command_id))",
        )
        .execute(&runtime.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE test_rejected_review_orchestration_recovery (command_id uuid PRIMARY KEY)",
        )
        .execute(&runtime.pool)
        .await?;
        sqlx::query(
            "CREATE FUNCTION test_review_orchestration_recovery_allowed(candidate uuid)
             RETURNS boolean LANGUAGE sql
             RETURN NOT EXISTS (
                 SELECT 1 FROM test_rejected_review_orchestration_recovery
                  WHERE command_id = candidate
             )",
        )
        .execute(&runtime.pool)
        .await?;
        sqlx::query(
            "ALTER TABLE review_orchestration_command_recovery
             ADD CONSTRAINT test_reject_orchestration_recovery
             CHECK (test_review_orchestration_recovery_allowed(command_id))",
        )
        .execute(&runtime.pool)
        .await?;
        Ok(Self {
            connection: Connection::connect(runtime.socket()).await?,
            pool: runtime.pool.clone(),
            target,
            next_request: 1,
        })
    }

    pub(crate) fn request_id(&mut self) -> u64 {
        let request = self.next_request;
        self.next_request += 1;
        request
    }

    pub(crate) async fn request_expect(
        &mut self,
        request: ClientRequest,
        expected: ServerMessage,
    ) -> Result<(), Box<dyn Error>> {
        let request_id = self.request_id();
        self.connection.request(request_id, request).await?;
        assert_eq!(
            response_within(&mut self.connection).await?.message(),
            &expected
        );
        Ok(())
    }

    pub(crate) async fn request_expect_after_lost_orchestration_receipt(
        &mut self,
        command_id: CommandId,
        request: ClientRequest,
        expected: ServerMessage,
    ) -> Result<(), Box<dyn Error>> {
        self.request_with_lost_orchestration_receipt(command_id, request.clone())
            .await?;
        self.request_expect(request, expected).await
    }

    pub(crate) async fn request_with_lost_orchestration_receipt(
        &mut self,
        command_id: CommandId,
        request: ClientRequest,
    ) -> Result<(), Box<dyn Error>> {
        sqlx::query(
            "INSERT INTO test_rejected_review_orchestration_receipt (command_id) VALUES ($1)",
        )
        .bind(command_id.into_uuid())
        .execute(&self.pool)
        .await?;
        let request_id = self.request_id();
        self.connection.request(request_id, request).await?;
        assert_eq!(
            protocol_error_code(response_within(&mut self.connection).await?.message()),
            ErrorCode::CommitAmbiguous,
        );
        let removed = sqlx::query(
            "DELETE FROM test_rejected_review_orchestration_receipt WHERE command_id = $1",
        )
        .bind(command_id.into_uuid())
        .execute(&self.pool)
        .await?;
        assert_eq!(removed.rows_affected(), 1);
        Ok(())
    }

    pub(crate) async fn request_with_lost_orchestration_recovery(
        &mut self,
        command_id: CommandId,
        request: ClientRequest,
    ) -> Result<(), Box<dyn Error>> {
        sqlx::query(
            "INSERT INTO test_rejected_review_orchestration_recovery (command_id) VALUES ($1)",
        )
        .bind(command_id.into_uuid())
        .execute(&self.pool)
        .await?;
        let request_id = self.request_id();
        self.connection.request(request_id, request).await?;
        assert_eq!(
            protocol_error_code(response_within(&mut self.connection).await?.message()),
            ErrorCode::CommitAmbiguous,
        );
        let removed = sqlx::query(
            "DELETE FROM test_rejected_review_orchestration_recovery WHERE command_id = $1",
        )
        .bind(command_id.into_uuid())
        .execute(&self.pool)
        .await?;
        assert_eq!(removed.rows_affected(), 1);
        Ok(())
    }

    pub(crate) async fn request_invalid(
        &mut self,
        request: ClientRequest,
    ) -> Result<(), Box<dyn Error>> {
        let request_id = self.request_id();
        self.connection.request(request_id, request).await?;
        assert_eq!(
            protocol_error_code(response_within(&mut self.connection).await?.message()),
            ErrorCode::InvalidRequest,
        );
        Ok(())
    }

    pub(crate) async fn create_target(&mut self) -> Result<(), Box<dyn Error>> {
        self.request_expect(
            ClientRequest::CreateReviewTarget {
                command_id: command()?,
                target_id: self.target,
                provider: String::from("github"),
                repository: String::from("keenwill/signalbox"),
                subject: ReviewTargetSubject::ChangeRequest {
                    number: CanonicalU64::new(343),
                },
                head_revision: String::from("reviewed-head-revision"),
                base_revision: Some(String::from("reviewed-base-revision")),
                stack_parent_target_id: None,
            },
            ServerMessage::ReviewTargetCreated {
                target_id: self.target,
            },
        )
        .await
    }

    pub(crate) async fn create_session_from_template(
        &mut self,
        template_name: &str,
    ) -> Result<CanonicalUuid, Box<dyn Error>> {
        let request_id = self.request_id();
        self.connection
            .request(
                request_id,
                ClientRequest::CreateSessionFromTemplate {
                    command_id: command()?,
                    template_name: String::from(template_name),
                    placement: SessionPlacement::Pathless {},
                    lifecycle: signalbox_process_protocol::SessionLifecycleMembers::default(),
                },
            )
            .await?;
        match response_within(&mut self.connection).await?.message() {
            ServerMessage::SessionCreated { session_id, .. } => Ok(*session_id),
            message => Err(io::Error::other(format!(
                "unexpected review-template session response: {message:?}"
            ))
            .into()),
        }
    }

    pub(crate) async fn submit_review_input(
        &mut self,
        session: CanonicalUuid,
        seed: u128,
    ) -> Result<(CanonicalUuid, CanonicalUuid), Box<dyn Error>> {
        let request_id = self.request_id();
        self.connection
            .request(
                request_id,
                ClientRequest::SubmitInput {
                    command_id: command()?,
                    session_id: session,
                    content: UserInputContent::text(format!("review pass fixture {seed}")),
                    expected_defaults_version: Some(CanonicalU64::new(1)),
                    model_settings: ModelSettingsOverlay::inherit_all(),
                    delivery: None,
                },
            )
            .await?;
        match response_within(&mut self.connection).await?.message() {
            ServerMessage::InputSubmitted {
                session_id,
                accepted_input_id,
                turn_id,
                ..
            } if *session_id == session => Ok((*accepted_input_id, *turn_id)),
            message => Err(io::Error::other(format!(
                "unexpected review-pass input response: {message:?}"
            ))
            .into()),
        }
    }

    pub(crate) async fn create_completed_turn_pass(
        &mut self,
        template_name: &str,
        workflow: ReviewWorkflow,
        seed: u128,
    ) -> Result<ReviewPassFixture, Box<dyn Error>> {
        let session = self.create_session_from_template(template_name).await?;
        let (accepted_input, turn) = self.submit_review_input(session, seed).await?;
        let run = review_identity(seed);
        let pass = review_identity(seed + 1);
        self.request_expect(
            ClientRequest::StartReviewRun {
                command_id: command()?,
                target_id: self.target,
                run_id: run,
                pass_id: pass,
                workflow,
                session_id: session,
                accepted_input_id: accepted_input,
            },
            ServerMessage::ReviewRunStarted {
                run_id: run,
                pass_id: pass,
            },
        )
        .await?;
        activate_turn(&self.pool, SessionId::from_uuid(session.into_uuid())).await?;
        self.request_expect(
            ClientRequest::ActivateReviewPass {
                command_id: command()?,
                run_id: run,
                pass_id: pass,
                turn_id: turn,
            },
            ServerMessage::ReviewPassActivated {
                run_id: run,
                pass_id: pass,
            },
        )
        .await?;
        let targets = support::parse_model_configuration(MODEL_CONFIGURATION)?.target_catalog();
        complete_active_text_turn(
            &self.pool,
            SessionId::from_uuid(session.into_uuid()),
            targets,
        )
        .await?;
        let frontier: Uuid = sqlx::query_scalar(
            "SELECT terminal_frontier_id FROM turn_lifecycle WHERE turn_id = $1",
        )
        .bind(turn.into_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(ReviewPassFixture {
            run,
            pass,
            turn,
            frontier: CanonicalUuid::from_uuid(frontier),
        })
    }

    pub(crate) async fn reject_mismatched_pass_completion(
        &mut self,
        fixture: ReviewPassFixture,
    ) -> Result<(), Box<dyn Error>> {
        self.request_invalid(ClientRequest::CompleteReviewPass {
            command_id: command()?,
            run_id: fixture.run,
            pass_id: fixture.pass,
            turn_id: Some(fixture.turn),
            output_frontier_id: Some(review_identity(0xdead)),
            outcome: ReviewPassTerminalOutcome::Succeeded,
        })
        .await
    }

    pub(crate) async fn complete_result_free_pass(
        &mut self,
        fixture: ReviewPassFixture,
    ) -> Result<(), Box<dyn Error>> {
        self.request_expect(
            ClientRequest::CompleteReviewPass {
                command_id: command()?,
                run_id: fixture.run,
                pass_id: fixture.pass,
                turn_id: Some(fixture.turn),
                output_frontier_id: Some(fixture.frontier),
                outcome: ReviewPassTerminalOutcome::Succeeded,
            },
            ServerMessage::ReviewPassCompleted {
                run_id: fixture.run,
                pass_id: fixture.pass,
                state: signalbox_process_protocol::ReviewPassLifecycle::Succeeded,
            },
        )
        .await
    }

    pub(crate) async fn complete_failed_pass(
        &mut self,
        fixture: ReviewPassFixture,
    ) -> Result<(), Box<dyn Error>> {
        self.request_expect(
            ClientRequest::CompleteReviewPass {
                command_id: command()?,
                run_id: fixture.run,
                pass_id: fixture.pass,
                turn_id: Some(fixture.turn),
                output_frontier_id: None,
                outcome: ReviewPassTerminalOutcome::Failed,
            },
            ServerMessage::ReviewPassCompleted {
                run_id: fixture.run,
                pass_id: fixture.pass,
                state: signalbox_process_protocol::ReviewPassLifecycle::Failed,
            },
        )
        .await
    }

    pub(crate) async fn record_findings(
        &mut self,
        fixture: ReviewPassFixture,
        findings: Vec<ReviewFindingInput>,
    ) -> Result<(), Box<dyn Error>> {
        let finding_count = CanonicalU64::new(u64::try_from(findings.len())?);
        self.request_expect(
            ClientRequest::RecordReviewFindings {
                command_id: command()?,
                run_id: fixture.run,
                pass_id: fixture.pass,
                turn_id: fixture.turn,
                output_frontier_id: fixture.frontier,
                findings,
            },
            ServerMessage::ReviewFindingsRecorded {
                run_id: fixture.run,
                pass_id: fixture.pass,
                finding_count,
            },
        )
        .await
    }

    pub(crate) async fn record_finding_event(
        &mut self,
        fixture: ReviewPassFixture,
        finding: CanonicalUuid,
        ordinal: u64,
        event: ReviewFindingEvent,
        expected_status: ReviewFindingStatus,
    ) -> Result<(), Box<dyn Error>> {
        self.request_expect(
            ClientRequest::RecordReviewFindingEvent {
                command_id: command()?,
                run_id: fixture.run,
                pass_id: fixture.pass,
                turn_id: fixture.turn,
                output_frontier_id: Some(fixture.frontier),
                finding_id: finding,
                event_ordinal: CanonicalU64::new(ordinal),
                event,
            },
            ServerMessage::ReviewFindingEventRecorded {
                finding_id: finding,
                status: expected_status,
            },
        )
        .await
    }

    pub(crate) fn start_attempt_request(
        &self,
        command_id: CommandId,
        attempt: CanonicalUuid,
    ) -> ClientRequest {
        ClientRequest::StartReviewOrchestration {
            command_id,
            attempt_id: attempt,
            target_id: self.target,
            concern_set_version: String::from(REVIEW_CONCERN_SET_VERSION),
            import_template_name: String::from(REVIEW_IMPORT_TEMPLATE),
            judgment_template_name: String::from(REVIEW_JUDGMENT_TEMPLATE),
            repair_template_name: String::from(REVIEW_REPAIR_TEMPLATE),
            publication_template_name: String::from(REVIEW_PUBLICATION_TEMPLATE),
            concerns: review_concern_inputs(),
        }
    }

    pub(crate) async fn start_attempt(
        &mut self,
        attempt: CanonicalUuid,
    ) -> Result<(), Box<dyn Error>> {
        let command_id = command()?;
        let request = self.start_attempt_request(command_id, attempt);
        self.request_expect_after_lost_orchestration_receipt(
            command_id,
            request,
            ServerMessage::ReviewOrchestrationStarted {
                attempt_id: attempt,
            },
        )
        .await
    }

    pub(crate) async fn reject_result_free_read_only_success(
        &mut self,
        fixture: ReviewPassFixture,
    ) -> Result<(), Box<dyn Error>> {
        self.request_invalid(ClientRequest::CompleteReviewPass {
            command_id: command()?,
            run_id: fixture.run,
            pass_id: fixture.pass,
            turn_id: Some(fixture.turn),
            output_frontier_id: Some(fixture.frontier),
            outcome: ReviewPassTerminalOutcome::Succeeded,
        })
        .await
    }

    pub(crate) async fn reject_restart_after_import(
        &mut self,
        attempt: CanonicalUuid,
    ) -> Result<(), Box<dyn Error>> {
        let request = self.start_attempt_request(command()?, attempt);
        self.request_invalid(request).await
    }

    pub(crate) async fn record_import(
        &mut self,
        attempt: CanonicalUuid,
        pass: CanonicalUuid,
    ) -> Result<(), Box<dyn Error>> {
        let command_id = command()?;
        self.request_expect_after_lost_orchestration_receipt(
            command_id,
            ClientRequest::RecordReviewImportOutcome {
                command_id,
                attempt_id: attempt,
                pass_id: Some(pass),
                external_link_id: None,
                context_digest: Some(CanonicalDigest::try_new("11".repeat(32))?),
                outcome: ReviewImportTerminalOutcome::Succeeded,
            },
            ServerMessage::ReviewOrchestrationAdvanced {
                attempt_id: attempt,
                state: ReviewOrchestrationState::AwaitingConcerns,
            },
        )
        .await
    }

    pub(crate) async fn record_import_with_lost_recovery(
        &mut self,
        attempt: CanonicalUuid,
        pass: CanonicalUuid,
    ) -> Result<ClientRequest, Box<dyn Error>> {
        let command_id = command()?;
        let request = ClientRequest::RecordReviewImportOutcome {
            command_id,
            attempt_id: attempt,
            pass_id: Some(pass),
            external_link_id: None,
            context_digest: Some(CanonicalDigest::try_new("11".repeat(32))?),
            outcome: ReviewImportTerminalOutcome::Succeeded,
        };
        self.request_with_lost_orchestration_recovery(command_id, request.clone())
            .await?;
        Ok(request)
    }

    pub(crate) async fn reject_fresh_import_after_progress(
        &mut self,
        attempt: CanonicalUuid,
        pass: CanonicalUuid,
    ) -> Result<(), Box<dyn Error>> {
        let request = ClientRequest::RecordReviewImportOutcome {
            command_id: command()?,
            attempt_id: attempt,
            pass_id: Some(pass),
            external_link_id: None,
            context_digest: Some(CanonicalDigest::try_new("11".repeat(32))?),
            outcome: ReviewImportTerminalOutcome::Succeeded,
        };
        self.request_invalid(request.clone()).await?;
        self.request_invalid(request).await
    }

    pub(crate) async fn record_concern(
        &mut self,
        attempt: CanonicalUuid,
        concern: &ReviewConcernEvidence,
        expected_state: ReviewOrchestrationState,
    ) -> Result<(), Box<dyn Error>> {
        let command_id = command()?;
        self.request_expect_after_lost_orchestration_receipt(
            command_id,
            ClientRequest::RecordReviewConcernOutcome {
                command_id,
                attempt_id: attempt,
                concern: concern.key.clone(),
                pass_id: Some(concern.pass),
                outcome: ReviewConcernTerminalOutcome::Succeeded,
            },
            ServerMessage::ReviewOrchestrationAdvanced {
                attempt_id: attempt,
                state: expected_state,
            },
        )
        .await
    }

    pub(crate) async fn record_failed_concern_with_lost_recovery(
        &mut self,
        attempt: CanonicalUuid,
        concern: &ReviewConcernEvidence,
        failed_pass: CanonicalUuid,
    ) -> Result<ClientRequest, Box<dyn Error>> {
        let command_id = command()?;
        let request = ClientRequest::RecordReviewConcernOutcome {
            command_id,
            attempt_id: attempt,
            concern: concern.key.clone(),
            pass_id: Some(failed_pass),
            outcome: ReviewConcernTerminalOutcome::Failed,
        };
        self.request_with_lost_orchestration_recovery(command_id, request.clone())
            .await?;
        Ok(request)
    }

    pub(crate) async fn record_concerns_after_first(
        &mut self,
        attempt: CanonicalUuid,
        concerns: &[ReviewConcernEvidence],
    ) -> Result<(), Box<dyn Error>> {
        self.record_concern(
            attempt,
            &concerns[1],
            ReviewOrchestrationState::AwaitingConcerns,
        )
        .await?;
        self.record_concern(
            attempt,
            &concerns[2],
            ReviewOrchestrationState::AwaitingConcerns,
        )
        .await?;
        self.record_concern(
            attempt,
            &concerns[3],
            ReviewOrchestrationState::AwaitingConcerns,
        )
        .await?;
        self.record_concern(
            attempt,
            &concerns[4],
            ReviewOrchestrationState::AwaitingJudgment,
        )
        .await
    }

    pub(crate) async fn record_complete_concerns(
        &mut self,
        attempt: CanonicalUuid,
        concerns: &[ReviewConcernEvidence],
    ) -> Result<(), Box<dyn Error>> {
        self.record_concern(
            attempt,
            &concerns[0],
            ReviewOrchestrationState::AwaitingConcerns,
        )
        .await?;
        self.record_concern(
            attempt,
            &concerns[1],
            ReviewOrchestrationState::AwaitingConcerns,
        )
        .await?;
        self.record_concern(
            attempt,
            &concerns[2],
            ReviewOrchestrationState::AwaitingConcerns,
        )
        .await?;
        self.record_concern(
            attempt,
            &concerns[3],
            ReviewOrchestrationState::AwaitingConcerns,
        )
        .await?;
        self.record_concern(
            attempt,
            &concerns[4],
            ReviewOrchestrationState::AwaitingJudgment,
        )
        .await
    }

    pub(crate) async fn record_judgment_plan(
        &mut self,
        attempt: CanonicalUuid,
        analysis_pass: CanonicalUuid,
        members: Vec<ReviewJudgmentPlanMember>,
    ) -> Result<(), Box<dyn Error>> {
        let command_id = command()?;
        self.request_expect_after_lost_orchestration_receipt(
            command_id,
            ClientRequest::RecordReviewJudgmentPlan {
                command_id,
                attempt_id: attempt,
                analysis_pass_id: analysis_pass,
                members,
            },
            ServerMessage::ReviewOrchestrationAdvanced {
                attempt_id: attempt,
                state: ReviewOrchestrationState::AwaitingJudgmentEffects,
            },
        )
        .await
    }

    pub(crate) async fn record_effect(
        &mut self,
        attempt: CanonicalUuid,
        finding: CanonicalUuid,
        pass: CanonicalUuid,
        expected_state: ReviewOrchestrationState,
    ) -> Result<(), Box<dyn Error>> {
        let command_id = command()?;
        self.request_expect_after_lost_orchestration_receipt(
            command_id,
            ClientRequest::RecordReviewJudgmentEffect {
                command_id,
                attempt_id: attempt,
                finding_id: finding,
                event_pass_id: Some(pass),
                outcome: ReviewJudgmentEffectTerminalOutcome::Applied,
            },
            ServerMessage::ReviewOrchestrationAdvanced {
                attempt_id: attempt,
                state: expected_state,
            },
        )
        .await
    }
}

pub(crate) fn review_finding(
    finding_id: CanonicalUuid,
    title: &str,
    category: &str,
) -> ReviewFindingInput {
    ReviewFindingInput {
        finding_id,
        file_path: String::from("apps/signalboxd/src/process_runtime.rs"),
        line_start: Some(CanonicalU64::new(1)),
        line_end: Some(CanonicalU64::new(1)),
        diff_side: Some(ReviewDiffSide::Right),
        title: String::from(title),
        body: String::from("The fixture supplies concrete repository evidence."),
        severity: ReviewSeverity::Medium,
        is_real_confidence: CanonicalU64::new(9_200),
        severity_label_confidence: CanonicalU64::new(8_000),
        category: String::from(category),
        recommended_fix: Some(String::from("Apply the bounded fixture repair.")),
    }
}

pub(crate) fn complete_judgment_members(
    findings: ReviewFindingFixtures,
) -> Vec<ReviewJudgmentPlanMember> {
    vec![
        ReviewJudgmentPlanMember {
            finding_id: findings.accepted_and_fixed,
            disposition: ReviewJudgmentDisposition::Accepted {},
            judgment: signalbox_process_protocol::ReviewJudgmentResult {
                bar_category: String::from("own-behavior-defect"),
                decline_class: None,
                confidence: CanonicalU64::new(5),
                reason: String::from("The fixture supplies concrete evidence."),
            },
        },
        ReviewJudgmentPlanMember {
            finding_id: findings.duplicate,
            disposition: ReviewJudgmentDisposition::Duplicate {
                canonical_finding_id: findings.accepted_and_fixed,
            },
            judgment: signalbox_process_protocol::ReviewJudgmentResult {
                bar_category: String::from("none"),
                decline_class: Some(String::from("duplicate")),
                confidence: CanonicalU64::new(5),
                reason: String::from("The fixture supplies concrete evidence."),
            },
        },
        ReviewJudgmentPlanMember {
            finding_id: findings.accepted_and_published,
            disposition: ReviewJudgmentDisposition::Accepted {},
            judgment: signalbox_process_protocol::ReviewJudgmentResult {
                bar_category: String::from("own-behavior-defect"),
                decline_class: None,
                confidence: CanonicalU64::new(5),
                reason: String::from("The fixture supplies concrete evidence."),
            },
        },
    ]
}

pub(crate) fn direct_cycle_members(
    findings: ReviewFindingFixtures,
) -> Vec<ReviewJudgmentPlanMember> {
    vec![
        ReviewJudgmentPlanMember {
            finding_id: findings.accepted_and_fixed,
            disposition: ReviewJudgmentDisposition::Duplicate {
                canonical_finding_id: findings.duplicate,
            },
            judgment: signalbox_process_protocol::ReviewJudgmentResult {
                bar_category: String::from("none"),
                decline_class: Some(String::from("duplicate")),
                confidence: CanonicalU64::new(5),
                reason: String::from("The fixture supplies concrete evidence."),
            },
        },
        ReviewJudgmentPlanMember {
            finding_id: findings.duplicate,
            disposition: ReviewJudgmentDisposition::Duplicate {
                canonical_finding_id: findings.accepted_and_fixed,
            },
            judgment: signalbox_process_protocol::ReviewJudgmentResult {
                bar_category: String::from("none"),
                decline_class: Some(String::from("duplicate")),
                confidence: CanonicalU64::new(5),
                reason: String::from("The fixture supplies concrete evidence."),
            },
        },
        ReviewJudgmentPlanMember {
            finding_id: findings.accepted_and_published,
            disposition: ReviewJudgmentDisposition::Accepted {},
            judgment: signalbox_process_protocol::ReviewJudgmentResult {
                bar_category: String::from("own-behavior-defect"),
                decline_class: None,
                confidence: CanonicalU64::new(5),
                reason: String::from("The fixture supplies concrete evidence."),
            },
        },
    ]
}

pub(crate) fn transitive_cycle_members(
    findings: ReviewFindingFixtures,
) -> Vec<ReviewJudgmentPlanMember> {
    vec![
        ReviewJudgmentPlanMember {
            finding_id: findings.accepted_and_fixed,
            disposition: ReviewJudgmentDisposition::Duplicate {
                canonical_finding_id: findings.duplicate,
            },
            judgment: signalbox_process_protocol::ReviewJudgmentResult {
                bar_category: String::from("none"),
                decline_class: Some(String::from("duplicate")),
                confidence: CanonicalU64::new(5),
                reason: String::from("The fixture supplies concrete evidence."),
            },
        },
        ReviewJudgmentPlanMember {
            finding_id: findings.duplicate,
            disposition: ReviewJudgmentDisposition::Duplicate {
                canonical_finding_id: findings.accepted_and_published,
            },
            judgment: signalbox_process_protocol::ReviewJudgmentResult {
                bar_category: String::from("none"),
                decline_class: Some(String::from("duplicate")),
                confidence: CanonicalU64::new(5),
                reason: String::from("The fixture supplies concrete evidence."),
            },
        },
        ReviewJudgmentPlanMember {
            finding_id: findings.accepted_and_published,
            disposition: ReviewJudgmentDisposition::Duplicate {
                canonical_finding_id: findings.accepted_and_fixed,
            },
            judgment: signalbox_process_protocol::ReviewJudgmentResult {
                bar_category: String::from("none"),
                decline_class: Some(String::from("duplicate")),
                confidence: CanonicalU64::new(5),
                reason: String::from("The fixture supplies concrete evidence."),
            },
        },
    ]
}

#[derive(Clone, Copy)]
pub(crate) enum PlanRejectionFanout {
    Complete,
    FirstConcernOnly,
}

pub(crate) async fn prove_orchestration_plan_rejection(
    driver: &mut ReviewRuntimeDriver,
    attempt: CanonicalUuid,
    import_pass: CanonicalUuid,
    concerns: &[ReviewConcernEvidence],
    analysis_pass: CanonicalUuid,
    members: Vec<ReviewJudgmentPlanMember>,
    fanout: PlanRejectionFanout,
) -> Result<(), Box<dyn Error>> {
    driver.start_attempt(attempt).await?;
    driver.record_import(attempt, import_pass).await?;
    match fanout {
        PlanRejectionFanout::Complete => {
            driver.record_complete_concerns(attempt, concerns).await?;
        }
        PlanRejectionFanout::FirstConcernOnly => {
            driver
                .record_concern(
                    attempt,
                    &concerns[0],
                    ReviewOrchestrationState::AwaitingConcerns,
                )
                .await?;
        }
    }
    driver
        .request_invalid(ClientRequest::RecordReviewJudgmentPlan {
            command_id: command()?,
            attempt_id: attempt,
            analysis_pass_id: analysis_pass,
            members,
        })
        .await
}

pub(crate) fn assert_complete_review_snapshot(
    snapshot: &ReviewOrchestrationSnapshot,
    attempt: CanonicalUuid,
    expected_counts: ReviewOrchestrationCounts,
    target: CanonicalUuid,
    concerns: &[ReviewConcernEvidence],
) {
    assert_eq!(snapshot.attempt_id, attempt);
    assert_eq!(snapshot.target_id, target);
    assert_eq!(snapshot.state, ReviewOrchestrationState::Complete);
    assert_eq!(snapshot.concern_set_version, REVIEW_CONCERN_SET_VERSION);
    assert_eq!(snapshot.concerns.len(), concerns.len());
    assert_eq!(snapshot.concerns[0].key, concerns[0].key);
    assert_eq!(
        snapshot.concerns[0].status,
        ReviewOrchestrationConcernStatus::Succeeded
    );
    assert_eq!(snapshot.concerns[0].pass_id, Some(concerns[0].pass));
    assert_eq!(snapshot.concerns[1].key, concerns[1].key);
    assert_eq!(
        snapshot.concerns[1].status,
        ReviewOrchestrationConcernStatus::Succeeded
    );
    assert_eq!(snapshot.concerns[1].pass_id, Some(concerns[1].pass));
    assert_eq!(snapshot.concerns[2].key, concerns[2].key);
    assert_eq!(
        snapshot.concerns[2].status,
        ReviewOrchestrationConcernStatus::Succeeded
    );
    assert_eq!(snapshot.concerns[2].pass_id, Some(concerns[2].pass));
    assert_eq!(snapshot.concerns[3].key, concerns[3].key);
    assert_eq!(
        snapshot.concerns[3].status,
        ReviewOrchestrationConcernStatus::Succeeded
    );
    assert_eq!(snapshot.concerns[3].pass_id, Some(concerns[3].pass));
    assert_eq!(snapshot.concerns[4].key, concerns[4].key);
    assert_eq!(
        snapshot.concerns[4].status,
        ReviewOrchestrationConcernStatus::Succeeded
    );
    assert_eq!(snapshot.concerns[4].pass_id, Some(concerns[4].pass));
    assert_eq!(snapshot.counts, expected_counts);
}

pub(crate) async fn read_complete_review_snapshot(
    driver: &mut ReviewRuntimeDriver,
    attempt: CanonicalUuid,
    expected_counts: ReviewOrchestrationCounts,
    concerns: &[ReviewConcernEvidence],
) -> Result<(), Box<dyn Error>> {
    let request_id = driver.request_id();
    driver
        .connection
        .request(
            request_id,
            ClientRequest::ReadReviewOrchestration {
                attempt_id: attempt,
            },
        )
        .await?;
    match response_within(&mut driver.connection).await?.message() {
        ServerMessage::ReviewOrchestration { snapshot } => {
            assert_complete_review_snapshot(
                snapshot,
                attempt,
                expected_counts,
                driver.target,
                concerns,
            );
            Ok(())
        }
        message => Err(io::Error::other(format!(
            "unexpected complete review-orchestration snapshot: {message:?}"
        ))
        .into()),
    }
}

pub(crate) async fn drive_review_orchestration_process_loop() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let target = review_identity(0xa000);
    let attempt = review_identity(0xa100);
    let findings = ReviewFindingFixtures {
        accepted_and_fixed: review_identity(0xb001),
        duplicate: review_identity(0xb002),
        accepted_and_published: review_identity(0xb003),
    };
    let expected_counts = ReviewOrchestrationCounts {
        finding_count: CanonicalU64::new(3),
        judgment_member_count: CanonicalU64::new(3),
        judgment_effect_applied_count: CanonicalU64::new(3),
        repair_fixed_count: CanonicalU64::new(1),
        publication_published_count: CanonicalU64::new(1),
    };
    let mut driver = ReviewRuntimeDriver::connect(&runtime, target).await?;
    driver.create_target().await?;
    driver.start_attempt(attempt).await?;

    let import = driver
        .create_completed_turn_pass(
            REVIEW_IMPORT_TEMPLATE,
            ReviewWorkflow::ImportExternalContext,
            0xc000,
        )
        .await?;
    driver.reject_mismatched_pass_completion(import).await?;
    driver.complete_result_free_pass(import).await?;
    let import_retry = driver
        .record_import_with_lost_recovery(attempt, import.pass)
        .await?;
    driver.reject_restart_after_import(attempt).await?;

    let failed_correctness = driver
        .create_completed_turn_pass(
            "review-concern-correctness",
            ReviewWorkflow::ReadOnlyReview,
            0xc050,
        )
        .await?;
    driver.complete_failed_pass(failed_correctness).await?;

    let correctness = driver
        .create_completed_turn_pass(
            "review-concern-correctness",
            ReviewWorkflow::ReadOnlyReview,
            0xc100,
        )
        .await?;
    driver
        .reject_result_free_read_only_success(correctness)
        .await?;
    driver
        .record_findings(
            correctness,
            vec![review_finding(
                findings.accepted_and_fixed,
                "Accepted repair",
                "correctness",
            )],
        )
        .await?;
    let interface = driver
        .create_completed_turn_pass(
            "review-concern-interface-and-type-design",
            ReviewWorkflow::ReadOnlyReview,
            0xc200,
        )
        .await?;
    driver
        .record_findings(
            interface,
            vec![review_finding(
                findings.duplicate,
                "Cross-concern duplicate",
                "interface-and-type-design",
            )],
        )
        .await?;
    let tests = driver
        .create_completed_turn_pass(
            "review-concern-test-quality",
            ReviewWorkflow::ReadOnlyReview,
            0xc300,
        )
        .await?;
    driver
        .record_findings(
            tests,
            vec![review_finding(
                findings.accepted_and_published,
                "Accepted publication",
                "test-quality",
            )],
        )
        .await?;
    let security = driver
        .create_completed_turn_pass(
            "review-concern-security",
            ReviewWorkflow::ReadOnlyReview,
            0xc400,
        )
        .await?;
    driver.record_findings(security, Vec::new()).await?;
    let documentation = driver
        .create_completed_turn_pass(
            "review-concern-documentation-code-drift",
            ReviewWorkflow::ReadOnlyReview,
            0xc500,
        )
        .await?;
    driver.record_findings(documentation, Vec::new()).await?;
    let concerns = vec![
        ReviewConcernEvidence {
            key: String::from("correctness"),
            pass: correctness.pass,
        },
        ReviewConcernEvidence {
            key: String::from("interface-and-type-design"),
            pass: interface.pass,
        },
        ReviewConcernEvidence {
            key: String::from("test-quality"),
            pass: tests.pass,
        },
        ReviewConcernEvidence {
            key: String::from("security"),
            pass: security.pass,
        },
        ReviewConcernEvidence {
            key: String::from("documentation-code-drift"),
            pass: documentation.pass,
        },
    ];
    let first_concern_retry = driver
        .record_failed_concern_with_lost_recovery(attempt, &concerns[0], failed_correctness.pass)
        .await?;
    driver
        .record_concern(
            attempt,
            &concerns[0],
            ReviewOrchestrationState::AwaitingConcerns,
        )
        .await?;
    driver
        .record_concerns_after_first(attempt, &concerns)
        .await?;
    driver
        .request_expect(
            import_retry,
            ServerMessage::ReviewOrchestrationAdvanced {
                attempt_id: attempt,
                state: ReviewOrchestrationState::AwaitingConcerns,
            },
        )
        .await?;
    driver
        .request_expect(
            first_concern_retry,
            ServerMessage::ReviewOrchestrationAdvanced {
                attempt_id: attempt,
                state: ReviewOrchestrationState::AwaitingConcerns,
            },
        )
        .await?;
    driver
        .reject_fresh_import_after_progress(attempt, import.pass)
        .await?;

    let analysis = driver
        .create_completed_turn_pass(
            REVIEW_JUDGMENT_TEMPLATE,
            ReviewWorkflow::JudgeFindings,
            0xc600,
        )
        .await?;
    driver.complete_result_free_pass(analysis).await?;
    driver
        .record_judgment_plan(attempt, analysis.pass, complete_judgment_members(findings))
        .await?;

    prove_orchestration_plan_rejection(
        &mut driver,
        review_identity(0xa200),
        import.pass,
        &concerns,
        analysis.pass,
        complete_judgment_members(findings),
        PlanRejectionFanout::FirstConcernOnly,
    )
    .await?;
    prove_orchestration_plan_rejection(
        &mut driver,
        review_identity(0xa300),
        import.pass,
        &concerns,
        analysis.pass,
        direct_cycle_members(findings),
        PlanRejectionFanout::Complete,
    )
    .await?;
    prove_orchestration_plan_rejection(
        &mut driver,
        review_identity(0xa400),
        import.pass,
        &concerns,
        analysis.pass,
        transitive_cycle_members(findings),
        PlanRejectionFanout::Complete,
    )
    .await?;

    let accepted_fixed = driver
        .create_completed_turn_pass(
            REVIEW_JUDGMENT_TEMPLATE,
            ReviewWorkflow::JudgeFindings,
            0xc700,
        )
        .await?;
    driver
        .record_finding_event(
            accepted_fixed,
            findings.accepted_and_fixed,
            1,
            ReviewFindingEvent::Accepted {
                confidence: CanonicalU64::new(5),
            },
            ReviewFindingStatus::Accepted,
        )
        .await?;
    driver
        .record_effect(
            attempt,
            findings.accepted_and_fixed,
            accepted_fixed.pass,
            ReviewOrchestrationState::AwaitingJudgmentEffects,
        )
        .await?;
    let duplicate = driver
        .create_completed_turn_pass(
            REVIEW_JUDGMENT_TEMPLATE,
            ReviewWorkflow::DedupeFindings,
            0xc800,
        )
        .await?;
    driver
        .record_finding_event(
            duplicate,
            findings.duplicate,
            1,
            ReviewFindingEvent::Duplicate {
                canonical_finding_id: findings.accepted_and_fixed,
            },
            ReviewFindingStatus::Duplicate,
        )
        .await?;
    driver
        .record_effect(
            attempt,
            findings.duplicate,
            duplicate.pass,
            ReviewOrchestrationState::AwaitingJudgmentEffects,
        )
        .await?;
    let accepted_published = driver
        .create_completed_turn_pass(
            REVIEW_JUDGMENT_TEMPLATE,
            ReviewWorkflow::JudgeFindings,
            0xc900,
        )
        .await?;
    driver
        .record_finding_event(
            accepted_published,
            findings.accepted_and_published,
            1,
            ReviewFindingEvent::Accepted {
                confidence: CanonicalU64::new(5),
            },
            ReviewFindingStatus::Accepted,
        )
        .await?;
    driver
        .record_effect(
            attempt,
            findings.accepted_and_published,
            accepted_published.pass,
            ReviewOrchestrationState::AwaitingRepair,
        )
        .await?;

    let repair = driver
        .create_completed_turn_pass(REVIEW_REPAIR_TEMPLATE, ReviewWorkflow::FixFindings, 0xca00)
        .await?;
    driver
        .record_finding_event(
            repair,
            findings.accepted_and_fixed,
            2,
            ReviewFindingEvent::Fixed {},
            ReviewFindingStatus::Fixed,
        )
        .await?;
    let repair_command = command()?;
    let repair_request = ClientRequest::RecordReviewRepairOutcomes {
        command_id: repair_command,
        attempt_id: attempt,
        outcomes: vec![
            ReviewRepairOutcome {
                finding_id: findings.accepted_and_fixed,
                event_pass_id: Some(repair.pass),
                outcome: ReviewRepairTerminalOutcome::Fixed,
            },
            ReviewRepairOutcome {
                finding_id: findings.accepted_and_published,
                event_pass_id: None,
                outcome: ReviewRepairTerminalOutcome::Failed,
            },
        ],
    };
    driver
        .request_with_lost_orchestration_receipt(repair_command, repair_request.clone())
        .await?;

    let external_link = review_identity(0xd000);
    driver
        .request_expect(
            ClientRequest::ReserveReviewExternalLink {
                command_id: command()?,
                external_link_id: external_link,
                finding_id: findings.accepted_and_published,
                provider: String::from("github"),
                object_kind: ReviewExternalObjectKind::ReviewComment,
            },
            ServerMessage::ReviewExternalLinkReserved {
                external_link_id: external_link,
            },
        )
        .await?;
    let publication = driver
        .create_completed_turn_pass(
            REVIEW_PUBLICATION_TEMPLATE,
            ReviewWorkflow::PublishReview,
            0xcb00,
        )
        .await?;
    driver
        .request_expect(
            ClientRequest::AttachReviewExternalLink {
                command_id: command()?,
                external_link_id: external_link,
                run_id: publication.run,
                pass_id: publication.pass,
                turn_id: publication.turn,
                output_frontier_id: publication.frontier,
                external_object: String::from("provider-comment-1"),
                event_ordinal: CanonicalU64::new(2),
            },
            ServerMessage::ReviewExternalLinkAttached {
                external_link_id: external_link,
                external_object: String::from("provider-comment-1"),
            },
        )
        .await?;
    let publication_command = command()?;
    driver
        .request_expect_after_lost_orchestration_receipt(
            publication_command,
            ClientRequest::RecordReviewPublicationOutcomes {
                command_id: publication_command,
                attempt_id: attempt,
                outcomes: vec![ReviewPublicationOutcome {
                    finding_id: findings.accepted_and_published,
                    external_link_id: Some(external_link),
                    outcome: ReviewPublicationTerminalOutcome::Published,
                }],
            },
            ServerMessage::ReviewOrchestrationAdvanced {
                attempt_id: attempt,
                state: ReviewOrchestrationState::Complete,
            },
        )
        .await?;
    driver
        .request_expect(
            repair_request,
            ServerMessage::ReviewOrchestrationAdvanced {
                attempt_id: attempt,
                state: ReviewOrchestrationState::AwaitingPublication,
            },
        )
        .await?;
    read_complete_review_snapshot(&mut driver, attempt, expected_counts, &concerns).await?;

    drop(driver);
    runtime.stop().await
}

/// One process client can drive the frozen five-concern review library through
/// its structural fan-out barrier, cross-concern deduplication, repair, and
/// reservation-backed publication against the real PostgreSQL adapters.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn review_orchestration_reaches_complete_through_the_process_protocol()
-> Result<(), Box<dyn Error>> {
    drive_review_orchestration_process_loop().await
}
