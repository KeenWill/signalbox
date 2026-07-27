//! Terminal client for the closed local Signalbox process protocol.

use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    os::unix::ffi::OsStrExt as _,
    path::{Path, PathBuf},
    process::ExitCode,
};

use arguments::{
    Command, DangerousToolAutoApprovalArgument, ImportSourceArgument, ParseOutcome, ReviewCommand,
};
use connection::ProcessClient;
use error::ClientError;
use presentation::{Output, SessionMetadataRow, SnapshotSelection};
use rustix::{
    fd::OwnedFd,
    fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags, fstat, openat, statat},
};
use signalbox_process_protocol::{
    CanonicalU64, CanonicalUuid, ClientRequest, CommandId, ConversationImportFormat,
    ConversationImportSource, ErrorCode, InputContent, MAX_FRAME_BYTES, ModelCallDisposition,
    ModelCallState, ModelSelection, ReviewPassSnapshot, ReviewRunSnapshot, ServerFrame,
    ServerMessage, SessionEvent, ToolBatchState, ToolDecision, TurnState, decode_server_line,
    encode_server_line,
};
use tokio::io::AsyncReadExt as _;
use transcript::{SnapshotIdentitySet, SnapshotRecord, TranscriptSnapshot, read_snapshot};
use uuid::Uuid;

mod arguments;
mod connection;
mod error;
mod presentation;
mod transcript;

const MAX_INPUT_CONTENT_BYTES: usize = 1_048_576;
const MAX_POSSIBLY_FRAMED_IMPORT_SOURCE_BYTES: usize = MAX_FRAME_BYTES / 4 * 3;
/// Smallest bounded metadata page the process protocol admits.
const MIN_METADATA_PAGE_SIZE: u64 = 1;
/// Largest bounded metadata page the process protocol admits.
const MAX_METADATA_PAGE_SIZE: u64 = 100;
/// Largest finding inventory one review run can own.
const MAX_REVIEW_FINDINGS_PER_RUN: u64 = 32;

/// One complete bounded `list_session_metadata` request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionMetadataPageRequest {
    pub(crate) required_tags: Vec<String>,
    pub(crate) title_contains: Option<String>,
    pub(crate) include_archived: bool,
    pub(crate) page_size: CanonicalU64,
    pub(crate) after_session_id: Option<CanonicalUuid>,
}

impl SessionMetadataPageRequest {
    fn request(&self) -> ClientRequest {
        ClientRequest::ListSessionMetadata {
            required_tags: self.required_tags.clone(),
            title_contains: self.title_contains.clone(),
            include_archived: self.include_archived,
            page_size: self.page_size,
            after_session_id: self.after_session_id,
        }
    }
}

enum PreparedImport {
    File(Vec<u8>),
    Scan(PreparedImportScan),
}

struct PreparedImportScan {
    root: OwnedFd,
    paths: Vec<ScannedImportPath>,
}

