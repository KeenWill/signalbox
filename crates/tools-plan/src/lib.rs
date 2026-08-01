//! Durable, session-scoped plan tools over an injected append/read port.

use std::{collections::HashSet, error::Error, fmt, future::Future, num::NonZeroU64};

use signalbox_application::{
    ClassifyOperatorFailure, CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    OperatorFailureClass, ToolArgumentValidator, ToolExecutionInvocation, ToolExecutor,
    ToolExecutorEvidence,
};
use signalbox_domain::{
    NormalizedToolArguments, SessionId, ToolAttemptDispatchCorrelation, ToolEffectClass,
    ToolExecutionErrorDetail, ToolPermissionDefault, ToolResultText, ToolResultTextFailure,
};
use signalbox_tool_contract::{
    ToolContract, ToolContractCompileError, compile_contract_definition,
};

/// Model-facing name for appending one durable plan event.
pub const PLAN_WRITE_NAME: &str = "plan_write";
/// Model-facing name for reading the invoking session's plan.
pub const PLAN_READ_NAME: &str = "plan_read";
/// Stable plan-tool registry names in declaration order.
pub const PLAN_TOOL_NAMES: [&str; 2] = [PLAN_WRITE_NAME, PLAN_READ_NAME];
/// Greatest Unicode scalar count admitted for one entry text.
pub const MAX_PLAN_TEXT_CHARS: usize = 4096;
/// Greatest number of current entries returned by one read.
pub const MAX_PLAN_READ_ENTRIES: usize = 100;
/// Least number of history events returned when history is requested.
pub const MIN_PLAN_HISTORY_EVENTS: usize = 1;
/// Greatest number of history events returned by one read.
pub const MAX_PLAN_HISTORY_EVENTS: usize = 100;

const INVALID_ARGUMENTS_DETAIL: &str = "invalid bounded plan-tool arguments";
const ENTRY_NOT_FOUND_DETAIL: &str = "plan entry not found";

/// One positive ordinal in a session's append-only plan history.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PlanEventOrdinal(NonZeroU64);

impl PlanEventOrdinal {
    /// Reconstitutes a positive durable ordinal.
    pub const fn try_from_u64(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the first event ordinal.
    pub const fn first() -> Self {
        Self(NonZeroU64::MIN)
    }

    /// Returns the next ordinal when representable.
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.get().checked_add(1) {
            Some(value) => Self::try_from_u64(value),
            None => None,
        }
    }

    /// Returns the durable integer.
    pub const fn as_u64(self) -> u64 {
        self.0.get()
    }
}

/// Stable entry identity: the ordinal of its creation event.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PlanEntryId(PlanEventOrdinal);

impl PlanEntryId {
    /// Names the entry created by one event.
    pub const fn from_creation_ordinal(ordinal: PlanEventOrdinal) -> Self {
        Self(ordinal)
    }

    /// Reconstitutes a positive entry identity.
    pub const fn try_from_u64(value: u64) -> Option<Self> {
        match PlanEventOrdinal::try_from_u64(value) {
            Some(ordinal) => Some(Self(ordinal)),
            None => None,
        }
    }

    /// Returns the creation-event ordinal.
    pub const fn creation_ordinal(self) -> PlanEventOrdinal {
        self.0
    }

    /// Returns the model-facing integer.
    pub const fn as_u64(self) -> u64 {
        self.0.as_u64()
    }
}

/// Closed current status vocabulary for plan entries.
#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    Hash,
    PartialEq,
    serde::Deserialize,
    serde::Serialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    /// Work has not started.
    Pending,
    /// Work is currently underway.
    InProgress,
    /// Work finished successfully.
    Completed,
    /// Work was intentionally left unfinished.
    Abandoned,
}

/// Checked plan-entry text.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PlanText(String);

impl PlanText {
    /// Admits nonempty text within the declared scalar bound.
    pub fn try_new(value: String) -> Result<Self, PlanTextError> {
        if value.is_empty() {
            return Err(PlanTextError::Empty);
        }
        if value.contains('\0') {
            return Err(PlanTextError::ContainsNull);
        }
        let characters = value.chars().count();
        if characters > MAX_PLAN_TEXT_CHARS {
            return Err(PlanTextError::TooLong { characters });
        }
        Ok(Self(value))
    }

    /// Borrows the exact text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the exact text.
    pub fn into_string(self) -> String {
        self.0
    }
}

/// Why plan text was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanTextError {
    /// Empty entries are not meaningful steps.
    Empty,
    /// PostgreSQL text cannot retain U+0000.
    ContainsNull,
    /// Text exceeded the declared scalar bound.
    TooLong {
        /// Observed Unicode scalar count.
        characters: usize,
    },
}

