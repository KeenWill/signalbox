//! Bounded, read-only tools for conversations visible through an injected port.

use std::{cmp::Ordering, error::Error, fmt, future::Future, num::NonZeroU64};

use signalbox_application::{
    ClassifyOperatorFailure, CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    OperatorFailureClass, ToolArgumentValidator, ToolExecutionInvocation, ToolExecutor,
    ToolExecutorEvidence,
};
use signalbox_domain::{
    ImportedConversationId, NormalizedToolArguments, SessionId, ToolEffectClass,
    ToolExecutionErrorDetail, ToolPermissionDefault, ToolResultText,
};
use signalbox_tool_contract::{
    ToolContract, ToolContractCompileError, compile_contract_definition,
};

/// Model-facing name for the unified conversation inventory.
pub const LIST_CONVERSATIONS_NAME: &str = "list_conversations";
/// Model-facing name for reading the invoking session's transcript.
pub const READ_OWN_CONVERSATION_NAME: &str = "read_own_conversation";
/// Model-facing name for reading a selected native-session transcript.
pub const READ_CONVERSATION_NAME: &str = "read_conversation";
/// Model-facing name for reading a selected imported transcript.
pub const READ_IMPORTED_CONVERSATION_NAME: &str = "read_imported_conversation";

/// Stable conversation-tool registry names in declaration order.
pub const CONVERSATION_TOOL_NAMES: [&str; 4] = [
    LIST_CONVERSATIONS_NAME,
    READ_OWN_CONVERSATION_NAME,
    READ_CONVERSATION_NAME,
    READ_IMPORTED_CONVERSATION_NAME,
];

/// Greatest number of inventory rows one invocation may request.
pub const MAX_CONVERSATION_LIST_RESULTS: usize = 100;
/// Greatest number of transcript entries one invocation may request.
pub const MAX_TRANSCRIPT_ENTRIES: usize = 100;
/// Greatest aggregate visible-content bytes one transcript page may carry.
pub const MAX_TRANSCRIPT_CONTENT_BYTES: usize = 128 * 1024;
/// Greatest title prefix emitted for one listed conversation.
pub const MAX_LIST_TITLE_BYTES: usize = 1024;

const UUID_TEXT_BYTES: usize = 36;
const INVALID_ARGUMENTS_DETAIL: &str = "invalid bounded conversation-tool arguments";
const CONVERSATION_NOT_FOUND_DETAIL: &str = "conversation does not exist";
const IMPORTED_CONVERSATION_NOT_FOUND_DETAIL: &str = "imported conversation does not exist";

/// One position in the unified inventory order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversationCursor {
    /// A native session position.
    Native(SessionId),
    /// An imported-conversation position.
    Imported(ImportedConversationId),
}

impl ConversationCursor {
    fn identity_uuid(self) -> uuid::Uuid {
        match self {
            Self::Native(session) => session.into_uuid(),
            Self::Imported(conversation) => conversation.into_uuid(),
        }
    }

    const fn origin_rank(self) -> u8 {
        match self {
            Self::Native(_) => 0,
            Self::Imported(_) => 1,
        }
    }
}

/// One bounded unified inventory request supplied to the injected port.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConversationListRequest {
    after: Option<ConversationCursor>,
    max_results: usize,
}

impl ConversationListRequest {
    /// Retains the exact exclusive cursor and requested row bound.
    pub const fn new(after: Option<ConversationCursor>, max_results: usize) -> Self {
        Self { after, max_results }
    }

    /// Returns the exclusive inventory cursor.
    pub const fn after(&self) -> Option<ConversationCursor> {
        self.after
    }

    /// Returns the maximum inventory rows requested.
    pub const fn max_results(&self) -> usize {
        self.max_results
    }
}

/// One model-visible conversation inventory item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConversationListItem {
    /// One native session and its organizational facts.
    Native {
        /// Native session identity.
        session: SessionId,
        /// Exact optional visible title.
        title: Option<String>,
        /// Whether the native session is archived.
        archived: bool,
    },
    /// One immutable imported conversation.
    Imported {
        /// Imported conversation identity.
        conversation: ImportedConversationId,
        /// Exact optional visible title.
        title: Option<String>,
        /// Total normalized imported entry count.
        entry_count: u64,
    },
}

impl ConversationListItem {
    /// Returns this item's unified inventory position.
    pub const fn cursor(&self) -> ConversationCursor {
        match self {
            Self::Native { session, .. } => ConversationCursor::Native(*session),
            Self::Imported { conversation, .. } => ConversationCursor::Imported(*conversation),
        }
    }