struct ScannedImportPath {
    relative: PathBuf,
    display: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConversationImportOutcome {
    Inserted(CanonicalUuid),
    AlreadyImported(CanonicalUuid),
}

#[derive(Default)]
pub(crate) struct ImportScanSummary {
    pub(crate) imported: usize,
    pub(crate) already_imported: usize,
    pub(crate) skipped: usize,
}

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

async fn execute(
    arguments: arguments::Arguments,
    socket_environment: Option<OsString>,
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<(), ClientError> {
    let input = if matches!(
        arguments.command,
        Command::Send { .. } | Command::Reconcile { .. } | Command::Stop { .. }
    ) {
        Some(read_input(stdin)?)
    } else {
        None
    };
    let prepared_import = match &arguments.command {
        Command::Import {
            source: ImportSourceArgument::File(path),
            ..
        } => Some(PreparedImport::File(read_import_source(path).await?)),
        Command::Import {
            source: ImportSourceArgument::Scan(path),
            ..
        } => Some(PreparedImport::Scan(collect_import_paths(path)?)),
        Command::Create { .. }
        | Command::Continue { .. }
        | Command::List
        | Command::Search(_)
        | Command::Send { .. }
        | Command::Model { .. }
        | Command::Transcript { .. }
        | Command::Follow { .. }
        | Command::Reconcile { .. }
        | Command::Review(_)
        | Command::Stop { .. }
        | Command::Approve { .. }
        | Command::Deny { .. } => None,
    };
    let socket = socket_path(arguments.socket, socket_environment)?;
    let mut client = ProcessClient::new(socket);
    let mut output = Output::new(stdout, stderr, arguments.raw_output);

    match arguments.command {
        Command::Create {
            selection,
            command_id,
        } => create(&mut client, &mut output, selection, command_id).await,
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
        Command::List => list(&mut client, &mut output).await,
        Command::Search(page) => search(&mut client, &mut output, page).await,
        Command::Send {
            session_id,
            command_id,
            defaults_version,
        } => {
            let input = input.ok_or(ClientError::Input("standard-input content was not read"))?;
            send(
                &mut client,
                &mut output,
                session_id,
                command_id,
                defaults_version,
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
        } => {
            replace_session_model(
                &mut client,
                &mut output,
                session_id,
                selection,
                command_id,
                defaults_version,
                dangerous_tool_auto_approval,
            )
            .await
        }
        Command::Transcript { session_id } => {
            let mut snapshot = transcript(&mut client, session_id).await?;
            output.snapshot(&mut snapshot)?;
            Ok(())
        }
        Command::Follow { session_id } => follow(&mut client, &mut output, session_id).await,
        Command::Import { format, .. } => {
            match prepared_import.ok_or(ClientError::Input("import source was not prepared"))? {
                PreparedImport::File(source) => {
                    let outcome = import_conversation(&mut client, format, source).await?;
                    write_single_import_outcome(&mut output, outcome)
                }
                PreparedImport::Scan(scan) => {
                    scan_conversations(&mut client, &mut output, format, scan).await
                }
            }
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
        Command::Review(command) => review(&mut client, &mut output, *command).await,
        Command::Stop {
            session_id,
            turn_id,
            command_id,
            defaults_version,
        } => {
            let input = input.ok_or(ClientError::Input("standard-input content was not read"))?;
            stop(
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

async fn read_import_source(path: &Path) -> Result<Vec<u8>, ClientError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(ClientError::source_file)?;
    read_import_file(file).await
}

async fn read_import_file(file: tokio::fs::File) -> Result<Vec<u8>, ClientError> {
    let read_limit = MAX_POSSIBLY_FRAMED_IMPORT_SOURCE_BYTES
        .checked_add(1)
        .ok_or(ClientError::Protocol("import read bound overflow"))?;
    let read_limit = u64::try_from(read_limit)
        .map_err(|_| ClientError::Protocol("import read bound is not representable"))?;
    let mut bounded = file.take(read_limit);
    let mut source = Vec::new();
    bounded
        .read_to_end(&mut source)
        .await
        .map_err(ClientError::source_file)?;
    if source.len() > MAX_POSSIBLY_FRAMED_IMPORT_SOURCE_BYTES {
        return Err(ClientError::SourceExceedsFrame);
    }
    Ok(source)
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
        .take((MAX_INPUT_CONTENT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.is_empty() {
        return Err(ClientError::Input(
            "standard-input content must not be empty",
        ));
    }
    if bytes.len() > MAX_INPUT_CONTENT_BYTES {
        return Err(ClientError::Input(
            "standard-input content exceeds the 1 MiB UTF-8 byte limit",
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

async fn create(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    selection: ModelSelection,
    command_id: Option<CommandId>,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let mut connection = client
        .mutation_request(ClientRequest::CreateSession {
            command_id,
            initial_model_selection: selection,
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::SessionCreated { session_id } => {
            output.session_created(session_id)?;
            Ok(())
        }
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("create returned an unexpected response").mutation()),
    }
}

async fn continue_imported(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    imported_conversation_id: CanonicalUuid,
    through_position: CanonicalU64,
    relationship: signalbox_process_protocol::ImportedSessionRelationship,
    selection: ModelSelection,
    command_id: Option<CommandId>,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let mut connection = client
        .mutation_request(ClientRequest::CreateSessionFromImportedFrontier {
            command_id,
            imported_conversation_id,
            through_position,
            relationship,
            initial_model_selection: selection,
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::SessionCreated { session_id } => {
            output.session_created(session_id)?;
            Ok(())
        }
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("continue returned an unexpected response").mutation()),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ObservedSessionDefaults {
    version: CanonicalU64,
    dangerous_tool_auto_approval: bool,
}

async fn replace_session_model(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    selection: ModelSelection,
    command_id: Option<CommandId>,
    defaults_version: Option<CanonicalU64>,
    dangerous_tool_auto_approval: Option<DangerousToolAutoApprovalArgument>,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let observed = match (defaults_version, dangerous_tool_auto_approval) {
        (Some(version), Some(posture)) => ObservedSessionDefaults {
            version,
            dangerous_tool_auto_approval: matches!(
                posture,
                DangerousToolAutoApprovalArgument::ApproveAll
            ),
        },
        (None, None) => observe_session_defaults(client, session_id).await?,
        (Some(_), None) | (None, Some(_)) => {
            return Err(ClientError::Input(
                "model recovery requires the complete printed defaults facts",
            ));
        }
    };
    output.recovery_value("defaults_version", &observed.version.value().to_string())?;
    output.recovery_value(
        "dangerous_tool_auto_approval",
        if observed.dangerous_tool_auto_approval {
            "approve-all"
        } else {
            "disabled"
        },
    )?;

    let mut connection = client
        .mutation_request(ClientRequest::ReplaceSessionDefaults {
            command_id,
            session_id,
            expected_defaults_version: observed.version,
            model_selection: selection,
            dangerous_tool_auto_approval: observed.dangerous_tool_auto_approval,
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::SessionDefaultsReplaced {
            session_id: replaced_session,
            defaults_version: installed_version,
            model_selection,
            dangerous_tool_auto_approval,
        } if replaced_session == session_id
            && model_selection == selection
            && dangerous_tool_auto_approval == observed.dangerous_tool_auto_approval
            && observed
                .version
                .value()
                .checked_add(1)
                .is_some_and(|expected| installed_version.value() == expected) =>
        {
            output.session_defaults_replaced(
                replaced_session,
                installed_version.value(),
                &selection_display(model_selection),
            )?;
            Ok(())
        }
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(
            ClientError::Protocol("model replacement returned an unexpected response").mutation(),
        ),
    }
}

async fn observe_session_defaults(
    client: &mut ProcessClient,
    selected_session: CanonicalUuid,
) -> Result<ObservedSessionDefaults, ClientError> {
    let mut page = SessionMetadataPageRequest {
        required_tags: Vec::new(),
        title_contains: None,
        include_archived: true,
        page_size: CanonicalU64::new(MAX_METADATA_PAGE_SIZE),
        after_session_id: None,
    };
    loop {
        let mut selected = None;
        let next_after_session_id = read_session_metadata_page(client, &page, |frame| {
            if let ServerMessage::SessionMetadataSummary {
                session_id,
                defaults_version,
                dangerous_tool_auto_approval,
                ..
            } = frame.message()
                && *session_id == selected_session
            {
                selected = Some(ObservedSessionDefaults {
                    version: *defaults_version,
                    dangerous_tool_auto_approval: *dangerous_tool_auto_approval,
                });
            }
            Ok(())
        })
        .await?;
        if let Some(selected) = selected {
            return Ok(selected);
        }
        let Some(next) = next_after_session_id else {
            return Err(ClientError::Input("the selected session was not listed"));
        };
        page.after_session_id = Some(next);
    }
}

/// Reads and validates exactly one bounded metadata page, presenting each
/// summary frame to `consume` and returning the page's continuation cursor.
async fn read_session_metadata_page(
    client: &mut ProcessClient,
    page: &SessionMetadataPageRequest,
    mut consume: impl FnMut(&ServerFrame) -> Result<(), ClientError>,
) -> Result<Option<CanonicalUuid>, ClientError> {
    let mut connection = client.request(page.request()).await?;
    match connection.message().await? {
        ServerMessage::SessionMetadataPageStart {} => {}
        ServerMessage::Error {
            code,
            message,
            detail,
        } => return Err(ClientError::remote(code, message, detail)),
        _ => {
            return Err(ClientError::Protocol(
                "session metadata page did not begin with its start frame",
            ));
        }
    }
    let mut prior_session = page.after_session_id;
    let mut last_in_page = None;
    let mut summary_count = 0_u64;
    loop {
        let frame = connection.frame().await?;
        match frame.message() {
            ServerMessage::SessionMetadataSummary { session_id, .. } => {
                if prior_session
                    .is_some_and(|prior: CanonicalUuid| prior.into_uuid() >= session_id.into_uuid())
                {
                    return Err(ClientError::Protocol(
                        "session metadata summaries were not strictly ordered",
                    ));
                }
                summary_count = summary_count.checked_add(1).ok_or(ClientError::Protocol(
                    "session metadata summary count overflowed",
                ))?;
                if summary_count > page.page_size.value() {
                    return Err(ClientError::Protocol(
                        "session metadata page exceeded its requested bound",
                    ));
                }
                prior_session = Some(*session_id);
                last_in_page = Some(*session_id);
                consume(&frame)?;
            }
            ServerMessage::SessionMetadataPageEnd {
                session_count,
                next_after_session_id,
            } => {
                if session_count.value() != summary_count
                    || next_after_session_id.is_some() && *next_after_session_id != last_in_page
                {
                    return Err(ClientError::Protocol(
                        "session metadata page count or cursor was invalid",
                    ));
                }
                return Ok(*next_after_session_id);
            }
            ServerMessage::Error {
                code,
                message,
                detail,
            } => return Err(ClientError::remote(*code, message.clone(), *detail)),
            _ => {
                return Err(ClientError::Protocol(
                    "session metadata page sequence or count was invalid",
                ));
            }
        }
    }
}

fn collect_import_paths(root: &Path) -> Result<PreparedImportScan, ClientError> {
    let root_metadata = std::fs::symlink_metadata(root).map_err(ClientError::scan_directory)?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(ClientError::Input("--scan requires a directory"));
    }

    let root_fd = openat(
        CWD,
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)
    .map_err(ClientError::scan_directory)?;
    let root_directory = Dir::read_from(&root_fd)
        .map_err(std::io::Error::from)
        .map_err(ClientError::scan_directory)?;
    let mut pending = vec![(PathBuf::new(), root_directory)];
    let mut paths = Vec::new();
    while let Some((relative_directory, directory)) = pending.last_mut() {
        let Some(entry) = directory.read() else {
            pending.pop();
            continue;
        };
        let entry = entry
            .map_err(std::io::Error::from)
            .map_err(ClientError::scan_directory)?;
        let name_bytes = entry.file_name().to_bytes();
        if name_bytes == b"." || name_bytes == b".." {
            continue;
        }
        let name = OsStr::from_bytes(name_bytes);
        let relative = relative_directory.join(name);
        let descriptor = directory
            .fd()
            .map_err(std::io::Error::from)
            .map_err(ClientError::scan_directory)?;
        let status = statat(descriptor, name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(std::io::Error::from)
            .map_err(ClientError::scan_directory)?;
        match FileType::from_raw_mode(status.st_mode) {
            FileType::Directory => {
                let child = openat(
                    descriptor,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(std::io::Error::from)
                .map_err(ClientError::scan_directory)?;
                let child = Dir::new(child)
                    .map_err(std::io::Error::from)
                    .map_err(ClientError::scan_directory)?;
                pending.push((relative, child));
            }
            FileType::RegularFile if relative.extension() == Some(OsStr::new("jsonl")) => {
                paths.push(ScannedImportPath {
                    display: root.join(&relative),
                    relative,
                });
            }
            FileType::RegularFile
            | FileType::Symlink
            | FileType::Fifo
            | FileType::Socket
            | FileType::CharacterDevice
            | FileType::BlockDevice
            | FileType::Unknown => {}
        }
    }
    paths.sort_by(|left, right| left.display.cmp(&right.display));
    Ok(PreparedImportScan {
        root: root_fd,
        paths,
    })
}

fn open_scanned_import_source(
    root: &OwnedFd,
    relative: &Path,
) -> Result<tokio::fs::File, ClientError> {
    let mut components = relative.components().peekable();
    let mut current = None;
    while let Some(component) = components.next() {
        let std::path::Component::Normal(name) = component else {
            return Err(ClientError::Protocol(
                "scan produced a non-relative candidate path",
            ));
        };
        let parent = current.as_ref().unwrap_or(root);
        let flags = if components.peek().is_some() {
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
        } else {
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC
        };
        current = Some(
            openat(parent, name, flags, Mode::empty())
                .map_err(std::io::Error::from)
                .map_err(ClientError::source_file)?,
        );
    }
    let descriptor = current.ok_or(ClientError::Protocol(
        "scan produced an empty candidate path",
    ))?;
    let status = fstat(&descriptor)
        .map_err(std::io::Error::from)
        .map_err(ClientError::source_file)?;
    if FileType::from_raw_mode(status.st_mode) != FileType::RegularFile {
        return Err(ClientError::source_file(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "scan candidate is no longer a regular file",
        )));
    }
    Ok(tokio::fs::File::from_std(File::from(descriptor)))
}

async fn import_conversation(
    client: &mut ProcessClient,
    format: ConversationImportFormat,
    source: Vec<u8>,
) -> Result<ConversationImportOutcome, ClientError> {
    let mut connection = client
        .mutation_request(ClientRequest::ImportConversation {
            format,
            source: ConversationImportSource::new(source),
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::ConversationImportInserted {
            imported_conversation_id,
        } => Ok(ConversationImportOutcome::Inserted(
            imported_conversation_id,
        )),
        ServerMessage::ConversationImportAlreadyImported {
            imported_conversation_id,
        } => Ok(ConversationImportOutcome::AlreadyImported(
            imported_conversation_id,
        )),
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("import returned an unexpected response").mutation()),
    }
}

fn write_single_import_outcome(
    output: &mut Output<'_>,
    outcome: ConversationImportOutcome,
) -> Result<(), ClientError> {
    match outcome {
        ConversationImportOutcome::Inserted(imported_conversation_id) => {
            output.conversation_import_inserted(imported_conversation_id)?;
        }
        ConversationImportOutcome::AlreadyImported(imported_conversation_id) => {
            output.conversation_import_already_imported(imported_conversation_id)?;
        }
    }
    Ok(())
}

async fn scan_conversations(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    format: ConversationImportFormat,
    scan: PreparedImportScan,
) -> Result<(), ClientError> {
    let mut summary = ImportScanSummary::default();
    for path in scan.paths {
        let outcome = match open_scanned_import_source(&scan.root, &path.relative) {
            Ok(file) => read_import_file(file).await,
            Err(error) => Err(error),
        };
        let outcome = match outcome {
            Ok(source) => import_conversation(client, format, source).await,
            Err(error) => Err(error),
        };
        match outcome {
            Ok(ConversationImportOutcome::Inserted(imported_conversation_id)) => {
                summary.imported += 1;
                output
                    .conversation_import_scan_inserted(&path.display, imported_conversation_id)?;
            }
            Ok(ConversationImportOutcome::AlreadyImported(imported_conversation_id)) => {
                summary.already_imported += 1;
                output.conversation_import_scan_already_imported(
                    &path.display,
                    imported_conversation_id,
                )?;
            }
            Err(error) => {
                summary.skipped += 1;
                output.conversation_import_scan_skipped(&path.display, &error)?;
            }
        }
    }
    output.conversation_import_scan_summary(&summary)?;
    if summary.skipped == 0 {
        Ok(())
    } else {
        Err(ClientError::ScanIncomplete {
            skipped_files: summary.skipped,
        })
    }
}

async fn list(client: &mut ProcessClient, output: &mut Output<'_>) -> Result<(), ClientError> {
    let mut spool = tempfile::tempfile()?;
    read_session_summaries(client, |_, frame| {
        spool.write_all(&encode_server_line(frame)?)?;
        Ok(())
    })
    .await?;
    spool.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(spool);
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? != 0 {
        match decode_server_line(&line)?.message() {
            ServerMessage::SessionSummary {
                session_id,
                defaults_version,
                model_selection,
            } => output.session_summary(
                *session_id,
                defaults_version.value(),
                &selection_display(*model_selection),
            )?,
            _ => {
                return Err(ClientError::Protocol(
                    "session-summary spool contained a non-summary frame",
                ));
            }
        }
        line.clear();
    }
    Ok(())
}

async fn search(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    page: SessionMetadataPageRequest,
) -> Result<(), ClientError> {
    let mut spool = tempfile::tempfile()?;
    let next_after_session_id = read_session_metadata_page(client, &page, |frame| {
        spool.write_all(&encode_server_line(frame)?)?;
        Ok(())
    })
    .await?;
    spool.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(spool);
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? != 0 {
        match decode_server_line(&line)?.message() {
            ServerMessage::SessionMetadataSummary {
                session_id,
                defaults_version,
                model_selection,
                dangerous_tool_auto_approval,
                title,
                tags,
                archived,
                last_writer,
            } => output.session_metadata_summary(&SessionMetadataRow {
                session_id: *session_id,
                defaults_version: defaults_version.value(),
                selection: &selection_display(*model_selection),
                dangerous_tool_auto_approval: *dangerous_tool_auto_approval,
                archived: *archived,
                last_writer: *last_writer,
                tags,
                title: title.as_deref(),
            })?,
            _ => {
                return Err(ClientError::Protocol(
                    "session-metadata spool contained a non-summary frame",
                ));
            }
        }
        line.clear();
    }
    if let Some(next_after_session_id) = next_after_session_id {
        output.next_page_cursor(next_after_session_id)?;
    }
    Ok(())
}

async fn send(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    command_id: Option<CommandId>,
    defaults_version: Option<CanonicalU64>,
    content: String,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let defaults_version =
        resolve_defaults_version(client, output, session_id, defaults_version).await?;

    let turn_id = submit_input(
        client,
        command_id,
        session_id,
        InputContent::new(content),
        defaults_version,
    )
    .await?;

    await_and_report_turn(client, output, session_id, turn_id).await
}

/// Supplies the owner reconciliation decision a turn parked on an ambiguous
/// model call requires, then continues the session with the given content.
///
/// The parked turn terminalizes as reconciliation-required — its ambiguity is
/// recorded, never resolved into a fabricated outcome — and the content becomes
/// the immediate successor turn this verb then follows to its own terminal.
async fn reconcile(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
    command_id: Option<CommandId>,
    defaults_version: Option<CanonicalU64>,
    content: String,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let defaults_version =
        resolve_defaults_version(client, output, session_id, defaults_version).await?;

    let successor_turn_id = reconcile_turn(
        client,
        command_id,
        session_id,
        turn_id,
        InputContent::new(content),
        defaults_version,
    )
    .await?;

    await_and_report_turn(client, output, session_id, successor_turn_id).await
}

/// Requests cancellation of the exact active turn through the interrupt
/// treatment, then continues the session with the given content.
///
/// The stopped turn terminalizes through the existing lifecycle — a prepared
/// call cancels directly, an issued call first enters its durable
/// cancellation-requested state — and the content becomes the
/// immediate-successor turn this verb then follows to its own terminal.
async fn stop(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    turn_id: Option<CanonicalUuid>,
    command_id: Option<CommandId>,
    defaults_version: Option<CanonicalU64>,
    content: String,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let expected_active_turn = match turn_id {
        Some(turn_id) => turn_id,
        None => observe_active_turn(client, session_id).await?,
    };
    output.recovery_value("turn", &expected_active_turn.to_string())?;
    let defaults_version =
        resolve_defaults_version(client, output, session_id, defaults_version).await?;

    let successor_turn_id = stop_turn(
        client,
        command_id,
        session_id,
        expected_active_turn,
        InputContent::new(content),
        defaults_version,
    )
    .await?;

    await_and_report_turn(client, output, session_id, successor_turn_id).await
}

/// Reads the authoritative transcript and returns the single turn holding the
/// session's active slot.
async fn observe_active_turn(
    client: &mut ProcessClient,
    session_id: CanonicalUuid,
) -> Result<CanonicalUuid, ClientError> {
    let mut snapshot = transcript(client, session_id).await?;
    snapshot
        .active_turn()?
        .ok_or(ClientError::Input("the session has no active turn to stop"))
}

async fn resolve_defaults_version(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    defaults_version: Option<CanonicalU64>,
) -> Result<CanonicalU64, ClientError> {
    let defaults_version = match defaults_version {
        Some(version) => version,
        None => {
            let mut selected = None;
            read_session_summaries(client, |summary, _| {
                if summary.session_id == session_id {
                    selected = Some(CanonicalU64::new(summary.defaults_version));
                }
                Ok(())
            })
            .await?;
            selected.ok_or(ClientError::Input("the selected session was not listed"))?
        }
    };
    output.recovery_value("defaults_version", &defaults_version.value().to_string())?;
    Ok(defaults_version)
}

async fn await_and_report_turn(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
) -> Result<(), ClientError> {
    match await_turn_terminal(client, session_id, turn_id).await? {
        TurnTerminal::Completed => {
            let mut snapshot = transcript(client, session_id).await?;
            let state = snapshot.turn_state(turn_id)?;
            if !matches!(state.as_ref(), Some(TurnState::Completed { .. })) {
                return Err(ClientError::Protocol(
                    "terminal reread did not retain completed turn state",
                ));
            }
            write_assistant_texts(&mut snapshot, output, turn_id)?;
            Ok(())
        }
        TurnTerminal::Failed => Err(ClientError::TurnFailed),
        TurnTerminal::Refused => Err(ClientError::TurnRefused),
        TurnTerminal::Cancelled => Err(ClientError::TurnCancelled),
        TurnTerminal::ReconciliationRequired => Err(ClientError::TurnReconciliationRequired),
    }
}

/// Supplies one owner decision for a pending tool request and validates the
/// exact recorded receipt.
async fn decide(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    tool_request_id: CanonicalUuid,
    command_id: Option<CommandId>,
    decision: ToolDecision,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let mut connection = client
        .mutation_request(ClientRequest::DecideToolRequest {
            command_id,
            session_id,
            tool_request_id,
            decision: decision.clone(),
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::ToolRequestDecided {
            tool_request_id: decided_request,
            decision: recorded_decision,
        } if decided_request == tool_request_id && recorded_decision == decision => {
            output.tool_request_decided(tool_request_id, &decision)?;
            Ok(())
        }
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("decision returned an unexpected receipt").mutation()),
    }
}

async fn submit_input(
    client: &mut ProcessClient,
    command_id: CommandId,
    session_id: CanonicalUuid,
    content: InputContent,
    defaults_version: CanonicalU64,
) -> Result<CanonicalUuid, ClientError> {
    let mut connection = client
        .mutation_request(ClientRequest::SubmitInput {
            command_id,
            session_id,
            content,
            expected_defaults_version: defaults_version,
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::InputSubmitted {
            session_id: submitted_session,
            turn_id,
            ..
        } if submitted_session == session_id => Ok(turn_id),
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("submit returned an unexpected response").mutation()),
    }
}

async fn reconcile_turn(
    client: &mut ProcessClient,
    command_id: CommandId,
    session_id: CanonicalUuid,
    expected_active_turn_id: CanonicalUuid,
    content: InputContent,
    defaults_version: CanonicalU64,
) -> Result<CanonicalUuid, ClientError> {
    let mut connection = client
        .mutation_request(ClientRequest::ReconcileTurn {
            command_id,
            session_id,
            expected_active_turn_id,
            content,
            expected_defaults_version: defaults_version,
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::InputSubmitted {
            session_id: submitted_session,
            turn_id,
            ..
        } if submitted_session == session_id => Ok(turn_id),
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("reconcile returned an unexpected response").mutation()),
    }
}

async fn stop_turn(
    client: &mut ProcessClient,
    command_id: CommandId,
    session_id: CanonicalUuid,
    expected_active_turn_id: CanonicalUuid,
    content: InputContent,
    defaults_version: CanonicalU64,
) -> Result<CanonicalUuid, ClientError> {
    let mut connection = client
        .mutation_request(ClientRequest::StopTurn {
            command_id,
            session_id,
            expected_active_turn_id,
            content,
            expected_defaults_version: defaults_version,
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::InputSubmitted {
            session_id: submitted_session,
            turn_id,
            ..
        } if submitted_session == session_id => Ok(turn_id),
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("stop returned an unexpected response").mutation()),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TurnTerminal {
    Completed,
    Failed,
    Refused,
    Cancelled,
    ReconciliationRequired,
}

async fn await_turn_terminal(
    client: &mut ProcessClient,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
) -> Result<TurnTerminal, ClientError> {
    loop {
        let mut connection = client
            .request(ClientRequest::FollowSession { session_id })
            .await?;
        let mut snapshot = read_snapshot(&mut connection, session_id).await?;
        let state = snapshot.turn_state(turn_id)?;
        if let Some(terminal) = terminal_snapshot_state(state.as_ref())? {
            return Ok(terminal);
        }
        let mut observed_cursor = snapshot.cursor();
        loop {
            match connection.message().await? {
                ServerMessage::SessionEvent {
                    cursor,
                    session_id: event_session,
                    event,
                } if event_session == session_id => {
                    if cursor.value() <= observed_cursor {
                        continue;
                    }
                    observed_cursor = cursor.value();
                    if let Some(terminal) = terminal_event_state(&event, turn_id) {
                        return Ok(terminal);
                    }
                    if model_call_recovery_transition(&event, turn_id)
                        || tool_recovery_transition(&event, turn_id)
                    {
                        let mut refreshed = transcript(client, session_id).await?;
                        let refreshed_state = refreshed.turn_state(turn_id)?;
                        let Some(terminal) = terminal_snapshot_state(refreshed_state.as_ref())?
                        else {
                            return Err(ClientError::Protocol(
                                "an ambiguous model call did not produce recovery or terminal state",
                            ));
                        };
                        return Ok(terminal);
                    }
                }
                ServerMessage::Error {
                    code: ErrorCode::ResyncRequired,
                    ..
                } => break,
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => return Err(ClientError::remote(code, message, detail)),
                _ => {
                    return Err(ClientError::Protocol(
                        "follow returned an unexpected response",
                    ));
                }
            }
        }
    }
}

fn tool_recovery_transition(event: &SessionEvent, selected_turn: CanonicalUuid) -> bool {
    matches!(
        event,
        SessionEvent::ToolBatchTransition {
            turn_id,
            state: ToolBatchState::RecoveryRequired { .. },
            ..
        } if *turn_id == selected_turn
    )
}

fn model_call_recovery_transition(event: &SessionEvent, selected_turn: CanonicalUuid) -> bool {
    matches!(
        event,
        SessionEvent::ModelCallTransition {
            turn_id,
            state: ModelCallState::Terminal {
                disposition: ModelCallDisposition::Ambiguous,
            },
            ..
        } if *turn_id == selected_turn
    )
}

fn terminal_snapshot_state(state: Option<&TurnState>) -> Result<Option<TurnTerminal>, ClientError> {
    match state {
        Some(TurnState::Completed { .. }) => Ok(Some(TurnTerminal::Completed)),
        Some(TurnState::Failed { .. }) => Ok(Some(TurnTerminal::Failed)),
        Some(TurnState::Refused { .. }) => Ok(Some(TurnTerminal::Refused)),
        Some(TurnState::Cancelled { .. }) => Ok(Some(TurnTerminal::Cancelled)),
        Some(
            TurnState::ReconciliationRequired { .. } | TurnState::ToolReconciliationRequired { .. },
        ) => Ok(Some(TurnTerminal::ReconciliationRequired)),
        Some(
            TurnState::Queued { .. }
            | TurnState::ActiveRunning { .. }
            | TurnState::ActiveAwaitingToolApproval { .. },
        ) => Ok(None),
        Some(
            TurnState::ActiveAwaitingModelCallRecovery { .. }
            | TurnState::ActiveAwaitingToolRecovery { .. },
        ) => Err(ClientError::TurnRecoveryRequired),
        None => Err(ClientError::Protocol(
            "follow snapshot omitted the submitted turn",
        )),
    }
}

fn terminal_event_state(
    event: &SessionEvent,
    selected_turn: CanonicalUuid,
) -> Option<TurnTerminal> {
    match event {
        SessionEvent::TurnCompleted { turn_id, .. } if *turn_id == selected_turn => {
            Some(TurnTerminal::Completed)
        }
        SessionEvent::TurnFailed { turn_id, .. } if *turn_id == selected_turn => {
            Some(TurnTerminal::Failed)
        }
        SessionEvent::TurnRefused { turn_id, .. } if *turn_id == selected_turn => {
            Some(TurnTerminal::Refused)
        }
        SessionEvent::TurnCancelled { turn_id, .. } if *turn_id == selected_turn => {
            Some(TurnTerminal::Cancelled)
        }
        SessionEvent::TurnReconciliationRequired { turn_id, .. } if *turn_id == selected_turn => {
            Some(TurnTerminal::ReconciliationRequired)
        }
        SessionEvent::TurnToolReconciliationRequired { turn_id, .. }
            if *turn_id == selected_turn =>
        {
            Some(TurnTerminal::ReconciliationRequired)
        }
        SessionEvent::SessionCreated {}
        | SessionEvent::InputAccepted { .. }
        | SessionEvent::TurnActivated { .. }
        | SessionEvent::ModelCallTransition { .. }
        | SessionEvent::ToolBatchTransition { .. }
        | SessionEvent::TurnCompleted { .. }
        | SessionEvent::TurnFailed { .. }
        | SessionEvent::TurnRefused { .. }
        | SessionEvent::TurnCancelled { .. }
        | SessionEvent::TurnReconciliationRequired { .. }
        | SessionEvent::TurnToolReconciliationRequired { .. } => None,
    }
}

async fn transcript(
    client: &mut ProcessClient,
    session_id: CanonicalUuid,
) -> Result<TranscriptSnapshot, ClientError> {
    let mut connection = client
        .request(ClientRequest::ReadTranscript { session_id })
        .await?;
    read_snapshot(&mut connection, session_id).await
}

async fn follow(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
) -> Result<(), ClientError> {
    let mut displayed_entries = SnapshotIdentitySet::new()?;
    loop {
        let mut connection = client
            .request(ClientRequest::FollowSession { session_id })
            .await?;
        let mut snapshot = read_snapshot(&mut connection, session_id).await?;
        output.followed_snapshot(&mut snapshot, &mut displayed_entries)?;
        let mut observed_cursor = snapshot.cursor();
        loop {
            match connection.message().await? {
                ServerMessage::SessionEvent {
                    cursor,
                    session_id: event_session,
                    event,
                } if event_session == session_id => {
                    if cursor.value() <= observed_cursor {
                        continue;
                    }
                    observed_cursor = cursor.value();
                    output.event(observed_cursor, session_id, &event)?;
                    if let Some(selection) = terminal_snapshot_selection(&event) {
                        let mut refreshed = transcript(client, session_id).await?;
                        output.terminal_material(
                            &mut refreshed,
                            &mut displayed_entries,
                            selection,
                        )?;
                    }
                }
                ServerMessage::Error {
                    code: ErrorCode::ResyncRequired,
                    ..
                } => break,
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => return Err(ClientError::remote(code, message, detail)),
                _ => {
                    return Err(ClientError::Protocol(
                        "follow returned an unexpected response",
                    ));
                }
            }
        }
    }
}

fn terminal_snapshot_selection(event: &SessionEvent) -> Option<SnapshotSelection> {
    match event {
        SessionEvent::TurnCompleted {
            turn_id,
            model_call_id,
            completion_entry_id,
            ..
        } => Some(SnapshotSelection::Completed {
            turn_id: *turn_id,
            model_call_id: *model_call_id,
            terminal_entry_id: *completion_entry_id,
        }),
        SessionEvent::TurnFailed {
            turn_id,
            failure_entry_id,
            ..
        } => Some(SnapshotSelection::Failed {
            turn_id: *turn_id,
            terminal_entry_id: *failure_entry_id,
        }),
        SessionEvent::TurnCancelled {
            turn_id,
            cancellation_entry_id,
            ..
        } => Some(SnapshotSelection::Cancelled {
            turn_id: *turn_id,
            terminal_entry_id: *cancellation_entry_id,
        }),
        SessionEvent::ToolBatchTransition {
            turn_id,
            model_call_id,
            state: ToolBatchState::Proposed { .. },
        } => Some(SnapshotSelection::ToolBatchProposed {
            turn_id: *turn_id,
            model_call_id: *model_call_id,
        }),
        SessionEvent::ToolBatchTransition {
            turn_id,
            model_call_id,
            state: ToolBatchState::ResultsProjected { .. },
        } => Some(SnapshotSelection::ToolBatchResults {
            turn_id: *turn_id,
            model_call_id: *model_call_id,
        }),
        SessionEvent::ToolBatchTransition {
            state: ToolBatchState::RecoveryRequired { .. },
            ..
        } => None,
        SessionEvent::TurnToolReconciliationRequired {
            turn_id,
            tool_attempt_id,
            terminal_frontier_id,
        } => Some(SnapshotSelection::ToolReconciliation {
            turn_id: *turn_id,
            tool_attempt_id: *tool_attempt_id,
            terminal_frontier_id: *terminal_frontier_id,
        }),
        SessionEvent::TurnRefused { .. } | SessionEvent::TurnReconciliationRequired { .. } => None,
        SessionEvent::SessionCreated {}
        | SessionEvent::InputAccepted { .. }
        | SessionEvent::TurnActivated { .. }
        | SessionEvent::ModelCallTransition { .. } => None,
    }
}

fn write_assistant_texts(
    snapshot: &mut TranscriptSnapshot,
    output: &mut Output<'_>,
    selected_turn: CanonicalUuid,
) -> Result<(), ClientError> {
    let mut selected_entry = false;
    for record in snapshot.replay()? {
        match record? {
            SnapshotRecord::Entry(entry) => {
                selected_entry = matches!(
                    entry.kind,
                    transcript::SnapshotEntryKind::Text(
                        signalbox_process_protocol::TranscriptTextEntry::Assistant {
                            turn_id,
                            ..
                        }
                    ) if turn_id == selected_turn
                );
            }
            SnapshotRecord::Content(content) if selected_entry => {
                let ends_with_newline = content.content.as_str().ends_with('\n');
                output.assistant_text_fragment(
                    content.content.as_str(),
                    content.final_fragment,
                    ends_with_newline,
                )?;
                if content.final_fragment {
                    selected_entry = false;
                }
            }
            SnapshotRecord::Turn(_) | SnapshotRecord::Content(_) => {}
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SessionSummary {
    session_id: CanonicalUuid,
    defaults_version: u64,
}

async fn read_session_summaries(
    client: &mut ProcessClient,
    mut consume: impl FnMut(SessionSummary, &ServerFrame) -> Result<(), ClientError>,
) -> Result<(), ClientError> {
    let mut connection = client.request(ClientRequest::ListSessions {}).await?;
    match connection.message().await? {
        ServerMessage::SessionsStart {} => {}
        ServerMessage::Error {
            code,
            message,
            detail,
        } => return Err(ClientError::remote(code, message, detail)),
        _ => {
            return Err(ClientError::Protocol(
                "session list did not begin with its start frame",
            ));
        }
    }
    let mut prior_session = None;
    let mut summary_count = 0_u64;
    loop {
        let frame = connection.frame().await?;
        match frame.message() {
            ServerMessage::SessionSummary {
                session_id,
                defaults_version,
                ..
            } => {
                if prior_session
                    .is_some_and(|prior: CanonicalUuid| prior.into_uuid() >= session_id.into_uuid())
                {
                    return Err(ClientError::Protocol(
                        "session summaries were not strictly ordered",
                    ));
                }
                let summary = SessionSummary {
                    session_id: *session_id,
                    defaults_version: defaults_version.value(),
                };
                consume(summary, &frame)?;
                prior_session = Some(*session_id);
                summary_count = summary_count
                    .checked_add(1)
                    .ok_or(ClientError::Protocol("session summary count overflowed"))?;
            }
            ServerMessage::SessionsEnd { session_count }
                if session_count.value() == summary_count =>
            {
                return Ok(());
            }
            ServerMessage::Error {
                code,
                message,
                detail,
            } => return Err(ClientError::remote(*code, message.clone(), *detail)),
            _ => {
                return Err(ClientError::Protocol(
                    "session list sequence or count was invalid",
                ));
            }
        }
    }
}

async fn review(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    command: ReviewCommand,
) -> Result<(), ClientError> {
    match command {
        ReviewCommand::CreateTarget {
            command_id,
            target_id,
            provider,
            repository,
            subject,
            head_revision,
            base_revision,
            stack_parent_target_id,
        } => {
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::CreateReviewTarget {
                    command_id,
                    target_id,
                    provider,
                    repository,
                    subject,
                    head_revision,
                    base_revision,
                    stack_parent_target_id,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewTargetCreated {
                    target_id: recorded,
                } if recorded == target_id => {
                    output.review_acknowledgement(&format!("target={recorded} created"))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review target creation returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::StartRun {
            command_id,
            target_id,
            run_id,
            pass_id,
            workflow,
            session_id,
            accepted_input_id,
        } => {
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::StartReviewRun {
                    command_id,
                    target_id,
                    run_id,
                    pass_id,
                    workflow,
                    session_id,
                    accepted_input_id,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewRunStarted {
                    run_id: recorded_run,
                    pass_id: recorded_pass,
                } if recorded_run == run_id && recorded_pass == pass_id => {
                    output.review_acknowledgement(&format!(
                        "run={recorded_run} pass={recorded_pass} started"
                    ))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review run creation returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::ActivatePass {
            command_id,
            run_id,
            pass_id,
            turn_id,
        } => {
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::ActivateReviewPass {
                    command_id,
                    run_id,
                    pass_id,
                    turn_id,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewPassActivated {
                    run_id: recorded_run,
                    pass_id: recorded_pass,
                } if recorded_run == run_id && recorded_pass == pass_id => {
                    output.review_acknowledgement(&format!(
                        "run={recorded_run} pass={recorded_pass} activated"
                    ))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review pass activation returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::RecordFinding {
            command_id,
            run_id,
            pass_id,
            turn_id,
            output_frontier_id,
            finding,
        } => {
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::RecordReviewFindings {
                    command_id,
                    run_id,
                    pass_id,
                    turn_id,
                    output_frontier_id,
                    findings: vec![finding],
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewFindingsRecorded {
                    run_id: recorded_run,
                    pass_id: recorded_pass,
                    finding_count,
                } if recorded_run == run_id
                    && recorded_pass == pass_id
                    && finding_count.value() == 1 =>
                {
                    output.review_acknowledgement(&format!(
                        "run={recorded_run} pass={recorded_pass} findings=1 recorded"
                    ))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review finding admission returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::ReadTarget { target_id } => {
            let mut connection = client
                .request(ClientRequest::ReadReviewTarget { target_id })
                .await?;
            match connection.message().await? {
                ServerMessage::ReviewTarget { target } if target.target_id == target_id => {
                    output.review_target(&target)?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail)),
                _ => Err(ClientError::Protocol(
                    "review target read returned an unexpected response",
                )),
            }
        }
        ReviewCommand::ReadRun { run_id } => {
            let mut connection = client
                .request(ClientRequest::ReadReviewRun { run_id })
                .await?;
            match connection.message().await? {
                ServerMessage::ReviewRun { run, pass }
                    if run.run_id == run_id
                        && review_run_response_is_coherent(&run, pass.as_ref()) =>
                {
                    output.review_run(&run, pass.as_ref())?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail)),
                _ => Err(ClientError::Protocol(
                    "review run read returned an unexpected response",
                )),
            }
        }
        ReviewCommand::ReadFinding { finding_id } => {
            let mut connection = client
                .request(ClientRequest::ReadReviewFinding { finding_id })
                .await?;
            match connection.message().await? {
                ServerMessage::ReviewFinding { finding }
                    if finding.finding.finding_id == finding_id =>
                {
                    output.review_finding(&finding)?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail)),
                _ => Err(ClientError::Protocol(
                    "review finding read returned an unexpected response",
                )),
            }
        }
        ReviewCommand::ListFindings { run_id } => {
            let mut connection = client
                .request(ClientRequest::ListReviewFindings { run_id })
                .await?;
            let start = connection.frame().await?;
            match start.message() {
                ServerMessage::ReviewFindingsStart { run_id: selected } if *selected == run_id => {}
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => {
                    return Err(ClientError::remote(*code, message.clone(), *detail));
                }
                _ => {
                    return Err(ClientError::Protocol(
                        "review finding list did not start correctly",
                    ));
                }
            }
            let mut spool = tempfile::tempfile()?;
            let mut count = 0_u64;
            let mut previous_finding_id: Option<CanonicalUuid> = None;
            loop {
                let frame = connection.frame().await?;
                match frame.message() {
                    ServerMessage::ReviewFindingItem { finding } if finding.run_id == run_id => {
                        let finding_id = finding.finding.finding_id;
                        if previous_finding_id
                            .is_some_and(|previous| finding_id.into_uuid() <= previous.into_uuid())
                        {
                            return Err(ClientError::Protocol(
                                "review finding list identity order was invalid",
                            ));
                        }
                        previous_finding_id = Some(finding_id);
                        count = count.checked_add(1).ok_or(ClientError::Protocol(
                            "review finding list count overflowed",
                        ))?;
                        if count > MAX_REVIEW_FINDINGS_PER_RUN {
                            return Err(ClientError::Protocol(
                                "review finding list exceeded its admitted bound",
                            ));
                        }
                        spool.write_all(&encode_server_line(&frame)?)?;
                    }
                    ServerMessage::ReviewFindingsEnd { finding_count }
                        if finding_count.value() == count =>
                    {
                        break;
                    }
                    ServerMessage::Error {
                        code,
                        message,
                        detail,
                    } => {
                        return Err(ClientError::remote(*code, message.clone(), *detail));
                    }
                    _ => {
                        return Err(ClientError::Protocol(
                            "review finding list sequence or count was invalid",
                        ));
                    }
                }
            }
            spool.seek(SeekFrom::Start(0))?;
            let mut reader = BufReader::new(spool);
            let mut line = Vec::new();
            while reader.read_until(b'\n', &mut line)? != 0 {
                match decode_server_line(&line)?.message() {
                    ServerMessage::ReviewFindingItem { finding } => {
                        output.review_finding(finding)?;
                    }
                    _ => {
                        return Err(ClientError::Protocol(
                            "review finding spool contained a non-finding frame",
                        ));
                    }
                }
                line.clear();
            }
            Ok(())
        }
    }
}

fn review_run_response_is_coherent(
    run: &ReviewRunSnapshot,
    pass: Option<&ReviewPassSnapshot>,
) -> bool {
    match (run.pass_id, pass) {
        (None, None) => true,
        (Some(pass_id), Some(pass)) => {
            pass.pass_id == pass_id && pass.run_id == run.run_id && pass.target_id == run.target_id
        }
        (None, Some(_)) | (Some(_), None) => false,
    }
}

fn review_command_identity(
    output: &mut Output<'_>,
    supplied: Option<CommandId>,
) -> Result<CommandId, ClientError> {
    let (command_id, generated) = command_identity(supplied)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    Ok(command_id)
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
mod tests {
    use std::{
        error::Error,
        ffi::OsString,
        fs,
        io::{self, Cursor},
        os::unix::fs::symlink,
        path::PathBuf,
        process::ExitCode,
        time::Duration,
    };

    use signalbox_process_protocol::{
        CanonicalU64, CanonicalUuid, ClientFrame, ClientRequest, CommandId, FrameEncodeError,
        InputContent, ModelCallDisposition, ModelCallState, ModelSelection, ReviewFindingInput,
        ReviewFindingSnapshot, ReviewFindingStatus, ReviewPassKind, ReviewPassLifecycle,
        ReviewPassSnapshot, ReviewRunLifecycle, ReviewRunSnapshot, ReviewSeverity, ReviewWorkflow,
        ServerFrame, ServerMessage, SessionEvent, ToolBatchState, ToolDecision, TurnState,
        decode_client_line, encode_server_line,
    };
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::UnixListener,
        time::timeout,
    };
    use uuid::Uuid;

    use super::{
        MAX_INPUT_CONTENT_BYTES, MAX_POSSIBLY_FRAMED_IMPORT_SOURCE_BYTES,
        MAX_REVIEW_FINDINGS_PER_RUN, ProcessClient, ReviewCommand, SessionMetadataPageRequest,
        SnapshotSelection, TurnTerminal, collect_import_paths, create, decide,
        model_call_recovery_transition, open_scanned_import_source, read_input, reconcile_turn,
        review, run, search, socket_path, stop_turn, submit_input, terminal_event_state,
        terminal_snapshot_selection, terminal_snapshot_state, tool_recovery_transition,
    };
    use crate::{error::ClientError, presentation::Output};

    #[test]
    fn coherent_review_run_response_is_accepted() {
        let pass = review_pass_snapshot();
        let run = review_run_snapshot(Some(pass.pass_id));

        assert!(super::review_run_response_is_coherent(&run, Some(&pass)));
    }

    #[test]
    fn review_run_response_rejects_a_missing_recorded_pass() {
        let recorded_pass = review_pass_snapshot();
        let run = review_run_snapshot(Some(recorded_pass.pass_id));

        assert!(!super::review_run_response_is_coherent(&run, None));
    }

    #[test]
    fn review_run_response_rejects_cross_wired_pass_ancestry() {
        const FOREIGN_TARGET_IDENTITY: u128 = 4;

        let mut pass = review_pass_snapshot();
        let run = review_run_snapshot(Some(pass.pass_id));
        pass.target_id = CanonicalUuid::from_uuid(Uuid::from_u128(FOREIGN_TARGET_IDENTITY));

        assert!(!super::review_run_response_is_coherent(&run, Some(&pass)));
    }

    #[test]
    fn empty_standard_input_is_rejected() {
        assert!(read_input(&mut Cursor::new(Vec::<u8>::new())).is_err());
    }

    #[test]
    fn nul_in_standard_input_is_rejected() {
        assert!(read_input(&mut Cursor::new(b"before\0after".to_vec())).is_err());
    }

    #[test]
    fn oversized_standard_input_is_rejected() {
        assert!(read_input(&mut Cursor::new(vec![b'a'; MAX_INPUT_CONTENT_BYTES + 1])).is_err());
    }

    #[test]
    fn exact_limit_standard_input_is_accepted() {
        let exact = vec![b'a'; MAX_INPUT_CONTENT_BYTES];
        assert_eq!(
            read_input(&mut Cursor::new(exact.clone()))
                .ok()
                .map(|value| value.into_bytes()),
            Some(exact)
        );
    }

    #[test]
    fn send_fails_explicitly_when_model_call_recovery_is_required() {
        let state = TurnState::ActiveAwaitingModelCallRecovery {
            ended_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
            recovery_model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
        };

        assert!(matches!(
            terminal_snapshot_state(Some(&state)),
            Err(ClientError::TurnRecoveryRequired)
        ));
    }

    #[test]
    fn send_classifies_cancelled_snapshot_truth() {
        let state = TurnState::Cancelled {
            terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
            terminal_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            terminal_model_call_id: None,
        };

        assert_eq!(
            terminal_snapshot_state(Some(&state))
                .expect("cancelled state is terminal protocol truth"),
            Some(TurnTerminal::Cancelled)
        );
    }

    #[test]
    fn send_classifies_reconciliation_required_snapshot_truth() {
        let state = TurnState::ReconciliationRequired {
            terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
            terminal_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            terminal_model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
        };

        assert_eq!(
            terminal_snapshot_state(Some(&state))
                .expect("reconciliation state is terminal protocol truth"),
            Some(TurnTerminal::ReconciliationRequired)
        );
    }

    #[test]
    fn send_classifies_cancelled_event_for_its_turn() {
        let selected_turn = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let event = SessionEvent::TurnCancelled {
            turn_id: selected_turn,
            cancellation_entry_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
        };

        assert_eq!(
            terminal_event_state(&event, selected_turn),
            Some(TurnTerminal::Cancelled)
        );
    }

    #[test]
    fn send_classifies_reconciliation_required_event_for_its_turn() {
        let selected_turn = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let event = SessionEvent::TurnReconciliationRequired {
            turn_id: selected_turn,
            model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
        };

        assert_eq!(
            terminal_event_state(&event, selected_turn),
            Some(TurnTerminal::ReconciliationRequired)
        );
    }

    #[test]
    fn cli_socket_path_must_be_absolute() {
        assert!(matches!(
            socket_path(Some(PathBuf::from("relative.sock")), None),
            Err(ClientError::Input(
                "the local process socket path must be absolute"
            ))
        ));
    }

    #[test]
    fn environment_socket_path_must_be_absolute() {
        assert!(matches!(
            socket_path(None, Some(OsString::from("relative.sock"))),
            Err(ClientError::Input(
                "the local process socket path must be absolute"
            ))
        ));
    }

    #[test]
    fn selected_turn_ambiguous_model_call_requests_recovery_reread() {
        let selected_turn = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let event = SessionEvent::ModelCallTransition {
            turn_id: selected_turn,
            model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            state: ModelCallState::Terminal {
                disposition: ModelCallDisposition::Ambiguous,
            },
        };

        assert!(model_call_recovery_transition(&event, selected_turn));
        assert!(!model_call_recovery_transition(
            &event,
            CanonicalUuid::from_uuid(Uuid::from_u128(3))
        ));
    }

    #[test]
    fn selected_turn_tool_recovery_requests_authoritative_reread() {
        let selected_turn = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let event = SessionEvent::ToolBatchTransition {
            turn_id: selected_turn,
            model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            state: ToolBatchState::RecoveryRequired {
                tool_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
            },
        };

        assert!(tool_recovery_transition(&event, selected_turn));
        assert!(!tool_recovery_transition(
            &event,
            CanonicalUuid::from_uuid(Uuid::from_u128(4))
        ));
    }

    #[test]
    fn tool_batch_events_select_their_exact_material() {
        let turn = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let call = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let frontier = CanonicalUuid::from_uuid(Uuid::from_u128(3));
        assert_eq!(
            terminal_snapshot_selection(&SessionEvent::ToolBatchTransition {
                turn_id: turn,
                model_call_id: call,
                state: ToolBatchState::Proposed {
                    frontier_id: frontier,
                },
            }),
            Some(SnapshotSelection::ToolBatchProposed {
                turn_id: turn,
                model_call_id: call,
            })
        );
        assert_eq!(
            terminal_snapshot_selection(&SessionEvent::ToolBatchTransition {
                turn_id: turn,
                model_call_id: call,
                state: ToolBatchState::ResultsProjected {
                    frontier_id: frontier,
                },
            }),
            Some(SnapshotSelection::ToolBatchResults {
                turn_id: turn,
                model_call_id: call,
            })
        );
    }

    #[test]
    fn refused_terminal_event_requests_no_side_reread() {
        assert!(
            terminal_snapshot_selection(&SessionEvent::TurnRefused {
                turn_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
                model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
                terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
            })
            .is_none()
        );
    }

    #[test]
    fn cancellation_event_selects_its_exact_marker_for_reread() {
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));

        assert!(matches!(
            terminal_snapshot_selection(&SessionEvent::TurnCancelled {
                turn_id,
                cancellation_entry_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
                terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
            }),
            Some(SnapshotSelection::Cancelled {
                turn_id: selected,
                terminal_entry_id,
            }) if selected == turn_id && terminal_entry_id == CanonicalUuid::from_uuid(Uuid::from_u128(2))
        ));
    }

    #[test]
    fn reconciliation_event_selects_no_semantic_material_for_reread() {
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));

        assert!(
            terminal_snapshot_selection(&SessionEvent::TurnReconciliationRequired {
                turn_id,
                model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
                terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
            })
            .is_none()
        );
    }

    #[test]
    fn tool_reconciliation_event_selects_terminal_tool_results() {
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let tool_attempt_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let terminal_frontier_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));

        assert_eq!(
            terminal_snapshot_selection(&SessionEvent::TurnToolReconciliationRequired {
                turn_id,
                tool_attempt_id,
                terminal_frontier_id,
            }),
            Some(SnapshotSelection::ToolReconciliation {
                turn_id,
                tool_attempt_id,
                terminal_frontier_id,
            })
        );
    }

    #[tokio::test]
    async fn invalid_send_input_fails_before_a_missing_socket_is_opened() {
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();
        let mut error = Vec::new();
        let exit = run(
            [
                OsString::from("--socket"),
                OsString::from("/does/not/exist"),
                OsString::from("send"),
                OsString::from("00000000-0000-0000-0000-000000000001"),
            ],
            None,
            &mut input,
            &mut output,
            &mut error,
        )
        .await;
        assert_eq!(exit, ExitCode::FAILURE);
        assert!(String::from_utf8_lossy(&error).contains("must not be empty"));
    }

    #[tokio::test]
    async fn missing_import_source_fails_before_a_missing_socket_is_opened() {
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();
        let mut error = Vec::new();
        let exit = run(
            [
                OsString::from("--socket"),
                OsString::from("/does/not/exist/hub.sock"),
                OsString::from("import"),
                OsString::from("--format"),
                OsString::from("claude-code"),
                OsString::from("/does/not/exist/session.jsonl"),
            ],
            None,
            &mut input,
            &mut output,
            &mut error,
        )
        .await;

        assert_eq!(exit, ExitCode::FAILURE);
        assert!(output.is_empty());
        assert!(
            String::from_utf8_lossy(&error)
                .contains("conversation import source file could not be read")
        );
    }

    #[tokio::test]
    async fn import_source_beyond_the_possible_frame_payload_is_read_bounded()
    -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let source_path = directory.path().join("oversized.jsonl");
        let source_file = std::fs::File::create(&source_path)?;
        source_file.set_len(u64::try_from(MAX_POSSIBLY_FRAMED_IMPORT_SOURCE_BYTES + 1)?)?;
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();
        let mut error = Vec::new();

        let exit = run(
            [
                OsString::from("--socket"),
                OsString::from("/does/not/exist/hub.sock"),
                OsString::from("import"),
                OsString::from("--format"),
                OsString::from("claude-code"),
                source_path.into_os_string(),
            ],
            None,
            &mut input,
            &mut output,
            &mut error,
        )
        .await;

        assert_eq!(exit, ExitCode::FAILURE);
        assert!(output.is_empty());
        assert!(
            String::from_utf8_lossy(&error)
                .contains("conversation import source cannot fit within the process frame bound")
        );
        Ok(())
    }

    #[tokio::test]
    async fn import_source_at_the_reader_bound_is_encoded_before_connecting()
    -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let source_path = directory.path().join("boundary.jsonl");
        let source_file = std::fs::File::create(&source_path)?;
        source_file.set_len(u64::try_from(MAX_POSSIBLY_FRAMED_IMPORT_SOURCE_BYTES)?)?;
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();
        let mut error = Vec::new();

        let exit = run(
            [
                OsString::from("--socket"),
                OsString::from("/does/not/exist/hub.sock"),
                OsString::from("import"),
                OsString::from("--format"),
                OsString::from("claude-code"),
                source_path.into_os_string(),
            ],
            None,
            &mut input,
            &mut output,
            &mut error,
        )
        .await;

        assert_eq!(exit, ExitCode::FAILURE);
        assert!(output.is_empty());
        assert!(
            String::from_utf8_lossy(&error)
                .contains("conversation import source cannot fit within the process frame bound")
        );
        Ok(())
    }

    /// S28 / INV-038: a directory replaced after enumeration cannot redirect
    /// a queued candidate read through a symbolic link.
    #[tokio::test]
    async fn s28_inv038_scan_refuses_directory_symlink_replacement() -> Result<(), Box<dyn Error>> {
        let root = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        let queued_directory = root.path().join("queued");
        let retained_directory = root.path().join("retained");
        fs::create_dir(&queued_directory)?;
        fs::write(queued_directory.join("conversation.jsonl"), b"inside")?;
        fs::write(outside.path().join("conversation.jsonl"), b"outside")?;
        let scan = collect_import_paths(root.path())?;
        let candidate = scan
            .paths
            .first()
            .ok_or("fixture must select one candidate")?;
        let relative = candidate.relative.clone();
        fs::rename(&queued_directory, retained_directory)?;
        symlink(outside.path(), &queued_directory)?;

        let opened = open_scanned_import_source(&scan.root, &relative);

        assert!(matches!(opened, Err(ClientError::SourceFile(_))));
        Ok(())
    }

    /// S28 / INV-038: a regular candidate replaced after enumeration by a
    /// FIFO is rejected without waiting for a writer.
    #[cfg(not(target_vendor = "apple"))]
    #[tokio::test]
    async fn s28_inv038_scan_refuses_fifo_replacement_without_blocking()
    -> Result<(), Box<dyn Error>> {
        let root = tempfile::tempdir()?;
        let candidate_path = root.path().join("conversation.jsonl");
        fs::write(&candidate_path, b"inside")?;
        let scan = collect_import_paths(root.path())?;
        let candidate = scan
            .paths
            .first()
            .ok_or("fixture must select one candidate")?;
        let relative = candidate.relative.clone();
        fs::remove_file(&candidate_path)?;
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &candidate_path,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )?;

        let opened = open_scanned_import_source(&scan.root, &relative);

        assert!(matches!(opened, Err(ClientError::SourceFile(_))));
        Ok(())
    }

    #[tokio::test]
    async fn search_rejects_a_page_that_exceeds_its_requested_bound() -> Result<(), Box<dyn Error>>
    {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                request.request(),
                &ClientRequest::ListSessionMetadata {
                    required_tags: Vec::new(),
                    title_contains: None,
                    include_archived: false,
                    page_size: CanonicalU64::new(1),
                    after_session_id: None,
                }
            );
            let frame = |message| {
                ServerFrame::try_new_for_version(request.version(), request.request_id(), message)
                    .map_err(io::Error::other)
            };
            let summary = |seed| ServerMessage::SessionMetadataSummary {
                session_id: CanonicalUuid::from_uuid(Uuid::from_u128(seed)),
                defaults_version: CanonicalU64::new(1),
                model_selection: ModelSelection::Direct {
                    selection_id: CanonicalUuid::from_uuid(Uuid::from_u128(9)),
                },
                dangerous_tool_auto_approval: false,
                title: None,
                tags: Vec::new(),
                archived: false,
                last_writer: None,
            };
            let mut response = Vec::new();
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::SessionMetadataPageStart {})?)
                    .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(summary(1))?).map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(summary(2))?).map_err(io::Error::other)?,
            );
            writer.write_all(&response).await?;
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);
        let result = search(
            &mut client,
            &mut output,
            SessionMetadataPageRequest {
                required_tags: Vec::new(),
                title_contains: None,
                include_archived: false,
                page_size: CanonicalU64::new(1),
                after_session_id: None,
            },
        )
        .await;

        assert!(matches!(
            result,
            Err(ClientError::Protocol(
                "session metadata page exceeded its requested bound"
            ))
        ));
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn review_list_rejects_terminal_count_before_writing_items() -> Result<(), Box<dyn Error>>
    {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let run_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let finding_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                request.request(),
                &ClientRequest::ListReviewFindings { run_id }
            );
            let frame = |message| {
                ServerFrame::try_new_for_version(request.version(), request.request_id(), message)
                    .map_err(io::Error::other)
            };
            let finding = ReviewFindingSnapshot {
                target_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
                run_id,
                producing_pass_id: CanonicalUuid::from_uuid(Uuid::from_u128(4)),
                finding: ReviewFindingInput {
                    finding_id,
                    file_path: String::from("src/review.rs"),
                    line_start: Some(CanonicalU64::new(11)),
                    line_end: Some(CanonicalU64::new(14)),
                    diff_side: None,
                    title: String::from("Retain the exact edge"),
                    body: String::from("The terminal count must authenticate the list."),
                    severity: ReviewSeverity::High,
                    confidence: CanonicalU64::new(9_000),
                    category: String::from("correctness"),
                    recommended_fix: None,
                },
                status: ReviewFindingStatus::Open,
                event_count: CanonicalU64::new(0),
            };
            let mut response = Vec::new();
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::ReviewFindingsStart { run_id })?)
                    .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::ReviewFindingItem { finding })?)
                    .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::ReviewFindingsEnd {
                    finding_count: CanonicalU64::new(2),
                })?)
                .map_err(io::Error::other)?,
            );
            writer.write_all(&response).await?;
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);
        let error = review(
            &mut client,
            &mut output,
            ReviewCommand::ListFindings { run_id },
        )
        .await
        .expect_err("the mismatched terminal count must reject the list");

        assert_eq!(
            error.to_string(),
            "the server violated the process protocol: review finding list sequence or count was invalid"
        );
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn review_list_rejects_an_over_bound_inventory_before_writing_items()
    -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let run_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                request.request(),
                &ClientRequest::ListReviewFindings { run_id }
            );
            let response =
                over_bound_review_findings_response(&request, run_id).map_err(io::Error::other)?;
            writer.write_all(&response).await?;
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);
        let error = review(
            &mut client,
            &mut output,
            ReviewCommand::ListFindings { run_id },
        )
        .await
        .expect_err("the over-bound finding inventory must be rejected");

        assert_eq!(
            error.to_string(),
            "the server violated the process protocol: review finding list exceeded its admitted bound"
        );
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn create_connection_failure_is_definitely_uncommitted() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let mut client = ProcessClient::new(directory.path().join("missing.sock"));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);

        let result = create(
            &mut client,
            &mut output,
            ModelSelection::Direct {
                selection_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
            },
            Some(CommandId::try_from_uuid(Uuid::from_u128(2))?),
        )
        .await;

        assert!(matches!(result, Err(ClientError::Io(_))));
        Ok(())
    }

    #[tokio::test]
    async fn submit_connection_failure_is_definitely_uncommitted() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let mut client = ProcessClient::new(directory.path().join("missing.sock"));

        let result = submit_input(
            &mut client,
            CommandId::try_from_uuid(Uuid::from_u128(1))?,
            CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            InputContent::new(String::from("queued content")),
            CanonicalU64::new(1),
        )
        .await;

        assert!(matches!(result, Err(ClientError::Io(_))));
        Ok(())
    }

    #[tokio::test]
    async fn submit_input_releases_its_connection_after_acceptance() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert!(matches!(
                request.request(),
                ClientRequest::SubmitInput {
                    session_id: requested_session,
                    ..
                } if *requested_session == session_id
            ));
            let response = ServerFrame::try_new_for_version(
                request.version(),
                request.request_id(),
                ServerMessage::InputSubmitted {
                    session_id,
                    accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
                    acceptance_position: CanonicalU64::new(1),
                    turn_id,
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
                .await?;

            let mut byte = [0_u8; 1];
            let read = timeout(Duration::from_secs(1), reader.read(&mut byte))
                .await
                .map_err(io::Error::other)??;
            assert_eq!(read, 0);
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let submitted_turn = submit_input(
            &mut client,
            CommandId::try_from_uuid(Uuid::from_u128(4))?,
            session_id,
            InputContent::new(String::from("queued content")),
            CanonicalU64::new(1),
        )
        .await?;
        assert_eq!(submitted_turn, turn_id);
        server.await??;
        Ok(())
    }

    /// INV-033: the reconciliation verb names the exact parked turn on the
    /// wire and returns the accepted successor turn.
    #[tokio::test]
    async fn reconcile_turn_names_the_parked_turn_and_returns_its_successor()
    -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let parked_turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let successor_turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(5));
        let server = tokio::spawn(async move {
            let (stream, mut writer) = listener.accept().await?.0.into_split();
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert!(matches!(
                request.request(),
                ClientRequest::ReconcileTurn {
                    session_id: requested_session,
                    expected_active_turn_id: requested_turn,
                    ..
                } if *requested_session == session_id && *requested_turn == parked_turn_id
            ));
            let response = ServerFrame::try_new_for_version(
                request.version(),
                request.request_id(),
                ServerMessage::InputSubmitted {
                    session_id,
                    accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
                    acceptance_position: CanonicalU64::new(2),
                    turn_id: successor_turn_id,
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
                .await?;
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let accepted_successor = reconcile_turn(
            &mut client,
            CommandId::try_from_uuid(Uuid::from_u128(4))?,
            session_id,
            parked_turn_id,
            InputContent::new(String::from("continue after reconciliation")),
            CanonicalU64::new(1),
        )
        .await?;
        assert_eq!(accepted_successor, successor_turn_id);
        server.await??;
        Ok(())
    }

    /// INV-033: the stop verb names the exact expected active turn on the
    /// wire and returns the accepted successor turn.
    #[tokio::test]
    async fn inv033_stop_turn_names_the_active_turn_and_returns_its_successor()
    -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let active_turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let successor_turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(5));
        let server = tokio::spawn(async move {
            let (stream, mut writer) = listener.accept().await?.0.into_split();
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert!(matches!(
                request.request(),
                ClientRequest::StopTurn {
                    session_id: requested_session,
                    expected_active_turn_id: requested_turn,
                    ..
                } if *requested_session == session_id && *requested_turn == active_turn_id
            ));
            let response = ServerFrame::try_new_for_version(
                request.version(),
                request.request_id(),
                ServerMessage::InputSubmitted {
                    session_id,
                    accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
                    acceptance_position: CanonicalU64::new(2),
                    turn_id: successor_turn_id,
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
                .await?;
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let accepted_successor = stop_turn(
            &mut client,
            CommandId::try_from_uuid(Uuid::from_u128(4))?,
            session_id,
            active_turn_id,
            InputContent::new(String::from("continue after the stop")),
            CanonicalU64::new(1),
        )
        .await?;
        assert_eq!(accepted_successor, successor_turn_id);
        server.await??;
        Ok(())
    }

    /// INV-033: a decision verb sends the exact closed decision and validates
    /// that the receipt echoes the same request and decision.
    #[tokio::test]
    async fn inv033_decide_validates_the_exact_recorded_receipt() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let tool_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let server = tokio::spawn(async move {
            let (stream, mut writer) = listener.accept().await?.0.into_split();
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert!(matches!(
                request.request(),
                ClientRequest::DecideToolRequest {
                    session_id: requested_session,
                    tool_request_id: requested_tool,
                    decision: ToolDecision::Deny { reason },
                    ..
                } if *requested_session == session_id
                    && *requested_tool == tool_request_id
                    && reason == "writes outside the workspace"
            ));
            let response = ServerFrame::try_new_for_version(
                request.version(),
                request.request_id(),
                ServerMessage::ToolRequestDecided {
                    tool_request_id,
                    decision: ToolDecision::Deny {
                        reason: String::from("writes outside the workspace"),
                    },
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
                .await?;
            Ok::<(), io::Error>(())
        });

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);
        let mut client = ProcessClient::new(socket);
        decide(
            &mut client,
            &mut output,
            session_id,
            tool_request_id,
            Some(CommandId::try_from_uuid(Uuid::from_u128(4))?),
            ToolDecision::Deny {
                reason: String::from("writes outside the workspace"),
            },
        )
        .await?;
        server.await??;
        assert_eq!(
            String::from_utf8(stdout)?,
            format!("tool_request={tool_request_id} decision=deny\n")
        );
        assert_eq!(String::from_utf8(stderr)?, "");
        Ok(())
    }

    /// INV-033: a receipt naming a different request or decision is a
    /// protocol violation, never silently accepted.
    #[tokio::test]
    async fn inv033_decide_rejects_a_receipt_for_a_different_decision() -> Result<(), Box<dyn Error>>
    {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let tool_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let server = tokio::spawn(async move {
            let (stream, mut writer) = listener.accept().await?.0.into_split();
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            let response = ServerFrame::try_new_for_version(
                request.version(),
                request.request_id(),
                ServerMessage::ToolRequestDecided {
                    tool_request_id,
                    decision: ToolDecision::Approve {},
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
                .await?;
            Ok::<(), io::Error>(())
        });

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);
        let mut client = ProcessClient::new(socket);
        let result = decide(
            &mut client,
            &mut output,
            session_id,
            tool_request_id,
            Some(CommandId::try_from_uuid(Uuid::from_u128(4))?),
            ToolDecision::Deny {
                reason: String::from("writes outside the workspace"),
            },
        )
        .await;
        assert!(matches!(result, Err(ClientError::AmbiguousMutation)));
        server.await??;
        Ok(())
    }

    fn over_bound_review_findings_response(
        request: &ClientFrame,
        run_id: CanonicalUuid,
    ) -> Result<Vec<u8>, FrameEncodeError> {
        const FIRST_FINDING_IDENTITY: u128 = 10;

        let frame = |message| {
            ServerFrame::try_new_for_version(request.version(), request.request_id(), message)
        };
        let mut response =
            encode_server_line(&frame(ServerMessage::ReviewFindingsStart { run_id })?)?;
        for offset in 0..=MAX_REVIEW_FINDINGS_PER_RUN {
            let finding_id = CanonicalUuid::from_uuid(Uuid::from_u128(
                FIRST_FINDING_IDENTITY + u128::from(offset),
            ));
            let finding = ReviewFindingSnapshot {
                target_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
                run_id,
                producing_pass_id: CanonicalUuid::from_uuid(Uuid::from_u128(4)),
                finding: ReviewFindingInput {
                    finding_id,
                    file_path: String::from("src/review.rs"),
                    line_start: None,
                    line_end: None,
                    diff_side: None,
                    title: String::from("Bound the list"),
                    body: String::from("The client must reject an over-bound inventory."),
                    severity: ReviewSeverity::High,
                    confidence: CanonicalU64::new(9_000),
                    category: String::from("availability"),
                    recommended_fix: None,
                },
                status: ReviewFindingStatus::Open,
                event_count: CanonicalU64::new(0),
            };
            response.extend_from_slice(&encode_server_line(&frame(
                ServerMessage::ReviewFindingItem { finding },
            )?)?);
        }
        response.extend_from_slice(&encode_server_line(&frame(
            ServerMessage::ReviewFindingsEnd {
                finding_count: CanonicalU64::new(MAX_REVIEW_FINDINGS_PER_RUN + 1),
            },
        )?)?);
        Ok(response)
    }

    fn review_run_snapshot(pass_id: Option<CanonicalUuid>) -> ReviewRunSnapshot {
        ReviewRunSnapshot {
            target_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
            run_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            workflow: ReviewWorkflow::ReadOnlyReview,
            policy_version: CanonicalU64::new(1),
            minimum_judge_confidence: CanonicalU64::new(8_000),
            minimum_publication_confidence: CanonicalU64::new(9_000),
            state: ReviewRunLifecycle::Queued,
            pass_id,
        }
    }

    fn review_pass_snapshot() -> ReviewPassSnapshot {
        ReviewPassSnapshot {
            pass_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
            run_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            target_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
            kind: ReviewPassKind::ReadOnlyReview,
            session_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
            accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(6)),
            origin_turn_id: CanonicalUuid::from_uuid(Uuid::from_u128(7)),
            state: ReviewPassLifecycle::Queued,
            turn_id: None,
            output_frontier_id: None,
        }
    }
}
