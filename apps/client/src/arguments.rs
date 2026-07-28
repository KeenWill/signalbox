use std::{ffi::OsString, fmt, iter, path::PathBuf};

use clap::{
    ArgGroup, Args as ClapArgs, CommandFactory, Parser, Subcommand, ValueEnum, error::ErrorKind,
};
use signalbox_process_protocol::{
    CanonicalU64, CanonicalUuid, CommandId, ConversationCursor, ConversationImportFormat,
    ConversationOrigin, ConversationOriginFilter, ImportedSessionRelationship,
    MAX_SESSION_METADATA_INDEXED_UTF8_BYTES, MAX_SESSION_METADATA_REQUIRED_TAGS,
    MAX_SESSION_METADATA_TOTAL_UTF8_BYTES, ModelSelection, ReviewDiffSide, ReviewFindingInput,
    ReviewSeverity, ReviewTargetSubject, ReviewWorkflow,
};
use uuid::Uuid;

use crate::{
    ConversationsPageRequest, MAX_METADATA_PAGE_SIZE, MIN_METADATA_PAGE_SIZE,
    SessionMetadataPageRequest,
};

/// The specification's ordinary default metadata page size.
const DEFAULT_SEARCH_RESULT_LIMIT: &str = "50";

#[derive(Debug)]
pub(crate) struct Arguments {
    pub(crate) socket: Option<PathBuf>,
    pub(crate) raw_output: bool,
    pub(crate) command: Command,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SendDeliveryArgument {
    StartWhenIdle,
    Queue {
        expected_active_turn_id: Option<CanonicalUuid>,
    },
}

#[derive(Debug)]
pub(crate) enum Command {
    Create {
        selection: Option<ModelSelection>,
        template: Option<String>,
        command_id: Option<CommandId>,
        system_prompt_file: Option<PathBuf>,
    },
    Continue {
        imported_conversation_id: CanonicalUuid,
        through_position: CanonicalU64,
        relationship: ImportedSessionRelationship,
        selection: ModelSelection,
        command_id: Option<CommandId>,
    },
    List,
    Templates,
    Search(SessionMetadataPageRequest),
    Conversations(ConversationsPageRequest),
    Send {
        session_id: CanonicalUuid,
        command_id: Option<CommandId>,
        defaults_version: Option<CanonicalU64>,
        delivery: SendDeliveryArgument,
    },
    Steer {
        session_id: CanonicalUuid,
        command_id: Option<CommandId>,
        turn_id: Option<CanonicalUuid>,
    },
    Model {
        session_id: CanonicalUuid,
        selection: ModelSelection,
        command_id: Option<CommandId>,
        defaults_version: Option<CanonicalU64>,
        dangerous_tool_auto_approval: Option<DangerousToolAutoApprovalArgument>,
        system_prompt: SystemPromptArgument,
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
    Review(Box<ReviewCommand>),
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
pub(crate) enum ReviewCommand {
    CreateTarget {
        command_id: Option<CommandId>,
        target_id: CanonicalUuid,
        provider: String,
        repository: String,
        subject: ReviewTargetSubject,
        head_revision: String,
        base_revision: Option<String>,
        stack_parent_target_id: Option<CanonicalUuid>,
    },
    StartRun {
        command_id: Option<CommandId>,
        target_id: CanonicalUuid,
        run_id: CanonicalUuid,
        pass_id: CanonicalUuid,
        workflow: ReviewWorkflow,
        session_id: CanonicalUuid,
        accepted_input_id: CanonicalUuid,
    },
    ActivatePass {
        command_id: Option<CommandId>,
        run_id: CanonicalUuid,
        pass_id: CanonicalUuid,
        turn_id: CanonicalUuid,
    },
    RecordFinding {
        command_id: Option<CommandId>,
        run_id: CanonicalUuid,
        pass_id: CanonicalUuid,
        turn_id: CanonicalUuid,
        output_frontier_id: CanonicalUuid,
        finding: ReviewFindingInput,
    },
    ListFindings {
        run_id: CanonicalUuid,
    },
    ReadTarget {
        target_id: CanonicalUuid,
    },
    ReadRun {
        run_id: CanonicalUuid,
    },
    ReadFinding {
        finding_id: CanonicalUuid,
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
    /// List available session templates.
    Templates,
    /// Read one filtered page of current session metadata.
    Search(SearchArguments),
    /// Read one filtered page listing native sessions and imported
    /// conversations together.
    Conversations(ConversationsArguments),
    /// Submit standard input and print the reply after completion.
    Send(SendArguments),
    /// Steer the active turn with standard-input content.
    Steer(SteerArguments),
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
    /// Drive and inspect headless review workflows.
    Review(ReviewArguments),
    /// Stop the active turn and continue with standard-input content.
    Stop(StopArguments),
    /// Approve one pending tool request.
    Approve(DecideArguments),
    /// Deny one pending tool request with an explicit reason.
    Deny(DenyArguments),
}

#[derive(Debug, ClapArgs)]
struct ReviewArguments {
    #[command(subcommand)]
    command: ReviewSubcommand,
}

#[derive(Debug, Subcommand)]
enum ReviewSubcommand {
    /// Register one immutable external change-request snapshot.
    CreateTarget(CreateReviewTargetArguments),
    /// Admit one queued run and its sole session-backed pass.
    StartRun(StartReviewRunArguments),
    /// Bind a queued pass to its canonical active turn.
    ActivatePass(ActivateReviewPassArguments),
    /// Conclude one read-only pass with exactly one finding.
    RecordFinding(RecordReviewFindingArguments),
    /// List findings for one exact run.
    ListFindings(ReviewRunArguments),
    /// Read one target snapshot.
    ReadTarget(ReviewTargetArguments),
    /// Read one run and pass projection.
    ReadRun(ReviewRunArguments),
    /// Read one finding aggregate.
    ReadFinding(ReviewFindingArguments),
}

#[derive(Debug, ClapArgs)]
struct CreateReviewTargetArguments {
    #[arg(value_name = "TARGET", value_parser = canonical_uuid)]
    target_id: CanonicalUuid,
    #[arg(long)]
    provider: String,
    #[arg(long)]
    repository: String,
    #[arg(long, value_name = "DECIMAL", value_parser = positive_canonical_u64)]
    change_request: CanonicalU64,
    #[arg(long)]
    head_revision: String,
    #[arg(long)]
    base_revision: String,
    #[arg(long, value_name = "TARGET", value_parser = canonical_uuid)]
    stack_parent_target_id: Option<CanonicalUuid>,
    #[arg(long, value_name = "UUID", value_parser = command_id)]
    command_id: Option<CommandId>,
}

#[derive(Debug, ClapArgs)]
struct StartReviewRunArguments {
    #[arg(value_name = "TARGET", value_parser = canonical_uuid)]
    target_id: CanonicalUuid,
    #[arg(value_name = "RUN", value_parser = canonical_uuid)]
    run_id: CanonicalUuid,
    #[arg(value_name = "PASS", value_parser = canonical_uuid)]
    pass_id: CanonicalUuid,
    #[arg(long, value_enum)]
    workflow: ReviewWorkflowArgument,
    #[arg(long, value_name = "SESSION", value_parser = canonical_uuid)]
    session_id: CanonicalUuid,
    #[arg(long, value_name = "INPUT", value_parser = canonical_uuid)]
    accepted_input_id: CanonicalUuid,
    #[arg(long, value_name = "UUID", value_parser = command_id)]
    command_id: Option<CommandId>,
}

#[derive(Debug, ClapArgs)]
struct ActivateReviewPassArguments {
    #[arg(value_name = "RUN", value_parser = canonical_uuid)]
    run_id: CanonicalUuid,
    #[arg(value_name = "PASS", value_parser = canonical_uuid)]
    pass_id: CanonicalUuid,
    #[arg(long, value_name = "TURN", value_parser = canonical_uuid)]
    turn_id: CanonicalUuid,
    #[arg(long, value_name = "UUID", value_parser = command_id)]
    command_id: Option<CommandId>,
}

#[derive(Debug, ClapArgs)]
struct RecordReviewFindingArguments {
    #[arg(value_name = "RUN", value_parser = canonical_uuid)]
    run_id: CanonicalUuid,
    #[arg(value_name = "PASS", value_parser = canonical_uuid)]
    pass_id: CanonicalUuid,
    #[arg(long, value_name = "TURN", value_parser = canonical_uuid)]
    turn_id: CanonicalUuid,
    #[arg(long, value_name = "FRONTIER", value_parser = canonical_uuid)]
    output_frontier_id: CanonicalUuid,
    #[arg(long, value_name = "FINDING", value_parser = canonical_uuid)]
    finding_id: CanonicalUuid,
    #[arg(long)]
    file_path: String,
    #[arg(long, requires = "line_end", value_parser = review_line_number)]
    line_start: Option<CanonicalU64>,
    #[arg(long, requires = "line_start", value_parser = review_line_number)]
    line_end: Option<CanonicalU64>,
    #[arg(long, value_enum)]
    diff_side: Option<ReviewDiffSideArgument>,
    #[arg(long)]
    title: String,
    #[arg(long)]
    body: String,
    #[arg(long, value_enum)]
    severity: ReviewSeverityArgument,
    #[arg(long, value_parser = review_confidence)]
    confidence: CanonicalU64,
    #[arg(long)]
    category: String,
    #[arg(long)]
    recommended_fix: Option<String>,
    #[arg(long, value_name = "UUID", value_parser = command_id)]
    command_id: Option<CommandId>,
}

#[derive(Debug, ClapArgs)]
struct ReviewTargetArguments {
    #[arg(value_name = "TARGET", value_parser = canonical_uuid)]
    target_id: CanonicalUuid,
}

#[derive(Debug, ClapArgs)]
struct ReviewRunArguments {
    #[arg(value_name = "RUN", value_parser = canonical_uuid)]
    run_id: CanonicalUuid,
}

#[derive(Debug, ClapArgs)]
struct ReviewFindingArguments {
    #[arg(value_name = "FINDING", value_parser = canonical_uuid)]
    finding_id: CanonicalUuid,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ReviewWorkflowArgument {
    ImportExternalContext,
    ReadOnlyReview,
    JudgeFindings,
    DedupeFindings,
    PublishReview,
    FixFindings,
    PropagateStack,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ReviewDiffSideArgument {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ReviewSeverityArgument {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, ClapArgs)]
#[command(group(
    ArgGroup::new("creation_source")
        .required(true)
        .multiple(false)
        .args(["model", "alias", "template"])
))]
struct CreateArguments {
    /// Select a model configuration directly.
    #[arg(long, value_name = "UUID", value_parser = canonical_uuid)]
    model: Option<CanonicalUuid>,
    /// Select a configured model alias.
    #[arg(long, value_name = "UUID", value_parser = canonical_uuid)]
    alias: Option<CanonicalUuid>,
    /// Copy the named daemon-owned template bundle at creation.
    #[arg(
        long,
        value_name = "NAME",
        conflicts_with_all = ["model", "alias", "system_prompt_file"]
    )]
    template: Option<String>,
    /// Read the exact optional session system prompt from one file.
    #[arg(long, value_name = "PATH")]
    system_prompt_file: Option<PathBuf>,
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
struct ConversationsArguments {
    /// Require an exact case-sensitive title substring.
    #[arg(long, value_name = "SUBSTRING", value_parser = metadata_filter_text)]
    title: Option<String>,
    /// Select native sessions, imported conversations, or both.
    #[arg(long, value_name = "ORIGIN", value_enum, default_value_t = ConversationOriginArgument::All)]
    origin: ConversationOriginArgument,
    /// Include archived native sessions, which the default view excludes.
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
    /// Continue after the exact origin-qualified cursor a prior page printed.
    #[arg(long, value_name = "ORIGIN:UUID", value_parser = conversation_cursor)]
    after: Option<ConversationCursor>,
}

#[derive(Clone, Copy, Debug, PartialEq, ValueEnum)]
enum ConversationOriginArgument {
    /// Native sessions only.
    Native,
    /// Imported conversations only.
    Imported,
    /// Both origin classes.
    All,
}

impl ConversationOriginArgument {
    const fn wire(self) -> ConversationOriginFilter {
        match self {
            Self::Native => ConversationOriginFilter::Native,
            Self::Imported => ConversationOriginFilter::Imported,
            Self::All => ConversationOriginFilter::All,
        }
    }
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
    /// Queue this input behind the exact active turn instead of requiring an
    /// idle session slot.
    #[arg(long)]
    queue: bool,
    /// Exact expected active turn paired with queued-input recovery values.
    #[arg(
        long,
        value_name = "UUID",
        requires_all = ["queue", "command_id", "defaults_version"],
        value_parser = canonical_uuid
    )]
    turn: Option<CanonicalUuid>,
}