    /// Borrows the exact optional visible title.
    pub fn title(&self) -> Option<&str> {
        match self {
            Self::Native { title, .. } | Self::Imported { title, .. } => title.as_deref(),
        }
    }
}

/// One bounded unified inventory page returned by the injected port.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationListPage {
    items: Vec<ConversationListItem>,
    has_more: bool,
}

impl ConversationListPage {
    /// Supplies visible rows and whether a later row exists.
    pub fn new(items: Vec<ConversationListItem>, has_more: bool) -> Self {
        Self { items, has_more }
    }

    /// Borrows the visible inventory rows.
    pub fn items(&self) -> &[ConversationListItem] {
        &self.items
    }

    /// Returns whether a later inventory row exists.
    pub const fn has_more(&self) -> bool {
        self.has_more
    }

    /// Returns the visible rows and continuation fact.
    pub fn into_parts(self) -> (Vec<ConversationListItem>, bool) {
        (self.items, self.has_more)
    }
}

/// One native transcript read requested through the injected port.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConversationTranscriptRequest {
    session: SessionId,
    after_position: Option<NonZeroU64>,
    max_entries: usize,
    max_bytes: usize,
}

impl ConversationTranscriptRequest {
    /// Retains one exact native target, cursor, and pair of bounds.
    pub const fn new(
        session: SessionId,
        after_position: Option<NonZeroU64>,
        max_entries: usize,
        max_bytes: usize,
    ) -> Self {
        Self {
            session,
            after_position,
            max_entries,
            max_bytes,
        }
    }

    /// Returns the selected native session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the exclusive transcript position.
    pub const fn after_position(&self) -> Option<NonZeroU64> {
        self.after_position
    }

    /// Returns the maximum entry count requested.
    pub const fn max_entries(&self) -> usize {
        self.max_entries
    }

    /// Returns the maximum aggregate visible-content bytes requested.
    pub const fn max_bytes(&self) -> usize {
        self.max_bytes
    }
}

/// One imported transcript read requested through the injected port.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImportedTranscriptRequest {
    conversation: ImportedConversationId,
    after_position: Option<NonZeroU64>,
    max_entries: usize,
    max_bytes: usize,
}

impl ImportedTranscriptRequest {
    /// Retains one exact imported target, cursor, and pair of bounds.
    pub const fn new(
        conversation: ImportedConversationId,
        after_position: Option<NonZeroU64>,
        max_entries: usize,
        max_bytes: usize,
    ) -> Self {
        Self {
            conversation,
            after_position,
            max_entries,
            max_bytes,
        }
    }

    /// Returns the selected imported conversation.
    pub const fn conversation(&self) -> ImportedConversationId {
        self.conversation
    }

    /// Returns the exclusive transcript position.
    pub const fn after_position(&self) -> Option<NonZeroU64> {
        self.after_position
    }

    /// Returns the maximum entry count requested.
    pub const fn max_entries(&self) -> usize {
        self.max_entries
    }

    /// Returns the maximum aggregate visible-content bytes requested.
    pub const fn max_bytes(&self) -> usize {
        self.max_bytes
    }
}

/// Model-visible semantic kind for one transcript entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptEntryKind {
    /// User-authored visible content.
    User,
    /// Assistant-authored visible content.
    Assistant,
    /// One model-authored tool request.
    ToolUse,
    /// One tool result, denial, or closure.
    ToolResult,
    /// One system-authored marker or source event.
    System,
}

/// One model-visible transcript entry returned by the injected port.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptEntry {
    position: NonZeroU64,
    kind: TranscriptEntryKind,
    content: String,
    content_truncated: bool,
}

impl TranscriptEntry {
    /// Supplies one visible entry without any raw or hidden source material.
    pub fn new(
        position: NonZeroU64,
        kind: TranscriptEntryKind,
        content: String,
        content_truncated: bool,
    ) -> Self {
        Self {
            position,
            kind,
            content,
            content_truncated,
        }
    }

    /// Returns the positive transcript position.
    pub const fn position(&self) -> NonZeroU64 {
        self.position
    }

    /// Returns the model-visible semantic kind.
    pub const fn kind(&self) -> TranscriptEntryKind {
        self.kind
    }

    /// Borrows the exact visible content supplied by the port.
    pub fn content(&self) -> &str {
        &self.content
    }

    /// Returns whether visible content bytes were omitted from this entry.
    pub const fn content_truncated(&self) -> bool {
        self.content_truncated
    }
}

/// One bounded transcript page returned by the injected port.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptPage {
    entries: Vec<TranscriptEntry>,
    has_more: bool,
}

