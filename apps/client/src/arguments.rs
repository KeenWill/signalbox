use std::{ffi::OsString, fmt, iter, path::PathBuf};

use clap::{
    ArgGroup, Args as ClapArgs, CommandFactory, Parser, Subcommand, ValueEnum, error::ErrorKind,
};
use signalbox_process_protocol::{
    CanonicalU64, CanonicalUuid, CommandId, ConversationImportFormat,
    MAX_SESSION_METADATA_INDEXED_UTF8_BYTES, MAX_SESSION_METADATA_REQUIRED_TAGS,
    MAX_SESSION_METADATA_TOTAL_UTF8_BYTES, ModelSelection,
};
use uuid::Uuid;

use crate::{MAX_METADATA_PAGE_SIZE, MIN_METADATA_PAGE_SIZE, SessionMetadataPageRequest};

/// The specification's ordinary default metadata page size.
const DEFAULT_SEARCH_RESULT_LIMIT: &str = "50";

#[derive(Debug)]
pub(crate) struct Arguments {
    pub(crate) socket: Option<PathBuf>,
    pub(crate) raw_output: bool,
    pub(crate) command: Command,
}

#[derive(Debug)]
pub(crate) enum Command {
    Create {
        selection: ModelSelection,
        command_id: Option<CommandId>,
    },
    List,
    Search(SessionMetadataPageRequest),
    Send {
        session_id: CanonicalUuid,
        command_id: Option<CommandId>,
        defaults_version: Option<CanonicalU64>,
    },
    Model {
        session_id: CanonicalUuid,
        selection: ModelSelection,
        command_id: Option<CommandId>,
        defaults_version: Option<CanonicalU64>,
        dangerous_tool_auto_approval: Option<DangerousToolAutoApprovalArgument>,
    },
    Transcript {
        session_id: CanonicalUuid,
    },
    Follow {
        session_id: CanonicalUuid,
    },
    Import {
        format: ConversationImportFormat,
        source: ImportSourceArgument,
    },
    Reconcile {
        session_id: CanonicalUuid,
        turn_id: CanonicalUuid,
        command_id: Option<CommandId>,
        defaults_version: Option<CanonicalU64>,
    },
    Stop {
        session_id: CanonicalUuid,
        turn_id: Option<CanonicalUuid>,
        command_id: Option<CommandId>,
        defaults_version: Option<CanonicalU64>,
    },
    Approve {
        session_id: CanonicalUuid,
        tool_request_id: CanonicalUuid,
        command_id: Option<CommandId>,
    },
    Deny {
        session_id: CanonicalUuid,
        tool_request_id: CanonicalUuid,
        reason: String,
        command_id: Option<CommandId>,
    },
}

#[derive(Debug)]
pub(crate) enum ImportSourceArgument {
    File(PathBuf),
    Scan(PathBuf),
}

#[derive(Debug)]
pub(crate) enum ParseOutcome {
    Help(String),
    Run(Arguments),
}

#[derive(Debug)]
pub(crate) struct UsageError(clap::Error);

impl fmt::Display for UsageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for UsageError {}

#[derive(Debug, Parser)]
#[command(
    name = "signalbox",
    about = "Terminal client for the local Signalbox process protocol",
    disable_version_flag = true,
    args_override_self = false
)]
struct Cli {
    /// Override SIGNALBOX_SOCKET_PATH.
    #[arg(long, value_name = "PATH", global = true)]
    socket: Option<PathBuf>,
    /// Write process-derived text without terminal-safe escaping.
    #[arg(long, global = true)]
    raw_output: bool,
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Create a session.
    Create(CreateArguments),
    /// List current sessions.
    List,
    /// Read one filtered page of current session metadata.
    Search(SearchArguments),
    /// Submit standard input and print the reply after completion.
    Send(SendArguments),
    /// Install a new forward-only session model-defaults epoch.
    Model(ModelArguments),
    /// Print one authoritative session transcript.
    Transcript(SessionArguments),
    /// Print a snapshot and follow durable session updates.
    Follow(SessionArguments),
    /// Import Claude Code sessions or Codex rollout JSONL files.
    Import(ImportArguments),
    /// Reconcile a turn parked on an ambiguous model call and continue with
    /// standard-input content.
    Reconcile(ReconcileArguments),
    /// Stop the active turn and continue with standard-input content.
    Stop(StopArguments),
    /// Approve one pending tool request.
    Approve(DecideArguments),
    /// Deny one pending tool request with an explicit reason.
    Deny(DenyArguments),
}