#[derive(Debug, ClapArgs)]
struct SteerArguments {
    /// Session whose active turn should receive standard-input steering.
    #[arg(value_name = "SESSION", value_parser = canonical_uuid)]
    session_id: CanonicalUuid,
    /// Reuse an exact non-reserved durable command identity.
    #[arg(long, value_name = "UUID", requires = "turn", value_parser = command_id)]
    command_id: Option<CommandId>,
    /// Exact expected active turn paired with a recovery command identity.
    #[arg(long, value_name = "UUID", requires = "command_id", value_parser = canonical_uuid)]
    turn: Option<CanonicalUuid>,
}

#[derive(Debug, ClapArgs)]
#[command(group(
    ArgGroup::new("selection")
        .required(true)
        .multiple(false)
        .args(["model", "alias"])
))]
#[command(group(
    ArgGroup::new("system_prompt")
        .required(false)
        .multiple(false)
        .args(["system_prompt_file", "clear_system_prompt"])
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
    /// Read the exact replacement session system prompt from one file.
    #[arg(long, value_name = "PATH")]
    system_prompt_file: Option<PathBuf>,
    /// Install the replacement epoch without a session system prompt.
    #[arg(long)]
    clear_system_prompt: bool,
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

/// The model verb's replacement choice for the session system prompt.
#[derive(Debug)]
pub(crate) enum SystemPromptArgument {
    /// Copy the exact current prompt forward unchanged.
    Keep,
    /// Replace the prompt with the exact content of one file.
    File(PathBuf),
    /// Install the replacement epoch without a prompt.
    Clear,
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
                (Some(selection_id), None) => Some(ModelSelection::Direct { selection_id }),
                (None, Some(alias_id)) => Some(ModelSelection::Alias { alias_id }),
                (None, None) if arguments.template.is_some() => None,
                (None, None) | (Some(_), Some(_)) => {
                    return Err(UsageError(Cli::command().error(
                        ErrorKind::ArgumentConflict,
                        "create requires exactly one of --model, --alias, or --template",
                    )));
                }
            },
            template: arguments.template,
            command_id: arguments.command_id,
            system_prompt_file: arguments.system_prompt_file,
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
        CliCommand::Templates => Command::Templates,
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
        CliCommand::Conversations(arguments) => {
            if arguments.title.as_deref().map_or(0, str::len)
                > MAX_SESSION_METADATA_TOTAL_UTF8_BYTES
            {
                return Err(UsageError(Cli::command().error(
                    ErrorKind::ValueValidation,
                    format!(
                        "the --title query carries at most \
                         {MAX_SESSION_METADATA_TOTAL_UTF8_BYTES} UTF-8 bytes"
                    ),
                )));
            }
            Command::Conversations(ConversationsPageRequest {
                title_contains: arguments.title,
                origin: arguments.origin.wire(),
                include_archived: arguments.include_archived,
                page_size: arguments.limit,
                after: arguments.after,
            })
        }
        CliCommand::Send(arguments) => {
            if arguments.queue
                && !matches!(
                    (
                        arguments.command_id,
                        arguments.defaults_version,
                        arguments.turn,
                    ),
                    (None, None, None) | (Some(_), Some(_), Some(_))
                )
            {
                return Err(UsageError(Cli::command().error(
                    ErrorKind::ArgumentConflict,
                    "queued send recovery requires --command-id, --defaults-version, and --turn together",
                )));
            }
            let delivery = if arguments.queue {
                SendDeliveryArgument::Queue {
                    expected_active_turn_id: arguments.turn,
                }
            } else {
                SendDeliveryArgument::StartWhenIdle
            };
            Command::Send {
                session_id: arguments.session_id,
                command_id: arguments.command_id,
                defaults_version: arguments.defaults_version,
                delivery,
            }
        }
        CliCommand::Steer(arguments) => Command::Steer {
            session_id: arguments.session_id,
            command_id: arguments.command_id,
            turn_id: arguments.turn,
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
            system_prompt: match (arguments.system_prompt_file, arguments.clear_system_prompt) {
                (Some(path), false) => SystemPromptArgument::File(path),
                (None, true) => SystemPromptArgument::Clear,
                (None, false) => SystemPromptArgument::Keep,
                (Some(_), true) => {
                    return Err(UsageError(Cli::command().error(
                        ErrorKind::ArgumentConflict,
                        "model admits at most one of --system-prompt-file or --clear-system-prompt",
                    )));
                }
            },
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
        CliCommand::Review(arguments) => Command::Review(Box::new(match arguments.command {
            ReviewSubcommand::CreateTarget(arguments) => ReviewCommand::CreateTarget {
                command_id: arguments.command_id,
                target_id: arguments.target_id,
                provider: arguments.provider,
                repository: arguments.repository,
                subject: ReviewTargetSubject::ChangeRequest {
                    number: arguments.change_request,
                },
                head_revision: arguments.head_revision,
                base_revision: Some(arguments.base_revision),
                stack_parent_target_id: arguments.stack_parent_target_id,
            },
            ReviewSubcommand::StartRun(arguments) => ReviewCommand::StartRun {
                command_id: arguments.command_id,
                target_id: arguments.target_id,
                run_id: arguments.run_id,
                pass_id: arguments.pass_id,
                workflow: match arguments.workflow {
                    ReviewWorkflowArgument::ImportExternalContext => {
                        ReviewWorkflow::ImportExternalContext
                    }
                    ReviewWorkflowArgument::ReadOnlyReview => ReviewWorkflow::ReadOnlyReview,
                    ReviewWorkflowArgument::JudgeFindings => ReviewWorkflow::JudgeFindings,
                    ReviewWorkflowArgument::DedupeFindings => ReviewWorkflow::DedupeFindings,
                    ReviewWorkflowArgument::PublishReview => ReviewWorkflow::PublishReview,
                    ReviewWorkflowArgument::FixFindings => ReviewWorkflow::FixFindings,
                    ReviewWorkflowArgument::PropagateStack => ReviewWorkflow::PropagateStack,
                },
                session_id: arguments.session_id,
                accepted_input_id: arguments.accepted_input_id,
            },
            ReviewSubcommand::ActivatePass(arguments) => ReviewCommand::ActivatePass {
                command_id: arguments.command_id,
                run_id: arguments.run_id,
                pass_id: arguments.pass_id,
                turn_id: arguments.turn_id,
            },
            ReviewSubcommand::RecordFinding(arguments) => {
                if arguments
                    .line_start
                    .zip(arguments.line_end)
                    .is_some_and(|(start, end)| end.value() < start.value())
                {
                    return Err(UsageError(Cli::command().error(
                        ErrorKind::ArgumentConflict,
                        "--line-end must not precede --line-start",
                    )));
                }
                ReviewCommand::RecordFinding {
                    command_id: arguments.command_id,
                    run_id: arguments.run_id,
                    pass_id: arguments.pass_id,
                    turn_id: arguments.turn_id,
                    output_frontier_id: arguments.output_frontier_id,
                    finding: ReviewFindingInput {
                        finding_id: arguments.finding_id,
                        file_path: arguments.file_path,
                        line_start: arguments.line_start,
                        line_end: arguments.line_end,
                        diff_side: arguments.diff_side.map(|side| match side {
                            ReviewDiffSideArgument::Left => ReviewDiffSide::Left,
                            ReviewDiffSideArgument::Right => ReviewDiffSide::Right,
                        }),
                        title: arguments.title,
                        body: arguments.body,
                        severity: match arguments.severity {
                            ReviewSeverityArgument::Info => ReviewSeverity::Info,
                            ReviewSeverityArgument::Low => ReviewSeverity::Low,
                            ReviewSeverityArgument::Medium => ReviewSeverity::Medium,
                            ReviewSeverityArgument::High => ReviewSeverity::High,
                            ReviewSeverityArgument::Critical => ReviewSeverity::Critical,
                        },
                        confidence: arguments.confidence,
                        category: arguments.category,
                        recommended_fix: arguments.recommended_fix,
                    },
                }
            }
            ReviewSubcommand::ListFindings(arguments) => ReviewCommand::ListFindings {
                run_id: arguments.run_id,
            },
            ReviewSubcommand::ReadTarget(arguments) => ReviewCommand::ReadTarget {
                target_id: arguments.target_id,
            },
            ReviewSubcommand::ReadRun(arguments) => ReviewCommand::ReadRun {
                run_id: arguments.run_id,
            },
            ReviewSubcommand::ReadFinding(arguments) => ReviewCommand::ReadFinding {
                finding_id: arguments.finding_id,
            },
        })),
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

/// Parses one origin-qualified unified cursor exactly as a prior page printed
/// it: `native:<uuid>` or `imported:<uuid>`.
fn conversation_cursor(value: &str) -> Result<ConversationCursor, String> {
    let Some((origin, identity)) = value.split_once(':') else {
        return Err("the cursor must be native:<uuid> or imported:<uuid>".to_owned());
    };
    let origin = match origin {
        "native" => ConversationOrigin::NativeSession,
        "imported" => ConversationOrigin::ImportedConversation,
        _ => return Err("the cursor origin must be native or imported".to_owned()),
    };
    Ok(ConversationCursor::new(origin, canonical_uuid(identity)?))
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

fn positive_canonical_u64(value: &str) -> Result<CanonicalU64, String> {
    let parsed = canonical_u64(value)?;
    if parsed.value() == 0 {
        return Err("decimal value must be positive".to_owned());
    }
    Ok(parsed)
}

fn review_line_number(value: &str) -> Result<CanonicalU64, String> {
    let parsed = positive_canonical_u64(value)?;
    if parsed.value() > u64::from(u32::MAX) {
        return Err("review line number exceeds the unsigned 32-bit range".to_owned());
    }
    Ok(parsed)
}

fn review_confidence(value: &str) -> Result<CanonicalU64, String> {
    const MAXIMUM_REVIEW_CONFIDENCE_BASIS_POINTS: u64 = 10_000;

    let parsed = canonical_u64(value)?;
    if parsed.value() > MAXIMUM_REVIEW_CONFIDENCE_BASIS_POINTS {
        return Err(format!(
            "review confidence must not exceed \
             {MAXIMUM_REVIEW_CONFIDENCE_BASIS_POINTS} basis points"
        ));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, path::Path};

    use signalbox_process_protocol::{
        CanonicalU64, CanonicalUuid, ConversationCursor, ConversationImportFormat,
        ConversationOrigin, ConversationOriginFilter, ImportedSessionRelationship,
        MAX_SESSION_METADATA_INDEXED_UTF8_BYTES, MAX_SESSION_METADATA_REQUIRED_TAGS,
        MAX_SESSION_METADATA_TOTAL_UTF8_BYTES,
    };
    use uuid::Uuid;

    use super::{
        Arguments, Command, ConversationsPageRequest, DangerousToolAutoApprovalArgument,
        ImportSourceArgument, ParseOutcome, SendDeliveryArgument, SessionMetadataPageRequest,
        UsageError, parse,
    };

    #[derive(Clone, Copy)]
    struct ReviewFindingArgumentFixture {
        line_start: &'static str,
        line_end: &'static str,
        confidence: &'static str,
    }

    fn review_finding_arguments(fixture: ReviewFindingArgumentFixture) -> Vec<OsString> {
        const RUN_ID: &str = "00000000-0000-0000-0000-000000000001";
        const PASS_ID: &str = "00000000-0000-0000-0000-000000000002";
        const TURN_ID: &str = "00000000-0000-0000-0000-000000000003";
        const FRONTIER_ID: &str = "00000000-0000-0000-0000-000000000004";
        const FINDING_ID: &str = "00000000-0000-0000-0000-000000000005";

        vec![
            "review",
            "record-finding",
            RUN_ID,
            PASS_ID,
            "--turn-id",
            TURN_ID,
            "--output-frontier-id",
            FRONTIER_ID,
            "--finding-id",
            FINDING_ID,
            "--file-path",
            "src/lib.rs",
            "--line-start",
            fixture.line_start,
            "--line-end",
            fixture.line_end,
            "--title",
            "fixture finding",
            "--body",
            "fixture body",
            "--severity",
            "high",
            "--confidence",
            fixture.confidence,
            "--category",
            "correctness",
        ]
        .into_iter()
        .map(OsString::from)
        .collect()
    }

    #[test]
    fn review_finding_rejects_domain_scalar_bounds_locally() {
        let zero_line = parse(review_finding_arguments(ReviewFindingArgumentFixture {
            line_start: "0",
            line_end: "1",
            confidence: "9000",
        }));
        let oversized_line = parse(review_finding_arguments(ReviewFindingArgumentFixture {
            line_start: "1",
            line_end: "4294967296",
            confidence: "9000",
        }));
        let reversed_line = parse(review_finding_arguments(ReviewFindingArgumentFixture {
            line_start: "9",
            line_end: "7",
            confidence: "9000",
        }));
        let oversized_confidence = parse(review_finding_arguments(ReviewFindingArgumentFixture {
            line_start: "1",
            line_end: "1",
            confidence: "10001",
        }));

        assert!(zero_line.is_err());
        assert!(oversized_line.is_err());
        assert!(reversed_line.is_err());
        assert!(oversized_confidence.is_err());
    }

    #[test]
    fn review_target_rejects_zero_change_request_number() {
        const TARGET_ID: &str = "00000000-0000-0000-0000-000000000001";

        let result = parse(
            [
                "review",
                "create-target",
                TARGET_ID,
                "--provider",
                "example-host",
                "--repository",
                "owner/repository",
                "--change-request",
                "0",
                "--head-revision",
                "head",
                "--base-revision",
                "base",
            ]
            .map(Into::into),
        );

        assert!(result.is_err());
    }

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

    #[test]
    fn queued_send_accepts_fresh_intent_without_recovery_values() {
        const SESSION: &str = "00000000-0000-0000-0000-000000000001";

        assert!(matches!(
            parse(["send", SESSION, "--queue"].map(Into::into)),
            Ok(ParseOutcome::Run(super::Arguments {
                command: Command::Send {
                    delivery: SendDeliveryArgument::Queue {
                        expected_active_turn_id: None,
                    },
                    ..
                },
                ..
            }))
        ));
    }

    #[test]
    fn queued_send_recovery_requires_command_defaults_and_turn_together() {
        const SESSION: &str = "00000000-0000-0000-0000-000000000001";
        const TURN: &str = "00000000-0000-0000-0000-000000000002";

        assert!(
            parse(
                [
                    "send",
                    SESSION,
                    "--queue",
                    "--command-id",
                    SESSION,
                    "--defaults-version",
                    "1",
                ]
                .map(Into::into)
            )
            .is_err()
        );
        assert!(parse(["send", SESSION, "--queue", "--turn", TURN].map(Into::into)).is_err());
        assert!(matches!(
            parse(
                [
                    "send",
                    SESSION,
                    "--queue",
                    "--command-id",
                    SESSION,
                    "--defaults-version",
                    "1",
                    "--turn",
                    TURN,
                ]
                .map(Into::into)
            ),
            Ok(ParseOutcome::Run(super::Arguments {
                command: Command::Send {
                    delivery: SendDeliveryArgument::Queue {
                        expected_active_turn_id: Some(_),
                    },
                    ..
                },
                ..
            }))
        ));
    }

    #[test]
    fn steer_recovery_requires_command_and_turn_together() {
        const SESSION: &str = "00000000-0000-0000-0000-000000000001";
        const TURN: &str = "00000000-0000-0000-0000-000000000002";

        assert!(matches!(
            parse(["steer", SESSION].map(Into::into)),
            Ok(ParseOutcome::Run(super::Arguments {
                command: Command::Steer {
                    command_id: None,
                    turn_id: None,
                    ..
                },
                ..
            }))
        ));
        assert!(parse(["steer", SESSION, "--command-id", SESSION].map(Into::into)).is_err());
        assert!(parse(["steer", SESSION, "--turn", TURN].map(Into::into)).is_err());
        assert!(matches!(
            parse(["steer", SESSION, "--command-id", SESSION, "--turn", TURN,].map(Into::into)),
            Ok(ParseOutcome::Run(super::Arguments {
                command: Command::Steer {
                    command_id: Some(_),
                    turn_id: Some(_),
                    ..
                },
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
    fn conversations_defaults_to_the_unfiltered_unified_view() {
        let parsed =
            parse(["conversations"].map(Into::into)).expect("the bare conversations verb parses");

        let ParseOutcome::Run(arguments) = parsed else {
            panic!("the successful conversations parse runs the client");
        };
        let Command::Conversations(page) = arguments.command else {
            panic!("the successful conversations parse selects the conversations command");
        };
        assert_eq!(
            page,
            ConversationsPageRequest {
                title_contains: None,
                origin: ConversationOriginFilter::All,
                include_archived: false,
                page_size: CanonicalU64::new(50),
                after: None,
            }
        );
    }

    #[test]
    fn conversations_carries_every_named_filter_to_one_bounded_page() {
        let cursor_identity = "00000000-0000-0000-0000-000000000001";
        let parsed = parse(
            [
                "conversations",
                "--title",
                "Active plan",
                "--origin",
                "imported",
                "--include-archived",
                "--limit",
                "1",
                "--after",
                "imported:00000000-0000-0000-0000-000000000001",
            ]
            .map(Into::into),
        )
        .expect("every named conversations filter parses");

        let ParseOutcome::Run(arguments) = parsed else {
            panic!("the successful conversations parse runs the client");
        };
        let Command::Conversations(page) = arguments.command else {
            panic!("the successful conversations parse selects the conversations command");
        };
        assert_eq!(
            page,
            ConversationsPageRequest {
                title_contains: Some(String::from("Active plan")),
                origin: ConversationOriginFilter::Imported,
                include_archived: true,
                page_size: CanonicalU64::new(1),
                after: Some(ConversationCursor::new(
                    ConversationOrigin::ImportedConversation,
                    CanonicalUuid::from_uuid(
                        Uuid::parse_str(cursor_identity)
                            .expect("the fixture cursor identity is canonical UUID text")
                    ),
                )),
            }
        );
    }

    #[test]
    fn conversations_accepts_the_native_cursor_origin_spelling() {
        let parsed = parse(
            [
                "conversations",
                "--after",
                "native:00000000-0000-0000-0000-000000000002",
            ]
            .map(Into::into),
        )
        .expect("the native cursor origin parses");

        let ParseOutcome::Run(arguments) = parsed else {
            panic!("the successful conversations parse runs the client");
        };
        let Command::Conversations(page) = arguments.command else {
            panic!("the successful conversations parse selects the conversations command");
        };
        assert_eq!(
            page.after,
            Some(ConversationCursor::new(
                ConversationOrigin::NativeSession,
                CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            ))
        );
    }

    #[test]
    fn conversations_rejects_a_result_limit_outside_the_admitted_page_bound() {
        assert!(parse(["conversations", "--limit", "0"].map(Into::into)).is_err());
        assert!(parse(["conversations", "--limit", "101"].map(Into::into)).is_err());
    }

    #[test]
    fn conversations_rejects_empty_or_nul_title_text() {
        assert!(parse(["conversations", "--title", ""].map(Into::into)).is_err());
        assert!(parse(["conversations", "--title", "before\0after"].map(Into::into)).is_err());
    }

    #[test]
    fn conversations_rejects_a_title_beyond_the_query_byte_bound() {
        let whole_bound_title = "t".repeat(MAX_SESSION_METADATA_TOTAL_UTF8_BYTES);
        let one_byte_beyond = "t".repeat(MAX_SESSION_METADATA_TOTAL_UTF8_BYTES + 1);

        assert!(
            parse(["conversations", "--title", whole_bound_title.as_str()].map(Into::into)).is_ok()
        );
        assert!(
            parse(["conversations", "--title", one_byte_beyond.as_str()].map(Into::into)).is_err()
        );
    }

    #[test]
    fn conversations_rejects_an_unknown_origin_filter() {
        assert!(parse(["conversations", "--origin", "everything"].map(Into::into)).is_err());
    }

    #[test]
    fn conversations_rejects_a_malformed_cursor_before_socket_use() {
        assert!(
            parse(
                [
                    "conversations",
                    "--after",
                    "00000000-0000-0000-0000-000000000001"
                ]
                .map(Into::into)
            )
            .is_err()
        );
        assert!(
            parse(
                [
                    "conversations",
                    "--after",
                    "archived:00000000-0000-0000-0000-000000000001",
                ]
                .map(Into::into)
            )
            .is_err()
        );
        assert!(parse(["conversations", "--after", "native:not-a-uuid"].map(Into::into)).is_err());
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
    fn create_template_is_simple_and_excludes_every_explicit_default_flag() {
        const TEMPLATE_NAME: &str = "reviewer";
        let parsed = parse(["create", "--template", TEMPLATE_NAME].map(Into::into))
            .expect("template creation parses");
        let ParseOutcome::Run(arguments) = parsed else {
            panic!("template creation must run");
        };
        let Command::Create {
            selection,
            template,
            system_prompt_file,
            ..
        } = arguments.command
        else {
            panic!("template creation maps to create");
        };

        assert_eq!(selection, None);
        assert_eq!(template.as_deref(), Some(TEMPLATE_NAME));
        assert_eq!(system_prompt_file, None);
        assert!(
            parse(
                [
                    "create",
                    "--template",
                    TEMPLATE_NAME,
                    "--model",
                    "00000000-0000-0000-0000-000000000001",
                ]
                .map(Into::into)
            )
            .is_err()
        );
        assert!(
            parse(
                [
                    "create",
                    "--template",
                    TEMPLATE_NAME,
                    "--alias",
                    "00000000-0000-0000-0000-000000000002",
                ]
                .map(Into::into)
            )
            .is_err()
        );
        assert!(
            parse(
                [
                    "create",
                    "--template",
                    TEMPLATE_NAME,
                    "--system-prompt-file",
                    "prompt.txt",
                ]
                .map(Into::into)
            )
            .is_err()
        );
    }

    #[test]
    fn templates_maps_to_the_read_only_listing_command() {
        let parsed = parse(["templates"].map(Into::into)).expect("templates parses");
        let ParseOutcome::Run(arguments) = parsed else {
            panic!("templates must run");
        };
        let Command::Templates = arguments.command else {
            panic!("templates maps to its listing command");
        };
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