impl TranscriptPage {
    /// Supplies visible entries and whether a later entry exists.
    pub fn new(entries: Vec<TranscriptEntry>, has_more: bool) -> Self {
        Self { entries, has_more }
    }

    /// Borrows the visible transcript entries.
    pub fn entries(&self) -> &[TranscriptEntry] {
        &self.entries
    }

    /// Returns whether a later transcript entry exists.
    pub const fn has_more(&self) -> bool {
        self.has_more
    }

    /// Returns the visible entries and continuation fact.
    pub fn into_parts(self) -> (Vec<TranscriptEntry>, bool) {
        (self.entries, self.has_more)
    }
}

/// Daemon-implemented boundary for conversation inventory and transcript reads.
///
/// Implementations project only content already visible after persistence and
/// application redaction. This boundary has no operation or argument for raw
/// source records, credentials, redacted payloads, writes, deletion, or search.
pub trait ConversationIntrospectionPort: Send {
    /// Sanitized adapter failure returned when no trustworthy result exists.
    type Error: ClassifyOperatorFailure;

    /// Reads one bounded unified inventory page in strict cursor order.
    fn list_conversations(
        &mut self,
        request: ConversationListRequest,
    ) -> impl Future<Output = Result<ConversationListPage, Self::Error>> + Send;

    /// Reads one bounded visible native-session transcript page.
    fn read_conversation(
        &mut self,
        request: ConversationTranscriptRequest,
    ) -> impl Future<Output = Result<Option<TranscriptPage>, Self::Error>> + Send;

    /// Reads one bounded visible imported transcript page.
    fn read_imported_conversation(
        &mut self,
        request: ImportedTranscriptRequest,
    ) -> impl Future<Output = Result<Option<TranscriptPage>, Self::Error>> + Send;
}

/// Typed list-conversations arguments.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListConversationsArguments {
    /// Optional exclusive unified inventory cursor.
    pub after: Option<ConversationCursorArguments>,
    /// Maximum rows returned, from 1 through 100.
    #[schemars(range(min = 1, max = MAX_CONVERSATION_LIST_RESULTS))]
    pub max_results: usize,
}

/// Model-facing unified cursor shape.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConversationCursorArguments {
    /// Origin class of the cursor identity.
    pub kind: ConversationCursorKind,
    /// Canonical lowercase hyphenated UUID.
    #[schemars(length(min = UUID_TEXT_BYTES, max = UUID_TEXT_BYTES))]
    pub id: String,
}

/// Model-facing unified cursor origin.
#[derive(Clone, Copy, Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConversationCursorKind {
    /// A native session cursor.
    Native,
    /// An imported-conversation cursor.
    Imported,
}

/// Typed own-conversation read arguments.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadOwnConversationArguments {
    /// Optional exclusive positive transcript position.
    #[schemars(range(min = 1))]
    pub after_position: Option<u64>,
    /// Maximum entries returned, from 1 through 100.
    #[schemars(range(min = 1, max = MAX_TRANSCRIPT_ENTRIES))]
    pub max_entries: usize,
    /// Maximum aggregate visible-content bytes, from 1 through 131072.
    #[schemars(range(min = 1, max = MAX_TRANSCRIPT_CONTENT_BYTES))]
    pub max_bytes: usize,
}

/// Typed selected native-conversation read arguments.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadConversationArguments {
    /// Canonical lowercase hyphenated native session UUID.
    #[schemars(length(min = UUID_TEXT_BYTES, max = UUID_TEXT_BYTES))]
    pub session_id: String,
    /// Optional exclusive positive transcript position.
    #[schemars(range(min = 1))]
    pub after_position: Option<u64>,
    /// Maximum entries returned, from 1 through 100.
    #[schemars(range(min = 1, max = MAX_TRANSCRIPT_ENTRIES))]
    pub max_entries: usize,
    /// Maximum aggregate visible-content bytes, from 1 through 131072.
    #[schemars(range(min = 1, max = MAX_TRANSCRIPT_CONTENT_BYTES))]
    pub max_bytes: usize,
}

/// Typed imported-conversation read arguments.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadImportedConversationArguments {
    /// Canonical lowercase hyphenated imported-conversation UUID.
    #[schemars(length(min = UUID_TEXT_BYTES, max = UUID_TEXT_BYTES))]
    pub imported_conversation_id: String,
    /// Optional exclusive positive transcript position.
    #[schemars(range(min = 1))]
    pub after_position: Option<u64>,
    /// Maximum entries returned, from 1 through 100.
    #[schemars(range(min = 1, max = MAX_TRANSCRIPT_ENTRIES))]
    pub max_entries: usize,
    /// Maximum aggregate visible-content bytes, from 1 through 131072.
    #[schemars(range(min = 1, max = MAX_TRANSCRIPT_CONTENT_BYTES))]
    pub max_bytes: usize,
}