#[derive(Debug, ClapArgs)]
#[command(group(
    ArgGroup::new("selection")
        .required(true)
        .multiple(false)
        .args(["model", "alias"])
))]
struct CreateArguments {
    /// Select a model configuration directly.
    #[arg(long, value_name = "UUID", value_parser = canonical_uuid)]
    model: Option<CanonicalUuid>,
    /// Select a configured model alias.
    #[arg(long, value_name = "UUID", value_parser = canonical_uuid)]
    alias: Option<CanonicalUuid>,
    /// Reuse an exact non-reserved durable command identity.
    #[arg(long, value_name = "UUID", value_parser = command_id)]
    command_id: Option<CommandId>,
}

#[derive(Debug, ClapArgs)]
struct SearchArguments {
    /// Require an exact case-sensitive title substring.
    #[arg(long, value_name = "SUBSTRING", value_parser = metadata_filter_text)]
    title: Option<String>,
    /// Require an exact tag; repeat the option to require every named tag.
    #[arg(long = "tag", value_name = "TAG", value_parser = metadata_filter_text)]
    tags: Vec<String>,
    /// Include archived sessions, which the default view excludes.
    #[arg(long)]
    include_archived: bool,
    /// Read at most this many results, from 1 through 100.
    #[arg(
        long,
        value_name = "COUNT",
        default_value = DEFAULT_SEARCH_RESULT_LIMIT,
        value_parser = metadata_page_size
    )]
    limit: CanonicalU64,
    /// Continue after the exact session identity a prior page printed.
    #[arg(long, value_name = "SESSION", value_parser = canonical_uuid)]
    after: Option<CanonicalUuid>,
}

#[derive(Debug, ClapArgs)]
struct SendArguments {
    /// Session to receive standard-input content.
    #[arg(value_name = "SESSION", value_parser = canonical_uuid)]
    session_id: CanonicalUuid,
    /// Reuse an exact non-reserved durable command identity.
    #[arg(
        long,
        value_name = "UUID",
        requires = "defaults_version",
        value_parser = command_id
    )]
    command_id: Option<CommandId>,
    /// Exact defaults version paired with a recovery command identity.
    #[arg(
        long,
        value_name = "DECIMAL",
        requires = "command_id",
        value_parser = canonical_u64
    )]
    defaults_version: Option<CanonicalU64>,
}

#[derive(Debug, ClapArgs)]
#[command(group(
    ArgGroup::new("selection")
        .required(true)
        .multiple(false)
        .args(["model", "alias"])
))]
struct ModelArguments {
    /// Session whose future turns should use the replacement model.
    #[arg(value_name = "SESSION", value_parser = canonical_uuid)]
    session_id: CanonicalUuid,
    /// Select a model configuration directly.
    #[arg(long, value_name = "UUID", value_parser = canonical_uuid)]
    model: Option<CanonicalUuid>,
    /// Select a configured model alias.
    #[arg(long, value_name = "UUID", value_parser = canonical_uuid)]
    alias: Option<CanonicalUuid>,
    /// Reuse an exact non-reserved durable command identity.
    #[arg(
        long,
        value_name = "UUID",
        requires_all = ["defaults_version", "dangerous_tool_auto_approval"],
        value_parser = command_id
    )]
    command_id: Option<CommandId>,
    /// Exact defaults epoch paired with a recovery command identity.
    #[arg(
        long,
        value_name = "DECIMAL",
        requires_all = ["command_id", "dangerous_tool_auto_approval"],
        value_parser = canonical_u64
    )]
    defaults_version: Option<CanonicalU64>,
    /// Exact copied dangerous-tool posture paired with recovery values.
    #[arg(
        long,
        value_enum,
        requires_all = ["command_id", "defaults_version"]
    )]
    dangerous_tool_auto_approval: Option<DangerousToolAutoApprovalArgument>,
}

