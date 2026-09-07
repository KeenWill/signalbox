//! Terminal client for the closed local Signalbox process protocol.

use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    os::unix::ffi::OsStrExt as _,
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use arguments::{
    Command, DangerousToolAutoApprovalArgument, DelegationTextArgument, GoalCommand,
    GoalTextArgument, ImportSourceArgument, ParseOutcome, ReviewCommand, SendDeliveryArgument,
    SessionCommand, SystemPromptArgument, ThroughPositionArgument,
};
use connection::ProcessClient;
use error::ClientError;
use presentation::{
    BlobUploadPresentation, ChildResultPresentation, ConversationRow, ImportedEntryRow,
    OperatorStatusPresentationCounts, Output, SessionAwaitRegisteredPresentation,
    SessionMessageSentPresentation, SessionMetadataRow, SessionSpawnedPresentation,
    SnapshotSelection,
};
use rustix::{
    fd::OwnedFd,
    fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags, fchmod, fstat, openat, statat},
};
use serde::{Deserialize, de::DeserializeOwned};
use sha2::{Digest as _, Sha256};
use signalbox_process_protocol::{
    BlobChunk, CanonicalBlobDigest, CanonicalU64, CanonicalUuid, ClientFrame, ClientRequest,
    CommandId, ConversationCursor, ConversationImportFormat, ConversationImportSource,
    ConversationOrigin, ConversationOriginFilter, ConversationSummary, DelegationMessageDirection,
    DelegationOutcome, DelegationPolicy, DelegationProvenance, DelegationReason,
    DelegationWaitMode, DescendantTerminationScope, ErrorCode, ErrorDetail, FrameEncodeError,
    GoalHistoryEvent, GoalLifecycleState, InputContent, InputDelivery, MAX_BLOB_CHUNK_BYTES,
    MAX_BLOB_READ_BYTES, MAX_CONTENT_FRAGMENT_BYTES, MAX_CONVERSATION_IMPORT_CHUNK_BYTES,
    MAX_FRAME_BYTES, ModelCallDisposition, ModelCallState, ModelSelection, ModelSettingsOverlay,
    OperatorStatusMessage, ProtocolVersion, RejectionDetail, RequestId,
    ReviewConcernTerminalOutcome, ReviewFindingEvent, ReviewFindingInput, ReviewFindingStatus,
    ReviewImportTerminalOutcome, ReviewJudgmentEffectTerminalOutcome, ReviewJudgmentPlanMember,
    ReviewOrchestrationConcernInput, ReviewOrchestrationState, ReviewPassLifecycle,
    ReviewPassSnapshot, ReviewPassTerminalOutcome, ReviewPublicationOutcome,
    ReviewPublicationTerminalOutcome, ReviewRepairOutcome, ReviewRepairTerminalOutcome,
    ReviewRunSnapshot, RunnerConnectionHealth, RunnerProjection, RunnerProjectionState,
    RunnerStateTransitionState, ServerFrame, ServerMessage, SessionEvent, SessionLifecycleMembers,
    SessionPlacement, SystemPromptMember, SystemPromptText, ToolBatchState, ToolDecision,
    TurnState, decode_server_line, encode_client_line, encode_server_line,
};
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};
use transcript::{SnapshotIdentitySet, SnapshotRecord, TranscriptSnapshot, read_snapshot};
use uuid::Uuid;

mod arguments;
mod chat;
mod connection;
mod error;
mod presentation;
mod runner;
mod transcript;

const MAX_INPUT_CONTENT_FRAME_BYTES: usize = MAX_FRAME_BYTES / 4 * 3;
const MAX_SYSTEM_PROMPT_FRAME_BYTES: usize = MAX_FRAME_BYTES / 4 * 3;
const MAX_REVIEW_JSON_INPUT_BYTES: usize = MAX_FRAME_BYTES / 4 * 3;
const MAX_SINGLE_FRAME_IMPORT_SOURCE_BYTES: usize = MAX_FRAME_BYTES / 4 * 3;
/// Bounded memory used while hashing one client-local blob source.
const BLOB_HASH_BUFFER_BYTES: usize = 64 * 1024;