struct ListConversationsContract;

impl ToolContract for ListConversationsContract {
    type Arguments = ListConversationsArguments;
    const NAME: &'static str = LIST_CONVERSATIONS_NAME;
    const DESCRIPTION: &'static str =
        "Lists a bounded page of native and imported conversations visible to Signalbox.";
}

struct ReadOwnConversationContract;

impl ToolContract for ReadOwnConversationContract {
    type Arguments = ReadOwnConversationArguments;
    const NAME: &'static str = READ_OWN_CONVERSATION_NAME;
    const DESCRIPTION: &'static str =
        "Reads a bounded visible transcript page from the invoking session.";
}

struct ReadConversationContract;

impl ToolContract for ReadConversationContract {
    type Arguments = ReadConversationArguments;
    const NAME: &'static str = READ_CONVERSATION_NAME;
    const DESCRIPTION: &'static str =
        "Reads a bounded visible transcript page from a selected native session.";
}

struct ReadImportedConversationContract;

impl ToolContract for ReadImportedConversationContract {
    type Arguments = ReadImportedConversationArguments;
    const NAME: &'static str = READ_IMPORTED_CONVERSATION_NAME;
    const DESCRIPTION: &'static str =
        "Reads a bounded visible transcript page from a selected imported conversation.";
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConversationToolKind {
    List,
    ReadOwn,
    ReadOther,
    ReadImported,
}

impl ConversationToolKind {
    const ALL: [Self; 4] = [
        Self::List,
        Self::ReadOwn,
        Self::ReadOther,
        Self::ReadImported,
    ];

    /// Own-session transcript access is automatic because the target is the
    /// trusted dispatch correlation. Inventory, selected native reads, and
    /// imported reads can cross conversation boundaries and therefore require
    /// explicit user approval by default.
    const fn permission(self) -> ToolPermissionDefault {
        match self {
            Self::ReadOwn => ToolPermissionDefault::Auto,
            Self::List | Self::ReadOther | Self::ReadImported => ToolPermissionDefault::Confirm,
        }
    }

    fn definition(self) -> Result<signalbox_application::ToolDefinition, ToolContractCompileError> {
        match self {
            Self::List => compile_contract_definition::<ListConversationsContract>(
                self.permission(),
                ToolEffectClass::EffectFree,
            ),
            Self::ReadOwn => compile_contract_definition::<ReadOwnConversationContract>(
                self.permission(),
                ToolEffectClass::EffectFree,
            ),
            Self::ReadOther => compile_contract_definition::<ReadConversationContract>(
                self.permission(),
                ToolEffectClass::EffectFree,
            ),
            Self::ReadImported => compile_contract_definition::<ReadImportedConversationContract>(
                self.permission(),
                ToolEffectClass::EffectFree,
            ),
        }
    }
}

/// A static conversation declaration could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversationToolConstructionError {
    /// One static contract name was invalid.
    Name,
    /// One static contract schema was invalid.
    Schema,
    /// One static sanitized error detail was invalid.
    ErrorDetail,
    /// The catalog unexpectedly contained a duplicate.
    Duplicate,
}

impl fmt::Display for ConversationToolConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Name => "conversation-tool static name is invalid",
            Self::Schema => "conversation-tool static schema is invalid",
            Self::ErrorDetail => "conversation-tool static error detail is invalid",
            Self::Duplicate => "conversation-tool catalog is duplicated",
        })
    }
}

impl Error for ConversationToolConstructionError {}

/// Compiled conversation catalog and executor around one injected read port.
#[derive(Clone, Debug)]
pub struct ConversationTools<Port> {
    catalog: CompiledToolCatalog,
    executor: ConversationExecutor<Port>,
}

impl<Port> ConversationTools<Port> {
    /// Compiles the four read-only tools around one injected port.
    pub fn try_new(port: Port) -> Result<Self, ConversationToolConstructionError> {
        let invalid_arguments_detail = detail(INVALID_ARGUMENTS_DETAIL)?;
        let conversation_not_found_detail = detail(CONVERSATION_NOT_FOUND_DETAIL)?;
        let imported_conversation_not_found_detail =
            detail(IMPORTED_CONVERSATION_NOT_FOUND_DETAIL)?;
        let compiled = ConversationToolKind::ALL
            .into_iter()
            .map(|kind| {
                let definition = kind.definition().map_err(|error| match error {
                    ToolContractCompileError::Name => ConversationToolConstructionError::Name,
                    ToolContractCompileError::Schema => ConversationToolConstructionError::Schema,
                })?;
                Ok(CompiledTool::new(
                    definition,
                    ConversationArgumentValidator {
                        kind,
                        detail: invalid_arguments_detail.clone(),
                    },
                ))
            })
            .collect::<Result<Vec<_>, ConversationToolConstructionError>>()?;
        let catalog = CompiledToolCatalog::try_new(compiled)
            .map_err(|_| ConversationToolConstructionError::Duplicate)?;
        Ok(Self {
            catalog,
            executor: ConversationExecutor {
                port,
                conversation_not_found_detail,
                imported_conversation_not_found_detail,
            },
        })
    }