impl fmt::Display for PlanTextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("plan text is empty"),
            Self::ContainsNull => formatter.write_str("plan text contains U+0000"),
            Self::TooLong { characters } => write!(
                formatter,
                "plan text has {characters} characters, above {MAX_PLAN_TEXT_CHARS}"
            ),
        }
    }
}

impl Error for PlanTextError {}

/// One checked append request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlanEventDraft {
    /// Creates a pending entry.
    Create {
        /// Initial text.
        text: PlanText,
    },
    /// Revises an entry's text.
    Revise {
        /// Target entry.
        entry: PlanEntryId,
        /// Replacement text.
        text: PlanText,
    },
    /// Changes an entry's status.
    SetStatus {
        /// Target entry.
        entry: PlanEntryId,
        /// New status.
        status: PlanStatus,
    },
}

/// Exact trusted invocation provenance retained on every event.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PlanEventProvenance(ToolAttemptDispatchCorrelation);

impl PlanEventProvenance {
    /// Retains the trusted physical-dispatch correlation.
    pub const fn from_invocation(correlation: ToolAttemptDispatchCorrelation) -> Self {
        Self(correlation)
    }

    /// Returns the owning session.
    pub const fn session(self) -> SessionId {
        self.0.session()
    }

    /// Returns the complete dispatch correlation.
    pub const fn correlation(self) -> ToolAttemptDispatchCorrelation {
        self.0
    }
}

/// One immutable event kind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlanEventKind {
    /// A pending entry was created.
    Created {
        /// Initial text.
        text: PlanText,
    },
    /// An entry's text was revised.
    TextRevised {
        /// Target entry.
        entry: PlanEntryId,
        /// Replacement text.
        text: PlanText,
    },
    /// An entry's status was changed.
    StatusChanged {
        /// Target entry.
        entry: PlanEntryId,
        /// New status.
        status: PlanStatus,
    },
}

/// One durable plan event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanEvent {
    ordinal: PlanEventOrdinal,
    provenance: PlanEventProvenance,
    kind: PlanEventKind,
}

impl PlanEvent {
    /// Reconstitutes one stored event.
    pub const fn new(
        ordinal: PlanEventOrdinal,
        provenance: PlanEventProvenance,
        kind: PlanEventKind,
    ) -> Self {
        Self {
            ordinal,
            provenance,
            kind,
        }
    }

    /// Returns the session-local ordinal.
    pub const fn ordinal(&self) -> PlanEventOrdinal {
        self.ordinal
    }

    /// Returns trusted provenance.
    pub const fn provenance(&self) -> PlanEventProvenance {
        self.provenance
    }

    /// Borrows event content.
    pub const fn kind(&self) -> &PlanEventKind {
        &self.kind
    }
}

/// One folded current entry, retained even when abandoned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanEntry {
    id: PlanEntryId,
    text: PlanText,
    status: PlanStatus,
}

impl PlanEntry {
    /// Supplies one folded entry.
    pub const fn new(id: PlanEntryId, text: PlanText, status: PlanStatus) -> Self {
        Self { id, text, status }
    }

    /// Returns the creation-event identity.
    pub const fn id(&self) -> PlanEntryId {
        self.id
    }

    /// Borrows the latest text.
    pub const fn text(&self) -> &PlanText {
        &self.text
    }

    /// Returns the latest status.
    pub const fn status(&self) -> PlanStatus {
        self.status
    }
}

/// Folded current plan in creation order.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FoldedPlan {
    entries: Vec<PlanEntry>,
}

impl FoldedPlan {
    /// Borrows current entries in creation order.
    pub fn entries(&self) -> &[PlanEntry] {
        &self.entries
    }

    /// Returns current entries in creation order.
    pub fn into_entries(self) -> Vec<PlanEntry> {
        self.entries
    }
}

/// Why an event sequence cannot represent one session plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanFoldError {
    /// Ordinals were not contiguous from one.
    NoncontiguousOrdinal {
        /// Next required ordinal.
        expected: PlanEventOrdinal,
    },
    /// Event provenance crossed session boundaries.
    MixedSessions,
    /// A mutation named no previously created entry.
    UnknownEntry {
        /// Missing target.
        entry: PlanEntryId,
    },
}

impl fmt::Display for PlanFoldError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoncontiguousOrdinal { expected } => {
                write!(
                    formatter,
                    "plan required event ordinal {}",
                    expected.as_u64()
                )
            }
            Self::MixedSessions => formatter.write_str("plan history mixes sessions"),
            Self::UnknownEntry { entry } => {
                write!(
                    formatter,
                    "plan history names unknown entry {}",
                    entry.as_u64()
                )
            }
        }
    }
}

impl Error for PlanFoldError {}