mod conversation_import;
use conversation_import::ImportScanSummary;
#[cfg(test)]
use conversation_import::{
    ConversationImportOutcome, conversation_import_chunk_read_limit, open_scanned_import_source,
    read_import_file, source_fits_single_shot_import,
};
use conversation_import::{
    PreparedImport, collect_import_paths, import_conversation_file, imported, open_import_source,
    read_imported_conversation, scan_conversations, write_single_import_outcome,
};
mod delegation;
use delegation::session_delegation;
#[cfg(test)]
use delegation::{
    DelegationRejectionExpectation, DelegationRejectionOperation, delegation_rejection_matches,
    read_delegation_content_file,
};
mod session;
use session::ObservedSessionDefaults;
use session::read_session_defaults;
use session::{
    ModelSystemPromptChoice, compact, continue_imported, create, create_from_template,
    read_session_metadata_page, read_system_prompt_file, replace_session_model,
    update_session_placement,
};
#[cfg(test)]
use session::{
    placement_update_receipt_matches, placement_update_rejection_matches,
    replacement_receipt_settings_match,
};
mod credential;
mod goal;
use goal::goal;
#[cfg(test)]
use goal::{GoalHistoryReplay, decode_goal_mutation_receipt, read_goal_text_file};
mod listing;
use listing::{conversations, list, list_templates, search};
mod review;
use review::review;
#[cfg(test)]
use review::{
    ReviewConcernsFile, ReviewFindingsFile, read_review_json_file,
    review_concern_state_is_coherent, review_finding_event_status,
    review_judgment_effect_state_is_coherent, review_judgment_plan_state_is_coherent,
    review_pass_completion_is_coherent, review_publication_state_is_coherent,
    review_repair_state_is_coherent, review_run_response_is_coherent,
    validate_review_finding_count,
};
mod turn;
use turn::SubmitInputReceipt;
use turn::submit_input;
#[cfg(test)]
use turn::{
    TurnTerminal, await_turn_terminal, blocker_recovery_snapshot_state,
    model_call_recovery_transition, queued_turn_recovery, queued_turn_runner_recovery,
    reconcile_turn, selected_turn_recovery_transition, session_recovery_transition,
    terminal_event_state, terminal_snapshot_state, tool_recovery_transition,
};
use turn::{
    child_lifecycle_terminalization, decide, descendant_scope, reconcile, send, steer, stop,
    stop_turn,
};
mod follow_status;
use follow_status::terminal_snapshot_selection;
use follow_status::{
    FOLLOW_RECOVERY_REFETCH_INTERVAL, follow, placement_display, read_session_summaries, status,
    transcript as transcript_command, write_assistant_texts,
};

mod paging;
use paging::{ConversationsPageRequest, SessionMetadataPageRequest};
mod deployment_limits;
use deployment_limits::{
    ClientDeploymentLimits, command_uses_deployment_limits, read_deployment_limits,
    validate_message_policy, validate_metadata_page_policy, validate_system_prompt_policy,
};
mod blob;
#[cfg(test)]
use blob::PreparedBlobSource;
#[cfg(test)]
use blob::hash_blob_source;
use blob::{open_blob_source, read_blob_chunk, read_blob_metadata, upload_blob, write_blob_output};

/// Parses and runs one terminal-client invocation.
pub async fn run(
    arguments: impl IntoIterator<Item = OsString>,
    socket_environment: Option<OsString>,
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode {
    let parsed = match arguments::parse(arguments) {
        Ok(ParseOutcome::Help(help)) => {
            return if write!(stdout, "{help}").is_ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
        Ok(ParseOutcome::Run(arguments)) => arguments,
        Err(error) => {
            let _ = write!(stderr, "{error}");
            return ExitCode::from(2);
        }
    };
    let raw_output = parsed.raw_output;
    let result = execute(parsed, socket_environment, stdin, stdout, stderr).await;
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let mut output = Output::new(stdout, stderr, raw_output);
            let _ = output.error(&error);
            ExitCode::FAILURE
        }
    }
}