    /// Returns the catalog and executor as separate composition roles.
    pub fn into_parts(self) -> (CompiledToolCatalog, ConversationExecutor<Port>) {
        (self.catalog, self.executor)
    }
}

fn detail(value: &str) -> Result<ToolExecutionErrorDetail, ConversationToolConstructionError> {
    ToolExecutionErrorDetail::try_new(String::from(value))
        .map_err(|_| ConversationToolConstructionError::ErrorDetail)
}

#[derive(Clone, Debug)]
struct ConversationArgumentValidator {
    kind: ConversationToolKind,
    detail: ToolExecutionErrorDetail,
}

impl ToolArgumentValidator for ConversationArgumentValidator {
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail> {
        decode_operation(self.kind, arguments)
            .map(|_| ())
            .map_err(|_| self.detail.clone())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TranscriptBounds {
    after_position: Option<NonZeroU64>,
    max_entries: usize,
    max_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConversationOperation {
    List(ConversationListRequest),
    ReadOwn(TranscriptBounds),
    ReadOther(ConversationTranscriptRequest),
    ReadImported(ImportedTranscriptRequest),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InvalidConversationArguments;

fn decode_operation(
    kind: ConversationToolKind,
    arguments: &NormalizedToolArguments,
) -> Result<ConversationOperation, InvalidConversationArguments> {
    match kind {
        ConversationToolKind::List => {
            let decoded: ListConversationsArguments = serde_json::from_str(arguments.as_str())
                .map_err(|_| InvalidConversationArguments)?;
            check_list_bound(decoded.max_results)?;
            let after = decoded.after.map(decode_cursor).transpose()?;
            Ok(ConversationOperation::List(ConversationListRequest::new(
                after,
                decoded.max_results,
            )))
        }
        ConversationToolKind::ReadOwn => {
            let decoded: ReadOwnConversationArguments = serde_json::from_str(arguments.as_str())
                .map_err(|_| InvalidConversationArguments)?;
            let bounds = decode_transcript_bounds(
                decoded.after_position,
                decoded.max_entries,
                decoded.max_bytes,
            )?;
            Ok(ConversationOperation::ReadOwn(bounds))
        }
        ConversationToolKind::ReadOther => {
            let decoded: ReadConversationArguments = serde_json::from_str(arguments.as_str())
                .map_err(|_| InvalidConversationArguments)?;
            let bounds = decode_transcript_bounds(
                decoded.after_position,
                decoded.max_entries,
                decoded.max_bytes,
            )?;
            let session = SessionId::from_uuid(decode_uuid(&decoded.session_id)?);
            Ok(ConversationOperation::ReadOther(
                ConversationTranscriptRequest::new(
                    session,
                    bounds.after_position,
                    bounds.max_entries,
                    bounds.max_bytes,
                ),
            ))
        }
        ConversationToolKind::ReadImported => {
            let decoded: ReadImportedConversationArguments =
                serde_json::from_str(arguments.as_str())
                    .map_err(|_| InvalidConversationArguments)?;
            let bounds = decode_transcript_bounds(
                decoded.after_position,
                decoded.max_entries,
                decoded.max_bytes,
            )?;
            let conversation =
                ImportedConversationId::from_uuid(decode_uuid(&decoded.imported_conversation_id)?);
            Ok(ConversationOperation::ReadImported(
                ImportedTranscriptRequest::new(
                    conversation,
                    bounds.after_position,
                    bounds.max_entries,
                    bounds.max_bytes,
                ),
            ))
        }
    }
}

fn decode_cursor(
    arguments: ConversationCursorArguments,
) -> Result<ConversationCursor, InvalidConversationArguments> {
    let identity = decode_uuid(&arguments.id)?;
    Ok(match arguments.kind {
        ConversationCursorKind::Native => {
            ConversationCursor::Native(SessionId::from_uuid(identity))
        }
        ConversationCursorKind::Imported => {
            ConversationCursor::Imported(ImportedConversationId::from_uuid(identity))
        }
    })
}

fn decode_transcript_bounds(
    after_position: Option<u64>,
    max_entries: usize,
    max_bytes: usize,
) -> Result<TranscriptBounds, InvalidConversationArguments> {
    if max_entries == 0
        || max_entries > MAX_TRANSCRIPT_ENTRIES
        || max_bytes == 0
        || max_bytes > MAX_TRANSCRIPT_CONTENT_BYTES
    {
        return Err(InvalidConversationArguments);
    }
    let after_position = after_position
        .map(|position| NonZeroU64::new(position).ok_or(InvalidConversationArguments))
        .transpose()?;
    Ok(TranscriptBounds {
        after_position,
        max_entries,
        max_bytes,
    })
}

fn check_list_bound(max_results: usize) -> Result<(), InvalidConversationArguments> {
    if max_results == 0 || max_results > MAX_CONVERSATION_LIST_RESULTS {
        return Err(InvalidConversationArguments);
    }
    Ok(())
}

fn decode_uuid(value: &str) -> Result<uuid::Uuid, InvalidConversationArguments> {
    if value.len() != UUID_TEXT_BYTES {
        return Err(InvalidConversationArguments);
    }
    let parsed = uuid::Uuid::parse_str(value).map_err(|_| InvalidConversationArguments)?;
    if parsed.hyphenated().to_string() != value {
        return Err(InvalidConversationArguments);
    }
    Ok(parsed)
}

fn kind_for_name(name: &str) -> Option<ConversationToolKind> {
    match name {
        LIST_CONVERSATIONS_NAME => Some(ConversationToolKind::List),
        READ_OWN_CONVERSATION_NAME => Some(ConversationToolKind::ReadOwn),
        READ_CONVERSATION_NAME => Some(ConversationToolKind::ReadOther),
        READ_IMPORTED_CONVERSATION_NAME => Some(ConversationToolKind::ReadImported),
        _ => None,
    }
}

/// Executor for the four bounded conversation reads.
#[derive(Clone, Debug)]
pub struct ConversationExecutor<Port> {
    port: Port,
    conversation_not_found_detail: ToolExecutionErrorDetail,
    imported_conversation_not_found_detail: ToolExecutionErrorDetail,
}

impl<Port> ConversationExecutor<Port> {
    /// Returns the injected port for explicit ownership handoff.
    pub fn into_port(self) -> Port {
        self.port
    }
}

/// Failure inside the conversation executor.
#[derive(Debug)]
pub enum ConversationExecutorError<PortError> {
    /// Executor argument decoding disagreed with catalog validation.
    ArgumentValidationDrift,
    /// The injected port failed without trustworthy tool evidence.
    Port(PortError),
    /// The injected port violated ordering or requested bounds.
    PortContract,
    /// Compact result encoding unexpectedly failed.
    ResultEncoding,
}

impl<PortError> fmt::Display for ConversationExecutorError<PortError>
where
    PortError: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ArgumentValidationDrift => {
                formatter.write_str("conversation-tool argument validation drifted")
            }
            Self::Port(error) => error.fmt(formatter),
            Self::PortContract => {
                formatter.write_str("conversation read port contract was violated")
            }
            Self::ResultEncoding => formatter.write_str("conversation-tool result encoding failed"),
        }
    }
}

impl<PortError> Error for ConversationExecutorError<PortError>
where
    PortError: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Port(error) => Some(error),
            Self::ArgumentValidationDrift | Self::PortContract | Self::ResultEncoding => None,
        }
    }
}