/// Folds one complete session history into current entries.
pub fn fold_plan_events(events: &[PlanEvent]) -> Result<FoldedPlan, PlanFoldError> {
    let mut expected = PlanEventOrdinal::first();
    let mut session = None;
    let mut entries = Vec::<PlanEntry>::new();
    for (index, event) in events.iter().enumerate() {
        if event.ordinal() != expected {
            return Err(PlanFoldError::NoncontiguousOrdinal { expected });
        }
        let event_session = event.provenance().session();
        if session.is_some_and(|session| session != event_session) {
            return Err(PlanFoldError::MixedSessions);
        }
        session = Some(event_session);
        match event.kind() {
            PlanEventKind::Created { text } => entries.push(PlanEntry::new(
                PlanEntryId::from_creation_ordinal(event.ordinal()),
                text.clone(),
                PlanStatus::Pending,
            )),
            PlanEventKind::TextRevised { entry, text } => {
                let current = entries
                    .iter_mut()
                    .find(|current| current.id() == *entry)
                    .ok_or(PlanFoldError::UnknownEntry { entry: *entry })?;
                current.text = text.clone();
            }
            PlanEventKind::StatusChanged { entry, status } => {
                let current = entries
                    .iter_mut()
                    .find(|current| current.id() == *entry)
                    .ok_or(PlanFoldError::UnknownEntry { entry: *entry })?;
                current.status = *status;
            }
        }
        if index + 1 < events.len() {
            expected = expected
                .checked_next()
                .ok_or(PlanFoldError::NoncontiguousOrdinal { expected })?;
        }
    }
    Ok(FoldedPlan { entries })
}

/// Port request for one atomic session-local append.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanAppendRequest {
    provenance: PlanEventProvenance,
    draft: PlanEventDraft,
}

impl PlanAppendRequest {
    /// Binds a checked draft to trusted provenance.
    pub const fn new(provenance: PlanEventProvenance, draft: PlanEventDraft) -> Self {
        Self { provenance, draft }
    }

    /// Returns the invoking session.
    pub const fn session(&self) -> SessionId {
        self.provenance.session()
    }

    /// Returns trusted provenance.
    pub const fn provenance(&self) -> PlanEventProvenance {
        self.provenance
    }

    /// Borrows requested event content.
    pub const fn draft(&self) -> &PlanEventDraft {
        &self.draft
    }
}

/// Typed evidence that one append was safely refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanAppendRejection {
    /// A mutation named no created entry in the invoking session.
    UnknownEntry {
        /// Missing creation-event identity.
        entry: PlanEntryId,
    },
}

/// Result of attempting one atomic append.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlanAppendOutcome {
    /// The event was durably appended.
    Appended(PlanEvent),
    /// No event was appended for the typed reason.
    Rejected(PlanAppendRejection),
}

/// Port request for one bounded current page and optional history.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlanReadRequest {
    session: SessionId,
    after_entry: Option<PlanEntryId>,
    history_limit: Option<usize>,
}

impl PlanReadRequest {
    /// Supplies one trusted target and optional history bound.
    pub const fn new(
        session: SessionId,
        after_entry: Option<PlanEntryId>,
        history_limit: Option<usize>,
    ) -> Self {
        let history_limit = match history_limit {
            Some(limit) if limit < MIN_PLAN_HISTORY_EVENTS => Some(MIN_PLAN_HISTORY_EVENTS),
            Some(limit) if limit > MAX_PLAN_HISTORY_EVENTS => Some(MAX_PLAN_HISTORY_EVENTS),
            requested => requested,
        };
        Self {
            session,
            after_entry,
            history_limit,
        }
    }

    /// Returns the trusted session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the exclusive entry cursor.
    pub const fn after_entry(&self) -> Option<PlanEntryId> {
        self.after_entry
    }

    /// Returns the fixed current-entry bound.
    pub const fn max_entries(&self) -> usize {
        MAX_PLAN_READ_ENTRIES
    }

    /// Returns the optional history bound.
    pub const fn history_limit(&self) -> Option<usize> {
        self.history_limit
    }
}

/// Whether a bounded plan page contains all requested evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanPageCompleteness {
    /// No requested evidence was omitted.
    Complete,
    /// Later requested evidence was omitted.
    Truncated,
}

impl PlanPageCompleteness {
    /// Returns whether requested evidence was omitted.
    pub const fn is_truncated(self) -> bool {
        match self {
            Self::Complete => false,
            Self::Truncated => true,
        }
    }
}

/// Optional bounded chronological history prefix.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanHistoryPage {
    events: Vec<PlanEvent>,
    completeness: PlanPageCompleteness,
}

impl PlanHistoryPage {
    /// Supplies returned events and their labeled completeness.
    pub fn new(events: Vec<PlanEvent>, completeness: PlanPageCompleteness) -> Self {
        Self {
            events,
            completeness,
        }
    }

