#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        collections::{BTreeSet, VecDeque},
        error::Error,
        io::{self, Write},
        sync::{Arc, Mutex, OnceLock, mpsc},
        thread,
    };

    use signalbox_application::{
        EligibilityNudge, EligibilityNudgeOutcome, ImportConversationError,
        ImportedConversationConverter,
    };
    use signalbox_conversation_import_claude_code::ClaudeCodeJsonlConversionFailure;
    use signalbox_conversation_import_codex::CodexRolloutJsonlConversionFailure;
    use signalbox_domain::{
        AcceptedInputId, Actor, ContextFrontierId, DelegationMessageId, DirectModelSelection,
        DurableCommandId, FastModeOverlay, FastModeSupport, FrozenAliasDefinition,
        FrozenModelSelection, Goal, GoalStatement, GoalUserProvenance, ImportedConversation,
        ImportedConversationFormat, ImportedConversationId, ImportedTranscriptEntryId, ModelAlias,
        ModelCallId, ModelCapabilities, ModelChangeAdjustment, ModelSelectionRequest,
        ModelSettingsOverlay, ModelSettingsPrecedence, ReasoningLevel, ReviewPass,
        ReviewPassAcceptedInputEvidence, ReviewPassEvidence, ReviewPassId, ReviewPassKind,
        ReviewPassRef, ReviewPassState, ReviewPassTurnEvidence, ReviewPassTurnOutcome,
        ReviewPolicy, ReviewRun, ReviewRunId, ReviewRunRef, ReviewRunState, ReviewTargetId,
        ReviewWorkflowKind, RunnerGeneration, RunnerId, RunnerWorkingDirectory,
        SemanticTranscriptEntryId, SessionConfigurationDefaultsVersion, SessionId,
        SessionInputPosition, SessionLifecycleApplication, SessionLifecycleOperation,
        SessionLifecycleState, SessionMetadataLastWriter, SessionMetadataUpdatedAt,
        SessionModelSettingsChanged, SettingOverlay, SubmitInputRejectedResult,
        ToolApprovalDecision, ToolAttemptId, ToolRequestId, TurnAttemptId, TurnId,
        TurnModelSettingsResolved, ValidatedModelSettings,
    };
    use signalbox_process_protocol::{
        CanonicalU64, CanonicalUuid, ClientRequest, CommandId, ConversationImportRejectionClass,
        DelegationToolRequestState as WireDelegationToolRequestState, ErrorCode, ErrorDetail,
        FinishCondition as WireFinishCondition, FrameEncodeError, GoalLifecycleState,
        ImportedContentKind, ImportedSourceSpeaker, ImportedSpeaker, MAX_CONTENT_FRAGMENT_BYTES,
        MetadataActor, ProtocolVersion, RejectionDetail, ReviewFindingInput, ReviewSeverity,
        RunnerPlacementRevision as WireRunnerPlacementRevision,
        RunnerSandboxProfile as WireRunnerSandboxProfile,
        RunnerStateTransitionState as WireRunnerStateTransitionState,
        RunnerWorkingDirectory as WireRunnerWorkingDirectory, ServerFrame, ServerMessage,
        SessionEvent, ToolBatchState, ToolDecision, TranscriptEntry, TranscriptTextEntry,
        TurnState, UserInputContent, decode_server_line, encode_server_line,
    };
    use sqlx::postgres::PgPoolOptions;
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, duplex},
        net::UnixStream,
        sync::{Semaphore, watch},
        time::{Duration, Instant, timeout},
    };
    use uuid::Uuid;

    use super::{
        CommittedForegroundDelivery, ContextCompactionRangeLoadError, ConversationImportState,
        ConversionFailureDisposition, DispatchedTurnTerminalDisposition,
        GENERAL_BUFFERED_INBOUND_FRAMES, INBOUND_READ_AHEAD_BYTES, ImportedConversationRepository,
        ImportedConversationRepositoryError, ImportedRawBlobStorageError, InboundFrameBudgets,
        IncomingLine, InternalDiagnostic, MAX_ACTIVE_CONNECTIONS, MAX_BUFFERED_INBOUND_FRAMES,
        MAX_CONCURRENT_BLOB_READS, MAX_CONCURRENT_IMPORTS, MAX_CONCURRENT_REVIEW_COMMANDS,
        MAX_FRAME_BYTES, MAX_IMPORT_ADMISSION_WAITERS, OperationalImportError,
        PendingConversationImport, ProcessConnectionError, ProcessRuntimeError, ProcessUpdateEvent,
        ProtocolError, RESERVED_ACTIVE_IMPORT_INBOUND_FRAMES,
        RESERVED_POOL_CONNECTIONS_OUTSIDE_SNAPSHOTS, RequestId, ReviewCommandAdmission,
        SnapshotReaderAdmission, SnapshotSpoolError, SubmitInputModelExecutionDiagnostic,
        acquire_import_permit, acquire_import_waiter_permit, acquire_inbound_frame_permit,
        acquire_inbound_frame_permit_after_input, acquire_review_command_permit,
        acquire_review_command_permit_while_buffered, acquire_snapshot_reader_permit,
        admit_snapshot_reader, admitted_user_content, blob_upload_begin_preflight,
        bounded_rendered_compaction_boundary, canonical_review_request_digest,
        claude_conversion_failure_disposition, codex_conversion_failure_disposition,
        consume_snapshot_queued_update, context_compaction_failure_disposition,
        domain_finish_condition, execute_import, foreground_peer_activity,
        handle_append_conversation_import, handle_begin_conversation_import,
        handle_commit_conversation_import, import_evidence,
        imported_conversation_internal_diagnostic, inspect_connection_completion,
        internal_protocol_error, lifecycle_command_needs_eligibility_nudge, map_rejection,
        nudge_after_process_await_rejection, nudge_after_process_message_rejection,
        nudge_delegation_issuer, nudge_delegation_wake, observe_outbox_metrics_once,
        operational_import_error, preserve_committed_foreground_wait, process_delegation_rejection,
        process_delegation_rejection_for_recipient, read_frame_line,
        retain_inbound_frame_permit_during_import_admission,
        retry_context_compaction_range_database_reads, run_until_shutdown,
        snapshot_reader_capacity, spool_error_display, spool_goal_snapshot,
        submit_input_model_execution_diagnostic, try_acquire_blob_read_permit,
        unavailable_protocol_error, wait_for_connection_loss, wire_goal_event,
        wire_metadata_last_writer, wire_model_call_state, wire_tool_decision, wire_turn_state,
        wire_uuid, write_content, write_context_compaction_repository_error,
        write_delegation_port_error, write_snapshot_spool_error, write_transcript_entry,
    };

    macro_rules! assert_import_failure_ordinal {
        ($mapping:path, $ordinal:literal, $failure:expr, $class:path) => {{
            let ordinal = $ordinal;
            assert_eq!(
                $mapping(($failure)(ordinal)),
                ConversionFailureDisposition::Rejected(import_evidence($class, Some(ordinal)))
            );
        }};
    }

    macro_rules! assert_simple_import_failures {
        (
            $mapping:path,
            $failure_type:ident;
            $( $ordinal:literal => $failure:ident => $class:path ),+ $(,)?
        ) => {
            $(
                assert_import_failure_ordinal!(
                    $mapping,
                    $ordinal,
                    |line| $failure_type::$failure { line },
                    $class
                );
            )+
        };
    }

    #[test]
    fn a_resumed_session_requests_an_eligibility_pass() {
        assert!(lifecycle_command_needs_eligibility_nudge(
            &SessionLifecycleApplication::Resumed {
                state: SessionLifecycleState::Created,
            },
            &SessionLifecycleOperation::Resume,
        ));
    }

    impl super::ClassifyConversationImportError for io::Error {
        fn disposition(self) -> super::ConversionFailureDisposition {
            super::ConversionFailureDisposition::Rejected(super::import_evidence(
                signalbox_process_protocol::ConversationImportRejectionClass::InvalidJson,
                None,
            ))
        }
    }

    use crate::{FatalExecutionSupervisor, TelemetryMetrics};
    use signalbox_model_provider_runtime::ContextCompactionModelError;
    use signalbox_persistence::{
        context_compaction::{
            ContextCompactionRepositoryError, FailedContextCompactionDisposition,
        },
        conversation_import::{
            ImportedConversationCorruption, ImportedConversationIdentityCollision,
        },
        model_execution::{
            ModelCallCorruption, ModelCallIdentityCollision, ModelCallRepositoryError,
        },
        outbox::{
            DispatchedModelCallDisposition, DispatchedModelCallState, DispatchedOutboxEventKind,
            DispatchedReconciliationOperation, DispatchedRunnerState, DispatchedToolBatchState,
        },
        process_read::{
            ProcessImportedContentKind, ProcessImportedSourceSpeaker, ProcessReadError,
            ProcessReconciliationOperation, ProcessTranscriptEntry, ProcessTurnState,
        },
        session_delegation::{
            DelegationOperationRejection, DelegationRequestExecutionState,
            ProcessDelegationRequestRejection,
        },
    };

    #[derive(Clone, Debug, Default)]
    struct RecordingEligibilityNudge {
        sessions: Arc<Mutex<Vec<SessionId>>>,
    }

    impl EligibilityNudge for RecordingEligibilityNudge {
        fn nudge(&self, session: SessionId) -> EligibilityNudgeOutcome {
            self.sessions
                .lock()
                .expect("recording nudge lock remains available")
                .push(session);
            EligibilityNudgeOutcome::Enqueued
        }
    }

    #[test]
    fn s19_descendant_scope_decode_is_exact() {
        assert_eq!(
            super::decode_descendant_scope(
                signalbox_process_protocol::DescendantTerminationScope::ParentAlone,
            ),
            signalbox_domain::DescendantTerminationScope::ParentAlone
        );
        assert_eq!(
            super::decode_descendant_scope(
                signalbox_process_protocol::DescendantTerminationScope::ParentAndDescendants,
            ),
            signalbox_domain::DescendantTerminationScope::ParentAndDescendants
        );
    }
    use signalbox_process_protocol::{ModelCallDisposition, ModelCallState};

    #[test]
    fn durable_metric_mapping_ignores_content_and_uses_only_closed_labels() {
        let metrics = TelemetryMetrics::new().expect("static metric descriptors are valid");
        let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(1));
        let turn = TurnId::from_uuid(Uuid::from_u128(2));
        let attempt = TurnAttemptId::from_uuid(Uuid::from_u128(3));
        let call = ModelCallId::from_uuid(Uuid::from_u128(4));
        let input = DispatchedOutboxEventKind::InputAccepted {
            accepted_input,
            turn,
            acceptance_position: SessionInputPosition::first(),
            content: signalbox_domain::UserContent::try_text(
                "synthetic prompt with tool arguments".to_owned(),
            )
            .expect("the telemetry fixture content is valid"),
        };
        let activation = DispatchedOutboxEventKind::TurnActivated {
            turn,
            current_attempt: attempt,
        };
        let terminal_call = DispatchedOutboxEventKind::ModelCallTransition {
            turn,
            call,
            state: DispatchedModelCallState::Terminal(DispatchedModelCallDisposition::Ambiguous),
        };
        let mut last_sequence = None;

        observe_outbox_metrics_once(Some(&metrics), &mut last_sequence, 1, &input);
        observe_outbox_metrics_once(Some(&metrics), &mut last_sequence, 2, &activation);
        observe_outbox_metrics_once(Some(&metrics), &mut last_sequence, 2, &activation);
        observe_outbox_metrics_once(Some(&metrics), &mut last_sequence, 3, &terminal_call);
        let rendered = metrics.render().expect("static registry encodes");

        assert!(rendered.contains("signalbox_turns_started_total 1"));
        assert!(rendered.contains("disposition=\"ambiguous\""));
        assert!(!rendered.contains("synthetic prompt with tool arguments"));
        assert!(!rendered.contains(&accepted_input.into_uuid().to_string()));
        assert!(!rendered.contains(&turn.into_uuid().to_string()));
        assert!(!rendered.contains(&attempt.into_uuid().to_string()));
        assert!(!rendered.contains(&call.into_uuid().to_string()));
    }
    struct PendingResponseWriter;

    thread_local! {
        /// Telemetry captured on this thread alone.
        static CAPTURED_TELEMETRY: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    }

    /// Appends every formatted event to the emitting thread's own buffer.
    #[derive(Clone, Copy, Default)]
    struct CapturedTelemetry;

    impl Write for CapturedTelemetry {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            CAPTURED_TELEMETRY.with(|captured| captured.borrow_mut().extend_from_slice(buffer));
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for CapturedTelemetry {
        type Writer = Self;

        fn make_writer(&'writer self) -> Self::Writer {
            *self
        }
    }

    /// Records the telemetry `record` emits on this thread.
    ///
    /// The subscriber is installed once for the whole test process rather than
    /// scoped to this thread. `tracing` caches each callsite's interest
    /// process-wide, but `set_default` binds a subscriber to one thread, so a
    /// sibling test that reaches a callsite first on another thread registers
    /// it against no subscriber at all -- recording it as uninteresting for
    /// every thread, including the one that installed a capture. The event then
    /// is not merely written late; it is never emitted, and the assertion reads
    /// an empty buffer.
    ///
    /// Writes are routed per thread so concurrent tests never read each other's
    /// events, which keeps assertions on both presence and absence honest.
    fn capture_telemetry(record: impl FnOnce()) -> String {
        static INSTALLED: OnceLock<()> = OnceLock::new();

        INSTALLED.get_or_init(|| {
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(CapturedTelemetry)
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("no other global telemetry subscriber is installed");
        });
        CAPTURED_TELEMETRY.with(|captured| captured.borrow_mut().clear());
        record();
        CAPTURED_TELEMETRY
            .with(|captured| String::from_utf8(captured.borrow().clone()))
            .expect("captured telemetry is UTF-8")
    }

    fn capture_internal_diagnostic(session_id: Uuid, diagnostic: InternalDiagnostic) -> String {
        capture_telemetry(|| {
            let _ = internal_protocol_error(Some(session_id), diagnostic);
        })
    }

    fn capture_submit_input_model_execution_diagnostic(
        session_id: Uuid,
        error: &ModelCallRepositoryError,
    ) -> String {
        let diagnostic = submit_input_model_execution_diagnostic(error);
        capture_telemetry(|| {
            let _ = diagnostic.into_protocol_error(CanonicalUuid::from_uuid(session_id));
        })
    }

    #[test]
    fn internal_diagnostic_uses_canonical_session_and_typed_labels() {
        let session_id = Uuid::from_u128(1);
        let diagnostic = InternalDiagnostic::SessionMetadataCorruption;
        let encoded = capture_internal_diagnostic(session_id, diagnostic);

        assert!(encoded.contains(&format!("session_id={session_id}")));
        assert!(encoded.contains("failure_class=FailClosedCorruption"));
        assert!(encoded.contains(r#"cause_code="session_metadata_corruption""#));
        assert!(!encoded.contains("Some("));
    }

    #[test]
    fn internal_diagnostic_preserves_distinct_integrity_causes() {
        assert_eq!(
            InternalDiagnostic::ContextCompactionIdentityCollision.cause_code(),
            "context_compaction_repository_identity_collision"
        );
        assert_eq!(
            InternalDiagnostic::ContextCompactionRepositoryCorruption.cause_code(),
            "context_compaction_repository_corruption"
        );
        assert_eq!(
            InternalDiagnostic::ContextCompactionUnconfiguredTarget.cause_code(),
            "context_compaction_unconfigured_target"
        );
        assert_eq!(
            InternalDiagnostic::SessionModelCredentialMissing.cause_code(),
            "session_model_credential_missing"
        );
        assert_eq!(
            InternalDiagnostic::ToolLoopIdentityCollision.cause_code(),
            "tool_loop_identity_collision"
        );
        assert_eq!(
            InternalDiagnostic::ToolLoopCorruption.cause_code(),
            "tool_loop_corruption"
        );
        assert_eq!(
            InternalDiagnostic::ToolLoopInvalidTransition.cause_code(),
            "tool_loop_invalid_transition"
        );
        assert_eq!(
            InternalDiagnostic::SubmitInputModelExecutionIdentityCollision.cause_code(),
            "submit_input_model_execution_identity_collision"
        );
        assert_eq!(
            InternalDiagnostic::SubmitInputModelExecutionCorruption.cause_code(),
            "submit_input_model_execution_corruption"
        );
        assert_eq!(
            InternalDiagnostic::SubmitInputModelExecutionNoLiveExecution.cause_code(),
            "submit_input_model_execution_no_live_execution"
        );
        assert_eq!(
            InternalDiagnostic::SubmitInputModelExecutionInvalidTransition.cause_code(),
            "submit_input_model_execution_invalid_transition"
        );
    }

    #[test]
    fn submit_input_model_execution_identity_collision_keeps_its_diagnostic() {
        let error =
            ModelCallRepositoryError::IdentityCollision(ModelCallIdentityCollision::ModelCall);

        assert_eq!(
            submit_input_model_execution_diagnostic(&error),
            SubmitInputModelExecutionDiagnostic::Internal(
                InternalDiagnostic::SubmitInputModelExecutionIdentityCollision
            )
        );
    }

    #[test]
    fn submit_input_model_execution_corruption_keeps_its_diagnostic() {
        let error = ModelCallRepositoryError::Corruption(ModelCallCorruption::Missing(
            "synthetic model-call row",
        ));

        assert_eq!(
            submit_input_model_execution_diagnostic(&error),
            SubmitInputModelExecutionDiagnostic::Internal(
                InternalDiagnostic::SubmitInputModelExecutionCorruption
            )
        );
    }

    #[test]
    fn submit_input_model_execution_no_live_execution_keeps_its_diagnostic() {
        let error = ModelCallRepositoryError::NoLiveExecution;

        assert_eq!(
            submit_input_model_execution_diagnostic(&error),
            SubmitInputModelExecutionDiagnostic::Internal(
                InternalDiagnostic::SubmitInputModelExecutionNoLiveExecution
            )
        );
    }

    #[test]
    fn submit_input_model_execution_invalid_transition_keeps_its_diagnostic() {
        let error = ModelCallRepositoryError::InvalidTransition("synthetic transition");

        assert_eq!(
            submit_input_model_execution_diagnostic(&error),
            SubmitInputModelExecutionDiagnostic::Internal(
                InternalDiagnostic::SubmitInputModelExecutionInvalidTransition
            )
        );
    }

    #[test]
    fn submit_input_model_execution_diagnostic_omits_dynamic_source_detail() {
        let dynamic_detail = "synthetic-credential-prompt-and-provider-prose";
        let session_id = Uuid::from_u128(1);
        let error = ModelCallRepositoryError::InvalidTransition(dynamic_detail);
        let encoded = capture_submit_input_model_execution_diagnostic(session_id, &error);

        assert!(encoded.contains(&format!("session_id={session_id}")));
        assert!(encoded.contains("failure_class=CallerOrHubBug"));
        assert!(
            encoded.contains(r#"cause_code="submit_input_model_execution_invalid_transition""#)
        );
        assert!(!encoded.contains(dynamic_detail));
    }

    #[test]
    fn imported_conversation_identity_collision_keeps_its_diagnostic() {
        let error = ImportedConversationRepositoryError::IdentityCollision(
            ImportedConversationIdentityCollision::Conversation,
        );

        assert_eq!(
            imported_conversation_internal_diagnostic(&error),
            InternalDiagnostic::ImportedConversationIdentityCollision
        );
    }

    #[test]
    fn compaction_terminal_evidence_keeps_its_exact_disposition() {
        assert_eq!(
            context_compaction_failure_disposition(ContextCompactionModelError::Refused),
            FailedContextCompactionDisposition::Refused
        );
        assert_eq!(
            context_compaction_failure_disposition(
                ContextCompactionModelError::CancellationConfirmed
            ),
            FailedContextCompactionDisposition::Cancelled
        );
        assert_eq!(
            context_compaction_failure_disposition(ContextCompactionModelError::ProviderError),
            FailedContextCompactionDisposition::KnownFailed
        );
        assert_eq!(
            context_compaction_failure_disposition(ContextCompactionModelError::ProvenUnsent),
            FailedContextCompactionDisposition::KnownFailed
        );
        assert_eq!(
            context_compaction_failure_disposition(ContextCompactionModelError::BoundaryLoss),
            FailedContextCompactionDisposition::Ambiguous
        );
    }

    #[test]
    fn automatic_compaction_boundary_counts_the_rendered_json_envelope() {
        let first = serde_json::json!({
            "position": 1,
            "type": "user",
            "content": "x".repeat(90),
        });
        let second = serde_json::json!({
            "position": 2,
            "type": "assistant",
            "content": "y".repeat(90),
        });
        let first_bytes = u64::try_from(
            serde_json::to_vec(&first)
                .expect("the fixture JSON is serializable")
                .len(),
        )
        .expect("the fixture length fits u64");
        let second_bytes = u64::try_from(
            serde_json::to_vec(&second)
                .expect("the fixture JSON is serializable")
                .len(),
        )
        .expect("the fixture length fits u64");
        let first_array_bytes = first_bytes + 2;

        assert_eq!(
            bounded_rendered_compaction_boundary(
                &[first_bytes, second_bytes],
                &[(11, true), (12, true)],
                first_array_bytes,
            ),
            Some(11)
        );
    }

    #[test]
    fn automatic_compaction_boundary_never_crosses_the_model_budget_for_a_tool_exchange() {
        assert_eq!(
            bounded_rendered_compaction_boundary(
                &[60, 100, 100],
                &[(21, true), (22, false), (23, true)],
                170,
            ),
            Some(21)
        );
    }

    #[test]
    fn successor_compaction_rejects_an_unreachable_later_safe_boundary() {
        assert!(super::successor_compaction_cannot_advance(
            &[10, 100, 100],
            &[(31, true), (32, false), (33, true)],
            203,
        ));
        assert!(!super::successor_compaction_cannot_advance(
            &[10, 100, 100],
            &[(31, true), (32, false), (33, true)],
            204,
        ));
    }

    #[test]
    fn snapshot_delta_boundary_consumes_only_the_queued_prefix() {
        let mut queued = 2;

        assert!(consume_snapshot_queued_update(&mut queued));
        assert!(consume_snapshot_queued_update(&mut queued));
        assert!(!consume_snapshot_queued_update(&mut queued));
    }

    #[tokio::test]
    async fn automatic_compaction_range_read_retries_transient_database_failure() {
        let expected_range = String::from("rendered compaction range");
        let transient = ProcessReadError::Database(sqlx::Error::Io(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "synthetic transient range read",
        )));
        let mut outcomes = VecDeque::from([
            Err(ContextCompactionRangeLoadError::Read(transient)),
            Ok(expected_range.clone()),
        ]);

        let loaded = retry_context_compaction_range_database_reads(|| {
            std::future::ready(
                outcomes
                    .pop_front()
                    .expect("the fixture supplies one retry and one success"),
            )
        })
        .await
        .expect("a transient database read is retried");

        assert_eq!(loaded, expected_range);
        assert!(outcomes.is_empty());
    }

    impl tokio::io::AsyncWrite for PendingResponseWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
            _buffer: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            std::task::Poll::Pending
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    fn review_finding_input(identity: u128) -> ReviewFindingInput {
        ReviewFindingInput {
            finding_id: CanonicalUuid::from_uuid(Uuid::from_u128(identity)),
            file_path: String::from("src/lib.rs"),
            line_start: None,
            line_end: None,
            diff_side: None,
            title: String::from("Canonical finding"),
            body: String::from("Finding order does not change command meaning."),
            severity: ReviewSeverity::High,
            is_real_confidence: CanonicalU64::new(9_000),
            severity_label_confidence: CanonicalU64::new(8_500),
            category: String::from("correctness"),
            recommended_fix: None,
        }
    }

    #[test]
    fn review_findings_digest_uses_canonical_identity_order() -> Result<(), Box<dyn Error>> {
        let command_id = CommandId::try_from_uuid(Uuid::from_u128(1))?;
        let run_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let pass_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(4));
        let output_frontier_id = CanonicalUuid::from_uuid(Uuid::from_u128(5));
        let first = review_finding_input(6);
        let second = review_finding_input(7);
        let mut ordered = ClientRequest::RecordReviewFindings {
            command_id,
            run_id,
            pass_id,
            turn_id,
            output_frontier_id,
            findings: vec![first.clone(), second.clone()],
        };
        let mut reversed = ClientRequest::RecordReviewFindings {
            command_id,
            run_id,
            pass_id,
            turn_id,
            output_frontier_id,
            findings: vec![second, first],
        };

        assert_eq!(
            canonical_review_request_digest(&mut ordered),
            canonical_review_request_digest(&mut reversed)
        );
        assert_eq!(ordered, reversed);
        Ok(())
    }

    /// every stop refusal the interrupt treatment records reaches the
    /// wire as its recorded typed rejection, not as an encode invariant that
    /// closes the connection; the racing-target projections are covered by the
    /// reconciliation test below.
    #[test]
    fn stop_rejections_have_wire_projections() -> Result<(), Box<dyn Error>> {
        let session = SessionId::from_uuid(Uuid::from_u128(1));
        let actual_active_turn = TurnId::from_uuid(Uuid::from_u128(3));
        let existing_command = DurableCommandId::from_uuid(Uuid::from_u128(4));

        assert_eq!(
            map_rejection(SubmitInputRejectedResult::InterruptAlreadyApplied {
                session,
                active_turn: actual_active_turn,
                existing_command,
            })?,
            RejectionDetail::InterruptAlreadyApplied {
                session_id: wire_uuid(session.into_uuid()),
                active_turn_id: wire_uuid(actual_active_turn.into_uuid()),
                existing_command_id: wire_uuid(*existing_command.as_uuid()),
            }
        );
        assert_eq!(
            map_rejection(
                SubmitInputRejectedResult::InterruptUnavailableWhileAwaitingApproval {
                    session,
                    active_turn: actual_active_turn,
                }
            )?,
            RejectionDetail::InterruptUnavailableWhileAwaitingApproval {
                session_id: wire_uuid(session.into_uuid()),
                active_turn_id: wire_uuid(actual_active_turn.into_uuid()),
            }
        );
        assert_eq!(
            map_rejection(
                SubmitInputRejectedResult::SafePointUnavailableWhileStopping {
                    session,
                    active_turn: actual_active_turn,
                    existing_command,
                }
            )?,
            RejectionDetail::SafePointUnavailableWhileStopping {
                session_id: wire_uuid(session.into_uuid()),
                active_turn_id: wire_uuid(actual_active_turn.into_uuid()),
                existing_command_id: wire_uuid(*existing_command.as_uuid()),
            }
        );
        Ok(())
    }

    /// the receipt projection is exact — the wire
    /// surface records only reason-bearing denials, so a reason-free denial
    /// fails closed instead of fabricating an empty reason.
    #[test]
    fn reason_free_denial_has_no_wire_receipt() -> Result<(), Box<dyn Error>> {
        assert_eq!(
            wire_tool_decision(&ToolApprovalDecision::Approve)?,
            ToolDecision::Approve {}
        );
        assert_eq!(
            wire_tool_decision(&ToolApprovalDecision::Deny {
                reason: Some(
                    signalbox_domain::ToolDenialReason::try_new(String::from(
                        "writes outside the workspace"
                    ))
                    .map_err(|error| io::Error::other(format!("{error:?}")))?
                ),
            })?,
            ToolDecision::Deny {
                reason: String::from("writes outside the workspace"),
            }
        );
        assert!(matches!(
            wire_tool_decision(&ToolApprovalDecision::Deny { reason: None }),
            Err(ProcessConnectionError::EncodeInvariant)
        ));
        Ok(())
    }

    /// The post-lock statement time every metadata projection fixture carries.
    /// No projection depends on the value, only on its passing through.
    const METADATA_WRITE_UNIX_MICROS: u64 = 17;

    /// Projects one domain agency and pins both members against the fixture it
    /// came from. A failure names the agency at the call site.
    #[track_caller]
    fn assert_metadata_last_writer_projects(actor: Actor, expected_actor: MetadataActor) {
        let writer = SessionMetadataLastWriter::new(
            SessionMetadataUpdatedAt::from_unix_micros(METADATA_WRITE_UNIX_MICROS),
            actor,
        );
        let projected = wire_metadata_last_writer(writer);
        assert_eq!(projected.actor(), expected_actor);
        assert_eq!(
            projected.updated_at_unix_micros().value(),
            writer.updated_at().as_unix_micros()
        );
    }

    /// the metadata last-writer projection is total over the domain
    /// agencies durable metadata records, and each carried reference lands in
    /// its own member. A projection gap here is not a degraded field: both
    /// callers propagate it as an encode invariant, which is fatal to the
    /// daemon and re-fires on every read of the durable row.
    #[test]
    fn metadata_last_writer_projects_every_domain_agency() {
        let turn = TurnId::from_uuid(Uuid::from_u128(2));
        let request = ToolRequestId::from_uuid(Uuid::from_u128(3));

        assert_metadata_last_writer_projects(Actor::User, MetadataActor::User {});
        assert_metadata_last_writer_projects(Actor::Core, MetadataActor::Core {});
        assert_metadata_last_writer_projects(
            Actor::Model { turn },
            MetadataActor::Model {
                turn_id: wire_uuid(turn.into_uuid()),
            },
        );
        assert_metadata_last_writer_projects(Actor::Recovery, MetadataActor::Recovery {});
        assert_metadata_last_writer_projects(
            Actor::Tool { request },
            MetadataActor::Tool {
                tool_request_id: wire_uuid(request.into_uuid()),
            },
        );
    }

    fn compaction_session() -> SessionId {
        SessionId::from_uuid(Uuid::from_u128(1))
    }

    /// S03: an explicit compaction whose commit outcome cannot be
    /// decided raises the same fatal recovery signal its automatic sibling
    /// raises through the scheduler pass, and still answers the client with the
    /// stable ambiguous code.
    ///
    /// Without the report the connection handler has nowhere left to go: it
    /// holds no `PreparedContextCompaction` to terminalize, replay of the same
    /// command finds it pending, a fresh command finds the nonterminal call,
    /// and the startup scan that does reconcile this state only runs in the
    /// next incarnation.
    #[tokio::test]
    async fn s03_ambiguous_explicit_compaction_commit_raises_the_fatal_recovery_signal()
    -> Result<(), Box<dyn Error>> {
        let (supervisor, signal) = FatalExecutionSupervisor::new(());
        let reporter = supervisor.recovery_reporter();
        let (mut writer, mut reader) = duplex(1_024);

        write_context_compaction_repository_error(
            &mut writer,
            ProtocolVersion::One,
            RequestId::try_new(11)?,
            compaction_session(),
            Some(&reporter),
            ContextCompactionRepositoryError::CommitAmbiguous(sqlx::Error::PoolClosed),
        )
        .await?;
        drop(writer);
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;

        assert!(signal.is_triggered());
        assert!(matches!(
            decode_server_line(&encoded)?.message(),
            ServerMessage::Error {
                code: ErrorCode::CommitAmbiguous,
                ..
            }
        ));
        Ok(())
    }

    /// S03: a failure proven to precede the commit boundary is
    /// ordinary unavailability and raises no recovery signal, so the reaction
    /// stays scoped to the one declared class that needs it.
    #[tokio::test]
    async fn s03_decided_explicit_compaction_failure_raises_no_recovery_signal()
    -> Result<(), Box<dyn Error>> {
        let (supervisor, signal) = FatalExecutionSupervisor::new(());
        let reporter = supervisor.recovery_reporter();
        let (mut writer, mut reader) = duplex(1_024);

        write_context_compaction_repository_error(
            &mut writer,
            ProtocolVersion::One,
            RequestId::try_new(12)?,
            compaction_session(),
            Some(&reporter),
            ContextCompactionRepositoryError::Database(sqlx::Error::PoolClosed),
        )
        .await?;
        drop(writer);
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;

        assert!(!signal.is_triggered());
        assert!(matches!(
            decode_server_line(&encoded)?.message(),
            ServerMessage::Error {
                code: ErrorCode::Unavailable,
                ..
            }
        ));
        Ok(())
    }

    #[tokio::test]
    async fn delegation_commit_ambiguity_uses_the_mutation_recovery_code()
    -> Result<(), Box<dyn Error>> {
        let (mut writer, mut reader) = duplex(1_024);

        write_delegation_port_error(
            &mut writer,
            ProtocolVersion::One,
            RequestId::try_new(13)?,
            CanonicalUuid::from_uuid(Uuid::from_u128(14)),
            crate::session_delegation::PostgresSessionDelegationPortError::Repository(
                signalbox_persistence::session_delegation::SessionDelegationRepositoryError::CommitAmbiguous(
                    sqlx::Error::PoolClosed,
                ),
            ),
        )
        .await?;
        drop(writer);
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;

        assert!(matches!(
            decode_server_line(&encoded)?.message(),
            ServerMessage::Error {
                code: ErrorCode::CommitAmbiguous,
                ..
            }
        ));
        Ok(())
    }

    #[test]
    fn commit_ambiguity_selects_the_stable_process_error_code() {
        assert_eq!(
            ProtocolError::mutation_definitely_unavailable().code,
            ErrorCode::Unavailable
        );
        assert_eq!(
            ProtocolError::mutation_commit_ambiguous().code,
            ErrorCode::CommitAmbiguous
        );
        assert!(
            ProtocolError::without_detail(ErrorCode::UnsupportedVersion)
                .message
                .contains("supported version: 1")
        );
    }

    /// direct blob-read admission exposes one fixed non-waiting
    /// process-wide capacity.
    #[test]
    fn blob_read_admission_has_fixed_nonwaiting_capacity() -> Result<(), Box<dyn Error>> {
        let budget = Arc::new(Semaphore::new(MAX_CONCURRENT_BLOB_READS));
        let held = Arc::clone(&budget)
            .try_acquire_many_owned(u32::try_from(MAX_CONCURRENT_BLOB_READS)?)
            .map_err(io::Error::other)?;

        assert_eq!(MAX_CONCURRENT_BLOB_READS, 16);
        assert!(try_acquire_blob_read_permit(Arc::clone(&budget)).is_none());
        drop(held);
        assert_eq!(budget.available_permits(), MAX_CONCURRENT_BLOB_READS);
        Ok(())
    }

    #[tokio::test]
    async fn blob_read_disconnect_detection_survives_pipelined_input() -> Result<(), Box<dyn Error>>
    {
        let (mut client, server) = UnixStream::pair()?;
        let (reader, _writer) = server.into_split();
        let mut reader = BufReader::new(reader);
        client.write_all(b"pipelined request").await?;
        assert_eq!(reader.fill_buf().await?, b"pipelined request");
        drop(client);

        timeout(Duration::from_secs(1), wait_for_connection_loss(&reader)).await?;
        assert_eq!(reader.buffer(), b"pipelined request");
        Ok(())
    }

    /// a reconciliation decision that lost its race to another
    /// decision reaches the wire as its recorded typed rejection, not as an
    /// encode invariant that closes the connection.
    #[test]
    fn racing_reconciliation_rejections_have_wire_projections() -> Result<(), Box<dyn Error>>
    {
        let session = SessionId::from_uuid(uuid::Uuid::from_u128(1));
        let expected_active_turn = TurnId::from_uuid(uuid::Uuid::from_u128(2));
        let actual_active_turn = TurnId::from_uuid(uuid::Uuid::from_u128(3));

        assert_eq!(
            map_rejection(SubmitInputRejectedResult::NoActiveTurn {
                session,
                expected_active_turn,
            })?,
            RejectionDetail::NoActiveTurn {
                session_id: wire_uuid(session.into_uuid()),
                expected_active_turn_id: wire_uuid(expected_active_turn.into_uuid()),
            }
        );
        assert_eq!(
            map_rejection(SubmitInputRejectedResult::ActiveTurnMismatch {
                session,
                expected_active_turn,
                actual_active_turn,
            })?,
            RejectionDetail::ActiveTurnMismatch {
                session_id: wire_uuid(session.into_uuid()),
                expected_active_turn_id: wire_uuid(expected_active_turn.into_uuid()),
                active_turn_id: wire_uuid(actual_active_turn.into_uuid()),
            }
        );
        Ok(())
    }

    #[track_caller]
    fn complete_frame(line: Option<IncomingLine>) -> Vec<u8> {
        let Some(IncomingLine::Complete(line)) = line else {
            panic!("fixture expected one complete frame");
        };
        line
    }

    #[track_caller]
    fn oversized_frame_identity(
        line: Option<IncomingLine>,
    ) -> (RequestId, Option<ProtocolVersion>) {
        let Some(IncomingLine::Oversized {
            request_id,
            admitted_version,
        }) = line
        else {
            panic!("fixture expected one oversized frame");
        };
        (request_id, admitted_version)
    }

    #[tokio::test]
    async fn frame_reader_accepts_the_exact_cap_and_rejects_the_next_byte()
    -> Result<(), Box<dyn Error>> {
        let mut exact = vec![b'x'; MAX_FRAME_BYTES];
        exact[MAX_FRAME_BYTES - 1] = b'\n';
        let mut exact_reader = BufReader::new(exact.as_slice());
        let line = complete_frame(read_frame_line(&mut exact_reader).await?);
        assert_eq!(line.len(), MAX_FRAME_BYTES);

        let mut oversized = vec![b'x'; MAX_FRAME_BYTES + 1];
        oversized[MAX_FRAME_BYTES] = b'\n';
        let mut oversized_reader = BufReader::new(oversized.as_slice());
        let (request_id, admitted_version) =
            oversized_frame_identity(read_frame_line(&mut oversized_reader).await?);
        assert_eq!(request_id.value(), 0);
        assert_eq!(admitted_version, None);

        let correlated_request_id = 9;
        let request_members = format!(r#""request_id":"{correlated_request_id}""#);
        let mut correlated = format!(
            r#"{{"version":1,{request_members},"request":{{"type":"list_sessions","padding":""#
        )
        .into_bytes();
        let suffix = b"\"}}";
        correlated.resize(MAX_FRAME_BYTES - suffix.len(), b'x');
        correlated.extend_from_slice(suffix);
        correlated.push(b'\n');
        let mut correlated_reader = BufReader::new(correlated.as_slice());
        let (request_id, admitted_version) =
            oversized_frame_identity(read_frame_line(&mut correlated_reader).await?);
        assert_eq!(request_id.value(), correlated_request_id);
        assert_eq!(admitted_version, Some(ProtocolVersion::One));
        Ok(())
    }

    #[tokio::test]
    async fn inbound_frame_budget_bounds_raw_accumulation_and_waits_for_shutdown()
    -> Result<(), Box<dyn Error>> {
        assert_eq!(
            MAX_BUFFERED_INBOUND_FRAMES * MAX_FRAME_BYTES,
            64 * 1024 * 1024
        );
        let budget = Arc::new(Semaphore::new(MAX_BUFFERED_INBOUND_FRAMES));
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let mut permits = Vec::new();
        for _ in 0..MAX_BUFFERED_INBOUND_FRAMES {
            permits.push(
                acquire_inbound_frame_permit(Arc::clone(&budget), &mut shutdown_receiver.clone())
                    .await?
                    .ok_or_else(|| io::Error::other("the running fixture must acquire a permit"))?,
            );
        }

        assert!(
            timeout(
                Duration::from_millis(20),
                acquire_inbound_frame_permit(Arc::clone(&budget), &mut shutdown_receiver.clone()),
            )
            .await
            .is_err(),
            "the ninth frame accumulator must wait"
        );

        drop(permits.pop());
        let released = timeout(
            Duration::from_secs(1),
            acquire_inbound_frame_permit(Arc::clone(&budget), &mut shutdown_receiver.clone()),
        )
        .await??
        .ok_or_else(|| io::Error::other("a released frame slot must be acquired"))?;
        permits.push(released);

        shutdown.send(true)?;
        assert!(
            acquire_inbound_frame_permit(Arc::clone(&budget), &mut shutdown_receiver.clone())
                .await?
                .is_none(),
            "a connection waiting for the full budget must stop on shutdown"
        );
        Ok(())
    }

    #[tokio::test]
    async fn idle_reader_does_not_reserve_an_inbound_frame_slot() -> Result<(), Box<dyn Error>> {
        assert_eq!(
            MAX_ACTIVE_CONNECTIONS * INBOUND_READ_AHEAD_BYTES,
            1024 * 1024
        );
        let budget = Arc::new(Semaphore::new(1));
        let (mut client, server) = duplex(8);
        let mut reader = BufReader::new(server);
        let (_shutdown, mut shutdown_receiver) = watch::channel(false);
        let acquire = acquire_inbound_frame_permit_after_input(
            &mut reader,
            Arc::clone(&budget),
            &mut shutdown_receiver,
        );
        tokio::pin!(acquire);

        assert!(
            timeout(Duration::from_millis(20), &mut acquire)
                .await
                .is_err()
        );
        assert_eq!(budget.available_permits(), 1);

        client.write_all(b"{").await?;
        let permit = timeout(Duration::from_secs(1), &mut acquire)
            .await??
            .ok_or_else(|| io::Error::other("ready input must acquire a frame slot"))?;
        assert_eq!(budget.available_permits(), 0);
        drop(permit);
        Ok(())
    }

    /// The orchestration snapshot holds one pooled connection, like every
    /// other review read: its whole reconstruction runs inside a single
    /// `REPEATABLE READ` transaction. A three-connection pool therefore starts,
    /// where the two-connection form of this read needed four.
    #[tokio::test]
    async fn review_orchestration_snapshot_holds_one_pool_connection() -> Result<(), Box<dyn Error>>
    {
        let capacity = 2;
        let budget = Arc::new(Semaphore::new(capacity));
        let (_shutdown, mut shutdown_receiver) = watch::channel(false);

        let permit = admit_snapshot_reader(
            &read_review_orchestration_request(),
            Arc::clone(&budget),
            &mut shutdown_receiver,
        )
        .await?
        .ok_or_else(|| io::Error::other("the running fixture must be admitted"))?
        .ok_or_else(|| io::Error::other("the snapshot read must hold a reader permit"))?;

        assert_eq!(budget.available_permits(), capacity - 1);
        drop(permit);
        assert_eq!(budget.available_permits(), capacity);
        assert_eq!(snapshot_reader_capacity(3, None), Some(1));
        assert!(snapshot_reader_capacity(2, None).is_none());
        Ok(())
    }

    #[tokio::test]
    async fn snapshot_reader_budget_reserves_two_pool_connections() -> Result<(), Box<dyn Error>> {
        let max_pool_connections = 10;
        let capacity = snapshot_reader_capacity(max_pool_connections, None)
            .ok_or_else(|| io::Error::other("the production pool must admit snapshot readers"))?;
        assert_eq!(
            capacity,
            usize::try_from(max_pool_connections - RESERVED_POOL_CONNECTIONS_OUTSIDE_SNAPSHOTS)?
        );
        assert!(snapshot_reader_capacity(2, None).is_none());

        let budget = Arc::new(Semaphore::new(capacity));
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let permits = Arc::clone(&budget)
            .acquire_many_owned(u32::try_from(capacity)?)
            .await?;
        assert!(
            timeout(
                Duration::from_millis(20),
                acquire_snapshot_reader_permit(Arc::clone(&budget), &mut shutdown_receiver.clone()),
            )
            .await
            .is_err(),
            "the next snapshot reader must leave two pool slots free"
        );

        shutdown.send(true)?;
        assert!(
            acquire_snapshot_reader_permit(Arc::clone(&budget), &mut shutdown_receiver.clone())
                .await?
                .is_none()
        );
        drop(permits);
        Ok(())
    }

    #[test]
    fn enlarged_pool_applies_the_configured_snapshot_reader_limit() {
        let configured_limit = 3;
        let enlarged_pool_connections = u32::try_from(configured_limit)
            .expect("the effective ceiling fits PostgreSQL pool capacity")
            + RESERVED_POOL_CONNECTIONS_OUTSIDE_SNAPSHOTS
            + 1;

        assert_eq!(
            snapshot_reader_capacity(enlarged_pool_connections, Some(configured_limit)),
            Some(configured_limit)
        );
    }

    /// The wire vocabulary as text. The review read verbs are enumerated from
    /// the protocol itself so a later one cannot be admitted by a list here
    /// staying silent about it.
    const WIRE_VOCABULARY: &str = include_str!("../../../../crates/process-protocol/src/lib.rs");

    fn client_request_variant_names(source: &str) -> BTreeSet<String> {
        let declaration = "pub enum ClientRequest {";
        let start = source
            .find(declaration)
            .expect("the wire vocabulary declares the client request enum");
        let body = &source[start + declaration.len()..];
        let end = body
            .find("\n}\n")
            .expect("the client request enum body is closed");
        body[..end]
            .lines()
            .filter_map(|line| {
                let variant = line.strip_prefix("    ")?;
                if !variant.starts_with(|character: char| character.is_ascii_uppercase()) {
                    return None;
                }
                Some(
                    variant
                        .split(|character: char| !character.is_ascii_alphanumeric())
                        .next()?
                        .to_owned(),
                )
            })
            .collect()
    }

    /// The review verbs that read the database, taken from the wire vocabulary.
    fn review_read_verbs_in_vocabulary() -> BTreeSet<String> {
        client_request_variant_names(WIRE_VOCABULARY)
            .into_iter()
            .filter(|name| {
                name.contains("Review") && (name.starts_with("Read") || name.starts_with("List"))
            })
            .collect()
    }

    /// The scraper carries logic no assertion can inspect, so it is pinned on
    /// its own: one name per declaration, single-line and braced forms alike,
    /// with doc comments and field lines excluded.
    #[test]
    fn client_request_variant_names_reads_one_name_per_declaration() {
        let source = concat!(
            "pub enum ClientRequest {\n",
            "    /// Read one target.\n",
            "    ReadReviewTarget { target_id: CanonicalUuid },\n",
            "    ListReviewFindings {\n",
            "        run_id: CanonicalUuid,\n",
            "    },\n",
            "    ListTemplates {},\n",
            "}\n",
        );

        assert_eq!(
            client_request_variant_names(source),
            BTreeSet::from([
                String::from("ListReviewFindings"),
                String::from("ListTemplates"),
                String::from("ReadReviewTarget"),
            ])
        );
    }

    /// One fixture identity. No admission reads an identity's value; the verb
    /// carrying one is the whole input.
    fn fixture_identity(seed: u128) -> CanonicalUuid {
        CanonicalUuid::from_uuid(Uuid::from_u128(seed))
    }

    fn read_review_target_request() -> ClientRequest {
        ClientRequest::ReadReviewTarget {
            target_id: fixture_identity(1),
        }
    }

    fn read_review_run_request() -> ClientRequest {
        ClientRequest::ReadReviewRun {
            run_id: fixture_identity(2),
        }
    }

    fn read_review_finding_request() -> ClientRequest {
        ClientRequest::ReadReviewFinding {
            finding_id: fixture_identity(3),
        }
    }

    fn list_review_findings_request() -> ClientRequest {
        ClientRequest::ListReviewFindings {
            run_id: fixture_identity(4),
        }
    }

    fn read_review_orchestration_request() -> ClientRequest {
        ClientRequest::ReadReviewOrchestration {
            attempt_id: fixture_identity(5),
        }
    }

    /// Every review verb that reads the database reserves snapshot capacity.
    /// The reservation exists so `RESERVED_POOL_CONNECTIONS_OUTSIDE_SNAPSHOTS`
    /// connections stay available to the outbox dispatcher and mutations; a
    /// read verb dispatched without it spends that reserve silently.
    #[test]
    fn every_review_read_verb_reserves_snapshot_capacity() {
        assert_eq!(
            review_read_verbs_in_vocabulary(),
            BTreeSet::from([
                String::from("ListReviewFindings"),
                String::from("ReadReviewFinding"),
                String::from("ReadReviewOrchestration"),
                String::from("ReadReviewRun"),
                String::from("ReadReviewTarget"),
            ]),
            "a review read verb in the wire vocabulary has no admission of its own"
        );

        assert_eq!(
            SnapshotReaderAdmission::for_request(&read_review_target_request()),
            SnapshotReaderAdmission::OneConnection
        );
        assert_eq!(
            SnapshotReaderAdmission::for_request(&read_review_run_request()),
            SnapshotReaderAdmission::OneConnection
        );
        assert_eq!(
            SnapshotReaderAdmission::for_request(&read_review_finding_request()),
            SnapshotReaderAdmission::OneConnection
        );
        assert_eq!(
            SnapshotReaderAdmission::for_request(&list_review_findings_request()),
            SnapshotReaderAdmission::OneConnection
        );
        assert_eq!(
            SnapshotReaderAdmission::for_request(&read_review_orchestration_request()),
            SnapshotReaderAdmission::OneConnection
        );
    }

    /// A metadata point read opens a transaction, sets `REPEATABLE READ ONLY`,
    /// selects, and commits, so it holds a pooled connection across statements
    /// and belongs to the same admission. A defaults read is one statement and
    /// does not.
    #[test]
    fn point_reads_are_admitted_by_how_long_they_hold_a_connection() {
        assert_eq!(
            SnapshotReaderAdmission::for_request(&ClientRequest::ReadSessionMetadata {
                session_id: fixture_identity(6),
            }),
            SnapshotReaderAdmission::OneConnection
        );
        assert_eq!(
            SnapshotReaderAdmission::for_request(&ClientRequest::ReadSessionDefaults {
                session_id: fixture_identity(7),
                defaults_version: None,
            }),
            SnapshotReaderAdmission::NotRequired
        );
    }

    #[tokio::test]
    async fn review_read_admission_draws_on_the_shared_reader_budget() -> Result<(), Box<dyn Error>>
    {
        let capacity = 3;
        let budget = Arc::new(Semaphore::new(capacity));
        let (_shutdown, mut shutdown_receiver) = watch::channel(false);

        let permit = admit_snapshot_reader(
            &read_review_target_request(),
            Arc::clone(&budget),
            &mut shutdown_receiver,
        )
        .await?
        .ok_or_else(|| io::Error::other("the running fixture must be admitted"))?
        .ok_or_else(|| io::Error::other("a review read must hold a reader permit"))?;
        assert_eq!(budget.available_permits(), capacity - 1);
        drop(permit);

        assert!(
            admit_snapshot_reader(
                &ClientRequest::ListModelAliases {},
                Arc::clone(&budget),
                &mut shutdown_receiver,
            )
            .await?
            .ok_or_else(|| io::Error::other("the running fixture must be admitted"))?
            .is_none(),
            "a request that reads no snapshot holds no reader permit"
        );
        assert_eq!(budget.available_permits(), capacity);
        Ok(())
    }

    #[tokio::test]
    async fn queued_review_request_retains_its_inbound_frame_slot() -> Result<(), Box<dyn Error>> {
        let frame_budget = Arc::new(Semaphore::new(1));
        let review_budget = Arc::new(Semaphore::new(1));
        let occupied_review = Arc::clone(&review_budget).acquire_owned().await?;
        let frame_permit = Arc::clone(&frame_budget).acquire_owned().await?;
        let (_shutdown, mut shutdown_receiver) = watch::channel(false);
        let acquire = acquire_review_command_permit_while_buffered(
            ReviewCommandAdmission::Required,
            Some(frame_permit),
            Arc::clone(&review_budget),
            &mut shutdown_receiver,
            None,
        );
        tokio::pin!(acquire);

        assert!(
            timeout(Duration::from_millis(20), &mut acquire)
                .await
                .is_err()
        );
        assert_eq!(frame_budget.available_permits(), 0);
        drop(occupied_review);
        let (held_frame, review_permit) = timeout(Duration::from_secs(1), &mut acquire)
            .await??
            .ok_or_else(|| io::Error::other("the admitted request must retain both permits"))?;
        let held_frame =
            held_frame.ok_or_else(|| io::Error::other("the frame permit must remain"))?;
        assert!(review_permit.is_some());
        assert_eq!(frame_budget.available_permits(), 0);
        drop(held_frame);
        assert_eq!(frame_budget.available_permits(), 1);
        Ok(())
    }

    /// an expired active bulk-ingest deadline releases a frame held
    /// while a review mutation waits for its separate admission budget.
    #[tokio::test(start_paused = true)]
    async fn expired_bulk_ingest_deadline_cancels_review_admission()
    -> Result<(), Box<dyn Error>> {
        let frame_budget = Arc::new(Semaphore::new(1));
        let review_budget = Arc::new(Semaphore::new(1));
        let _occupied_review = Arc::clone(&review_budget).acquire_owned().await?;
        let frame_permit = Arc::clone(&frame_budget).acquire_owned().await?;
        let (_shutdown, mut shutdown_receiver) = watch::channel(false);

        let admission = acquire_review_command_permit_while_buffered(
            ReviewCommandAdmission::Required,
            Some(frame_permit),
            review_budget,
            &mut shutdown_receiver,
            Some(Instant::now()),
        )
        .await?;

        assert!(admission.is_none());
        assert_eq!(frame_budget.available_permits(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn review_command_permit_releases_before_response_write() -> Result<(), Box<dyn Error>> {
        let budget = Arc::new(Semaphore::new(1));
        let permit = Arc::clone(&budget).acquire_owned().await?;
        let mut pending = PendingResponseWriter;
        let mut response = super::ReviewResponseWriter::new(&mut pending, Some(permit));

        std::future::poll_fn(|context| {
            let pending = tokio::io::AsyncWrite::poll_write(
                std::pin::Pin::new(&mut response),
                context,
                b"response",
            );
            assert!(pending.is_pending());
            std::task::Poll::Ready(())
        })
        .await;

        let replacement = budget.try_acquire_owned()?;
        drop(replacement);
        Ok(())
    }

    #[test]
    fn terminal_review_state_reconstructs_its_historical_activation() {
        let reference = ReviewRunRef::new(
            ReviewTargetId::from_uuid(Uuid::from_u128(1)),
            ReviewRunId::from_uuid(Uuid::from_u128(2)),
        );
        let pass_reference =
            ReviewPassRef::new(reference, ReviewPassId::from_uuid(Uuid::from_u128(3)));
        let session = SessionId::from_uuid(Uuid::from_u128(4));
        let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(5));
        let origin_turn = TurnId::from_uuid(Uuid::from_u128(6));
        let active_turn = origin_turn;
        let terminal_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(7));
        let policy = ReviewPolicy::version_one();
        let mut queued_run = ReviewRun::new(reference, ReviewWorkflowKind::ReadOnlyReview, policy);
        let queued_pass = ReviewPass::try_new(
            pass_reference,
            ReviewPassKind::ReadOnlyReview,
            &mut queued_run,
            session,
            ReviewPassAcceptedInputEvidence::new(accepted_input, session, Some(origin_turn)),
        )
        .expect("the fixture pass owns its accepted input");
        let running_pass = queued_pass
            .transition(
                ReviewPassState::Running { turn: active_turn },
                Some(ReviewPassTurnEvidence::new(
                    active_turn,
                    session,
                    accepted_input,
                    ReviewPassTurnOutcome::Active,
                    None,
                )),
            )
            .expect("the fixture pass activates");
        let running_run = queued_run
            .transition(
                ReviewRunState::Running {
                    active_pass: pass_reference,
                },
                Some(ReviewPassEvidence::from_pass(&running_pass, policy)),
            )
            .expect("the fixture run activates");
        let failed_pass = running_pass
            .clone()
            .transition(
                ReviewPassState::Failed { turn: active_turn },
                Some(ReviewPassTurnEvidence::new(
                    active_turn,
                    session,
                    accepted_input,
                    ReviewPassTurnOutcome::Failed,
                    Some(terminal_frontier),
                )),
            )
            .expect("the fixture pass concludes");
        let failed_run = running_run
            .clone()
            .transition(
                ReviewRunState::Failed {
                    failed_pass: pass_reference,
                },
                Some(ReviewPassEvidence::from_pass(&failed_pass, policy)),
            )
            .expect("the fixture run concludes");

        assert!(super::review_activation_was_applied(
            &failed_run,
            &failed_pass,
            active_turn,
        ));
        let (reconstructed_run, reconstructed_pass) =
            super::historical_review_activation(&failed_run, &failed_pass, active_turn)
                .expect("terminal state retains the historical activation");
        assert_eq!(reconstructed_run, running_run);
        assert_eq!(reconstructed_pass, running_pass);
    }

    #[tokio::test]
    async fn review_command_budget_admits_one_claim_at_a_time() -> Result<(), Box<dyn Error>> {
        assert_eq!(MAX_CONCURRENT_REVIEW_COMMANDS, 1);
        let budget = Arc::new(Semaphore::new(MAX_CONCURRENT_REVIEW_COMMANDS));
        let (_shutdown, shutdown_receiver) = watch::channel(false);
        let first =
            acquire_review_command_permit(Arc::clone(&budget), &mut shutdown_receiver.clone())
                .await?
                .ok_or_else(|| {
                    io::Error::other("the first review command must acquire its permit")
                })?;

        assert!(
            timeout(
                Duration::from_millis(20),
                acquire_review_command_permit(Arc::clone(&budget), &mut shutdown_receiver.clone()),
            )
            .await
            .is_err(),
            "the second review command must wait for the first claim"
        );

        drop(first);
        let second = timeout(
            Duration::from_secs(1),
            acquire_review_command_permit(Arc::clone(&budget), &mut shutdown_receiver.clone()),
        )
        .await??
        .ok_or_else(|| io::Error::other("the second review command must acquire after release"))?;
        drop(second);
        Ok(())
    }

    #[test]
    fn claude_converter_failures_map_to_typed_content_silent_evidence() {
        use ClaudeCodeJsonlConversionFailure as Failure;
        use ConversationImportRejectionClass as Class;
        use ConversionFailureDisposition::Rejected;

        assert_eq!(
            claude_conversion_failure_disposition(Failure::EmptySource),
            Rejected(import_evidence(Class::EmptySource, None))
        );
        assert_simple_import_failures!(
            claude_conversion_failure_disposition,
            Failure;
            2 => BlankLine => Class::BlankLine,
            3 => InvalidUtf8 => Class::InvalidUtf8,
            4 => InvalidJson => Class::InvalidJson,
            5 => JsonDepthExceeded => Class::JsonDepthExceeded,
            6 => TopLevelNotObject => Class::TopLevelNotObject,
            7 => InvalidRecordType => Class::InvalidRecordType,
            8 => InvalidSourceMetadata => Class::InvalidSourceMetadata,
            9 => InvalidMessageEnvelope => Class::InvalidMessageEnvelope,
            10 => InvalidMessageRole => Class::InvalidMessageRole,
            11 => MessageRoleMismatch => Class::MessageRoleMismatch,
            12 => InvalidMessageContent => Class::InvalidMessageContent,
        );
        assert_import_failure_ordinal!(
            claude_conversion_failure_disposition,
            13,
            |line| Failure::InvalidContentBlock { line, block: 1 },
            Class::InvalidContentBlock
        );
        assert_import_failure_ordinal!(
            claude_conversion_failure_disposition,
            14,
            |line| Failure::InvalidToolResultBlock {
                line,
                block: 1,
                result_block: 2,
            },
            Class::InvalidToolResultBlock
        );
        assert_eq!(
            claude_conversion_failure_disposition(Failure::PositionExhausted),
            ConversionFailureDisposition::Internal
        );
    }

    #[test]
    fn codex_converter_failures_map_to_typed_content_silent_evidence() {
        use CodexRolloutJsonlConversionFailure as Failure;
        use ConversationImportRejectionClass as Class;
        use ConversionFailureDisposition::Rejected;

        assert_eq!(
            codex_conversion_failure_disposition(Failure::EmptySource),
            Rejected(import_evidence(Class::EmptySource, None))
        );
        assert_simple_import_failures!(
            codex_conversion_failure_disposition,
            Failure;
            2 => BlankLine => Class::BlankLine,
            3 => InvalidUtf8 => Class::InvalidUtf8,
            4 => InvalidJson => Class::InvalidJson,
            5 => JsonDepthExceeded => Class::JsonDepthExceeded,
            6 => TopLevelNotObject => Class::TopLevelNotObject,
            7 => InvalidRecordType => Class::InvalidRecordType,
            8 => InvalidResponseItemType => Class::InvalidRecordType,
            9 => InvalidSourceMetadata => Class::InvalidSourceMetadata,
            10 => InvalidResponseItemEnvelope => Class::InvalidMessageEnvelope,
            11 => InvalidMessageRole => Class::InvalidMessageRole,
            12 => InvalidMessageContent => Class::InvalidMessageContent,
            14 => InvalidReasoning => Class::InvalidReasoning,
            16 => InvalidToolCall => Class::InvalidToolCall,
            17 => InvalidToolResult => Class::InvalidToolResult,
        );
        assert_import_failure_ordinal!(
            codex_conversion_failure_disposition,
            13,
            |line| Failure::InvalidMessageBlock { line, block: 1 },
            Class::InvalidContentBlock
        );
        assert_import_failure_ordinal!(
            codex_conversion_failure_disposition,
            15,
            |line| Failure::InvalidReasoningBlock { line, block: 1 },
            Class::InvalidReasoning
        );
        assert_import_failure_ordinal!(
            codex_conversion_failure_disposition,
            18,
            |line| Failure::InvalidToolResultBlock { line, block: 1 },
            Class::InvalidToolResultBlock
        );
        assert_eq!(
            codex_conversion_failure_disposition(Failure::PositionExhausted),
            ConversionFailureDisposition::Internal
        );
    }

    #[test]
    fn oversized_begin_is_rejected_without_reserving_import_capacity() {
        let limit = 8;
        let oversized = ClientRequest::BeginConversationImport {
            format: signalbox_process_protocol::ConversationImportFormat::CodexRolloutJsonlV1,
            declared_size_bytes: CanonicalU64::new(u64::try_from(limit + 1).expect("limit fits")),
        };
        let admitted = ClientRequest::BeginConversationImport {
            format: signalbox_process_protocol::ConversationImportFormat::CodexRolloutJsonlV1,
            declared_size_bytes: CanonicalU64::new(u64::try_from(limit).expect("limit fits")),
        };
        let zero_blob = ClientRequest::BeginBlobUpload {
            expected_digest: signalbox_process_protocol::CanonicalBlobDigest::from_bytes([0; 32]),
            expected_length_bytes: CanonicalU64::new(0),
        };
        let admitted_blob = ClientRequest::BeginBlobUpload {
            expected_digest: signalbox_process_protocol::CanonicalBlobDigest::from_bytes([0; 32]),
            expected_length_bytes: CanonicalU64::new(u64::try_from(limit).expect("limit fits")),
        };
        let oversized_blob = ClientRequest::BeginBlobUpload {
            expected_digest: signalbox_process_protocol::CanonicalBlobDigest::from_bytes([0; 32]),
            expected_length_bytes: CanonicalU64::new(u64::try_from(limit + 1).expect("limit fits")),
        };

        assert!(!super::conversation_import_request_requires_permit(
            &oversized,
            ConversationImportState::Inactive,
            limit,
            u64::MAX,
        ));
        assert!(super::conversation_import_request_requires_permit(
            &admitted,
            ConversationImportState::Inactive,
            limit,
            u64::MAX,
        ));
        assert!(!super::conversation_import_request_requires_permit(
            &admitted,
            ConversationImportState::Active,
            limit,
            u64::MAX,
        ));
        assert!(!super::conversation_import_request_requires_permit(
            &zero_blob,
            ConversationImportState::Inactive,
            limit,
            u64::try_from(limit).expect("limit fits"),
        ));
        assert!(super::conversation_import_request_requires_permit(
            &admitted_blob,
            ConversationImportState::Inactive,
            limit,
            u64::try_from(limit).expect("limit fits"),
        ));
        assert!(!super::conversation_import_request_requires_permit(
            &oversized_blob,
            ConversationImportState::Inactive,
            limit,
            u64::try_from(limit).expect("limit fits"),
        ));
    }

    /// each chunked bulk-ingest kind rejects every lifecycle request
    /// belonging to the other kind while preserving its own lifecycle.
    #[test]
    fn cross_kind_bulk_ingest_requests_are_classified_before_admission() {
        let append_blob = ClientRequest::AppendBlobUpload {
            chunk: signalbox_process_protocol::BlobChunk::new(vec![1]),
        };
        let append_import = ClientRequest::AppendConversationImport {
            chunk: signalbox_process_protocol::ConversationImportSource::new(vec![1]),
        };

        assert!(super::request_is_cross_kind_bulk_ingest(
            &append_blob,
            signalbox_process_protocol::BulkIngestKind::ConversationImport,
        ));
        assert!(super::request_is_cross_kind_bulk_ingest(
            &append_import,
            signalbox_process_protocol::BulkIngestKind::BlobUpload,
        ));
        assert!(!super::request_is_cross_kind_bulk_ingest(
            &append_blob,
            signalbox_process_protocol::BulkIngestKind::BlobUpload,
        ));
        assert!(!super::request_is_cross_kind_bulk_ingest(
            &append_import,
            signalbox_process_protocol::BulkIngestKind::ConversationImport,
        ));
    }

    /// inactivity resets after accepted lifecycle output while the
    /// whole-session deadline stays anchored to permit acquisition.
    #[tokio::test(start_paused = true)]
    async fn bulk_ingest_deadlines_have_independent_monotonic_anchors()
    -> Result<(), Box<dyn Error>> {
        let started_at = Instant::now();
        let permit = Arc::new(Semaphore::new(1)).acquire_owned().await?;
        let mut pending = Some(PendingConversationImport {
            format: signalbox_process_protocol::ConversationImportFormat::CodexRolloutJsonlV1,
            declared_size_bytes: 1,
            actual_size_bytes: 0,
            source: Vec::new(),
            import_permit: permit,
            started_at,
            idle_since: started_at,
        });

        assert_eq!(
            super::pending_bulk_ingest_deadline(&pending, &None, true),
            Some(started_at + super::BULK_INGEST_IDLE_TIMEOUT),
        );
        tokio::time::advance(Duration::from_secs(4 * 60)).await;
        pending
            .as_mut()
            .expect("the fixture import is active")
            .idle_since = Instant::now();
        assert_eq!(
            super::pending_bulk_ingest_deadline(&pending, &None, true),
            Some(started_at + Duration::from_secs(9 * 60)),
        );
        assert_eq!(
            super::pending_bulk_ingest_deadline(&pending, &None, false),
            Some(started_at + super::BULK_INGEST_SESSION_TIMEOUT),
        );
        Ok(())
    }

    /// an active upload classifies every second begin as the sole
    /// nonterminal duplicate-begin refusal before inspecting its new length.
    #[test]
    fn active_blob_upload_precedes_duplicate_begin_length_validation()
    -> Result<(), Box<dyn Error>> {
        let detail = blob_upload_begin_preflight(true, CanonicalU64::new(0), 8)
            .ok_or_else(|| io::Error::other("the active upload must reject a second begin"))?;

        assert_eq!(detail, RejectionDetail::BlobUploadAlreadyInProgress {});
        Ok(())
    }

    #[tokio::test]
    async fn duplicate_begin_refusal_preserves_the_active_import() -> Result<(), Box<dyn Error>> {
        let capacity = 1;
        let budget = Arc::new(Semaphore::new(capacity));
        let permit = Arc::clone(&budget).acquire_owned().await?;
        let format = signalbox_process_protocol::ConversationImportFormat::CodexRolloutJsonlV1;
        let source = b"partial".to_vec();
        let expected_source = source.clone();
        let declared_size_bytes = u64::try_from(source.len())?;
        let mut pending = Some(PendingConversationImport {
            format,
            declared_size_bytes,
            actual_size_bytes: declared_size_bytes,
            source,
            import_permit: permit,
            started_at: Instant::now(),
            idle_since: Instant::now(),
        });
        let request_id = RequestId::try_new(1)?;
        let (mut writer, mut reader) = duplex(1_024);

        handle_begin_conversation_import(
            &mut writer,
            ProtocolVersion::One,
            request_id,
            format,
            CanonicalU64::new(declared_size_bytes),
            usize::try_from(declared_size_bytes)?,
            None,
            None,
            &mut pending,
        )
        .await?;
        drop(writer);
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;
        let observed = decode_server_line(&encoded)?;
        let expected = ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request_id,
            ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                message: String::from("conversation import was rejected"),
                detail: ErrorDetail::invalid_request(
                    RejectionDetail::ConversationImportAlreadyInProgress {},
                ),
            },
        )?;
        let active = pending
            .as_ref()
            .expect("the active import remains available");

        assert_eq!(observed, expected);
        assert_eq!(active.source, expected_source);
        assert_eq!(budget.available_permits(), capacity - 1);
        Ok(())
    }

    #[tokio::test]
    async fn waiting_begin_releases_its_inbound_slot_before_import_admission()
    -> Result<(), Box<dyn Error>> {
        let capacity = 1;
        let frame_budgets = InboundFrameBudgets::new();
        let import_budget = Arc::new(Semaphore::new(capacity));
        let occupied_import = Arc::clone(&import_budget).acquire_owned().await?;
        let frame_permit = frame_budgets
            .for_connection(ConversationImportState::Inactive)
            .acquire_owned()
            .await?;
        let begin = ClientRequest::BeginConversationImport {
            format: signalbox_process_protocol::ConversationImportFormat::CodexRolloutJsonlV1,
            declared_size_bytes: CanonicalU64::new(u64::try_from(capacity)?),
        };
        let import_requires_permit = super::conversation_import_request_requires_permit(
            &begin,
            ConversationImportState::Inactive,
            capacity,
            u64::MAX,
        );

        let retained = retain_inbound_frame_permit_during_import_admission(
            &begin,
            import_requires_permit,
            frame_permit,
        );

        assert!(retained.is_none());
        assert_eq!(import_budget.available_permits(), 0);
        let general_slots = frame_budgets
            .for_connection(ConversationImportState::Inactive)
            .acquire_many_owned(u32::try_from(GENERAL_BUFFERED_INBOUND_FRAMES)?)
            .await?;
        let (_shutdown, mut shutdown_receiver) = watch::channel(false);
        let active_slot = timeout(
            Duration::from_secs(1),
            acquire_inbound_frame_permit(
                frame_budgets.for_connection(ConversationImportState::Active),
                &mut shutdown_receiver,
            ),
        )
        .await??
        .ok_or_else(|| io::Error::other("the active import must retain frame progress"))?;

        assert_eq!(general_slots.num_permits(), GENERAL_BUFFERED_INBOUND_FRAMES);
        assert_eq!(
            active_slot.num_permits(),
            RESERVED_ACTIVE_IMPORT_INBOUND_FRAMES
        );
        assert_eq!(
            general_slots.num_permits() + active_slot.num_permits(),
            MAX_BUFFERED_INBOUND_FRAMES
        );
        drop(occupied_import);
        Ok(())
    }

    #[tokio::test]
    async fn released_begin_waiters_have_a_separate_bound() -> Result<(), Box<dyn Error>> {
        let capacity = MAX_IMPORT_ADMISSION_WAITERS;
        let budget = Arc::new(Semaphore::new(capacity));
        let occupied = Arc::clone(&budget)
            .acquire_many_owned(u32::try_from(capacity)?)
            .await?;
        let (_shutdown, mut shutdown_receiver) = watch::channel(false);
        let acquire = acquire_import_waiter_permit(Arc::clone(&budget), &mut shutdown_receiver);
        tokio::pin!(acquire);

        assert!(
            timeout(Duration::from_millis(20), &mut acquire)
                .await
                .is_err()
        );
        assert_eq!(occupied.num_permits(), capacity);

        drop(occupied);
        let admitted = timeout(Duration::from_secs(1), &mut acquire)
            .await??
            .ok_or_else(|| io::Error::other("a released waiter place must admit the begin"))?;

        assert_eq!(admitted.num_permits(), 1);
        Ok(())
    }

    #[test]
    fn conversation_import_allocation_exhaustion_is_unavailable() {
        let diagnostic = InternalDiagnostic::ConversationImportAllocationFailure;
        let error = unavailable_protocol_error(diagnostic);

        assert_eq!(error.code, ErrorCode::Unavailable);
        assert_eq!(error.detail, ErrorDetail::none());
        assert_eq!(
            diagnostic.failure_class(),
            signalbox_application::OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            }
        );
    }

    #[test]
    fn conversation_import_capacity_grows_geometrically_within_declared_and_configured_bounds() {
        let chunk_capacity = 4;
        let declared_capacity = chunk_capacity * 4;
        let configured_capacity = declared_capacity * 2;
        let first_capacity = super::conversation_import_capacity_target(
            0,
            chunk_capacity,
            declared_capacity,
            configured_capacity,
        );
        let second_capacity = super::conversation_import_capacity_target(
            first_capacity,
            chunk_capacity * 2,
            declared_capacity,
            configured_capacity,
        );
        let retained_capacity = super::conversation_import_capacity_target(
            second_capacity,
            chunk_capacity * 2 - 1,
            declared_capacity,
            configured_capacity,
        );
        let third_capacity = super::conversation_import_capacity_target(
            retained_capacity,
            chunk_capacity * 2 + 1,
            declared_capacity,
            configured_capacity,
        );
        let declared_bound = super::conversation_import_capacity_target(
            third_capacity,
            declared_capacity,
            declared_capacity,
            configured_capacity,
        );
        let configured_bound = super::conversation_import_capacity_target(
            declared_bound,
            declared_capacity + 1,
            declared_capacity,
            configured_capacity,
        );

        assert_eq!(first_capacity, chunk_capacity);
        assert_eq!(second_capacity, chunk_capacity * 2);
        assert_eq!(retained_capacity, second_capacity);
        assert_eq!(third_capacity, declared_capacity);
        assert_eq!(declared_bound, declared_capacity);
        assert_eq!(configured_bound, configured_capacity);
    }

    #[tokio::test]
    async fn chunk_appends_assemble_exact_source_order() -> Result<(), Box<dyn Error>> {
        let budget = Arc::new(Semaphore::new(1));
        let permit = budget.clone().acquire_owned().await?;
        let first = b"first".to_vec();
        let second = b"second".to_vec();
        let expected_source = [first.as_slice(), second.as_slice()].concat();
        let expected_size = u64::try_from(expected_source.len())?;
        let limit = 32;
        let mut pending = Some(PendingConversationImport {
            format: signalbox_process_protocol::ConversationImportFormat::CodexRolloutJsonlV1,
            declared_size_bytes: expected_size,
            actual_size_bytes: 0,
            source: Vec::new(),
            import_permit: permit,
            started_at: Instant::now(),
            idle_since: Instant::now(),
        });
        let (mut writer, _reader) = duplex(1_024);

        handle_append_conversation_import(
            &mut writer,
            ProtocolVersion::One,
            RequestId::try_new(1)?,
            first,
            limit,
            &mut pending,
        )
        .await?;
        handle_append_conversation_import(
            &mut writer,
            ProtocolVersion::One,
            RequestId::try_new(2)?,
            second,
            limit,
            &mut pending,
        )
        .await?;

        let assembled = pending.as_ref().expect("the import remains pending");
        assert_eq!(assembled.source, expected_source);
        assert_eq!(assembled.actual_size_bytes, expected_size);
        assert!(assembled.source.capacity() <= limit);
        Ok(())
    }

    #[tokio::test]
    async fn begin_rejects_a_declared_size_above_the_configured_bound() -> Result<(), Box<dyn Error>>
    {
        let capacity = 1;
        let budget = Arc::new(Semaphore::new(capacity));
        let permit = budget.clone().acquire_owned().await?;
        let request_id = RequestId::try_new(1)?;
        let limit = 8;
        let declared_size_bytes = CanonicalU64::new(9);
        let mut pending = None;
        let (mut writer, mut reader) = duplex(1_024);

        handle_begin_conversation_import(
            &mut writer,
            ProtocolVersion::One,
            request_id,
            signalbox_process_protocol::ConversationImportFormat::CodexRolloutJsonlV1,
            declared_size_bytes,
            limit,
            Some(permit),
            None,
            &mut pending,
        )
        .await?;
        drop(writer);
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;
        let observed = decode_server_line(&encoded)?;
        let expected = ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request_id,
            ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                message: String::from("conversation import was rejected"),
                detail: ErrorDetail::invalid_request(
                    RejectionDetail::ConversationImportSourceTooLarge {
                        limit_bytes: CanonicalU64::new(u64::try_from(limit)?),
                        declared_size_bytes,
                        actual_size_bytes: None,
                    },
                ),
            },
        )?;

        assert_eq!(observed, expected);
        assert!(pending.is_none());
        assert_eq!(budget.available_permits(), capacity);
        Ok(())
    }

    #[tokio::test]
    async fn append_rejects_observed_size_above_the_configured_bound() -> Result<(), Box<dyn Error>>
    {
        let capacity = 1;
        let budget = Arc::new(Semaphore::new(capacity));
        let permit = budget.clone().acquire_owned().await?;
        let request_id = RequestId::try_new(1)?;
        let limit = 8;
        let declared_size_bytes = u64::try_from(limit)?;
        let prior_size_bytes = u64::try_from(limit - 1)?;
        let chunk = vec![b'x'; 2];
        let observed_size_bytes = prior_size_bytes + u64::try_from(chunk.len())?;
        let mut pending = Some(PendingConversationImport {
            format: signalbox_process_protocol::ConversationImportFormat::CodexRolloutJsonlV1,
            declared_size_bytes,
            actual_size_bytes: prior_size_bytes,
            source: vec![b'x'; usize::try_from(prior_size_bytes)?],
            import_permit: permit,
            started_at: Instant::now(),
            idle_since: Instant::now(),
        });
        let (mut writer, mut reader) = duplex(1_024);

        handle_append_conversation_import(
            &mut writer,
            ProtocolVersion::One,
            request_id,
            chunk,
            limit,
            &mut pending,
        )
        .await?;
        drop(writer);
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;
        let observed = decode_server_line(&encoded)?;
        let expected = ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request_id,
            ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                message: String::from("conversation import was rejected"),
                detail: ErrorDetail::invalid_request(
                    RejectionDetail::ConversationImportSourceTooLarge {
                        limit_bytes: CanonicalU64::new(u64::try_from(limit)?),
                        declared_size_bytes: CanonicalU64::new(declared_size_bytes),
                        actual_size_bytes: Some(CanonicalU64::new(observed_size_bytes)),
                    },
                ),
            },
        )?;

        assert_eq!(budget.available_permits(), capacity);
        assert_eq!(observed, expected);
        assert!(pending.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn commit_rejects_declared_and_actual_size_mismatch() -> Result<(), Box<dyn Error>> {
        let capacity = 1;
        let budget = Arc::new(Semaphore::new(capacity));
        let permit = budget.clone().acquire_owned().await?;
        let source = vec![b'x'];
        let actual_size_bytes = u64::try_from(source.len())?;
        let declared_size_bytes = actual_size_bytes + 1;
        let request_id = RequestId::try_new(1)?;
        let limit = 8;
        let mut pending = Some(PendingConversationImport {
            format: signalbox_process_protocol::ConversationImportFormat::CodexRolloutJsonlV1,
            declared_size_bytes,
            actual_size_bytes,
            source,
            import_permit: permit,
            started_at: Instant::now(),
            idle_since: Instant::now(),
        });
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://signalbox:fixture@127.0.0.1/signalbox")?;
        let repository = ImportedConversationRepository::new(pool);
        let (mut writer, mut reader) = duplex(1);
        let write = tokio::spawn(async move {
            let result = handle_commit_conversation_import(
                &mut writer,
                ProtocolVersion::One,
                request_id,
                limit,
                repository,
                &mut pending,
            )
            .await;
            (result, pending)
        });

        let reacquired = timeout(Duration::from_secs(1), budget.acquire_owned()).await??;
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;
        let (write_result, pending) = write.await?;
        write_result?;
        let observed = decode_server_line(&encoded)?;
        let expected = ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request_id,
            ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                message: String::from("conversation import was rejected"),
                detail: ErrorDetail::invalid_request(
                    RejectionDetail::ConversationImportSourceSizeMismatch {
                        declared_size_bytes: CanonicalU64::new(declared_size_bytes),
                        actual_size_bytes: CanonicalU64::new(actual_size_bytes),
                    },
                ),
            },
        )?;

        assert_eq!(reacquired.num_permits(), capacity);
        assert_eq!(observed, expected);
        assert!(pending.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn commit_rechecks_the_configured_total_bound() -> Result<(), Box<dyn Error>> {
        let budget = Arc::new(Semaphore::new(1));
        let permit = budget.clone().acquire_owned().await?;
        let request_id = RequestId::try_new(1)?;
        let declared_size_bytes = 7;
        let actual_size_bytes = 9;
        let limit = 8;
        let mut pending = Some(PendingConversationImport {
            format: signalbox_process_protocol::ConversationImportFormat::CodexRolloutJsonlV1,
            declared_size_bytes,
            actual_size_bytes,
            source: Vec::new(),
            import_permit: permit,
            started_at: Instant::now(),
            idle_since: Instant::now(),
        });
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://signalbox:fixture@127.0.0.1/signalbox")?;
        let repository = ImportedConversationRepository::new(pool);
        let (mut writer, mut reader) = duplex(1_024);

        handle_commit_conversation_import(
            &mut writer,
            ProtocolVersion::One,
            request_id,
            limit,
            repository,
            &mut pending,
        )
        .await?;
        drop(writer);
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;
        let observed = decode_server_line(&encoded)?;
        let expected = ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request_id,
            ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                message: String::from("conversation import was rejected"),
                detail: ErrorDetail::invalid_request(
                    RejectionDetail::ConversationImportSourceTooLarge {
                        limit_bytes: CanonicalU64::new(u64::try_from(limit)?),
                        declared_size_bytes: CanonicalU64::new(declared_size_bytes),
                        actual_size_bytes: Some(CanonicalU64::new(actual_size_bytes)),
                    },
                ),
            },
        )?;

        assert_eq!(observed, expected);
        assert!(pending.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn disconnect_drop_discards_partial_import_and_releases_its_permit()
    -> Result<(), Box<dyn Error>> {
        let capacity = 1;
        let budget = Arc::new(Semaphore::new(capacity));
        let permit = budget.clone().acquire_owned().await?;
        let pending = PendingConversationImport {
            format: signalbox_process_protocol::ConversationImportFormat::CodexRolloutJsonlV1,
            declared_size_bytes: 4,
            actual_size_bytes: 2,
            source: b"pa".to_vec(),
            import_permit: permit,
            started_at: Instant::now(),
            idle_since: Instant::now(),
        };

        drop(pending);
        let reacquired = timeout(Duration::from_secs(1), budget.acquire_owned()).await??;

        assert_eq!(reacquired.num_permits(), capacity);
        Ok(())
    }

    #[tokio::test]
    async fn import_budget_admits_one_retained_aggregate_at_a_time() -> Result<(), Box<dyn Error>> {
        assert_eq!(MAX_CONCURRENT_IMPORTS, 1);
        let budget = Arc::new(Semaphore::new(MAX_CONCURRENT_IMPORTS));
        let (_shutdown, shutdown_receiver) = watch::channel(false);
        let first = acquire_import_permit(Arc::clone(&budget), &mut shutdown_receiver.clone())
            .await?
            .ok_or_else(|| io::Error::other("the first import must acquire its permit"))?;

        assert!(
            timeout(
                Duration::from_millis(20),
                acquire_import_permit(Arc::clone(&budget), &mut shutdown_receiver.clone()),
            )
            .await
            .is_err(),
            "a second retained import aggregate must wait"
        );

        drop(first);
        let second = timeout(
            Duration::from_secs(1),
            acquire_import_permit(Arc::clone(&budget), &mut shutdown_receiver.clone()),
        )
        .await??
        .ok_or_else(|| io::Error::other("the released import permit must be acquired"))?;
        drop(second);
        Ok(())
    }

    #[tokio::test]
    async fn import_conversion_runs_off_the_async_worker() -> Result<(), Box<dyn Error>> {
        let async_worker = thread::current().id();
        let (thread_sender, thread_receiver) = mpsc::sync_channel(1);
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://signalbox:fixture@127.0.0.1/signalbox")?;
        let repository = ImportedConversationRepository::new(pool);

        let outcome = execute_import(
            ThreadReportingRejectConverter(thread_sender),
            Vec::new(),
            repository,
        )
        .await;
        let conversion_worker = thread_receiver.recv_timeout(Duration::from_secs(1))?;

        assert_eq!(
            outcome,
            Err(OperationalImportError::InvalidSource(
                super::import_evidence(
                    signalbox_process_protocol::ConversationImportRejectionClass::InvalidJson,
                    None,
                )
            ))
        );
        assert_ne!(conversion_worker, async_worker);
        Ok(())
    }

    #[tokio::test]
    async fn import_worker_termination_remains_distinct_from_repository_corruption()
    -> Result<(), Box<dyn Error>> {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://signalbox:fixture@127.0.0.1/signalbox")?;
        let repository = ImportedConversationRepository::new(pool);

        let outcome = execute_import(PanickingConverter, Vec::new(), repository).await;

        assert_eq!(
            outcome,
            Err(OperationalImportError::Internal(
                InternalDiagnostic::ConversationImportWorkerTerminated,
            )),
        );
        Ok(())
    }

    #[test]
    fn import_worker_termination_has_exact_operator_diagnostic() {
        let diagnostic = InternalDiagnostic::ConversationImportWorkerTerminated;

        assert_eq!(
            diagnostic.failure_class(),
            signalbox_application::OperatorFailureClass::CallerOrHubBug
        );
        assert_eq!(
            diagnostic.cause_code(),
            "conversation_import_worker_terminated"
        );
    }

    #[test]
    fn import_converter_contract_defect_has_exact_operator_diagnostic() {
        let error = ImportConversationError::<io::Error, ImportedConversationRepositoryError>::
            ConverterEntryIdentitySequenceMismatch;

        assert_eq!(
            operational_import_error(error),
            OperationalImportError::Internal(InternalDiagnostic::ConversationImportContractDefect,),
        );
        assert_eq!(
            InternalDiagnostic::ConversationImportContractDefect.failure_class(),
            signalbox_application::OperatorFailureClass::CallerOrHubBug,
        );
        assert_eq!(
            InternalDiagnostic::ConversationImportContractDefect.cause_code(),
            "conversation_import_contract_defect",
        );
    }

    #[test]
    fn import_repository_identity_collision_keeps_its_operator_class() {
        let error =
            ImportConversationError::<io::Error, ImportedConversationRepositoryError>::Store(
                ImportedConversationRepositoryError::IdentityCollision(
                    ImportedConversationIdentityCollision::Conversation,
                ),
            );

        assert_eq!(
            operational_import_error(error),
            OperationalImportError::Internal(
                InternalDiagnostic::ImportedConversationIdentityCollision,
            ),
        );
        assert_eq!(
            InternalDiagnostic::ImportedConversationIdentityCollision.failure_class(),
            signalbox_application::OperatorFailureClass::IdentityCollision,
        );
    }

    #[test]
    fn import_repository_corruption_keeps_its_operator_class() {
        let error =
            ImportConversationError::<io::Error, ImportedConversationRepositoryError>::Store(
                ImportedConversationRepositoryError::Corruption(
                    ImportedConversationCorruption::Missing("fixture required field"),
                ),
            );

        assert_eq!(
            operational_import_error(error),
            OperationalImportError::Internal(InternalDiagnostic::ImportedConversationCorruption,),
        );
        assert_eq!(
            InternalDiagnostic::ImportedConversationCorruption.failure_class(),
            signalbox_application::OperatorFailureClass::FailClosedCorruption,
        );
    }

    #[test]
    fn import_blob_unavailability_is_retryable_without_ambiguity() {
        let error =
            ImportConversationError::<io::Error, ImportedConversationRepositoryError>::Store(
                ImportedConversationRepositoryError::BlobStorage(
                    ImportedRawBlobStorageError::Unavailable,
                ),
            );

        assert_eq!(
            operational_import_error(error),
            OperationalImportError::Unavailable,
        );
    }

    #[test]
    fn import_catalog_database_failure_remains_retryable() {
        let error =
            ImportConversationError::<io::Error, ImportedConversationRepositoryError>::Store(
                ImportedConversationRepositoryError::BlobCatalog(
                    signalbox_persistence::blob::BlobCatalogRepositoryError::Database(
                        sqlx::Error::PoolTimedOut,
                    ),
                ),
            );

        assert_eq!(
            operational_import_error(error),
            OperationalImportError::Database,
        );
    }

    #[test]
    fn import_catalog_ambiguous_commit_remains_retryable() {
        let error =
            ImportConversationError::<io::Error, ImportedConversationRepositoryError>::Store(
                ImportedConversationRepositoryError::BlobCatalog(
                    signalbox_persistence::blob::BlobCatalogRepositoryError::CommitAmbiguous(
                        sqlx::Error::PoolTimedOut,
                    ),
                ),
            );

        assert_eq!(
            operational_import_error(error),
            OperationalImportError::Unavailable,
        );
    }

    #[test]
    fn import_blob_integrity_failure_is_fail_closed() {
        let error =
            ImportConversationError::<io::Error, ImportedConversationRepositoryError>::Store(
                ImportedConversationRepositoryError::BlobStorage(
                    ImportedRawBlobStorageError::Integrity,
                ),
            );

        assert_eq!(
            operational_import_error(error),
            OperationalImportError::Internal(InternalDiagnostic::ImportedConversationCorruption),
        );
    }

    struct PanickingConverter;

    impl ImportedConversationConverter for PanickingConverter {
        type Error = io::Error;

        fn format(&self) -> ImportedConversationFormat {
            ImportedConversationFormat::CodexRolloutJsonlV1
        }

        fn convert<NextEntryId>(
            &mut self,
            _conversation: ImportedConversationId,
            _source: &[u8],
            _next_entry_id: NextEntryId,
        ) -> Result<ImportedConversation, Self::Error>
        where
            NextEntryId: FnMut() -> ImportedTranscriptEntryId,
        {
            panic!("synthetic import worker panic")
        }
    }

    struct ThreadReportingRejectConverter(mpsc::SyncSender<thread::ThreadId>);

    impl ImportedConversationConverter for ThreadReportingRejectConverter {
        type Error = io::Error;

        fn format(&self) -> ImportedConversationFormat {
            ImportedConversationFormat::CodexRolloutJsonlV1
        }

        fn convert<NextEntryId>(
            &mut self,
            _conversation: ImportedConversationId,
            _source: &[u8],
            _next_entry_id: NextEntryId,
        ) -> Result<ImportedConversation, Self::Error>
        where
            NextEntryId: FnMut() -> ImportedTranscriptEntryId,
        {
            self.0
                .send(thread::current().id())
                .map_err(|_| io::Error::other("the test thread receiver closed"))?;
            Err(io::Error::other("fixture conversion rejection"))
        }
    }

    #[test]
    fn process_submission_admits_the_exact_content_bound() {
        let exact =
            UserInputContent::text("\u{1}".repeat(signalbox_domain::UserContent::MAX_TEXT_BYTES));
        assert!(admitted_user_content(exact).is_ok());
    }

    #[test]
    fn process_submission_rejects_content_over_the_bound() {
        assert!(
            admitted_user_content(UserInputContent::text(
                "x".repeat(signalbox_domain::UserContent::MAX_TEXT_BYTES + 1),
            ))
            .is_err()
        );
    }

    #[test]
    fn accepted_input_bound_keeps_snapshot_projection_representable() -> Result<(), Box<dyn Error>>
    {
        let frame = ServerFrame::try_new(
            RequestId::try_new(u64::MAX)?,
            ServerMessage::TranscriptTurn {
                turn_id: CanonicalUuid::from_uuid(Uuid::from_u128(u128::MAX)),
                acceptance_position: CanonicalU64::new(u64::MAX),
                model_settings: None,
                state: TurnState::Queued {
                    accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(u128::MAX - 1)),
                    content: UserInputContent::text(
                        "\u{1}".repeat(signalbox_domain::UserContent::MAX_TEXT_BYTES),
                    ),
                },
            },
        )?;

        assert!(encode_server_line(&frame)?.len() <= MAX_FRAME_BYTES);
        Ok(())
    }

    #[test]
    fn accepted_input_bound_keeps_update_projection_representable() -> Result<(), Box<dyn Error>> {
        let frame = ServerFrame::try_new(
            RequestId::try_new(u64::MAX)?,
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(u64::MAX),
                session_id: CanonicalUuid::from_uuid(Uuid::from_u128(u128::MAX)),
                event: SessionEvent::InputAccepted {
                    accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(u128::MAX - 1)),
                    turn_id: CanonicalUuid::from_uuid(Uuid::from_u128(u128::MAX - 2)),
                    acceptance_position: CanonicalU64::new(u64::MAX),
                    content: UserInputContent::text(
                        "\u{1}".repeat(signalbox_domain::UserContent::MAX_TEXT_BYTES),
                    ),
                },
            },
        )?;

        assert!(encode_server_line(&frame)?.len() <= MAX_FRAME_BYTES);
        Ok(())
    }

    #[test]
    fn oversized_connection_frame_does_not_fail_the_runtime() {
        assert!(
            inspect_connection_completion(Some(Ok(Err(ProcessConnectionError::Encode(
                FrameEncodeError::OversizedFrame
            )))))
            .is_ok()
        );
    }

    #[test]
    fn peer_io_failure_does_not_fail_the_runtime() {
        assert!(
            inspect_connection_completion(Some(Ok(Err(ProcessConnectionError::PeerIo(
                io::Error::new(io::ErrorKind::BrokenPipe, "fixture peer closed")
            )))))
            .is_ok()
        );
    }

    #[test]
    fn spool_read_failure_is_fatal_runtime_evidence() {
        let result = inspect_connection_completion(Some(Ok(Err(ProcessConnectionError::SpoolIo(
            io::Error::other("fixture spool read"),
        )))));

        assert!(matches!(result, Err(ProcessRuntimeError::SpoolIo(_))));
    }

    #[tokio::test]
    async fn pre_response_spool_io_is_reported_as_unavailable() -> Result<(), Box<dyn Error>> {
        let request_id = RequestId::try_new(9)?;
        let (mut writer, mut reader) = duplex(1_024);

        write_snapshot_spool_error(
            &mut writer,
            ProtocolVersion::One,
            request_id,
            SnapshotSpoolError::Io(io::Error::other("fixture spool write")),
        )
        .await?;
        drop(writer);
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;

        let frame = decode_server_line(&encoded)?;
        assert!(matches!(
            frame.message(),
            ServerMessage::Error {
                code: ErrorCode::Unavailable,
                ..
            }
        ));
        Ok(())
    }

    #[tokio::test]
    async fn goal_history_is_completed_in_spool_before_socket_write() -> Result<(), Box<dyn Error>>
    {
        let session = SessionId::from_uuid(Uuid::from_u128(40));
        let session_id = wire_uuid(session.into_uuid());
        let command = DurableCommandId::from_uuid(Uuid::from_u128(41));
        let statement = GoalStatement::try_new(String::from("finish the fixture task"))?;
        let goal = Goal::commission(session, statement.clone(), GoalUserProvenance::new(command));
        let request_id = RequestId::try_new(42)?;
        let mut spool = spool_goal_snapshot(&goal, ProtocolVersion::One, request_id, session_id)
            .await
            .map_err(|error| {
                io::Error::other(format!(
                    "goal spool fixture failed: {}",
                    spool_error_display(&error)
                ))
            })?;
        let mut encoded = Vec::new();
        spool.read_to_end(&mut encoded).await?;

        let mut expected = encode_server_line(&ServerFrame::try_new(
            request_id,
            ServerMessage::GoalHistoryStart {
                session_id,
                current_generation: CanonicalU64::new(goal.current().generation().get()),
                current_statement: statement.as_str().to_owned(),
            },
        )?)?;
        expected.extend(encode_server_line(&ServerFrame::try_new(
            request_id,
            ServerMessage::GoalHistoryState {
                current_state: GoalLifecycleState::Pursuing {},
            },
        )?)?);
        expected.extend(encode_server_line(&ServerFrame::try_new(
            request_id,
            ServerMessage::GoalHistoryItem {
                event_ordinal: CanonicalU64::new(goal.events()[0].ordinal().get()),
                generation: CanonicalU64::new(goal.events()[0].generation().get()),
                event: wire_goal_event(&goal.events()[0])?,
            },
        )?)?);
        expected.extend(encode_server_line(&ServerFrame::try_new(
            request_id,
            ServerMessage::GoalHistoryEnd {
                event_count: CanonicalU64::new(
                    u64::try_from(goal.events().len()).expect("fixture event count fits u64"),
                ),
            },
        )?)?);

        assert_eq!(encoded, expected);
        Ok(())
    }

    #[tokio::test]
    async fn blocked_follow_write_is_cancelled_by_shutdown() -> Result<(), Box<dyn Error>> {
        let (mut writer, _reader) = duplex(1);
        writer.write_all(b"x").await?;
        let (shutdown, mut shutdown_receiver) = watch::channel(false);
        let blocked_write = tokio::spawn(async move {
            run_until_shutdown(
                &mut shutdown_receiver,
                writer.write_all(b"blocked follow output"),
            )
            .await
        });
        tokio::task::yield_now().await;

        shutdown.send(true)?;

        let outcome = timeout(Duration::from_secs(1), blocked_write).await??;
        assert!(outcome.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn runtime_content_writer_preserves_empty_and_multibyte_text()
    -> Result<(), Box<dyn Error>> {
        let request_id = RequestId::try_new(7)?;
        let text = format!(
            "{}\u{1f980}tail",
            "a".repeat(MAX_CONTENT_FRAGMENT_BYTES - 1)
        );
        let (mut writer, mut reader) = duplex(MAX_FRAME_BYTES * 2);
        write_content(&mut writer, ProtocolVersion::One, request_id, 3, &text).await?;
        drop(writer);
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;

        let mut reconstructed = String::new();
        let mut expected_fragment = 0_u64;
        let lines = encoded.split_inclusive(|byte| *byte == b'\n');
        for line in lines {
            let frame = decode_server_line(line)?;
            match frame.message() {
                ServerMessage::TranscriptContent {
                    entry_index,
                    fragment_index,
                    final_fragment,
                    content_fragment,
                } => {
                    assert_eq!(entry_index.value(), 3);
                    assert_eq!(fragment_index.value(), expected_fragment);
                    reconstructed.push_str(content_fragment.as_str());
                    expected_fragment += 1;
                    assert_eq!(*final_fragment, expected_fragment == 2);
                }
                message => {
                    return Err(io::Error::other(format!("unexpected message: {message:?}")).into());
                }
            }
        }
        assert_eq!(expected_fragment, 2);
        assert_eq!(reconstructed, text);

        let (mut writer, mut reader) = duplex(1_024);
        write_content(&mut writer, ProtocolVersion::One, request_id, 0, "").await?;
        drop(writer);
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;
        let frame = decode_server_line(&encoded)?;
        assert!(matches!(
            frame.message(),
            ServerMessage::TranscriptContent {
                fragment_index,
                final_fragment: true,
                content_fragment,
                ..
            } if fragment_index.value() == 0 && content_fragment.as_str().is_empty()
        ));
        Ok(())
    }

    #[tokio::test]
    async fn s28_imported_entries_map_only_to_conservative_shapes() -> Result<(), Box<dyn Error>> {
        let request_id = RequestId::try_new(11)?;
        let source_session = SessionId::from_uuid(Uuid::from_u128(1));
        let conversation = ImportedConversationId::from_uuid(Uuid::from_u128(2));
        let imported_entry = ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(3));
        let semantic_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(4));
        let (mut writer, mut reader) = duplex(4_096);

        let source_attested = "source-attested";
        write_transcript_entry(
            &mut writer,
            ProtocolVersion::One,
            request_id,
            &ProcessTranscriptEntry::ImportedText {
                entry_index: 0,
                source_session,
                entry: semantic_entry,
                imported_conversation: conversation,
                imported_entry,
                source_speaker: ProcessImportedSourceSpeaker::User,
                content: String::from(source_attested),
            },
        )
        .await?;
        write_transcript_entry(
            &mut writer,
            ProtocolVersion::One,
            request_id,
            &ProcessTranscriptEntry::Imported {
                entry_index: 1,
                source_session,
                entry: SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(5)),
                imported_conversation: conversation,
                imported_entry: ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(6)),
                source_speaker: ProcessImportedSourceSpeaker::NotAttested,
                content_kind: ProcessImportedContentKind::ToolResult,
            },
        )
        .await?;
        drop(writer);

        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;
        let mut lines = encoded.split_inclusive(|byte| *byte == b'\n');
        let text = decode_server_line(
            lines
                .next()
                .ok_or_else(|| io::Error::other("missing imported text metadata"))?,
        )?;
        let ServerMessage::TranscriptTextEntry {
            entry:
                TranscriptTextEntry::Imported {
                    imported_conversation_id,
                    imported_entry_id,
                    source_speaker:
                        ImportedSourceSpeaker::Attested {
                            speaker: ImportedSpeaker::User,
                        },
                },
            ..
        } = text.message()
        else {
            panic!(
                "fixture expected imported text metadata, got {:?}",
                text.message()
            );
        };
        assert_eq!(
            imported_conversation_id.into_uuid(),
            conversation.into_uuid()
        );
        assert_eq!(imported_entry_id.into_uuid(), imported_entry.into_uuid());
        let content = decode_server_line(
            lines
                .next()
                .ok_or_else(|| io::Error::other("missing imported text content"))?,
        )?;
        let ServerMessage::TranscriptContent {
            final_fragment: true,
            content_fragment,
            ..
        } = content.message()
        else {
            panic!(
                "fixture expected imported text content, got {:?}",
                content.message()
            );
        };
        assert_eq!(content_fragment.as_str(), source_attested);
        assert!(matches!(
            decode_server_line(
                lines
                    .next()
                    .ok_or_else(|| io::Error::other("missing conservative imported entry"))?
            )?
            .message(),
            ServerMessage::TranscriptEntry {
                entry: TranscriptEntry::Imported {
                    source_speaker: ImportedSourceSpeaker::NotAttested {},
                    content_kind: ImportedContentKind::ToolResult,
                    ..
                },
                ..
            }
        ));
        assert!(lines.next().is_none());
        Ok(())
    }

    #[test]
    fn every_persistence_terminal_call_disposition_has_a_wire_projection() {
        assert_eq!(
            wire_model_call_state(DispatchedModelCallState::CancellationRequested),
            ModelCallState::CancellationRequested {}
        );
        assert_eq!(
            wire_model_call_state(DispatchedModelCallState::Terminal(
                DispatchedModelCallDisposition::Completed,
            )),
            ModelCallState::Terminal {
                disposition: ModelCallDisposition::Completed
            }
        );
        assert_eq!(
            wire_model_call_state(DispatchedModelCallState::Terminal(
                DispatchedModelCallDisposition::KnownFailed,
            )),
            ModelCallState::Terminal {
                disposition: ModelCallDisposition::KnownFailed
            }
        );
        assert_eq!(
            wire_model_call_state(DispatchedModelCallState::Terminal(
                DispatchedModelCallDisposition::Refused,
            )),
            ModelCallState::Terminal {
                disposition: ModelCallDisposition::Refused
            }
        );
        assert_eq!(
            wire_model_call_state(DispatchedModelCallState::Terminal(
                DispatchedModelCallDisposition::Cancelled,
            )),
            ModelCallState::Terminal {
                disposition: ModelCallDisposition::Cancelled
            }
        );
        assert_eq!(
            wire_model_call_state(DispatchedModelCallState::Terminal(
                DispatchedModelCallDisposition::Ambiguous,
            )),
            ModelCallState::Terminal {
                disposition: ModelCallDisposition::Ambiguous
            }
        );
    }

    #[test]
    fn goal_turn_retirement_projects_to_the_exact_wire_identity() {
        let turn = TurnId::from_uuid(Uuid::from_u128(7));
        let update = ProcessUpdateEvent::from_outbox(&DispatchedOutboxEventKind::TurnTerminal {
            turn,
            disposition: DispatchedTurnTerminalDisposition::Retired,
        })
        .expect("a client-visible event projects to one update");

        assert_eq!(
            update.wire().expect("the fixture event is representable"),
            SessionEvent::GoalTurnRetired {
                turn_id: CanonicalUuid::from_uuid(turn.into_uuid()),
            }
        );
    }

    /// a recorded session settings change reaches the wire as its
    /// typed projection without losing either settings snapshot.
    #[test]
    fn session_model_settings_change_projects_to_the_closed_wire_shape() {
        let session = SessionId::from_uuid(Uuid::from_u128(1));
        let command = DurableCommandId::from_uuid(Uuid::from_u128(2));
        let prior_selection = DirectModelSelection::from_uuid(Uuid::from_u128(3));
        let installed_selection = DirectModelSelection::from_uuid(Uuid::from_u128(4));
        let prior_version = SessionConfigurationDefaultsVersion::first();
        let installed_version = prior_version
            .checked_next()
            .expect("the initial defaults version has a successor");
        let prior_settings = ValidatedModelSettings::provider_defaults();
        let inherited = ModelSettingsOverlay::inherit_all();
        let installed_precedence = ModelSettingsPrecedence::new(
            inherited,
            ModelSettingsOverlay::new(
                SettingOverlay::Value(ReasoningLevel::Low),
                FastModeOverlay::Inherit,
                SettingOverlay::Inherit,
            ),
            inherited,
            inherited,
        );
        let installed_settings = ModelCapabilities::new(
            BTreeSet::from([ReasoningLevel::Low]),
            FastModeSupport::Unsupported,
            BTreeSet::new(),
        )
        .validate_precedence(installed_selection, installed_precedence)
        .expect("the fixture capability admits low reasoning");
        let caller_override = ModelSettingsOverlay::new(
            SettingOverlay::Value(ReasoningLevel::Low),
            FastModeOverlay::Inherit,
            SettingOverlay::Inherit,
        );
        let changed = SessionModelSettingsChanged::try_new(
            session,
            command,
            prior_version,
            installed_version,
            ModelSelectionRequest::Direct(prior_selection),
            ModelSelectionRequest::Direct(installed_selection),
            prior_settings,
            installed_settings,
            caller_override,
            Vec::new(),
        )
        .expect("the fixture changes direct model selection");
        let changed_update = ProcessUpdateEvent::from_outbox(
            &DispatchedOutboxEventKind::SessionModelSettingsChanged(changed),
        )
        .expect("the fixture event projects onto an update");

        assert_eq!(
            changed_update
                .wire()
                .expect("the fixture event is representable"),
            SessionEvent::SessionModelSettingsChanged {
                command_id: signalbox_process_protocol::CommandId::try_from_uuid(
                    command.into_uuid(),
                )
                .expect("fixture command identity is admitted"),
                prior_defaults_version: CanonicalU64::new(prior_version.as_u64()),
                installed_defaults_version: CanonicalU64::new(installed_version.as_u64()),
                prior_model: signalbox_process_protocol::ModelSelection::Direct {
                    selection_id: CanonicalUuid::from_uuid(prior_selection.into_uuid()),
                },
                installed_model: signalbox_process_protocol::ModelSelection::Direct {
                    selection_id: CanonicalUuid::from_uuid(installed_selection.into_uuid()),
                },
                prior_settings: signalbox_process_protocol::ModelSettingsSnapshot {
                    precedence: signalbox_process_protocol::ModelSettingsPrecedence {
                        per_call: signalbox_process_protocol::ModelSettingsOverlay::inherit_all(),
                        session: signalbox_process_protocol::ModelSettingsOverlay::inherit_all(),
                        profile: signalbox_process_protocol::ModelSettingsOverlay::inherit_all(),
                        global_default:
                            signalbox_process_protocol::ModelSettingsOverlay::inherit_all(),
                    },
                    effective: signalbox_process_protocol::EffectiveModelSettings {
                        reasoning_level: None,
                        fast_mode: signalbox_process_protocol::FastMode::Disabled,
                        service_tier: None,
                    },
                    reasoning_source: None,
                    fast_mode_source: None,
                    service_tier_source: None,
                    validated_for_selection_id: None,
                },
                installed_settings: signalbox_process_protocol::ModelSettingsSnapshot {
                    precedence: signalbox_process_protocol::ModelSettingsPrecedence {
                        per_call: signalbox_process_protocol::ModelSettingsOverlay::inherit_all(),
                        session: signalbox_process_protocol::ModelSettingsOverlay {
                            reasoning_level: signalbox_process_protocol::SettingOverlay::Value(
                                signalbox_process_protocol::ReasoningLevel::Low,
                            ),
                            fast_mode: signalbox_process_protocol::FastModeOverlay::Inherit,
                            service_tier: signalbox_process_protocol::SettingOverlay::Inherit,
                        },
                        profile: signalbox_process_protocol::ModelSettingsOverlay::inherit_all(),
                        global_default:
                            signalbox_process_protocol::ModelSettingsOverlay::inherit_all(),
                    },
                    effective: signalbox_process_protocol::EffectiveModelSettings {
                        reasoning_level: Some(signalbox_process_protocol::ReasoningLevel::Low),
                        fast_mode: signalbox_process_protocol::FastMode::Disabled,
                        service_tier: None,
                    },
                    reasoning_source: Some(signalbox_process_protocol::ModelSettingSource::Session,),
                    fast_mode_source: None,
                    service_tier_source: None,
                    validated_for_selection_id: Some(CanonicalUuid::from_uuid(
                        installed_selection.into_uuid(),
                    )),
                },
                caller_override: signalbox_process_protocol::ModelSettingsOverlay {
                    reasoning_level: signalbox_process_protocol::SettingOverlay::Value(
                        signalbox_process_protocol::ReasoningLevel::Low,
                    ),
                    fast_mode: signalbox_process_protocol::FastModeOverlay::Inherit,
                    service_tier: signalbox_process_protocol::SettingOverlay::Inherit,
                },
                adjustments: Vec::new(),
            }
        );
    }

    /// a recorded per-turn settings resolution reaches the wire with
    /// its requested alias and exact resolved-settings evidence.
    #[test]
    fn turn_model_settings_resolution_projects_to_the_closed_wire_shape() {
        let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(5));
        let turn = TurnId::from_uuid(Uuid::from_u128(6));
        let requested_alias = ModelAlias::from_uuid(Uuid::from_u128(3));
        let prior_selection = DirectModelSelection::from_uuid(Uuid::from_u128(2));
        let installed_selection = DirectModelSelection::from_uuid(Uuid::from_u128(4));
        let installed_version = SessionConfigurationDefaultsVersion::first();
        let caller_override = ModelSettingsOverlay::inherit_all();
        let session_settings = ModelSettingsOverlay::new(
            SettingOverlay::ProviderDefault,
            FastModeOverlay::Inherit,
            SettingOverlay::Inherit,
        );
        let precedence = ModelSettingsPrecedence::new(
            caller_override,
            session_settings,
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::inherit_all(),
        );
        let settings = ModelCapabilities::new(
            BTreeSet::new(),
            FastModeSupport::Unsupported,
            BTreeSet::new(),
        )
        .validate_precedence(installed_selection, precedence)
        .expect("the explicit provider default is supported");
        let resolved = TurnModelSettingsResolved::try_new(
            accepted_input,
            turn,
            installed_version,
            FrozenModelSelection::FrozenAlias {
                alias: requested_alias,
                definition: FrozenAliasDefinition::selecting(installed_selection),
            },
            caller_override,
            settings,
            Some(prior_selection),
            vec![ModelChangeAdjustment::ReasoningLevelCleared {
                from: ReasoningLevel::High,
            }],
        )
        .expect("provider-default settings are valid for the fixture selection");
        let resolved_update = ProcessUpdateEvent::from_outbox(
            &DispatchedOutboxEventKind::TurnModelSettingsResolved(resolved),
        )
        .expect("the fixture event projects onto an update");

        assert_eq!(
            resolved_update
                .wire()
                .expect("the fixture event is representable"),
            SessionEvent::TurnModelSettingsResolved {
                accepted_input_id: CanonicalUuid::from_uuid(accepted_input.into_uuid()),
                turn_id: CanonicalUuid::from_uuid(turn.into_uuid()),
                defaults_version: CanonicalU64::new(installed_version.as_u64()),
                requested_model: signalbox_process_protocol::ModelSelection::Alias {
                    alias_id: CanonicalUuid::from_uuid(requested_alias.into_uuid()),
                },
                selected_direct_id: CanonicalUuid::from_uuid(installed_selection.into_uuid()),
                per_call_override: signalbox_process_protocol::ModelSettingsOverlay::inherit_all(),
                settings: signalbox_process_protocol::ModelSettingsSnapshot {
                    precedence: signalbox_process_protocol::ModelSettingsPrecedence {
                        per_call: signalbox_process_protocol::ModelSettingsOverlay::inherit_all(),
                        session: signalbox_process_protocol::ModelSettingsOverlay {
                            reasoning_level:
                                signalbox_process_protocol::SettingOverlay::ProviderDefault,
                            fast_mode: signalbox_process_protocol::FastModeOverlay::Inherit,
                            service_tier: signalbox_process_protocol::SettingOverlay::Inherit,
                        },
                        profile: signalbox_process_protocol::ModelSettingsOverlay::inherit_all(),
                        global_default:
                            signalbox_process_protocol::ModelSettingsOverlay::inherit_all(),
                    },
                    effective: signalbox_process_protocol::EffectiveModelSettings {
                        reasoning_level: None,
                        fast_mode: signalbox_process_protocol::FastMode::Disabled,
                        service_tier: None,
                    },
                    reasoning_source: Some(signalbox_process_protocol::ModelSettingSource::Session,),
                    fast_mode_source: None,
                    service_tier_source: None,
                    validated_for_selection_id: Some(CanonicalUuid::from_uuid(
                        installed_selection.into_uuid(),
                    )),
                },
                adjusted_from_selection_id: Some(CanonicalUuid::from_uuid(
                    prior_selection.into_uuid(),
                )),
                adjustments: vec![
                    signalbox_process_protocol::ModelChangeAdjustment::ReasoningLevelCleared {
                        from: signalbox_process_protocol::ReasoningLevel::High,
                    },
                ],
            }
        );
    }

    #[test]
    fn committed_process_foreground_wait_retries_follow_up_read_failure() {
        let disposition = preserve_committed_foreground_wait::<u8, _>(Err("database"));

        assert_eq!(disposition, CommittedForegroundDelivery::Retry("database"));
    }

    #[test]
    fn delegation_updates_project_but_internal_wakes_do_not_follow() {
        let spawning_request = signalbox_domain::ToolRequestId::from_uuid(Uuid::from_u128(8));
        let child = SessionId::from_uuid(Uuid::from_u128(9));
        let update = ProcessUpdateEvent::from_outbox(&DispatchedOutboxEventKind::DelegationUpdate(
            signalbox_persistence::outbox::DispatchedDelegationUpdate::ChildSpawned {
                spawning_request,
                child,
                policy: signalbox_persistence::outbox::DispatchedDelegationPolicy::Background,
            },
        ))
        .expect("a delegation update is client-visible");

        assert_eq!(
            update.wire().expect("the fixture event is representable"),
            SessionEvent::ChildSpawned {
                spawning_request_id: CanonicalUuid::from_uuid(spawning_request.into_uuid()),
                child_session_id: CanonicalUuid::from_uuid(child.into_uuid()),
                relationship: signalbox_process_protocol::DelegationPolicy::Background {},
            }
        );
        assert!(
            ProcessUpdateEvent::from_outbox(&DispatchedOutboxEventKind::DelegationWake(
                signalbox_persistence::outbox::DispatchedDelegationWake::Result {
                    spawning_request,
                    awaiting_request: None,
                },
            ))
            .is_none()
        );
    }

    /// S17: committing an internal delivery wake makes the exact
    /// recipient eligible without projecting the wake onto follow streams.
    #[test]
    fn s17_internal_delegation_wake_nudges_exact_recipient() {
        let recipient = SessionId::from_uuid(Uuid::from_u128(10));
        let spawning_request = ToolRequestId::from_uuid(Uuid::from_u128(11));
        let nudge = RecordingEligibilityNudge::default();
        let recorded = Arc::clone(&nudge.sessions);

        nudge_delegation_wake(
            &nudge,
            recipient,
            &DispatchedOutboxEventKind::DelegationWake(
                signalbox_persistence::outbox::DispatchedDelegationWake::Result {
                    spawning_request,
                    awaiting_request: None,
                },
            ),
        );

        assert_eq!(
            recorded
                .lock()
                .expect("recorded nudge lock remains available")
                .as_slice(),
            &[recipient]
        );
    }

    #[test]
    fn completed_process_delegation_nudges_exact_issuer() {
        let issuer = SessionId::from_uuid(Uuid::from_u128(12));
        let nudge = RecordingEligibilityNudge::default();
        let recorded = Arc::clone(&nudge.sessions);

        nudge_delegation_issuer(&nudge, issuer);

        assert_eq!(
            recorded
                .lock()
                .expect("recorded nudge lock remains available")
                .as_slice(),
            &[issuer]
        );
    }

    #[test]
    fn definitive_process_message_rejection_nudges_exact_issuer() {
        let issuer = SessionId::from_uuid(Uuid::from_u128(17));
        let nudge = RecordingEligibilityNudge::default();
        let recorded = Arc::clone(&nudge.sessions);

        nudge_after_process_message_rejection(
            &nudge,
            issuer,
            ProcessDelegationRequestRejection::Operation(
                DelegationOperationRejection::RelationshipNotFound,
            ),
        );

        assert_eq!(
            recorded
                .lock()
                .expect("recorded nudge lock remains available")
                .as_slice(),
            &[issuer]
        );
    }

    #[test]
    fn definitive_process_await_rejection_nudges_exact_issuer() {
        let issuer = SessionId::from_uuid(Uuid::from_u128(19));
        let nudge = RecordingEligibilityNudge::default();
        let recorded = Arc::clone(&nudge.sessions);

        nudge_after_process_await_rejection(
            &nudge,
            issuer,
            ProcessDelegationRequestRejection::Operation(
                DelegationOperationRejection::DeliverySequenceExhausted,
            ),
        );

        assert_eq!(
            recorded
                .lock()
                .expect("recorded nudge lock remains available")
                .as_slice(),
            &[issuer]
        );
    }

    #[test]
    fn stale_process_await_rejection_does_not_nudge_issuer() {
        let issuer = SessionId::from_uuid(Uuid::from_u128(20));
        let nudge = RecordingEligibilityNudge::default();
        let recorded = Arc::clone(&nudge.sessions);

        nudge_after_process_await_rejection(
            &nudge,
            issuer,
            ProcessDelegationRequestRejection::Operation(
                DelegationOperationRejection::StaleDispatch {
                    state: DelegationRequestExecutionState::AttemptEnded,
                },
            ),
        );

        assert_eq!(
            recorded
                .lock()
                .expect("recorded nudge lock remains available")
                .as_slice(),
            &[]
        );
    }

    #[test]
    fn stale_process_message_rejection_does_not_nudge_issuer() {
        let issuer = SessionId::from_uuid(Uuid::from_u128(18));
        let nudge = RecordingEligibilityNudge::default();
        let recorded = Arc::clone(&nudge.sessions);

        nudge_after_process_message_rejection(
            &nudge,
            issuer,
            ProcessDelegationRequestRejection::Operation(
                DelegationOperationRejection::StaleDispatch {
                    state: DelegationRequestExecutionState::AttemptEnded,
                },
            ),
        );

        assert_eq!(
            recorded
                .lock()
                .expect("recorded nudge lock remains available")
                .as_slice(),
            &[]
        );
    }

    #[tokio::test]
    async fn foreground_delegation_peer_disconnect_abandons_socket_wait() {
        let (peer, daemon) = duplex(64);
        let mut reader = BufReader::new(daemon);
        drop(peer);

        let error = foreground_peer_activity(&mut reader)
            .await
            .expect_err("a disconnected foreground peer ends its socket wait");
        let source = error
            .source()
            .expect("peer failure retains its I/O source")
            .downcast_ref::<io::Error>()
            .expect("peer failure source is I/O");

        assert_eq!(source.kind(), io::ErrorKind::ConnectionAborted);
    }

    #[test]
    fn delegated_process_rejections_use_typed_wire_details() {
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(13));
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(14));
        let request_id = CanonicalUuid::from_uuid(Uuid::from_u128(15));
        let peer_id = CanonicalUuid::from_uuid(Uuid::from_u128(16));
        let relationship = process_delegation_rejection(
            ProcessDelegationRequestRejection::Operation(
                DelegationOperationRejection::RelationshipNotFound,
            ),
            session_id,
            turn_id,
            request_id,
            peer_id,
        );
        let session_not_found = process_delegation_rejection(
            ProcessDelegationRequestRejection::SessionNotFound,
            session_id,
            turn_id,
            request_id,
            peer_id,
        );
        let await_conflict = process_delegation_rejection(
            ProcessDelegationRequestRejection::AwaitConflict,
            session_id,
            turn_id,
            request_id,
            peer_id,
        );
        let message_conflict = process_delegation_rejection(
            ProcessDelegationRequestRejection::MessageConflict,
            session_id,
            turn_id,
            request_id,
            peer_id,
        );
        let awaiting_approval = process_delegation_rejection(
            ProcessDelegationRequestRejection::Operation(
                DelegationOperationRejection::StaleDispatch {
                    state: DelegationRequestExecutionState::AwaitingApproval,
                },
            ),
            session_id,
            turn_id,
            request_id,
            peer_id,
        );
        let denied = process_delegation_rejection(
            ProcessDelegationRequestRejection::Operation(
                DelegationOperationRejection::StaleDispatch {
                    state: DelegationRequestExecutionState::Denied,
                },
            ),
            session_id,
            turn_id,
            request_id,
            peer_id,
        );
        let approved = process_delegation_rejection(
            ProcessDelegationRequestRejection::Operation(
                DelegationOperationRejection::StaleDispatch {
                    state: DelegationRequestExecutionState::Approved,
                },
            ),
            session_id,
            turn_id,
            request_id,
            peer_id,
        );
        let prepared = process_delegation_rejection(
            ProcessDelegationRequestRejection::Operation(
                DelegationOperationRejection::StaleDispatch {
                    state: DelegationRequestExecutionState::Prepared,
                },
            ),
            session_id,
            turn_id,
            request_id,
            peer_id,
        );
        let closed = process_delegation_rejection(
            ProcessDelegationRequestRejection::Operation(
                DelegationOperationRejection::StaleDispatch {
                    state: DelegationRequestExecutionState::Closed,
                },
            ),
            session_id,
            turn_id,
            request_id,
            peer_id,
        );
        let attempt_ended = process_delegation_rejection(
            ProcessDelegationRequestRejection::Operation(
                DelegationOperationRejection::StaleDispatch {
                    state: DelegationRequestExecutionState::AttemptEnded,
                },
            ),
            session_id,
            turn_id,
            request_id,
            peer_id,
        );

        assert_eq!(
            session_not_found.detail,
            ErrorDetail::rejected(RejectionDetail::SessionNotFound { session_id })
        );
        assert_eq!(
            relationship.detail,
            ErrorDetail::rejected(RejectionDetail::DelegationRelationNotFound {
                session_id,
                peer_session_id: peer_id,
            })
        );
        assert_eq!(
            await_conflict.detail,
            ErrorDetail::rejected(RejectionDetail::DelegationAwaitConflict {
                tool_request_id: request_id,
            })
        );
        assert_eq!(
            message_conflict.detail,
            ErrorDetail::rejected(RejectionDetail::DelegationMessageConflict {
                tool_request_id: request_id,
            })
        );
        assert_eq!(
            awaiting_approval.detail,
            ErrorDetail::rejected(RejectionDetail::DelegationToolRequestNotExecutable {
                tool_request_id: request_id,
                state: WireDelegationToolRequestState::AwaitingApproval,
            })
        );
        assert_eq!(
            denied.detail,
            ErrorDetail::rejected(RejectionDetail::DelegationToolRequestNotExecutable {
                tool_request_id: request_id,
                state: WireDelegationToolRequestState::Denied,
            })
        );
        assert_eq!(
            approved.detail,
            ErrorDetail::rejected(RejectionDetail::DelegationToolRequestNotExecutable {
                tool_request_id: request_id,
                state: WireDelegationToolRequestState::Approved,
            })
        );
        assert_eq!(
            prepared.detail,
            ErrorDetail::rejected(RejectionDetail::DelegationToolRequestNotExecutable {
                tool_request_id: request_id,
                state: WireDelegationToolRequestState::Prepared,
            })
        );
        assert_eq!(
            closed.detail,
            ErrorDetail::rejected(RejectionDetail::DelegationToolRequestNotExecutable {
                tool_request_id: request_id,
                state: WireDelegationToolRequestState::Closed,
            })
        );
        assert_eq!(
            attempt_ended.detail,
            ErrorDetail::rejected(RejectionDetail::DelegationToolRequestNotExecutable {
                tool_request_id: request_id,
                state: WireDelegationToolRequestState::AttemptEnded,
            })
        );
    }

    #[test]
    fn delivery_sequence_exhaustion_names_the_operation_recipient() {
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(13));
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(14));
        let request_id = CanonicalUuid::from_uuid(Uuid::from_u128(15));
        let peer_id = CanonicalUuid::from_uuid(Uuid::from_u128(16));
        let recipient_id = CanonicalUuid::from_uuid(Uuid::from_u128(17));
        let error = process_delegation_rejection_for_recipient(
            ProcessDelegationRequestRejection::Operation(
                DelegationOperationRejection::DeliverySequenceExhausted,
            ),
            session_id,
            turn_id,
            request_id,
            peer_id,
            recipient_id,
        );

        assert_eq!(
            error.detail,
            ErrorDetail::rejected(RejectionDetail::DelegationDeliverySequenceExhausted {
                recipient_session_id: recipient_id,
                last: CanonicalU64::new(u64::MAX),
            })
        );
    }

    #[test]
    fn message_identity_collision_preserves_the_minted_identity() {
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(13));
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(14));
        let request_id = CanonicalUuid::from_uuid(Uuid::from_u128(15));
        let peer_id = CanonicalUuid::from_uuid(Uuid::from_u128(16));
        let message = DelegationMessageId::from_uuid(Uuid::from_u128(17));
        let error = process_delegation_rejection(
            ProcessDelegationRequestRejection::MessageIdentityCollision { message },
            session_id,
            turn_id,
            request_id,
            peer_id,
        );

        assert_eq!(
            error.detail,
            ErrorDetail::rejected(RejectionDetail::DelegationMessageIdentityCollision {
                message_id: wire_uuid(message.into_uuid()),
            })
        );
    }

    #[test]
    fn cancellation_and_reconciliation_project_to_exact_wire_shapes() {
        let turn = TurnId::from_uuid(Uuid::from_u128(1));
        let attempt = TurnAttemptId::from_uuid(Uuid::from_u128(2));
        let call = ModelCallId::from_uuid(Uuid::from_u128(3));
        let entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(4));
        let frontier = ContextFrontierId::from_uuid(Uuid::from_u128(5));

        assert_eq!(
            wire_turn_state(&ProcessTurnState::Cancelled {
                terminal_frontier: frontier,
                terminal_attempt: attempt,
                terminal_call: Some(call),
            }),
            TurnState::Cancelled {
                terminal_frontier_id: CanonicalUuid::from_uuid(frontier.into_uuid()),
                terminal_attempt_id: CanonicalUuid::from_uuid(attempt.into_uuid()),
                terminal_model_call_id: Some(CanonicalUuid::from_uuid(call.into_uuid())),
            }
        );
        assert_eq!(
            wire_turn_state(&ProcessTurnState::ReconciliationRequired {
                terminal_frontier: frontier,
                terminal_attempt: attempt,
                operation: ProcessReconciliationOperation::ModelCall(call),
            }),
            TurnState::ReconciliationRequired {
                terminal_frontier_id: CanonicalUuid::from_uuid(frontier.into_uuid()),
                terminal_attempt_id: CanonicalUuid::from_uuid(attempt.into_uuid()),
                terminal_model_call_id: CanonicalUuid::from_uuid(call.into_uuid()),
            }
        );

        let cancelled = ProcessUpdateEvent::from_outbox(&DispatchedOutboxEventKind::TurnTerminal {
            turn,
            disposition: DispatchedTurnTerminalDisposition::Cancelled {
                cancellation_entry: entry,
                terminal_frontier: frontier,
            },
        })
        .expect("a client-visible event projects to one update");
        assert_eq!(
            cancelled
                .wire()
                .expect("the fixture event is representable"),
            SessionEvent::TurnCancelled {
                turn_id: CanonicalUuid::from_uuid(turn.into_uuid()),
                cancellation_entry_id: CanonicalUuid::from_uuid(entry.into_uuid()),
                terminal_frontier_id: CanonicalUuid::from_uuid(frontier.into_uuid()),
            }
        );
        let reconciliation =
            ProcessUpdateEvent::from_outbox(&DispatchedOutboxEventKind::TurnTerminal {
                turn,
                disposition: DispatchedTurnTerminalDisposition::ReconciliationRequired {
                    operation: DispatchedReconciliationOperation::ModelCall(call),
                    terminal_frontier: frontier,
                },
            })
            .expect("a client-visible event projects to one update");
        assert_eq!(
            reconciliation
                .wire()
                .expect("the fixture event is representable"),
            SessionEvent::TurnReconciliationRequired {
                turn_id: CanonicalUuid::from_uuid(turn.into_uuid()),
                model_call_id: CanonicalUuid::from_uuid(call.into_uuid()),
                terminal_frontier_id: CanonicalUuid::from_uuid(frontier.into_uuid()),
            }
        );
        let tool_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(6));
        let recovery =
            ProcessUpdateEvent::from_outbox(&DispatchedOutboxEventKind::ToolBatchTransition {
                turn,
                producing_call: call,
                state: DispatchedToolBatchState::RecoveryRequired {
                    attempt: tool_attempt,
                },
            })
            .expect("a client-visible event projects to one update");
        assert_eq!(
            recovery.wire().expect("the fixture event is representable"),
            SessionEvent::ToolBatchTransition {
                turn_id: CanonicalUuid::from_uuid(turn.into_uuid()),
                model_call_id: CanonicalUuid::from_uuid(call.into_uuid()),
                state: ToolBatchState::RecoveryRequired {
                    tool_attempt_id: CanonicalUuid::from_uuid(tool_attempt.into_uuid()),
                },
            }
        );
    }

    /// the daemon preserves every bounded runner-placement
    /// fact while projecting one dispatched outbox transition to the wire.
    #[test]
    fn runner_state_transition_projects_to_the_closed_wire_shape() {
        let runner = RunnerId::from_uuid(Uuid::from_u128(7));
        let placement_revision =
            RunnerGeneration::try_from_u64(9).expect("the fixture revision is positive");
        let working_directory = RunnerWorkingDirectory::try_new("workspace/project".to_owned())
            .expect("the fixture directory is bounded exact text");
        let expected_working_directory = working_directory.as_str().to_owned();
        let update =
            ProcessUpdateEvent::from_outbox(&DispatchedOutboxEventKind::RunnerStateTransition {
                runner,
                placement_revision,
                sandbox: signalbox_domain::RunnerSandboxProfile::WorkspaceRestricted,
                working_directory: Some(working_directory),
                state: DispatchedRunnerState::WorkingDirectoryChanged,
            })
            .expect("a client-visible runner event projects to one update");

        assert_eq!(
            update.wire().expect("the fixture event is representable"),
            SessionEvent::RunnerStateTransition {
                runner_id: CanonicalUuid::from_uuid(runner.into_uuid()),
                placement_revision: WireRunnerPlacementRevision::try_new(placement_revision.get(),)
                    .expect("the fixture placement revision is positive"),
                sandbox_profile: WireRunnerSandboxProfile::WorkspaceRestricted,
                working_directory: Some(
                    WireRunnerWorkingDirectory::try_new(expected_working_directory)
                        .expect("the fixture wire directory is valid"),
                ),
                state: WireRunnerStateTransitionState::WorkingDirectoryChanged,
            }
        );
    }

    #[test]
    fn finish_condition_wire_union_admits_both_domain_variants() {
        let statement = signalbox_domain::FinishConditionStatement::try_new(String::from(
            "the branch is green",
        ))
        .expect("the fixture statement is admitted");
        assert_eq!(
            domain_finish_condition(WireFinishCondition::ExternalGate)
                .expect("the external gate is admitted"),
            signalbox_domain::FinishCondition::ExternalGate
        );
        assert_eq!(
            domain_finish_condition(WireFinishCondition::Declared {
                statement: statement.as_str().to_owned(),
            })
            .expect("the declared condition is admitted"),
            signalbox_domain::FinishCondition::Declared(statement)
        );
    }
}
