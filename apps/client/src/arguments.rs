use std::{ffi::OsString, fmt, iter, path::PathBuf};

use clap::{
    ArgGroup, Args as ClapArgs, CommandFactory, Parser, Subcommand, ValueEnum, error::ErrorKind,
};
use signalbox_process_protocol::{
    CanonicalU64, CanonicalUuid, CommandId, ConversationImportFormat, ImportedSessionRelationship,
    ModelSelection,
};
use uuid::Uuid;

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
    Continue {
        imported_conversation_id: CanonicalUuid,
        through_position: CanonicalU64,
        relationship: ImportedSessionRelationship,
        selection: ModelSelection,
        command_id: Option<CommandId>,
    },
    List,
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
    /// Create a live session from an imported conversation boundary.
    Continue(ContinueArguments),
    /// List current sessions.
    List,
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
#[command(group(
    ArgGroup::new("selection")
        .required(true)
        .multiple(false)
        .args(["model", "alias"])
))]
struct ContinueArguments {
    /// Imported conversation to continue from.
    #[arg(value_name = "IMPORTED_CONVERSATION", value_parser = canonical_uuid)]
    imported_conversation_id: CanonicalUuid,
    /// Inclusive positive imported entry position.
    #[arg(long, value_name = "DECIMAL", value_parser = positive_canonical_u64)]
    through_position: CanonicalU64,
    /// Record whether this session resumes or forks the imported boundary.
    #[arg(long, value_enum)]
    relationship: ImportedRelationshipArgument,
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

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ImportedRelationshipArgument {
    Resume,
    Fork,
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
        CliCommand::Continue(arguments) => Command::Continue {
            imported_conversation_id: arguments.imported_conversation_id,
            through_position: arguments.through_position,
            relationship: match arguments.relationship {
                ImportedRelationshipArgument::Resume => ImportedSessionRelationship::Resume,
                ImportedRelationshipArgument::Fork => ImportedSessionRelationship::Fork,
            },
            selection: match (arguments.model, arguments.alias) {
                (Some(selection_id), None) => ModelSelection::Direct { selection_id },
                (None, Some(alias_id)) => ModelSelection::Alias { alias_id },
                (None, None) | (Some(_), Some(_)) => {
                    return Err(UsageError(Cli::command().error(
                        ErrorKind::ArgumentConflict,
                        "continue requires exactly one of --model or --alias",
                    )));
                }
            },
            command_id: arguments.command_id,
        },
        CliCommand::List => Command::List,
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

fn positive_canonical_u64(value: &str) -> Result<CanonicalU64, String> {
    let parsed = canonical_u64(value)?;
    if parsed.value() == 0 {
        return Err("decimal value must be positive".to_owned());
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, path::Path};

    use signalbox_process_protocol::{ConversationImportFormat, ImportedSessionRelationship};

    use super::{
        Arguments, Command, DangerousToolAutoApprovalArgument, ImportSourceArgument, ParseOutcome,
        UsageError, parse,
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
    fn continue_maps_an_explicit_imported_frontier_and_relationship() {
        let parsed = parse(
            [
                "continue",
                "00000000-0000-0000-0000-000000000001",
                "--through-position",
                "2",
                "--relationship",
                "resume",
                "--model",
                "00000000-0000-0000-0000-000000000002",
            ]
            .map(Into::into),
        );

        assert_continue_parse(parsed, 2);
    }

    #[test]
    fn continue_rejects_zero_position() {
        let conversation = "00000000-0000-0000-0000-000000000001";
        let model = "00000000-0000-0000-0000-000000000002";
        assert!(
            parse(
                [
                    "continue",
                    conversation,
                    "--through-position",
                    "0",
                    "--relationship",
                    "resume",
                    "--model",
                    model,
                ]
                .map(Into::into)
            )
            .is_err()
        );
    }

    #[test]
    fn continue_requires_explicit_relationship() {
        let conversation = "00000000-0000-0000-0000-000000000001";
        let model = "00000000-0000-0000-0000-000000000002";
        assert!(
            parse(
                [
                    "continue",
                    conversation,
                    "--through-position",
                    "1",
                    "--model",
                    model,
                ]
                .map(Into::into)
            )
            .is_err()
        );
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
    fn assert_continue_parse(parsed: Result<ParseOutcome, UsageError>, expected_position: u64) {
        let Ok(ParseOutcome::Run(arguments)) = parsed else {
            panic!("the successful continue parse runs the client");
        };
        let Command::Continue {
            through_position,
            relationship,
            ..
        } = arguments.command
        else {
            panic!("the successful continue parse selects continue");
        };
        assert_eq!(through_position.value(), expected_position);
        assert_eq!(relationship, ImportedSessionRelationship::Resume);
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