    /// Borrows chronological events.
    pub fn events(&self) -> &[PlanEvent] {
        &self.events
    }

    /// Returns whether the history prefix is complete.
    pub const fn completeness(&self) -> PlanPageCompleteness {
        self.completeness
    }
}

/// One bounded folded-plan page returned by the port.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanReadPage {
    session: SessionId,
    entries: Vec<PlanEntry>,
    completeness: PlanPageCompleteness,
    history: Option<PlanHistoryPage>,
}

impl PlanReadPage {
    /// Supplies current entries and optional history.
    pub fn new(
        session: SessionId,
        entries: Vec<PlanEntry>,
        completeness: PlanPageCompleteness,
        history: Option<PlanHistoryPage>,
    ) -> Self {
        Self {
            session,
            entries,
            completeness,
            history,
        }
    }

    /// Returns the owning session carried by the port response.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Borrows current entries.
    pub fn entries(&self) -> &[PlanEntry] {
        &self.entries
    }

    /// Returns whether the current-entry page is complete.
    pub const fn completeness(&self) -> PlanPageCompleteness {
        self.completeness
    }

    /// Borrows optional history.
    pub const fn history(&self) -> Option<&PlanHistoryPage> {
        self.history.as_ref()
    }
}
impl PlanReadPage {
    fn evidence_units(&self) -> usize {
        self.entries.len()
            + self
                .history
                .as_ref()
                .map_or(0, |history| history.events.len())
    }

    fn truncated_to(&self, units: usize) -> Self {
        let reserved_history_units = usize::from(
            units > 0
                && self
                    .history
                    .as_ref()
                    .is_some_and(|history| !history.events.is_empty()),
        );
        let entry_units = units
            .saturating_sub(reserved_history_units)
            .min(self.entries.len());
        let history_units = units.saturating_sub(entry_units);
        let history = self.history.as_ref().map(|history| {
            let retained = history_units.min(history.events.len());
            PlanHistoryPage::new(
                history.events[..retained].to_vec(),
                retained_completeness(history.completeness, retained, history.events.len()),
            )
        });
        Self::new(
            self.session,
            self.entries[..entry_units].to_vec(),
            retained_completeness(self.completeness, entry_units, self.entries.len()),
            history,
        )
    }
}

fn retained_completeness(
    original: PlanPageCompleteness,
    retained: usize,
    available: usize,
) -> PlanPageCompleteness {
    if original.is_truncated() || retained < available {
        PlanPageCompleteness::Truncated
    } else {
        PlanPageCompleteness::Complete
    }
}

/// Durable boundary for the invoking session's plan.
pub trait SessionPlanPort: Send {
    /// Sanitized storage failure.
    type Error: ClassifyOperatorFailure + Error + 'static;

    /// Atomically assigns the next session ordinal and appends one event.
    fn append_plan_event(
        &mut self,
        request: PlanAppendRequest,
    ) -> impl Future<Output = Result<PlanAppendOutcome, Self::Error>> + Send;

    /// Reads a bounded folded page and optional bounded history.
    fn read_plan(
        &mut self,
        request: PlanReadRequest,
    ) -> impl Future<Output = Result<PlanReadPage, Self::Error>> + Send;
}

/// Typed append arguments. Each invocation appends exactly one event.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlanWriteArguments {
    /// Creates a pending entry.
    Create {
        /// Initial text.
        #[schemars(length(min = 1, max = MAX_PLAN_TEXT_CHARS))]
        text: String,
    },
    /// Revises entry text.
    Revise {
        /// Positive creation-event identity.
        #[schemars(range(min = 1))]
        entry_id: u64,
        /// Replacement text.
        #[schemars(length(min = 1, max = MAX_PLAN_TEXT_CHARS))]
        text: String,
    },
    /// Changes current status.
    SetStatus {
        /// Positive creation-event identity.
        #[schemars(range(min = 1))]
        entry_id: u64,
        /// New closed status.
        status: PlanStatus,
    },
}

/// Typed read arguments.
#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanReadArguments {
    /// Optional exclusive current-entry cursor.
    #[schemars(range(min = 1))]
    pub after_entry_id: Option<u64>,
    /// Whether to include bounded chronological history.
    #[serde(default)]
    pub include_history: bool,
}

struct PlanWriteContract;

impl ToolContract for PlanWriteContract {
    type Arguments = PlanWriteArguments;
    const NAME: &'static str = PLAN_WRITE_NAME;
    const DESCRIPTION: &'static str = "Appends one durable create, text-revision, or status-change event to the invoking session's plan.";
}

struct PlanReadContract;

impl ToolContract for PlanReadContract {
    type Arguments = PlanReadArguments;
    const NAME: &'static str = PLAN_READ_NAME;
    const DESCRIPTION: &'static str =
        "Reads the invoking session's folded current plan and optional bounded event history.";
}