impl<PortError> ClassifyOperatorFailure for ConversationExecutorError<PortError>
where
    PortError: ClassifyOperatorFailure,
{
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Port(error) => error.operator_failure_class(),
            Self::ArgumentValidationDrift | Self::PortContract | Self::ResultEncoding => {
                OperatorFailureClass::CallerOrHubBug
            }
        }
    }
}

impl<Port> ToolExecutor for ConversationExecutor<Port>
where
    Port: ConversationIntrospectionPort,
{
    type Error = ConversationExecutorError<Port::Error>;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        let kind = kind_for_name(invocation.request().name().as_str())
            .ok_or(ConversationExecutorError::ArgumentValidationDrift)?;
        let operation = decode_operation(kind, invocation.request().arguments())
            .map_err(|_| ConversationExecutorError::ArgumentValidationDrift)?;
        let requesting_session = invocation.correlation().session();
        let evidence = self
            .execute_operation(requesting_session, operation)
            .await?;
        Ok(invocation.bind(evidence))
    }
}

impl<Port> ConversationExecutor<Port>
where
    Port: ConversationIntrospectionPort,
{
    async fn execute_operation(
        &mut self,
        requesting_session: SessionId,
        operation: ConversationOperation,
    ) -> Result<ToolExecutorEvidence, ConversationExecutorError<Port::Error>> {
        match operation {
            ConversationOperation::List(request) => {
                let page = self
                    .port
                    .list_conversations(request)
                    .await
                    .map_err(ConversationExecutorError::Port)?;
                validate_list_page(request, &page)?;
                Ok(ToolExecutorEvidence::CompletedText(encode_list_page(page)?))
            }
            ConversationOperation::ReadOwn(bounds) => {
                let request = ConversationTranscriptRequest::new(
                    requesting_session,
                    bounds.after_position,
                    bounds.max_entries,
                    bounds.max_bytes,
                );
                self.read_native(request).await
            }
            ConversationOperation::ReadOther(request) => self.read_native(request).await,
            ConversationOperation::ReadImported(request) => {
                let page = self
                    .port
                    .read_imported_conversation(request)
                    .await
                    .map_err(ConversationExecutorError::Port)?;
                let Some(page) = page else {
                    return Ok(ToolExecutorEvidence::KnownFailed {
                        detail: Some(self.imported_conversation_not_found_detail.clone()),
                    });
                };
                validate_transcript_page(
                    request.after_position(),
                    request.max_entries(),
                    request.max_bytes(),
                    &page,
                )?;
                Ok(ToolExecutorEvidence::CompletedText(encode_imported_page(
                    request.conversation(),
                    page,
                )?))
            }
        }
    }

    async fn read_native(
        &mut self,
        request: ConversationTranscriptRequest,
    ) -> Result<ToolExecutorEvidence, ConversationExecutorError<Port::Error>> {
        let page = self
            .port
            .read_conversation(request)
            .await
            .map_err(ConversationExecutorError::Port)?;
        let Some(page) = page else {
            return Ok(ToolExecutorEvidence::KnownFailed {
                detail: Some(self.conversation_not_found_detail.clone()),
            });
        };
        validate_transcript_page(
            request.after_position(),
            request.max_entries(),
            request.max_bytes(),
            &page,
        )?;
        Ok(ToolExecutorEvidence::CompletedText(encode_native_page(
            request.session(),
            page,
        )?))
    }
}