#[derive(Debug, ClapArgs)]
struct ReconcileArguments {
    /// Session whose active turn is parked awaiting reconciliation.
    #[arg(value_name = "SESSION", value_parser = canonical_uuid)]
    session_id: CanonicalUuid,
    /// Exact parked turn observed in the session transcript.
    #[arg(value_name = "TURN", value_parser = canonical_uuid)]
    turn_id: CanonicalUuid,
    /// Reuse an exact non-reserved durable command identity.
    #[arg(
        long,
        value_name = "UUID",
        requires = "defaults_version",
        value_parser = command_id
    )]
    command_id: Option<CommandId>,
    /// Exact defaults version paired with a recovery command identity.
    #[arg(
        long,
        value_name = "DECIMAL",
        requires = "command_id",
        value_parser = canonical_u64
    )]
    defaults_version: Option<CanonicalU64>,
}

#[derive(Debug, ClapArgs)]
struct StopArguments {
    /// Session whose active turn should stop.
    #[arg(value_name = "SESSION", value_parser = canonical_uuid)]
    session_id: CanonicalUuid,
    /// Exact expected active turn paired with recovery values.
    #[arg(
        long,
        value_name = "UUID",
        requires_all = ["command_id", "defaults_version"],
        value_parser = canonical_uuid
    )]
    turn: Option<CanonicalUuid>,
    /// Reuse an exact non-reserved durable command identity.
    #[arg(
        long,
        value_name = "UUID",
        requires_all = ["defaults_version", "turn"],
        value_parser = command_id
    )]
    command_id: Option<CommandId>,
    /// Exact defaults version paired with a recovery command identity.
    #[arg(
        long,
        value_name = "DECIMAL",
        requires_all = ["command_id", "turn"],
        value_parser = canonical_u64
    )]
    defaults_version: Option<CanonicalU64>,
}

#[derive(Debug, ClapArgs)]
struct DecideArguments {
    /// Session the pending tool request belongs to.
    #[arg(value_name = "SESSION", value_parser = canonical_uuid)]
    session_id: CanonicalUuid,
    /// Pending tool request printed by the transcript.
    #[arg(value_name = "TOOL_REQUEST", value_parser = canonical_uuid)]
    tool_request_id: CanonicalUuid,
    /// Reuse an exact non-reserved durable command identity.
    #[arg(long, value_name = "UUID", value_parser = command_id)]
    command_id: Option<CommandId>,
}

#[derive(Debug, ClapArgs)]
struct DenyArguments {
    /// Session the pending tool request belongs to.
    #[arg(value_name = "SESSION", value_parser = canonical_uuid)]
    session_id: CanonicalUuid,
    /// Pending tool request printed by the transcript.
    #[arg(value_name = "TOOL_REQUEST", value_parser = canonical_uuid)]
    tool_request_id: CanonicalUuid,
    /// Exact denial explanation rendered to the model.
    #[arg(long, value_name = "TEXT")]
    reason: String,
    /// Reuse an exact non-reserved durable command identity.
    #[arg(long, value_name = "UUID", value_parser = command_id)]
    command_id: Option<CommandId>,
}

#[derive(Debug, ClapArgs)]
struct SessionArguments {
    /// Selected session.
    #[arg(value_name = "SESSION", value_parser = canonical_uuid)]
    session_id: CanonicalUuid,
}