/// A static plan declaration could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanToolConstructionError {
    /// A static name was invalid.
    Name,
    /// A static schema was invalid.
    Schema,
    /// A static error detail was invalid.
    ErrorDetail,
    /// The catalog contained a duplicate.
    Duplicate,
}

impl fmt::Display for PlanToolConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Name => "plan-tool static name is invalid",
            Self::Schema => "plan-tool static schema is invalid",
            Self::ErrorDetail => "plan-tool static error detail is invalid",
            Self::Duplicate => "plan-tool catalog is duplicated",
        })
    }
}

impl Error for PlanToolConstructionError {}

/// Compiled plan catalog and executor around one injected port.
#[derive(Clone, Debug)]
pub struct PlanTools<Port> {
    catalog: CompiledToolCatalog,
    executor: PlanExecutor<Port>,
}

impl<Port> PlanTools<Port> {
    /// Compiles both automatic session-scoped tools.
    pub fn try_new(port: Port) -> Result<Self, PlanToolConstructionError> {
        let invalid_detail = ToolExecutionErrorDetail::try_new(INVALID_ARGUMENTS_DETAIL.to_owned())
            .map_err(|_| PlanToolConstructionError::ErrorDetail)?;
        let entry_not_found_detail =
            ToolExecutionErrorDetail::try_new(ENTRY_NOT_FOUND_DETAIL.to_owned())
                .map_err(|_| PlanToolConstructionError::ErrorDetail)?;
        let write_definition = compile_contract_definition::<PlanWriteContract>(
            ToolPermissionDefault::Auto,
            ToolEffectClass::ExternalEffect,
        )
        .map_err(map_contract_error)?;
        let read_definition = compile_contract_definition::<PlanReadContract>(
            ToolPermissionDefault::Auto,
            ToolEffectClass::EffectFree,
        )
        .map_err(map_contract_error)?;
        let compiled = vec![
            CompiledTool::new(
                write_definition,
                PlanWriteArgumentValidator {
                    detail: invalid_detail.clone(),
                },
            ),
            CompiledTool::new(
                read_definition,
                PlanReadArgumentValidator {
                    detail: invalid_detail,
                },
            ),
        ];
        let catalog = CompiledToolCatalog::try_new(compiled)
            .map_err(|_| PlanToolConstructionError::Duplicate)?;
        Ok(Self {
            catalog,
            executor: PlanExecutor {
                port,
                entry_not_found_detail,
            },
        })
    }

    /// Returns catalog and executor as separate composition roles.
    pub fn into_parts(self) -> (CompiledToolCatalog, PlanExecutor<Port>) {
        (self.catalog, self.executor)
    }
}

fn map_contract_error(error: ToolContractCompileError) -> PlanToolConstructionError {
    match error {
        ToolContractCompileError::Name => PlanToolConstructionError::Name,
        ToolContractCompileError::Schema => PlanToolConstructionError::Schema,
    }
}

#[derive(Clone, Debug)]
struct PlanWriteArgumentValidator {
    detail: ToolExecutionErrorDetail,
}

impl ToolArgumentValidator for PlanWriteArgumentValidator {
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail> {
        decode_write_operation(arguments)
            .map(|_| ())
            .map_err(|_| self.detail.clone())
    }
}

#[derive(Clone, Debug)]
struct PlanReadArgumentValidator {
    detail: ToolExecutionErrorDetail,
}