fn validate_list_page<PortError>(
    request: ConversationListRequest,
    page: &ConversationListPage,
) -> Result<(), ConversationExecutorError<PortError>> {
    if page.items().len() > request.max_results() || (page.has_more() && page.items().is_empty()) {
        return Err(ConversationExecutorError::PortContract);
    }
    let mut previous = request.after();
    for item in page.items() {
        let cursor = item.cursor();
        if previous.is_some_and(|prior| cursor_order(cursor, prior) != Ordering::Greater) {
            return Err(ConversationExecutorError::PortContract);
        }
        previous = Some(cursor);
    }
    Ok(())
}

fn cursor_order(left: ConversationCursor, right: ConversationCursor) -> Ordering {
    left.identity_uuid()
        .cmp(&right.identity_uuid())
        .then_with(|| left.origin_rank().cmp(&right.origin_rank()))
}

fn validate_transcript_page<PortError>(
    after_position: Option<NonZeroU64>,
    max_entries: usize,
    max_bytes: usize,
    page: &TranscriptPage,
) -> Result<(), ConversationExecutorError<PortError>> {
    if page.entries().len() > max_entries || (page.has_more() && page.entries().is_empty()) {
        return Err(ConversationExecutorError::PortContract);
    }
    let mut previous = after_position;
    let mut visible_bytes = 0_usize;
    let mut content_was_truncated = false;
    for entry in page.entries() {
        if previous.is_some_and(|prior| entry.position() <= prior) || content_was_truncated {
            return Err(ConversationExecutorError::PortContract);
        }
        visible_bytes = visible_bytes
            .checked_add(entry.content().len())
            .ok_or(ConversationExecutorError::PortContract)?;
        if visible_bytes > max_bytes {
            return Err(ConversationExecutorError::PortContract);
        }
        previous = Some(entry.position());
        content_was_truncated = entry.content_truncated();
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct ConversationCursorOutput {
    kind: &'static str,
    id: String,
}

impl From<ConversationCursor> for ConversationCursorOutput {
    fn from(value: ConversationCursor) -> Self {
        match value {
            ConversationCursor::Native(session) => Self {
                kind: "native",
                id: session.into_uuid().to_string(),
            },
            ConversationCursor::Imported(conversation) => Self {
                kind: "imported",
                id: conversation.into_uuid().to_string(),
            },
        }
    }
}

#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ConversationListItemOutput {
    Native {
        session_id: String,
        title: Option<String>,
        title_truncated: bool,
        archived: bool,
    },
    Imported {
        imported_conversation_id: String,
        title: Option<String>,
        title_truncated: bool,
        entry_count: u64,
    },
}

impl From<ConversationListItem> for ConversationListItemOutput {
    fn from(value: ConversationListItem) -> Self {
        match value {
            ConversationListItem::Native {
                session,
                title,
                archived,
            } => {
                let (title, title_truncated) = bounded_title(title);
                Self::Native {
                    session_id: session.into_uuid().to_string(),
                    title,
                    title_truncated,
                    archived,
                }
            }
            ConversationListItem::Imported {
                conversation,
                title,
                entry_count,
            } => {
                let (title, title_truncated) = bounded_title(title);
                Self::Imported {
                    imported_conversation_id: conversation.into_uuid().to_string(),
                    title,
                    title_truncated,
                    entry_count,
                }
            }
        }
    }
}

fn bounded_title(title: Option<String>) -> (Option<String>, bool) {
    let Some(title) = title else {
        return (None, false);
    };
    let mut boundary = title.len().min(MAX_LIST_TITLE_BYTES);
    while !title.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let truncated = boundary < title.len();
    (Some(title[..boundary].to_owned()), truncated)
}

#[derive(serde::Serialize)]
struct ConversationListOutput {
    conversations: Vec<ConversationListItemOutput>,
    next_after: Option<ConversationCursorOutput>,
    truncated: bool,
}

fn encode_list_page<PortError>(
    page: ConversationListPage,
) -> Result<String, ConversationExecutorError<PortError>> {
    let (items, has_more) = page.into_parts();
    let next_after = if has_more {
        items
            .last()
            .map(ConversationListItem::cursor)
            .map(ConversationCursorOutput::from)
    } else {
        None
    };
    encode_result(&ConversationListOutput {
        conversations: items
            .into_iter()
            .map(ConversationListItemOutput::from)
            .collect(),
        next_after,
        truncated: has_more,
    })
}

#[derive(serde::Serialize)]
struct TranscriptEntryOutput {
    position: u64,
    kind: TranscriptEntryKind,
    content: String,
    content_truncated: bool,
}

impl From<TranscriptEntry> for TranscriptEntryOutput {
    fn from(value: TranscriptEntry) -> Self {
        Self {
            position: value.position.get(),
            kind: value.kind,
            content: value.content,
            content_truncated: value.content_truncated,
        }
    }
}

#[derive(serde::Serialize)]
struct NativeTranscriptOutput {
    session_id: String,
    entries: Vec<TranscriptEntryOutput>,
    next_after: Option<u64>,
    truncated: bool,
}

#[derive(serde::Serialize)]
struct ImportedTranscriptOutput {
    imported_conversation_id: String,
    entries: Vec<TranscriptEntryOutput>,
    next_after: Option<u64>,
    truncated: bool,
}

fn transcript_output_parts(
    page: TranscriptPage,
) -> (Vec<TranscriptEntryOutput>, Option<u64>, bool) {
    let (entries, has_more) = page.into_parts();
    let content_truncated = entries.iter().any(TranscriptEntry::content_truncated);
    let next_after = if has_more {
        entries.last().map(|entry| entry.position().get())
    } else {
        None
    };
    (
        entries
            .into_iter()
            .map(TranscriptEntryOutput::from)
            .collect(),
        next_after,
        has_more || content_truncated,
    )
}

fn encode_native_page<PortError>(
    session: SessionId,
    page: TranscriptPage,
) -> Result<String, ConversationExecutorError<PortError>> {
    let (entries, next_after, truncated) = transcript_output_parts(page);
    encode_result(&NativeTranscriptOutput {
        session_id: session.into_uuid().to_string(),
        entries,
        next_after,
        truncated,
    })
}

fn encode_imported_page<PortError>(
    conversation: ImportedConversationId,
    page: TranscriptPage,
) -> Result<String, ConversationExecutorError<PortError>> {
    let (entries, next_after, truncated) = transcript_output_parts(page);
    encode_result(&ImportedTranscriptOutput {
        imported_conversation_id: conversation.into_uuid().to_string(),
        entries,
        next_after,
        truncated,
    })
}

fn encode_result<PortError>(
    value: &impl serde::Serialize,
) -> Result<String, ConversationExecutorError<PortError>> {
    let encoded =
        serde_json::to_string(value).map_err(|_| ConversationExecutorError::ResultEncoding)?;
    ToolResultText::try_new(encoded)
        .map(ToolResultText::into_string)
        .map_err(|_| ConversationExecutorError::ResultEncoding)
}

#[cfg(test)]
mod tests;