/// Parses and runs one invocation against the process terminal.
///
/// The interactive `chat` verb uses asynchronous standard-input lines and
/// catches terminal interrupts. Every other verb retains the one-shot standard
/// input and output path exposed by [`run`].
pub async fn run_terminal(
    arguments: impl IntoIterator<Item = OsString>,
    socket_environment: Option<OsString>,
) -> ExitCode {
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    let parsed = match arguments::parse(arguments.clone()) {
        Ok(ParseOutcome::Help(help)) => {
            return if write!(std::io::stdout().lock(), "{help}").is_ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
        Ok(ParseOutcome::Run(arguments)) => arguments,
        Err(error) => {
            let _ = write!(std::io::stderr().lock(), "{error}");
            return ExitCode::from(2);
        }
    };
    let Command::Chat { session_id } = parsed.command else {
        return run(
            arguments,
            socket_environment,
            &mut std::io::stdin().lock(),
            &mut std::io::stdout().lock(),
            &mut std::io::stderr().lock(),
        )
        .await;
    };
    let raw_output = parsed.raw_output;
    let result = async {
        let socket = socket_path(parsed.socket, socket_environment)?;
        let mut client = ProcessClient::new(socket);
        let mut stdout = std::io::stdout().lock();
        let mut stderr = std::io::stderr().lock();
        let mut output = Output::new(&mut stdout, &mut stderr, raw_output);
        let deployment_limits = read_deployment_limits(&mut client).await?;
        let input = chat::terminal_input(deployment_limits.terminal_input_channel_capacity)?;
        chat::run(
            &mut client,
            &mut output,
            session_id,
            input,
            deployment_limits,
        )
        .await
    }
    .await;
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let mut stdout = std::io::stdout().lock();
            let mut stderr = std::io::stderr().lock();
            let mut output = Output::new(&mut stdout, &mut stderr, raw_output);
            let _ = output.error(&error);
            ExitCode::FAILURE
        }
    }
}