impl ToolArgumentValidator for PlanReadArgumentValidator {
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail> {
        decode_read_operation(arguments)
            .map(|_| ())
            .map_err(|_| self.detail.clone())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PlanOperation {
    Write(PlanEventDraft),
    Read {
        after_entry: Option<PlanEntryId>,
        include_history: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InvalidPlanArguments;

fn decode_write_operation(
    arguments: &NormalizedToolArguments,
) -> Result<PlanOperation, InvalidPlanArguments> {
    let decoded: PlanWriteArguments =
        serde_json::from_str(arguments.as_str()).map_err(|_| InvalidPlanArguments)?;
    let draft = match decoded {
        PlanWriteArguments::Create { text } => PlanEventDraft::Create {
            text: PlanText::try_new(text).map_err(|_| InvalidPlanArguments)?,
        },
        PlanWriteArguments::Revise { entry_id, text } => PlanEventDraft::Revise {
            entry: PlanEntryId::try_from_u64(entry_id).ok_or(InvalidPlanArguments)?,
            text: PlanText::try_new(text).map_err(|_| InvalidPlanArguments)?,
        },
        PlanWriteArguments::SetStatus { entry_id, status } => PlanEventDraft::SetStatus {
            entry: PlanEntryId::try_from_u64(entry_id).ok_or(InvalidPlanArguments)?,
            status,
        },
    };
    Ok(PlanOperation::Write(draft))
}

fn decode_read_operation(
    arguments: &NormalizedToolArguments,
) -> Result<PlanOperation, InvalidPlanArguments> {
    let decoded: PlanReadArguments =
        serde_json::from_str(arguments.as_str()).map_err(|_| InvalidPlanArguments)?;
    let after_entry = decoded
        .after_entry_id
        .map(|value| PlanEntryId::try_from_u64(value).ok_or(InvalidPlanArguments))
        .transpose()?;
    Ok(PlanOperation::Read {
        after_entry,
        include_history: decoded.include_history,
    })
}

/// Executor for both session-scoped plan operations.
#[derive(Clone, Debug)]
pub struct PlanExecutor<Port> {
    port: Port,
    entry_not_found_detail: ToolExecutionErrorDetail,
}

impl<Port> PlanExecutor<Port> {
    /// Returns the injected port.
    pub fn into_port(self) -> Port {
        self.port
    }
}

/// Failure inside the plan executor.
#[derive(Debug)]
pub enum PlanExecutorError<PortError> {
    /// Executor decoding disagreed with catalog validation.
    ArgumentValidationDrift,
    /// The injected port failed.
    Port(PortError),
    /// The injected port violated provenance, ordering, or bounds.
    PortContract,
    /// Compact result encoding failed.
    ResultEncoding,
}

impl<PortError: fmt::Display> fmt::Display for PlanExecutorError<PortError> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ArgumentValidationDrift => {
                formatter.write_str("plan-tool argument validation drifted")
            }
            Self::Port(error) => error.fmt(formatter),
            Self::PortContract => formatter.write_str("session plan port contract was violated"),
            Self::ResultEncoding => formatter.write_str("plan-tool result encoding failed"),
        }
    }
}

impl<PortError: Error + 'static> Error for PlanExecutorError<PortError> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Port(error) => Some(error),
            Self::ArgumentValidationDrift | Self::PortContract | Self::ResultEncoding => None,
        }
    }
}

impl<PortError: ClassifyOperatorFailure> ClassifyOperatorFailure for PlanExecutorError<PortError> {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Port(error) => error.operator_failure_class(),
            Self::ArgumentValidationDrift | Self::PortContract | Self::ResultEncoding => {
                OperatorFailureClass::CallerOrHubBug
            }
        }
    }
}

impl<Port: SessionPlanPort> ToolExecutor for PlanExecutor<Port> {
    type Error = PlanExecutorError<Port::Error>;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        let correlation = invocation.correlation();
        let operation = match invocation.request().name().as_str() {
            PLAN_WRITE_NAME => decode_write_operation(invocation.request().arguments()),
            PLAN_READ_NAME => decode_read_operation(invocation.request().arguments()),
            _ => Err(InvalidPlanArguments),
        }
        .map_err(|_| PlanExecutorError::ArgumentValidationDrift)?;
        let evidence = self.execute_operation(correlation, operation).await?;
        Ok(invocation.bind(evidence))
    }
}

impl<Port: SessionPlanPort> PlanExecutor<Port> {
    async fn execute_operation(
        &mut self,
        correlation: ToolAttemptDispatchCorrelation,
        operation: PlanOperation,
    ) -> Result<ToolExecutorEvidence, PlanExecutorError<Port::Error>> {
        match operation {
            PlanOperation::Write(draft) => {
                let request = PlanAppendRequest::new(
                    PlanEventProvenance::from_invocation(correlation),
                    draft,
                );
                let outcome = self
                    .port
                    .append_plan_event(request.clone())
                    .await
                    .map_err(PlanExecutorError::Port)?;
                match outcome {
                    PlanAppendOutcome::Appended(event) => {
                        validate_append(&request, &event)?;
                        Ok(ToolExecutorEvidence::CompletedText(encode_append(event)?))
                    }
                    PlanAppendOutcome::Rejected(rejection) => match rejection {
                        PlanAppendRejection::UnknownEntry { entry } => {
                            if missing_entry_matches_request(request.draft(), entry) {
                                Ok(ToolExecutorEvidence::KnownFailed {
                                    detail: Some(self.entry_not_found_detail.clone()),
                                })
                            } else {
                                Err(PlanExecutorError::PortContract)
                            }
                        }
                    },
                }
            }
            PlanOperation::Read {
                after_entry,
                include_history,
            } => {
                let history_limit = include_history.then_some(MAX_PLAN_HISTORY_EVENTS);
                let request =
                    PlanReadRequest::new(correlation.session(), after_entry, history_limit);
                let page = self
                    .port
                    .read_plan(request)
                    .await
                    .map_err(PlanExecutorError::Port)?;
                validate_read_page(request, &page)?;
                Ok(ToolExecutorEvidence::CompletedText(encode_read(page)?))
            }
        }
    }
}