#[derive(Debug, ClapArgs)]
#[command(group(
    ArgGroup::new("source")
        .required(true)
        .multiple(false)
        .args(["path", "scan"])
))]
struct ImportArguments {
    /// Select the source family and its current fixed converter version.
    #[arg(long, value_enum)]
    format: ImportFormatArgument,
    /// Read exactly one source file.
    #[arg(value_name = "FILE")]
    path: Option<PathBuf>,
    /// Recursively import every regular lowercase .jsonl file under a directory.
    #[arg(long, value_name = "DIR")]
    scan: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ImportFormatArgument {
    ClaudeCode,
    Codex,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub(crate) enum DangerousToolAutoApprovalArgument {
    Disabled,
    ApproveAll,
}

pub(crate) fn parse(
    values: impl IntoIterator<Item = OsString>,
) -> Result<ParseOutcome, UsageError> {
    let values = iter::once(OsString::from("signalbox")).chain(values);
    let parsed = match Cli::try_parse_from(values) {
        Ok(parsed) => parsed,
        Err(error) if error.kind() == ErrorKind::DisplayHelp => {
            return Ok(ParseOutcome::Help(error.to_string()));
        }
        Err(error) => return Err(UsageError(error)),
    };
    let command = match parsed.command {
        CliCommand::Create(arguments) => Command::Create {
            selection: match (arguments.model, arguments.alias) {
                (Some(selection_id), None) => ModelSelection::Direct { selection_id },
                (None, Some(alias_id)) => ModelSelection::Alias { alias_id },
                (None, None) | (Some(_), Some(_)) => {
                    return Err(UsageError(Cli::command().error(
                        ErrorKind::ArgumentConflict,
                        "create requires exactly one of --model or --alias",
                    )));
                }
            },
            command_id: arguments.command_id,
        },
        CliCommand::List => Command::List,
        CliCommand::Search(arguments) => {
            let mut distinct = arguments.tags.clone();
            distinct.sort();
            distinct.dedup();
            if distinct.len() != arguments.tags.len() {
                return Err(UsageError(Cli::command().error(
                    ErrorKind::ArgumentConflict,
                    "search requires distinct --tag values",
                )));
            }
            if arguments.tags.len() > MAX_SESSION_METADATA_REQUIRED_TAGS {
                return Err(UsageError(Cli::command().error(
                    ErrorKind::TooManyValues,
                    format!(
                        "search admits at most {MAX_SESSION_METADATA_REQUIRED_TAGS} --tag values"
                    ),
                )));
            }
            if arguments
                .tags
                .iter()
                .any(|tag| tag.len() > MAX_SESSION_METADATA_INDEXED_UTF8_BYTES)
            {
                return Err(UsageError(Cli::command().error(
                    ErrorKind::ValueValidation,
                    format!(
                        "each --tag value carries at most \
                         {MAX_SESSION_METADATA_INDEXED_UTF8_BYTES} UTF-8 bytes"
                    ),
                )));
            }
            let filter_utf8_bytes = arguments.tags.iter().map(String::len).fold(
                arguments.title.as_deref().map_or(0, str::len),
                usize::saturating_add,
            );
            if filter_utf8_bytes > MAX_SESSION_METADATA_TOTAL_UTF8_BYTES {
                return Err(UsageError(Cli::command().error(
                    ErrorKind::ValueValidation,
                    format!(
                        "the --title query and --tag values carry at most \
                         {MAX_SESSION_METADATA_TOTAL_UTF8_BYTES} UTF-8 bytes together"
                    ),
                )));
            }
            Command::Search(SessionMetadataPageRequest {
                required_tags: arguments.tags,
                title_contains: arguments.title,
                include_archived: arguments.include_archived,
                page_size: arguments.limit,
                after_session_id: arguments.after,
            })
        }
        CliCommand::Send(arguments) => Command::Send {
            session_id: arguments.session_id,
            command_id: arguments.command_id,
            defaults_version: arguments.defaults_version,
        },
        CliCommand::Model(arguments) => Command::Model {
            session_id: arguments.session_id,
            selection: match (arguments.model, arguments.alias) {
                (Some(selection_id), None) => ModelSelection::Direct { selection_id },
                (None, Some(alias_id)) => ModelSelection::Alias { alias_id },
                (None, None) | (Some(_), Some(_)) => {
                    return Err(UsageError(Cli::command().error(
                        ErrorKind::ArgumentConflict,
                        "model requires exactly one of --model or --alias",
                    )));
                }
            },
            command_id: arguments.command_id,
            defaults_version: arguments.defaults_version,
            dangerous_tool_auto_approval: arguments.dangerous_tool_auto_approval,
        },
        CliCommand::Transcript(arguments) => Command::Transcript {
            session_id: arguments.session_id,
        },
        CliCommand::Follow(arguments) => Command::Follow {
            session_id: arguments.session_id,
        },
        CliCommand::Import(arguments) => Command::Import {
            format: match arguments.format {
                ImportFormatArgument::ClaudeCode => {
                    ConversationImportFormat::ClaudeCodeSessionJsonlV2
                }
                ImportFormatArgument::Codex => ConversationImportFormat::CodexRolloutJsonlV1,
            },
            source: match (arguments.path, arguments.scan) {
                (Some(path), None) => ImportSourceArgument::File(path),
                (None, Some(path)) => ImportSourceArgument::Scan(path),
                (None, None) | (Some(_), Some(_)) => {
                    return Err(UsageError(Cli::command().error(
                        ErrorKind::ArgumentConflict,
                        "import requires exactly one FILE or --scan DIR",
                    )));
                }
            },
        },
        CliCommand::Reconcile(arguments) => Command::Reconcile {
            session_id: arguments.session_id,
            turn_id: arguments.turn_id,
            command_id: arguments.command_id,
            defaults_version: arguments.defaults_version,
        },
        CliCommand::Stop(arguments) => Command::Stop {
            session_id: arguments.session_id,
            turn_id: arguments.turn,
            command_id: arguments.command_id,
            defaults_version: arguments.defaults_version,
        },
        CliCommand::Approve(arguments) => Command::Approve {
            session_id: arguments.session_id,
            tool_request_id: arguments.tool_request_id,
            command_id: arguments.command_id,
        },
        CliCommand::Deny(arguments) => Command::Deny {
            session_id: arguments.session_id,
            tool_request_id: arguments.tool_request_id,
            reason: arguments.reason,
            command_id: arguments.command_id,
        },
    };
    Ok(ParseOutcome::Run(Arguments {
        socket: parsed.socket,
        raw_output: parsed.raw_output,
        command,
    }))
}

fn canonical_uuid(value: &str) -> Result<CanonicalUuid, String> {
    let parsed = Uuid::parse_str(value).map_err(|_| "UUID is invalid".to_owned())?;
    if parsed.hyphenated().to_string() != value {
        return Err("UUID must be lowercase canonical hyphenated text".to_owned());
    }
    Ok(CanonicalUuid::from_uuid(parsed))
}

fn command_id(value: &str) -> Result<CommandId, String> {
    CommandId::try_from_uuid(canonical_uuid(value)?.into_uuid())
        .map_err(|_| "command UUID uses a reserved value".to_owned())
}

fn metadata_filter_text(value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err("metadata filter text must not be empty".to_owned());
    }
    if value.contains('\0') {
        return Err("metadata filter text must not contain U+0000".to_owned());
    }
    Ok(value.to_owned())
}

fn metadata_page_size(value: &str) -> Result<CanonicalU64, String> {
    let parsed = canonical_u64(value)?;
    if !(MIN_METADATA_PAGE_SIZE..=MAX_METADATA_PAGE_SIZE).contains(&parsed.value()) {
        return Err(format!(
            "the result limit must be from {MIN_METADATA_PAGE_SIZE} through \
             {MAX_METADATA_PAGE_SIZE}"
        ));
    }
    Ok(parsed)
}

fn canonical_u64(value: &str) -> Result<CanonicalU64, String> {
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err("decimal value must use its shortest unsigned spelling".to_owned());
    }
    let parsed = value
        .parse::<u64>()
        .map_err(|_| "decimal value exceeds the unsigned 64-bit range".to_owned())?;
    Ok(CanonicalU64::new(parsed))
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, path::Path};

    use signalbox_process_protocol::{
        CanonicalU64, CanonicalUuid, ConversationImportFormat,
        MAX_SESSION_METADATA_INDEXED_UTF8_BYTES, MAX_SESSION_METADATA_REQUIRED_TAGS,
        MAX_SESSION_METADATA_TOTAL_UTF8_BYTES,
    };
    use uuid::Uuid;

    use super::{
        Arguments, Command, DangerousToolAutoApprovalArgument, ImportSourceArgument, ParseOutcome,
        SessionMetadataPageRequest, parse,
    };

    #[test]
    fn send_recovery_flags_are_an_exact_pair() {
        let session = "00000000-0000-0000-0000-000000000001";
        assert!(parse(["send", session, "--command-id", session].map(Into::into)).is_err());
        assert!(parse(["send", session, "--defaults-version", "1"].map(Into::into)).is_err());
        assert!(matches!(
            parse(
                [
                    "send",
                    session,
                    "--command-id",
                    session,
                    "--defaults-version",
                    "18446744073709551615",
                ]
                .map(Into::into)
            ),
            Ok(ParseOutcome::Run(super::Arguments {
                command: Command::Send { .. },
                ..
            }))
        ));
    }

    /// S30: model replacement recovery accepts only the complete set of
    /// pre-mutation facts printed by the client.
    #[test]
    fn s30_model_recovery_flags_are_one_complete_defaults_observation() {
        let session = "00000000-0000-0000-0000-000000000001";
        let selection = "00000000-0000-0000-0000-000000000002";
        assert!(
            parse(
                [
                    "model",
                    session,
                    "--model",
                    selection,
                    "--command-id",
                    session,
                    "--defaults-version",
                    "1",
                ]
                .map(Into::into)
            )
            .is_err()
        );
        assert!(matches!(
            parse(
                [
                    "model",
                    session,
                    "--model",
                    selection,
                    "--command-id",
                    session,
                    "--defaults-version",
                    "1",
                    "--dangerous-tool-auto-approval",
                    "approve-all",
                ]
                .map(Into::into)
            ),
            Ok(ParseOutcome::Run(Arguments {
                command: Command::Model {
                    dangerous_tool_auto_approval: Some(
                        DangerousToolAutoApprovalArgument::ApproveAll
                    ),
                    ..
                },
                ..
            }))
        ));
    }

    /// S07: stop recovery accepts only the complete printed observation —
    /// command identity, defaults version, and the exact expected turn.
    #[test]
    fn s07_stop_recovery_flags_are_one_complete_observation() {
        let session = "00000000-0000-0000-0000-000000000001";
        let turn = "00000000-0000-0000-0000-000000000002";

        assert!(parse(["stop", session, "--command-id", session].map(Into::into)).is_err());
        assert!(
            parse(
                [
                    "stop",
                    session,
                    "--command-id",
                    session,
                    "--defaults-version",
                    "1",
                ]
                .map(Into::into)
            )
            .is_err()
        );
        assert!(matches!(
            parse(["stop", session].map(Into::into)),
            Ok(ParseOutcome::Run(Arguments {
                command: Command::Stop {
                    turn_id: None,
                    command_id: None,
                    defaults_version: None,
                    ..
                },
                ..
            }))
        ));
        assert!(matches!(
            parse(
                [
                    "stop",
                    session,
                    "--turn",
                    turn,
                    "--command-id",
                    session,
                    "--defaults-version",
                    "1",
                ]
                .map(Into::into)
            ),
            Ok(ParseOutcome::Run(Arguments {
                command: Command::Stop {
                    turn_id: Some(recovered_turn),
                    command_id: Some(_),
                    defaults_version: Some(_),
                    ..
                },
                ..
            })) if recovered_turn.to_string() == turn
        ));
    }

    /// S10: both decision verbs bind the session and the exact pending
    /// request, and deny requires its explicit reason.
    #[test]
    fn s10_decision_verbs_bind_session_request_and_deny_reason() {
        let session = "00000000-0000-0000-0000-000000000001";
        let tool_request = "00000000-0000-0000-0000-000000000002";

        assert!(parse(["approve", session].map(Into::into)).is_err());
        assert!(matches!(
            parse(["approve", session, tool_request].map(Into::into)),
            Ok(ParseOutcome::Run(Arguments {
                command: Command::Approve {
                    session_id,
                    tool_request_id,
                    command_id: None,
                },
                ..
            })) if session_id.to_string() == session
                && tool_request_id.to_string() == tool_request
        ));
        assert!(parse(["deny", session, tool_request].map(Into::into)).is_err());
        assert!(matches!(
            parse(
                [
                    "deny",
                    session,
                    tool_request,
                    "--reason",
                    "writes outside the workspace",
                ]
                .map(Into::into)
            ),
            Ok(ParseOutcome::Run(Arguments {
                command: Command::Deny {
                    session_id,
                    tool_request_id,
                    reason,
                    command_id: None,
                },
                ..
            })) if session_id.to_string() == session
                && tool_request_id.to_string() == tool_request
                && reason == "writes outside the workspace"
        ));
    }

    #[test]
    fn search_defaults_to_the_ordinary_non_archived_page() {
        let parsed = parse(["search"].map(Into::into)).expect("the bare search verb parses");

        let ParseOutcome::Run(arguments) = parsed else {
            panic!("the successful search parse runs the client");
        };
        let Command::Search(page) = arguments.command else {
            panic!("the successful search parse selects the search command");
        };
        assert_eq!(
            page,
            SessionMetadataPageRequest {
                required_tags: Vec::new(),
                title_contains: None,
                include_archived: false,
                page_size: CanonicalU64::new(50),
                after_session_id: None,
            }
        );
    }

    #[test]
    fn search_carries_every_named_filter_to_one_bounded_page() {
        let session = "00000000-0000-0000-0000-000000000001";
        let parsed = parse(
            [
                "search",
                "--title",
                "Active plan",
                "--tag",
                "daily",
                "--tag",
                "plan",
                "--include-archived",
                "--limit",
                "1",
                "--after",
                session,
            ]
            .map(Into::into),
        )
        .expect("every named search filter parses");

        let ParseOutcome::Run(arguments) = parsed else {
            panic!("the successful search parse runs the client");
        };
        let Command::Search(page) = arguments.command else {
            panic!("the successful search parse selects the search command");
        };
        assert_eq!(
            page,
            SessionMetadataPageRequest {
                required_tags: vec![String::from("daily"), String::from("plan")],
                title_contains: Some(String::from("Active plan")),
                include_archived: true,
                page_size: CanonicalU64::new(1),
                after_session_id: Some(CanonicalUuid::from_uuid(
                    Uuid::parse_str(session).expect("the fixture session is canonical UUID text")
                )),
            }
        );
    }

    #[test]
    fn search_rejects_a_result_limit_outside_the_admitted_page_bound() {
        assert!(parse(["search", "--limit", "0"].map(Into::into)).is_err());
        assert!(parse(["search", "--limit", "101"].map(Into::into)).is_err());
    }

    #[test]
    fn search_rejects_empty_filter_text() {
        assert!(parse(["search", "--title", ""].map(Into::into)).is_err());
        assert!(parse(["search", "--tag", ""].map(Into::into)).is_err());
    }

    #[test]
    fn nul_in_search_filter_text_is_rejected() {
        assert!(parse(["search", "--title", "before\0after"].map(Into::into)).is_err());
        assert!(parse(["search", "--tag", "before\0after"].map(Into::into)).is_err());
    }

    #[test]
    fn search_rejects_a_repeated_tag_before_socket_use() {
        assert!(parse(["search", "--tag", "daily", "--tag", "daily"].map(Into::into)).is_err());
    }

    #[test]
    fn search_rejects_more_required_tags_than_the_process_filter_admits() {
        let admitted = search_requiring_tags(MAX_SESSION_METADATA_REQUIRED_TAGS);
        let one_tag_beyond = search_requiring_tags(MAX_SESSION_METADATA_REQUIRED_TAGS + 1);

        assert!(parse(admitted).is_ok());
        assert!(parse(one_tag_beyond).is_err());
    }

    #[test]
    fn search_rejects_a_required_tag_beyond_the_indexed_byte_bound() {
        let admitted = "t".repeat(MAX_SESSION_METADATA_INDEXED_UTF8_BYTES);
        let one_byte_beyond = "t".repeat(MAX_SESSION_METADATA_INDEXED_UTF8_BYTES + 1);

        assert!(parse(["search", "--tag", admitted.as_str()].map(Into::into)).is_ok());
        assert!(parse(["search", "--tag", one_byte_beyond.as_str()].map(Into::into)).is_err());
    }

    #[test]
    fn search_rejects_filter_text_beyond_the_aggregate_byte_bound() {
        let whole_bound_title = "t".repeat(MAX_SESSION_METADATA_TOTAL_UTF8_BYTES);

        assert!(parse(["search", "--title", whole_bound_title.as_str()].map(Into::into)).is_ok());
        assert!(
            parse(
                [
                    "search",
                    "--title",
                    whole_bound_title.as_str(),
                    "--tag",
                    "daily",
                ]
                .map(Into::into)
            )
            .is_err()
        );
    }

    /// One `search` invocation requiring the given number of distinct tags.
    fn search_requiring_tags(tags: usize) -> Vec<OsString> {
        let mut values = vec![OsString::from("search")];
        for tag in 0..tags {
            values.push(OsString::from("--tag"));
            values.push(OsString::from(tag.to_string()));
        }
        values
    }

    #[test]
    fn reconcile_binds_both_the_session_and_the_parked_turn() {
        let session = "00000000-0000-0000-0000-000000000001";
        let turn = "00000000-0000-0000-0000-000000000002";

        assert!(parse(["reconcile", session].map(Into::into)).is_err());
        assert!(matches!(
            parse(["reconcile", session, turn].map(Into::into)),
            Ok(ParseOutcome::Run(Arguments {
                command: Command::Reconcile {
                    session_id,
                    turn_id,
                    command_id: None,
                    defaults_version: None,
                },
                ..
            })) if session_id.to_string() == session && turn_id.to_string() == turn
        ));
    }

    #[test]
    fn duplicate_global_options_are_rejected() {
        assert!(parse(["--raw-output", "--raw-output", "list"].map(Into::into)).is_err());
    }

    #[test]
    fn socket_option_is_accepted_after_the_subcommand() {
        assert!(matches!(
            parse(["list", "--socket", "/tmp/hub.sock"].map(Into::into)),
            Ok(ParseOutcome::Run(Arguments {
                socket: Some(path),
                raw_output: false,
                command: Command::List,
            })) if path == Path::new("/tmp/hub.sock")
        ));
    }

    #[test]
    fn raw_output_option_is_accepted_after_the_subcommand() {
        assert!(matches!(
            parse(
                [
                    "follow",
                    "00000000-0000-0000-0000-000000000001",
                    "--raw-output"
                ]
                .map(Into::into)
            ),
            Ok(ParseOutcome::Run(Arguments {
                raw_output: true,
                command: Command::Follow { .. },
                ..
            }))
        ));
    }

    #[test]
    fn create_requires_exactly_one_model_selection() {
        assert!(
            parse(
                [
                    "create",
                    "--model",
                    "00000000-0000-0000-0000-000000000001",
                    "--alias",
                    "00000000-0000-0000-0000-000000000002",
                ]
                .map(Into::into)
            )
            .is_err()
        );
        assert!(parse(["create"].map(Into::into)).is_err());
    }

    #[test]
    fn import_maps_one_explicit_supported_format_and_file() {
        let parsed = parse(["import", "--format", "codex", "rollout.jsonl"].map(Into::into))
            .expect("the explicit supported format and one path parse");

        assert_codex_file_import(parsed, Path::new("rollout.jsonl"));
    }

    #[test]
    fn import_maps_one_explicit_supported_format_and_scan_directory() {
        let parsed = parse(
            [
                "import",
                "--format",
                "codex",
                "--scan",
                "conversation-directory",
            ]
            .map(Into::into),
        )
        .expect("the explicit supported format and scan directory parse");

        assert_codex_scan_import(parsed, Path::new("conversation-directory"));
    }

    #[test]
    fn import_requires_an_explicit_format() {
        assert!(parse(["import", "rollout.jsonl"].map(Into::into)).is_err());
    }

    #[test]
    fn import_rejects_an_unsupported_format() {
        assert!(
            parse(["import", "--format", "future-format", "rollout.jsonl"].map(Into::into))
                .is_err()
        );
    }

    #[test]
    fn import_rejects_a_file_together_with_a_scan_directory() {
        assert!(
            parse(
                [
                    "import",
                    "--format",
                    "codex",
                    "rollout.jsonl",
                    "--scan",
                    "conversation-directory",
                ]
                .map(Into::into)
            )
            .is_err()
        );
    }

    #[track_caller]
    fn assert_codex_file_import(parsed: ParseOutcome, expected_path: &Path) {
        let ParseOutcome::Run(arguments) = parsed else {
            panic!("the successful import parse runs the client");
        };
        let Command::Import { format, source } = arguments.command else {
            panic!("the successful import parse selects the import command");
        };
        assert_eq!(format, ConversationImportFormat::CodexRolloutJsonlV1);
        let ImportSourceArgument::File(path) = source else {
            panic!("the positional import source selects one file");
        };
        assert_eq!(path, expected_path);
    }

    #[track_caller]
    fn assert_codex_scan_import(parsed: ParseOutcome, expected_path: &Path) {
        let ParseOutcome::Run(arguments) = parsed else {
            panic!("the successful import parse runs the client");
        };
        let Command::Import { format, source } = arguments.command else {
            panic!("the successful import parse selects the import command");
        };
        assert_eq!(format, ConversationImportFormat::CodexRolloutJsonlV1);
        let ImportSourceArgument::Scan(path) = source else {
            panic!("the scan option selects one directory");
        };
        assert_eq!(path, expected_path);
    }

    #[test]
    fn help_is_generated_by_clap() {
        let Ok(ParseOutcome::Help(help)) = parse([OsString::from("--help")]) else {
            panic!("help must be recognized");
        };
        assert!(help.contains("Usage: signalbox"));
        assert!(help.contains("Commands:"));
    }
}