async fn execute(
    arguments: arguments::Arguments,
    socket_environment: Option<OsString>,
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<(), ClientError> {
    let input = if matches!(
        arguments.command,
        Command::Send { .. }
            | Command::Steer { .. }
            | Command::Reconcile { .. }
            | Command::Stop { .. }
    ) {
        Some(read_input(stdin)?)
    } else {
        None
    };
    let prepared_import = match &arguments.command {
        Command::Import {
            source: ImportSourceArgument::File(path),
            ..
        } => Some(PreparedImport::File(open_import_source(path).await?)),
        Command::Import {
            source: ImportSourceArgument::Scan(path),
            ..
        } => Some(PreparedImport::Scan(collect_import_paths(path)?)),
        Command::BlobUpload { .. }
        | Command::BlobMetadata { .. }
        | Command::BlobRead { .. }
        | Command::Create { .. }
        | Command::Place { .. }
        | Command::Runner(_)
        | Command::Continue { .. }
        | Command::Compact { .. }
        | Command::Session(_)
        | Command::Goal(_)
        | Command::Credential(_)
        | Command::Imported { .. }
        | Command::Status
        | Command::List
        | Command::ReloadConfiguration { .. }
        | Command::Templates
        | Command::Search(_)
        | Command::Conversations(_)
        | Command::Send { .. }
        | Command::Steer { .. }
        | Command::Model { .. }
        | Command::Transcript { .. }
        | Command::Follow { .. }
        | Command::Chat { .. }
        | Command::Reconcile { .. }
        | Command::Review(_)
        | Command::Stop { .. }
        | Command::Approve { .. }
        | Command::Deny { .. } => None,
    };
    let prepared_blob = match &arguments.command {
        Command::BlobUpload { source } => Some(open_blob_source(source)?),
        Command::Create { .. }
        | Command::Place { .. }
        | Command::Runner(_)
        | Command::Continue { .. }
        | Command::Compact { .. }
        | Command::Session(_)
        | Command::Goal(_)
        | Command::Credential(_)
        | Command::Imported { .. }
        | Command::Status
        | Command::List
        | Command::ReloadConfiguration { .. }
        | Command::Templates
        | Command::Search(_)
        | Command::Conversations(_)
        | Command::Send { .. }
        | Command::Steer { .. }
        | Command::Model { .. }
        | Command::Transcript { .. }
        | Command::Follow { .. }
        | Command::Chat { .. }
        | Command::Reconcile { .. }
        | Command::Review(_)
        | Command::Stop { .. }
        | Command::Approve { .. }
        | Command::Deny { .. }
        | Command::BlobMetadata { .. }
        | Command::BlobRead { .. }
        | Command::Import { .. } => None,
    };
    let system_prompt_text = match &arguments.command {
        Command::Create {
            system_prompt_file: Some(path),
            ..
        }
        | Command::Model {
            system_prompt: SystemPromptArgument::File(path),
            ..
        } => Some(read_system_prompt_file(path).await?),
        Command::Create { .. }
        | Command::Place { .. }
        | Command::Runner(_)
        | Command::Compact { .. }
        | Command::Session(_)
        | Command::Goal(_)
        | Command::Credential(_)
        | Command::Status
        | Command::List
        | Command::ReloadConfiguration { .. }
        | Command::Templates
        | Command::Search(_)
        | Command::Conversations(_)
        | Command::Send { .. }
        | Command::Steer { .. }
        | Command::Reconcile { .. }
        | Command::Stop { .. }
        | Command::Approve { .. }
        | Command::Deny { .. }
        | Command::Model { .. }
        | Command::Transcript { .. }
        | Command::Follow { .. }
        | Command::Chat { .. }
        | Command::Continue { .. }
        | Command::Imported { .. }
        | Command::Review(_)
        | Command::Import { .. }
        | Command::BlobUpload { .. }
        | Command::BlobMetadata { .. }
        | Command::BlobRead { .. } => None,
    };
    let socket = socket_path(arguments.socket, socket_environment)?;
    let mut client = ProcessClient::new(socket);
    let mut output = Output::new(stdout, stderr, arguments.raw_output);
    let deployment_limits = if command_uses_deployment_limits(&arguments.command) {
        Some(read_deployment_limits(&mut client).await?)
    } else {
        None
    };
    if let Some(input) = input.as_deref() {
        validate_message_policy(input, deployment_limits)?;
    }
    if let Some(system_prompt) = system_prompt_text.as_ref() {
        validate_system_prompt_policy(system_prompt, deployment_limits)?;
    }
    match &arguments.command {
        Command::Search(page) => validate_metadata_page_policy(page.page_size, deployment_limits)?,
        Command::Conversations(page) => {
            validate_metadata_page_policy(page.page_size, deployment_limits)?
        }
        _ => {}
    }

    match arguments.command {
        Command::Runner(command) => runner::run(&mut client, &mut output, command).await,
        Command::Create {
            selection,
            template,
            command_id,
            system_prompt_file: _,
            placement,
        } => match (selection, template) {
            (Some(selection), None) => {
                create(
                    &mut client,
                    &mut output,
                    selection,
                    command_id,
                    system_prompt_text,
                    placement,
                )
                .await
            }
            (None, Some(template)) => {
                create_from_template(&mut client, &mut output, template, command_id, placement)
                    .await
            }
            _ => Err(ClientError::Protocol(
                "create source was internally invalid",
            )),
        },
        Command::Place {
            session_id,
            expected_placement_version,
            replacement,
            command_id,
        } => {
            update_session_placement(
                &mut client,
                &mut output,
                session_id,
                expected_placement_version,
                replacement,
                command_id,
            )
            .await
        }
        Command::Continue {
            imported_conversation_id,
            through_position,
            relationship,
            selection,
            command_id,
        } => {
            continue_imported(
                &mut client,
                &mut output,
                imported_conversation_id,
                through_position,
                relationship,
                selection,
                command_id,
            )
            .await
        }
        Command::Compact {
            session_id,
            through_position,
            command_id,
        } => {
            compact(
                &mut client,
                &mut output,
                session_id,
                through_position,
                command_id,
            )
            .await
        }
        Command::Imported {
            imported_conversation_id,
        } => imported(&mut client, &mut output, imported_conversation_id).await,
        Command::Session(command) => session_delegation(&mut client, &mut output, command).await,
        Command::Credential(command) => {
            credential::credential(&mut client, &mut output, command).await
        }
        Command::Goal(command) => goal(&mut client, &mut output, command).await,
        Command::Status => status(&mut client, &mut output).await,
        Command::List => list(&mut client, &mut output).await,
        Command::ReloadConfiguration { command_id } => {
            session::reload_configuration(&mut client, &mut output, command_id).await
        }
        Command::Templates => list_templates(&mut client, &mut output).await,
        Command::Search(page) => search(&mut client, &mut output, page).await,
        Command::Conversations(page) => conversations(&mut client, &mut output, page).await,
        Command::Send {
            session_id,
            command_id,
            defaults_version,
            delivery,
        } => {
            let input = input.ok_or(ClientError::Input("standard-input content was not read"))?;
            send(
                &mut client,
                &mut output,
                session_id,
                command_id,
                defaults_version,
                delivery,
                input,
            )
            .await
        }
        Command::Steer {
            session_id,
            command_id,
            turn_id,
        } => {
            let input = input.ok_or(ClientError::Input("standard-input content was not read"))?;
            steer(
                &mut client,
                &mut output,
                session_id,
                command_id,
                turn_id,
                input,
            )
            .await
        }
        Command::Model {
            session_id,
            selection,
            command_id,
            defaults_version,
            dangerous_tool_auto_approval,
            system_prompt,
        } => {
            let system_prompt = match system_prompt {
                SystemPromptArgument::Keep => ModelSystemPromptChoice::Keep,
                SystemPromptArgument::Clear => ModelSystemPromptChoice::Clear,
                SystemPromptArgument::File(_) => ModelSystemPromptChoice::Replace(
                    system_prompt_text
                        .ok_or(ClientError::Input("system prompt file was not read"))?,
                ),
            };
            replace_session_model(
                &mut client,
                &mut output,
                session_id,
                selection,
                command_id,
                defaults_version,
                dangerous_tool_auto_approval,
                system_prompt,
            )
            .await
        }
        Command::Transcript { session_id } => {
            let mut snapshot = transcript_command(&mut client, session_id).await?;
            output.snapshot(&mut snapshot)?;
            Ok(())
        }
        Command::Follow { session_id } => follow(&mut client, &mut output, session_id).await,
        Command::Chat { .. } => Err(ClientError::Input(
            "chat requires the process terminal input path",
        )),
        Command::Import { format, .. } => {
            match prepared_import.ok_or(ClientError::Input("import source was not prepared"))? {
                PreparedImport::File(file) => {
                    let outcome = import_conversation_file(&mut client, format, file).await?;
                    write_single_import_outcome(&mut output, outcome)
                }
                PreparedImport::Scan(scan) => {
                    scan_conversations(&mut client, &mut output, format, scan).await
                }
            }
        }
        Command::BlobUpload { .. } => {
            let source = prepared_blob.ok_or(ClientError::Input("blob source was not prepared"))?;
            upload_blob(&mut client, &mut output, source).await
        }
        Command::BlobMetadata { digest } => {
            read_blob_metadata(&mut client, &mut output, digest).await
        }
        Command::BlobRead {
            digest,
            offset_bytes,
            length_bytes,
            output,
        } => {
            let bytes = read_blob_chunk(&mut client, digest, offset_bytes, length_bytes).await?;
            write_blob_output(&output, &bytes).await
        }
        Command::Reconcile {
            session_id,
            turn_id,
            command_id,
            defaults_version,
        } => {
            let input = input.ok_or(ClientError::Input("standard-input content was not read"))?;
            reconcile(
                &mut client,
                &mut output,
                session_id,
                turn_id,
                command_id,
                defaults_version,
                input,
            )
            .await
        }
        Command::Review(command) => {
            review(&mut client, &mut output, *command, deployment_limits).await
        }
        Command::Stop {
            session_id,
            turn_id,
            command_id,
            defaults_version,
            descendants,
        } => {
            let input = input.ok_or(ClientError::Input("standard-input content was not read"))?;
            stop(
                &mut client,
                &mut output,
                session_id,
                turn_id,
                command_id,
                defaults_version,
                descendants,
                input,
            )
            .await
        }
        Command::Approve {
            session_id,
            tool_request_id,
            command_id,
        } => {
            decide(
                &mut client,
                &mut output,
                session_id,
                tool_request_id,
                command_id,
                ToolDecision::Approve {},
            )
            .await
        }
        Command::Deny {
            session_id,
            tool_request_id,
            reason,
            command_id,
        } => {
            decide(
                &mut client,
                &mut output,
                session_id,
                tool_request_id,
                command_id,
                ToolDecision::Deny { reason },
            )
            .await
        }
    }
}

fn socket_path(
    override_path: Option<PathBuf>,
    socket_environment: Option<OsString>,
) -> Result<PathBuf, ClientError> {
    let path = match override_path {
        Some(path) if !path.as_os_str().is_empty() => path,
        Some(_) => return Err(ClientError::Input("--socket requires a nonempty path")),
        None => {
            let value = socket_environment.ok_or(ClientError::Input(
                "set SIGNALBOX_SOCKET_PATH or pass --socket",
            ))?;
            if value.is_empty() {
                return Err(ClientError::Input(
                    "set SIGNALBOX_SOCKET_PATH or pass --socket",
                ));
            }
            PathBuf::from(value)
        }
    };
    if !path.is_absolute() {
        return Err(ClientError::Input(
            "the local process socket path must be absolute",
        ));
    }
    Ok(path)
}

fn read_input(stdin: &mut dyn Read) -> Result<String, ClientError> {
    let mut bytes = Vec::new();
    stdin
        .take((MAX_INPUT_CONTENT_FRAME_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.is_empty() {
        return Err(ClientError::Input(
            "standard-input content must not be empty",
        ));
    }
    if bytes.len() > MAX_INPUT_CONTENT_FRAME_BYTES {
        return Err(ClientError::Input(
            "standard-input content exceeds the wire-frame UTF-8 byte guard",
        ));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| ClientError::Input("standard-input content must be valid UTF-8"))?;
    if text.contains('\0') {
        return Err(ClientError::Input(
            "standard-input content must not contain U+0000",
        ));
    }
    Ok(text)
}

fn command_identity(supplied: Option<CommandId>) -> Result<(CommandId, bool), ClientError> {
    match supplied {
        Some(command_id) => Ok((command_id, false)),
        None => CommandId::try_from_uuid(Uuid::now_v7())
            .map(|command_id| (command_id, true))
            .map_err(|_| ClientError::Protocol("UUIDv7 generator produced a reserved value")),
    }
}

fn selection_display(selection: ModelSelection) -> String {
    match selection {
        ModelSelection::Direct { selection_id } => format!("model={selection_id}"),
        ModelSelection::Alias { alias_id } => format!("alias={alias_id}"),
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