fn validate_append<PortError>(
    request: &PlanAppendRequest,
    event: &PlanEvent,
) -> Result<(), PlanExecutorError<PortError>> {
    if event.provenance() != request.provenance() || !event_matches_draft(event, request.draft()) {
        return Err(PlanExecutorError::PortContract);
    }
    Ok(())
}

fn event_matches_draft(event: &PlanEvent, draft: &PlanEventDraft) -> bool {
    match (event.kind(), draft) {
        (PlanEventKind::Created { text: stored }, PlanEventDraft::Create { text: requested }) => {
            stored == requested
        }
        (
            PlanEventKind::TextRevised {
                entry: stored_entry,
                text: stored_text,
            },
            PlanEventDraft::Revise {
                entry: requested_entry,
                text: requested_text,
            },
        ) => {
            stored_entry == requested_entry
                && stored_text == requested_text
                && event.ordinal() > stored_entry.creation_ordinal()
        }
        (
            PlanEventKind::StatusChanged {
                entry: stored_entry,
                status: stored_status,
            },
            PlanEventDraft::SetStatus {
                entry: requested_entry,
                status: requested_status,
            },
        ) => {
            stored_entry == requested_entry
                && stored_status == requested_status
                && event.ordinal() > stored_entry.creation_ordinal()
        }
        (
            PlanEventKind::Created { .. },
            PlanEventDraft::Revise { .. } | PlanEventDraft::SetStatus { .. },
        )
        | (
            PlanEventKind::TextRevised { .. },
            PlanEventDraft::Create { .. } | PlanEventDraft::SetStatus { .. },
        )
        | (
            PlanEventKind::StatusChanged { .. },
            PlanEventDraft::Create { .. } | PlanEventDraft::Revise { .. },
        ) => false,
    }
}

fn missing_entry_matches_request(draft: &PlanEventDraft, rejected_entry: PlanEntryId) -> bool {
    match draft {
        PlanEventDraft::Revise { entry, .. } | PlanEventDraft::SetStatus { entry, .. } => {
            *entry == rejected_entry
        }
        PlanEventDraft::Create { .. } => false,
    }
}

fn complete_history_matches_current(
    request: PlanReadRequest,
    page: &PlanReadPage,
    folded: FoldedPlan,
) -> bool {
    let mut expected = folded.into_entries();
    if let Some(after) = request.after_entry() {
        expected.retain(|entry| entry.id() > after);
    }
    let expected_completeness = if expected.len() > request.max_entries() {
        PlanPageCompleteness::Truncated
    } else {
        PlanPageCompleteness::Complete
    };
    expected.truncate(request.max_entries());
    page.entries() == expected && page.completeness() == expected_completeness
}

fn validate_read_page<PortError>(
    request: PlanReadRequest,
    page: &PlanReadPage,
) -> Result<(), PlanExecutorError<PortError>> {
    if page.session() != request.session()
        || page.entries().len() > request.max_entries()
        || (page.completeness().is_truncated() && page.entries().is_empty())
        || page.history().is_some() != request.history_limit().is_some()
    {
        return Err(PlanExecutorError::PortContract);
    }
    let mut previous = request.after_entry();
    for entry in page.entries() {
        if previous.is_some_and(|prior| entry.id() <= prior) {
            return Err(PlanExecutorError::PortContract);
        }
        previous = Some(entry.id());
    }
    if let Some(history) = page.history() {
        let limit = request
            .history_limit()
            .ok_or(PlanExecutorError::PortContract)?;
        if history.events().len() > limit
            || (history.completeness().is_truncated() && history.events().is_empty())
        {
            return Err(PlanExecutorError::PortContract);
        }
        let mut prior_ordinal = None;
        let mut provenance_attempts = HashSet::with_capacity(history.events().len());
        let folded =
            fold_plan_events(history.events()).map_err(|_| PlanExecutorError::PortContract)?;
        if !history.completeness().is_truncated()
            && !complete_history_matches_current(request, page, folded)
        {
            return Err(PlanExecutorError::PortContract);
        }

        for event in history.events() {
            if event.provenance().session() != request.session()
                || prior_ordinal.is_some_and(|prior| event.ordinal() <= prior)
                || !provenance_attempts.insert(event.provenance().correlation().attempt())
            {
                return Err(PlanExecutorError::PortContract);
            }
            prior_ordinal = Some(event.ordinal());
        }
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct ProvenanceOutput {
    turn_id: String,
    issuing_attempt_id: String,
    request_id: String,
    attempt_id: String,
    generation: u64,
}

impl From<PlanEventProvenance> for ProvenanceOutput {
    fn from(value: PlanEventProvenance) -> Self {
        let correlation = value.correlation();
        Self {
            turn_id: correlation.turn().into_uuid().to_string(),
            issuing_attempt_id: correlation.issuing_attempt().into_uuid().to_string(),
            request_id: correlation.request().into_uuid().to_string(),
            attempt_id: correlation.attempt().into_uuid().to_string(),
            generation: correlation.generation().as_u64(),
        }
    }
}

#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum EventKindOutput {
    Created { entry_id: u64, text: String },
    TextRevised { entry_id: u64, text: String },
    StatusChanged { entry_id: u64, status: PlanStatus },
}

#[derive(serde::Serialize)]
struct EventOutput {
    ordinal: u64,
    provenance: ProvenanceOutput,
    #[serde(flatten)]
    kind: EventKindOutput,
}

impl From<PlanEvent> for EventOutput {
    fn from(value: PlanEvent) -> Self {
        let ordinal = value.ordinal.as_u64();
        let provenance = value.provenance.into();
        let kind = match value.kind {
            PlanEventKind::Created { text } => EventKindOutput::Created {
                entry_id: ordinal,
                text: text.into_string(),
            },
            PlanEventKind::TextRevised { entry, text } => EventKindOutput::TextRevised {
                entry_id: entry.as_u64(),
                text: text.into_string(),
            },
            PlanEventKind::StatusChanged { entry, status } => EventKindOutput::StatusChanged {
                entry_id: entry.as_u64(),
                status,
            },
        };
        Self {
            ordinal,
            provenance,
            kind,
        }
    }
}

#[derive(serde::Serialize)]
struct AppendOutput {
    event: EventOutput,
}

fn encode_append<PortError>(event: PlanEvent) -> Result<String, PlanExecutorError<PortError>> {
    encode_result(&AppendOutput {
        event: event.into(),
    })
}

#[derive(serde::Serialize)]
struct EntryOutput {
    entry_id: u64,
    text: String,
    status: PlanStatus,
}

impl From<PlanEntry> for EntryOutput {
    fn from(value: PlanEntry) -> Self {
        Self {
            entry_id: value.id.as_u64(),
            text: value.text.into_string(),
            status: value.status,
        }
    }
}

#[derive(serde::Serialize)]
struct ReadOutput {
    entries: Vec<EntryOutput>,
    next_after_entry_id: Option<u64>,
    plan_truncated: bool,
    history: Option<Vec<EventOutput>>,
    history_truncated: bool,
}

fn encode_read<PortError>(page: PlanReadPage) -> Result<String, PlanExecutorError<PortError>> {
    if let Some(encoded) = encode_admitted_read(page.clone())? {
        return Ok(encoded);
    }
    let mut lower = 0_usize;
    let mut upper = page.evidence_units().saturating_sub(1);
    while lower < upper {
        let candidate_units = lower + (upper - lower).div_ceil(2);
        let candidate = page.truncated_to(candidate_units);
        if encode_admitted_read(candidate)?.is_some() {
            lower = candidate_units;
        } else {
            upper = candidate_units - 1;
        }
    }
    encode_admitted_read(page.truncated_to(lower))?.ok_or(PlanExecutorError::ResultEncoding)
}

fn encode_admitted_read<PortError>(
    page: PlanReadPage,
) -> Result<Option<String>, PlanExecutorError<PortError>> {
    let encoded =
        serde_json::to_string(&read_output(page)).map_err(|_| PlanExecutorError::ResultEncoding)?;
    match ToolResultText::try_new(encoded) {
        Ok(admitted) => Ok(Some(admitted.into_string())),
        Err(error) => match error.failure() {
            ToolResultTextFailure::TooLarge { .. } => Ok(None),
            ToolResultTextFailure::ContainsNull => Err(PlanExecutorError::ResultEncoding),
        },
    }
}

fn read_output(page: PlanReadPage) -> ReadOutput {
    let next_after_entry_id = if page.completeness.is_truncated() {
        page.entries.last().map(|entry| entry.id().as_u64())
    } else {
        None
    };
    let (history, history_truncated) = match page.history {
        Some(history) => (
            Some(history.events.into_iter().map(EventOutput::from).collect()),
            history.completeness.is_truncated(),
        ),
        None => (None, false),
    };
    ReadOutput {
        entries: page.entries.into_iter().map(EntryOutput::from).collect(),
        next_after_entry_id,
        plan_truncated: page.completeness.is_truncated(),
        history,
        history_truncated,
    }
}

fn encode_result<PortError>(
    value: &impl serde::Serialize,
) -> Result<String, PlanExecutorError<PortError>> {
    let encoded = serde_json::to_string(value).map_err(|_| PlanExecutorError::ResultEncoding)?;
    ToolResultText::try_new(encoded)
        .map(ToolResultText::into_string)
        .map_err(|_| PlanExecutorError::ResultEncoding)
}

#[cfg(test)]
mod tests;
