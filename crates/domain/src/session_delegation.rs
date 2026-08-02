//! Typed delegated-session relation, messages, outcomes, and wait subject.

use sha2::{Digest, Sha256};

use crate::{
    DelegationMessageId, DurableCommandId, GoalGeneration, NonEmptyUnicodeText, SessionId,
    ToolRequest, ToolRequestId, TurnId,
};

const SPAWN_SESSION_TOOL_NAME: &str = "spawn_session";
const AWAIT_SESSION_TOOL_NAME: &str = "await_session";
const SEND_SESSION_MESSAGE_TOOL_NAME: &str = "send_session_message";

/// Action applied to a bound child when its parent terminalizes.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BoundChildAction {
    KeepRunning,
    Stop,
    Cancel,
}

/// Durable relationship policy fixed when the child is spawned.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ChildRelationshipPolicy {
    Background,
    Bound {
        on_parent_stopped: BoundChildAction,
        on_parent_cancelled: BoundChildAction,
    },
}

/// Whether awaiting a child retains the parent turn slot.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DelegationWaitMode {
    Foreground,
    Background,
}

/// Explicit scope of a parent stop or cancellation command.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DescendantTerminationScope {
    ParentAlone,
    ParentAndDescendants,
}

/// Closed terminal action proven by an applied parent command.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ParentTerminationKind {
    Stopped,
    Cancelled,
}

/// Exact domain command source that applied a parent termination.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ParentTerminationCommandSource {
    /// An interrupt command applied to one exact live turn.
    Turn { turn: TurnId },
    /// A goal-stop command applied to one exact goal generation.
    Goal { generation: GoalGeneration },
}

/// Exact applied parent termination authority.
///
/// Raw identities cannot construct this proof. The scheduling slice supplies
/// it only from the exact applied stop or cancellation command result; this
/// foundation slice keeps that producer sealed.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ParentTerminationAuthority {
    parent: SessionId,
    source: ParentTerminationCommandSource,
    command: DurableCommandId,
    kind: ParentTerminationKind,
    scope: DescendantTerminationScope,
}

impl ParentTerminationAuthority {
    pub const fn parent(self) -> SessionId {
        self.parent
    }

    pub const fn source(self) -> ParentTerminationCommandSource {
        self.source
    }

    pub const fn turn(self) -> Option<TurnId> {
        match self.source {
            ParentTerminationCommandSource::Turn { turn } => Some(turn),
            ParentTerminationCommandSource::Goal { .. } => None,
        }
    }

    pub const fn goal_generation(self) -> Option<GoalGeneration> {
        match self.source {
            ParentTerminationCommandSource::Goal { generation } => Some(generation),
            ParentTerminationCommandSource::Turn { .. } => None,
        }
    }

    pub const fn command(self) -> DurableCommandId {
        self.command
    }

    pub const fn scope(self) -> DescendantTerminationScope {
        self.scope
    }

    pub const fn kind(self) -> ParentTerminationKind {
        self.kind
    }
}

/// Exact bounded nonempty content delivered across a delegation relation.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DelegationContent(NonEmptyUnicodeText);

impl DelegationContent {
    /// Maximum standalone UTF-8 size of one returned result.
    ///
    /// Tasks and messages also use this value, but their complete normalized
    /// JSON argument envelope can reject a shorter string.
    pub const MAX_UTF8_BYTES: usize = 1_048_576;

    pub fn try_new(value: String) -> Result<Self, DelegationContentError> {
        if value.len() > Self::MAX_UTF8_BYTES {
            let utf8_byte_length = value.len();
            return Err(DelegationContentError {
                value,
                failure: DelegationContentFailure::Oversized { utf8_byte_length },
            });
        }
        NonEmptyUnicodeText::try_new(value)
            .map(Self)
            .map_err(|error| {
                let (value, failure) = error.into_parts();
                DelegationContentError {
                    value,
                    failure: DelegationContentFailure::Invalid(failure),
                }
            })
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Concatenates exact assistant text parts in semantic order without
    /// inserting separators, then applies the delegation content bound.
    pub fn from_assistant_text(
        parts: &[crate::AssistantText],
    ) -> Result<Self, DelegationContentError> {
        let utf8_byte_length = parts.iter().map(|part| part.as_str().len()).sum();
        if utf8_byte_length > Self::MAX_UTF8_BYTES {
            let mut value = String::with_capacity(utf8_byte_length);
            for part in parts {
                value.push_str(part.as_str());
            }
            return Err(DelegationContentError {
                value,
                failure: DelegationContentFailure::Oversized { utf8_byte_length },
            });
        }
        let mut value = String::with_capacity(utf8_byte_length);
        for part in parts {
            value.push_str(part.as_str());
        }
        Self::try_new(value)
    }
}

/// Why delegated content was not admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DelegationContentFailure {
    Invalid(crate::NonEmptyUnicodeTextFailure),
    Oversized { utf8_byte_length: usize },
}

/// Failed content construction retaining the rejected string unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegationContentError {
    value: String,
    failure: DelegationContentFailure,
}

impl DelegationContentError {
    pub const fn failure(&self) -> DelegationContentFailure {
        self.failure
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn into_parts(self) -> (String, DelegationContentFailure) {
        (self.value, self.failure)
    }
}

impl std::fmt::Display for DelegationContentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.failure {
            DelegationContentFailure::Invalid(failure) => {
                write!(f, "invalid delegation content: {failure:?}")
            }
            DelegationContentFailure::Oversized { utf8_byte_length } => write!(
                f,
                "delegation content is {utf8_byte_length} bytes; maximum is {}",
                DelegationContent::MAX_UTF8_BYTES
            ),
        }
    }
}

impl std::error::Error for DelegationContentError {}

/// Why a tool request could not be sealed as one delegation operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DelegationRequestFailure {
    InvalidToolRequestPurpose,
    InvalidContent(DelegationContentError),
}

/// A rejected delegation request together with the unchanged logical request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegationRequestError {
    request: Box<ToolRequest>,
    failure: DelegationRequestFailure,
}

impl DelegationRequestError {
    pub const fn request(&self) -> &ToolRequest {
        &self.request
    }

    pub const fn failure(&self) -> &DelegationRequestFailure {
        &self.failure
    }

    pub fn into_request(self) -> ToolRequest {
        *self.request
    }
}

impl std::fmt::Display for DelegationRequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.failure {
            DelegationRequestFailure::InvalidToolRequestPurpose => {
                f.write_str("tool request is not the exact canonical delegation operation")
            }
            DelegationRequestFailure::InvalidContent(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for DelegationRequestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.failure {
            DelegationRequestFailure::InvalidContent(error) => Some(error),
            DelegationRequestFailure::InvalidToolRequestPurpose => None,
        }
    }
}

/// Canonical spawn request with its task and parent-chosen relationship sealed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegatedSpawnRequest {
    request: ToolRequest,
    task: DelegationContent,
    policy: ChildRelationshipPolicy,
}

impl DelegatedSpawnRequest {
    pub fn parse(
        request: ToolRequest,
        task: String,
        policy: ChildRelationshipPolicy,
    ) -> Result<Self, DelegationRequestError> {
        let value = parse_arguments(&request, SPAWN_SESSION_TOOL_NAME)?;
        let task = DelegationContent::try_new(task).map_err(|failure| DelegationRequestError {
            request: Box::new(request.clone()),
            failure: DelegationRequestFailure::InvalidContent(failure),
        })?;
        if value
            != serde_json::json!({
                "relationship": relationship_argument(policy),
                "task": task.as_str(),
            })
        {
            return Err(invalid_request(request));
        }
        Ok(Self {
            request,
            task,
            policy,
        })
    }

    pub const fn request(&self) -> &ToolRequest {
        &self.request
    }

    pub const fn task(&self) -> &DelegationContent {
        &self.task
    }

    pub const fn policy(&self) -> ChildRelationshipPolicy {
        self.policy
    }
}

/// Canonical await request with its exact child and wait mode sealed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegationAwaitRequest {
    request: ToolRequest,
    child: SessionId,
    mode: DelegationWaitMode,
}

impl DelegationAwaitRequest {
    pub fn parse(
        request: ToolRequest,
        child: SessionId,
        mode: DelegationWaitMode,
    ) -> Result<Self, DelegationRequestError> {
        let value = parse_arguments(&request, AWAIT_SESSION_TOOL_NAME)?;
        if value
            != serde_json::json!({
                "child_session_id": child.as_uuid().to_string(),
                "mode": wait_mode_argument(mode),
            })
        {
            return Err(invalid_request(request));
        }
        Ok(Self {
            request,
            child,
            mode,
        })
    }

    pub const fn request(&self) -> &ToolRequest {
        &self.request
    }

    pub const fn child(&self) -> SessionId {
        self.child
    }

    pub const fn mode(&self) -> DelegationWaitMode {
        self.mode
    }
}

/// Canonical message request with its peer and bounded content sealed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegationMessageRequest {
    request: ToolRequest,
    peer: SessionId,
    content: DelegationContent,
}

impl DelegationMessageRequest {
    pub fn parse(
        request: ToolRequest,
        peer: SessionId,
        content: String,
    ) -> Result<Self, DelegationRequestError> {
        let value = parse_arguments(&request, SEND_SESSION_MESSAGE_TOOL_NAME)?;
        let content =
            DelegationContent::try_new(content).map_err(|failure| DelegationRequestError {
                request: Box::new(request.clone()),
                failure: DelegationRequestFailure::InvalidContent(failure),
            })?;
        if value
            != serde_json::json!({
                "content": content.as_str(),
                "peer_session_id": peer.as_uuid().to_string(),
            })
        {
            return Err(invalid_request(request));
        }
        Ok(Self {
            request,
            peer,
            content,
        })
    }

    pub const fn request(&self) -> &ToolRequest {
        &self.request
    }

    pub const fn peer(&self) -> SessionId {
        self.peer
    }

    pub const fn content(&self) -> &DelegationContent {
        &self.content
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum TerminalChildTurnKind {
    Returned,
    Failed,
    Cancelled,
}

/// Exact child turn sealed by checked terminal scheduling evidence.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TerminalChildTurn {
    session: SessionId,
    turn: TurnId,
    kind: TerminalChildTurnKind,
    reason: DelegationOutcomeReason,
    result_digest: Option<[u8; 32]>,
}

impl TerminalChildTurn {
    /// Seals a live completed result from the completed execution candidate's
    /// own semantic entries. No independent content parameter is accepted.
    pub fn from_completed(value: &crate::CompletedModelCallTurn) -> Option<Self> {
        Some(terminal_from_completed_content(
            value.session(),
            value.turn(),
            delegation_content_from_live_completed(value),
        ))
    }

    pub fn from_scheduling(
        value: &crate::AcceptedInputTurnSchedulingProjection,
        reason: DelegationOutcomeReason,
    ) -> Option<Self> {
        let (kind, result_digest) = match reason {
            DelegationOutcomeReason::ChildExecutionFailed => match value.status() {
                crate::AcceptedInputTurnSchedulingStatus::TerminalFailed
                | crate::AcceptedInputTurnSchedulingStatus::TerminalRefused => {
                    (TerminalChildTurnKind::Failed, None)
                }
                crate::AcceptedInputTurnSchedulingStatus::Queued
                | crate::AcceptedInputTurnSchedulingStatus::Active
                | crate::AcceptedInputTurnSchedulingStatus::TerminalCompleted
                | crate::AcceptedInputTurnSchedulingStatus::TerminalCancelled
                | crate::AcceptedInputTurnSchedulingStatus::TerminalReconciliationRequired => {
                    return None;
                }
            },
            DelegationOutcomeReason::ChildCancelled => match value.status() {
                crate::AcceptedInputTurnSchedulingStatus::TerminalCancelled => {
                    (TerminalChildTurnKind::Cancelled, None)
                }
                crate::AcceptedInputTurnSchedulingStatus::Queued
                | crate::AcceptedInputTurnSchedulingStatus::Active
                | crate::AcceptedInputTurnSchedulingStatus::TerminalCompleted
                | crate::AcceptedInputTurnSchedulingStatus::TerminalFailed
                | crate::AcceptedInputTurnSchedulingStatus::TerminalRefused
                | crate::AcceptedInputTurnSchedulingStatus::TerminalReconciliationRequired => {
                    return None;
                }
            },
            DelegationOutcomeReason::ChildCompleted
            | DelegationOutcomeReason::ChildResultUnavailable
            | DelegationOutcomeReason::ParentStopped { .. }
            | DelegationOutcomeReason::ParentCancelled { .. } => return None,
        };
        Some(Self {
            session: value.session(),
            turn: value.turn(),
            kind,
            reason,
            result_digest,
        })
    }

    pub const fn session(self) -> SessionId {
        self.session
    }

    pub const fn turn(self) -> TurnId {
        self.turn
    }

    pub const fn reason(self) -> DelegationOutcomeReason {
        self.reason
    }
}

fn delegation_content_from_live_completed(
    value: &crate::CompletedModelCallTurn,
) -> Option<DelegationContent> {
    let mut assistant_text = Vec::with_capacity(value.assistant_entries().len());
    for entry in value.assistant_entries() {
        let (producing_call, text) = match entry.payload() {
            crate::SemanticTranscriptEntryPayload::AssistantText {
                producing_call,
                value,
            } => (producing_call, value),
            crate::SemanticTranscriptEntryPayload::Imported { .. }
            | crate::SemanticTranscriptEntryPayload::OriginAcceptedInput { .. }
            | crate::SemanticTranscriptEntryPayload::SteeringAcceptedInput { .. }
            | crate::SemanticTranscriptEntryPayload::ModelIdentityChanged { .. }
            | crate::SemanticTranscriptEntryPayload::ContextSummary { .. }
            | crate::SemanticTranscriptEntryPayload::TurnFailed { .. }
            | crate::SemanticTranscriptEntryPayload::AssistantToolUse { .. }
            | crate::SemanticTranscriptEntryPayload::ToolExecutionResult { .. }
            | crate::SemanticTranscriptEntryPayload::ToolDenied { .. }
            | crate::SemanticTranscriptEntryPayload::ToolClosed { .. }
            | crate::SemanticTranscriptEntryPayload::TurnCompleted { .. }
            | crate::SemanticTranscriptEntryPayload::TurnCancelled { .. } => return None,
        };
        if entry.source_session() != value.session() || *producing_call != value.call().id() {
            return None;
        }
        assistant_text.push(text);
    }
    let utf8_byte_length = assistant_text.iter().try_fold(0_usize, |total, text| {
        total.checked_add(text.as_str().len())
    })?;
    if utf8_byte_length > DelegationContent::MAX_UTF8_BYTES {
        return None;
    }
    let mut content = String::with_capacity(utf8_byte_length);
    for text in assistant_text {
        content.push_str(text.as_str());
    }
    DelegationContent::try_new(content).ok()
}

fn terminal_from_completed_content(
    session: SessionId,
    turn: TurnId,
    content: Option<DelegationContent>,
) -> TerminalChildTurn {
    let (kind, reason, result_digest) = match content {
        Some(content) => (
            TerminalChildTurnKind::Returned,
            DelegationOutcomeReason::ChildCompleted,
            Some(delegation_content_digest(&content)),
        ),
        None => (
            TerminalChildTurnKind::Failed,
            DelegationOutcomeReason::ChildResultUnavailable,
            None,
        ),
    };
    TerminalChildTurn {
        session,
        turn,
        kind,
        reason,
        result_digest,
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum DelegationProvenanceKind {
    ToolRequest {
        source_session: SessionId,
        source_turn: TurnId,
        request: ToolRequestId,
        purpose: DelegationToolRequestPurpose,
    },
    ChildTurn {
        terminal: TerminalChildTurn,
    },
    ParentCommand {
        authority: ParentTerminationAuthority,
    },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum DelegationToolRequestPurpose {
    Spawn,
    Await,
    SendMessage,
}

/// Exact typed authority for a delegation event.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DelegationProvenance {
    kind: DelegationProvenanceKind,
}

impl DelegationProvenance {
    pub fn from_spawn(request: &DelegatedSpawnRequest) -> Self {
        Self::from_request(request.request(), DelegationToolRequestPurpose::Spawn)
    }

    pub fn from_await(request: &DelegationAwaitRequest) -> Self {
        Self::from_request(request.request(), DelegationToolRequestPurpose::Await)
    }

    pub fn from_message(request: &DelegationMessageRequest) -> Self {
        Self::from_request(request.request(), DelegationToolRequestPurpose::SendMessage)
    }

    fn from_request(request: &ToolRequest, purpose: DelegationToolRequestPurpose) -> Self {
        Self {
            kind: DelegationProvenanceKind::ToolRequest {
                source_session: request.session(),
                source_turn: request.turn(),
                request: request.id(),
                purpose,
            },
        }
    }

    pub const fn from_terminal_child(terminal: TerminalChildTurn) -> Self {
        Self {
            kind: DelegationProvenanceKind::ChildTurn { terminal },
        }
    }

    pub const fn from_parent_termination(authority: ParentTerminationAuthority) -> Self {
        Self {
            kind: DelegationProvenanceKind::ParentCommand { authority },
        }
    }

    pub const fn tool_request(&self) -> Option<(SessionId, TurnId, ToolRequestId)> {
        match self.kind {
            DelegationProvenanceKind::ToolRequest {
                source_session,
                source_turn,
                request,
                ..
            } => Some((source_session, source_turn, request)),
            DelegationProvenanceKind::ChildTurn { .. }
            | DelegationProvenanceKind::ParentCommand { .. } => None,
        }
    }

    pub const fn child_turn(&self) -> Option<(SessionId, TurnId)> {
        match self.kind {
            DelegationProvenanceKind::ChildTurn { terminal } => {
                Some((terminal.session(), terminal.turn()))
            }
            DelegationProvenanceKind::ToolRequest { .. }
            | DelegationProvenanceKind::ParentCommand { .. } => None,
        }
    }

    pub const fn parent_command(&self) -> Option<ParentTerminationAuthority> {
        match self.kind {
            DelegationProvenanceKind::ParentCommand { authority } => Some(authority),
            DelegationProvenanceKind::ToolRequest { .. }
            | DelegationProvenanceKind::ChildTurn { .. } => None,
        }
    }
}

fn delegation_content_digest(content: &DelegationContent) -> [u8; 32] {
    Sha256::digest(content.as_str().as_bytes()).into()
}

fn parse_arguments(
    request: &ToolRequest,
    expected_name: &str,
) -> Result<serde_json::Value, DelegationRequestError> {
    if request.name().as_str() != expected_name {
        return Err(invalid_request(request.clone()));
    }
    serde_json::from_str(request.arguments().as_str()).map_err(|_| invalid_request(request.clone()))
}

fn invalid_request(request: ToolRequest) -> DelegationRequestError {
    DelegationRequestError {
        request: Box::new(request),
        failure: DelegationRequestFailure::InvalidToolRequestPurpose,
    }
}

fn relationship_argument(policy: ChildRelationshipPolicy) -> serde_json::Value {
    match policy {
        ChildRelationshipPolicy::Background => serde_json::json!({ "kind": "background" }),
        ChildRelationshipPolicy::Bound {
            on_parent_stopped,
            on_parent_cancelled,
        } => serde_json::json!({
            "kind": "bound",
            "on_parent_cancelled": action_argument(on_parent_cancelled),
            "on_parent_stopped": action_argument(on_parent_stopped),
        }),
    }
}

const fn action_argument(action: BoundChildAction) -> &'static str {
    match action {
        BoundChildAction::KeepRunning => "keep_running",
        BoundChildAction::Stop => "stop",
        BoundChildAction::Cancel => "cancel",
    }
}

const fn wait_mode_argument(mode: DelegationWaitMode) -> &'static str {
    match mode {
        DelegationWaitMode::Foreground => "foreground",
        DelegationWaitMode::Background => "background",
    }
}

/// Direction derived from a message's exact sender in the parent/child pair.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DelegationMessageDirection {
    ParentToChild,
    ChildToParent,
}

/// One immutable bidirectional message whose content is authoritative.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DelegationMessage {
    id: DelegationMessageId,
    direction: DelegationMessageDirection,
    content: DelegationContent,
    provenance: DelegationProvenance,
}

impl DelegationMessage {
    pub const fn id(&self) -> DelegationMessageId {
        self.id
    }
    pub const fn direction(&self) -> DelegationMessageDirection {
        self.direction
    }
    pub const fn content(&self) -> &DelegationContent {
        &self.content
    }
    pub const fn provenance(&self) -> DelegationProvenance {
        self.provenance
    }
}

/// Closed reason vocabulary for relation outcomes and no-change evaluations.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DelegationOutcomeReason {
    ChildCompleted,
    ChildExecutionFailed,
    ChildResultUnavailable,
    ChildCancelled,
    ParentStopped { scope: DescendantTerminationScope },
    ParentCancelled { scope: DescendantTerminationScope },
}

/// Closed child disposition kind carried by a validated outcome.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DelegationOutcomeKind {
    ResultReturned,
    ChildFailed,
    ChildStopped,
    ChildCancelled,
    ContinueRunning,
}

/// Validated child disposition with typed reason and sealed provenance.
///
/// Parent-policy construction remains crate-private so an external caller
/// cannot bypass the relationship policy chosen at spawn time:
///
/// ```compile_fail
/// use signalbox_domain::DelegationOutcome;
///
/// let _ = DelegationOutcome::from_parent_policy;
/// ```
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DelegationOutcome {
    kind: DelegationOutcomeKind,
    content: Option<DelegationContent>,
    reason: DelegationOutcomeReason,
    provenance: DelegationProvenance,
}

impl DelegationOutcome {
    /// Derives a returned, failed, or child-originated cancelled outcome from
    /// exact terminal child evidence. Stopped is not selectable here.
    pub fn from_terminal_child(
        terminal: TerminalChildTurn,
        content: Option<DelegationContent>,
    ) -> Option<Self> {
        let kind = match terminal.kind {
            TerminalChildTurnKind::Returned => match terminal.reason {
                DelegationOutcomeReason::ChildCompleted => match content.as_ref() {
                    Some(content)
                        if terminal.result_digest == Some(delegation_content_digest(content)) =>
                    {
                        DelegationOutcomeKind::ResultReturned
                    }
                    Some(_) | None => return None,
                },
                DelegationOutcomeReason::ChildExecutionFailed
                | DelegationOutcomeReason::ChildResultUnavailable
                | DelegationOutcomeReason::ChildCancelled
                | DelegationOutcomeReason::ParentStopped { .. }
                | DelegationOutcomeReason::ParentCancelled { .. } => return None,
            },
            TerminalChildTurnKind::Failed => match terminal.reason {
                DelegationOutcomeReason::ChildExecutionFailed
                | DelegationOutcomeReason::ChildResultUnavailable => match content.as_ref() {
                    None => DelegationOutcomeKind::ChildFailed,
                    Some(_) => return None,
                },
                DelegationOutcomeReason::ChildCompleted
                | DelegationOutcomeReason::ChildCancelled
                | DelegationOutcomeReason::ParentStopped { .. }
                | DelegationOutcomeReason::ParentCancelled { .. } => return None,
            },
            TerminalChildTurnKind::Cancelled => match terminal.reason {
                DelegationOutcomeReason::ChildCancelled => match content.as_ref() {
                    None => DelegationOutcomeKind::ChildCancelled,
                    Some(_) => return None,
                },
                DelegationOutcomeReason::ChildCompleted
                | DelegationOutcomeReason::ChildExecutionFailed
                | DelegationOutcomeReason::ChildResultUnavailable
                | DelegationOutcomeReason::ParentStopped { .. }
                | DelegationOutcomeReason::ParentCancelled { .. } => return None,
            },
        };
        Some(Self {
            kind,
            content,
            reason: terminal.reason,
            provenance: DelegationProvenance::from_terminal_child(terminal),
        })
    }

    /// Derives a policy disposition from exact applied parent authority.
    #[allow(dead_code, reason = "consumed by the stacked delegation aggregate")]
    pub(crate) const fn from_parent_policy(
        authority: ParentTerminationAuthority,
        action: BoundChildAction,
    ) -> Option<Self> {
        match authority.scope {
            DescendantTerminationScope::ParentAlone => None,
            DescendantTerminationScope::ParentAndDescendants => {
                let reason = match authority.kind {
                    ParentTerminationKind::Stopped => DelegationOutcomeReason::ParentStopped {
                        scope: DescendantTerminationScope::ParentAndDescendants,
                    },
                    ParentTerminationKind::Cancelled => DelegationOutcomeReason::ParentCancelled {
                        scope: DescendantTerminationScope::ParentAndDescendants,
                    },
                };
                let kind = match action {
                    BoundChildAction::KeepRunning => DelegationOutcomeKind::ContinueRunning,
                    BoundChildAction::Stop => DelegationOutcomeKind::ChildStopped,
                    BoundChildAction::Cancel => DelegationOutcomeKind::ChildCancelled,
                };
                Some(Self {
                    kind,
                    content: None,
                    reason,
                    provenance: DelegationProvenance::from_parent_termination(authority),
                })
            }
        }
    }

    pub const fn kind(&self) -> DelegationOutcomeKind {
        self.kind
    }

    pub const fn content(&self) -> Option<&DelegationContent> {
        self.content.as_ref()
    }

    pub const fn reason(&self) -> DelegationOutcomeReason {
        self.reason
    }

    pub const fn provenance(&self) -> DelegationProvenance {
        self.provenance
    }
}
/// Exact subject retained by a foreground parent turn wait.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ChildWait {
    awaiting_request: ToolRequestId,
    spawning_request: ToolRequestId,
    child: SessionId,
}

impl ChildWait {
    pub const fn awaiting_request(self) -> ToolRequestId {
        self.awaiting_request
    }
    pub const fn spawning_request(self) -> ToolRequestId {
        self.spawning_request
    }
    pub const fn child(self) -> SessionId {
        self.child
    }
}

/// One exact parent delivery registration made by an await tool request.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DelegationWait {
    awaiting_request: ToolRequestId,
    spawning_request: ToolRequestId,
    parent: SessionId,
    child: SessionId,
    mode: DelegationWaitMode,
}

impl DelegationWait {
    pub const fn awaiting_request(self) -> ToolRequestId {
        self.awaiting_request
    }
    pub const fn spawning_request(self) -> ToolRequestId {
        self.spawning_request
    }
    pub const fn parent(self) -> SessionId {
        self.parent
    }
    pub const fn child(self) -> SessionId {
        self.child
    }
    pub const fn mode(self) -> DelegationWaitMode {
        self.mode
    }
    pub const fn foreground_subject(self) -> Option<ChildWait> {
        match self.mode {
            DelegationWaitMode::Foreground => Some(ChildWait {
                awaiting_request: self.awaiting_request,
                spawning_request: self.spawning_request,
                child: self.child,
            }),
            DelegationWaitMode::Background => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        InitialToolApproval, NormalizedToolArguments, ToolCallProposal, ToolName,
        ToolRequestOrdinal,
        model_execution::completed_turn_fixture,
        test_support::{command_id, model_call_id, session_id, tool_request_id, turn_id},
    };
    use expect_test::expect;

    const TEST_TASK: &str = "inspect delegated work";

    /// Canonical request fixture: seed N derives request N+100, session N, turn
    /// N+10, call N+20, and ordinal zero.
    fn named_request(session: u128, name: &str, arguments: serde_json::Value) -> ToolRequest {
        ToolRequest::from_model_proposal(
            tool_request_id(session + 100),
            session_id(session),
            turn_id(session + 10),
            model_call_id(session + 20),
            ToolRequestOrdinal::from_u32(0),
            ToolCallProposal::new(
                ToolName::try_new(name.into()).expect("valid name"),
                NormalizedToolArguments::try_from_provider_text(arguments.to_string())
                    .expect("valid arguments"),
            ),
            InitialToolApproval::Confirm,
        )
    }

    fn content(value: &str) -> DelegationContent {
        DelegationContent::try_new(value.into()).expect("bounded nonempty content")
    }

    fn parent_termination_authority(
        scope: DescendantTerminationScope,
    ) -> ParentTerminationAuthority {
        ParentTerminationAuthority {
            parent: session_id(1),
            source: ParentTerminationCommandSource::Turn { turn: turn_id(2) },
            command: command_id(3),
            kind: ParentTerminationKind::Stopped,
            scope,
        }
    }

    #[test]
    fn spawn_request_seals_task_policy_and_provenance() {
        let raw = named_request(
            1,
            SPAWN_SESSION_TOOL_NAME,
            serde_json::json!({
                "relationship": { "kind": "background" },
                "task": TEST_TASK,
            }),
        );
        let expected_identity = (raw.session(), raw.turn(), raw.id());
        let request = DelegatedSpawnRequest::parse(
            raw,
            TEST_TASK.into(),
            ChildRelationshipPolicy::Background,
        )
        .expect("canonical spawn request");
        let provenance = DelegationProvenance::from_spawn(&request);

        assert_eq!(request.task(), &content(TEST_TASK));
        assert_eq!(request.policy(), ChildRelationshipPolicy::Background);
        assert_eq!(provenance.tool_request(), Some(expected_identity));
    }

    #[test]
    fn spawn_request_accepts_canonical_bound_relationship() {
        let policy = ChildRelationshipPolicy::Bound {
            on_parent_stopped: BoundChildAction::Stop,
            on_parent_cancelled: BoundChildAction::Cancel,
        };
        let raw = named_request(
            1,
            SPAWN_SESSION_TOOL_NAME,
            serde_json::json!({
                "relationship": {
                    "kind": "bound",
                    "on_parent_cancelled": "cancel",
                    "on_parent_stopped": "stop",
                },
                "task": TEST_TASK,
            }),
        );
        let request = DelegatedSpawnRequest::parse(raw, TEST_TASK.into(), policy)
            .expect("canonical bound relationship is admitted");

        assert_eq!(request.policy(), policy);
    }

    #[test]
    fn await_request_seals_child_and_mode() {
        let child = session_id(2);
        let raw = named_request(
            1,
            AWAIT_SESSION_TOOL_NAME,
            serde_json::json!({
                "child_session_id": child.as_uuid().to_string(),
                "mode": "foreground",
            }),
        );
        let expected_identity = (raw.session(), raw.turn(), raw.id());
        let request = DelegationAwaitRequest::parse(raw, child, DelegationWaitMode::Foreground)
            .expect("canonical await request");

        assert_eq!(request.child(), child);
        assert_eq!(request.mode(), DelegationWaitMode::Foreground);
        assert_eq!(
            DelegationProvenance::from_await(&request).tool_request(),
            Some(expected_identity)
        );
    }

    #[test]
    fn message_request_seals_peer_content_and_provenance() {
        let peer = session_id(2);
        let message = content("progress update");
        let raw = named_request(
            1,
            SEND_SESSION_MESSAGE_TOOL_NAME,
            serde_json::json!({
                "content": message.as_str(),
                "peer_session_id": peer.as_uuid().to_string(),
            }),
        );
        let expected_identity = (raw.session(), raw.turn(), raw.id());
        let request = DelegationMessageRequest::parse(raw, peer, message.as_str().into())
            .expect("canonical message request");

        assert_eq!(request.peer(), peer);
        assert_eq!(request.content(), &message);
        assert_eq!(
            DelegationProvenance::from_message(&request).tool_request(),
            Some(expected_identity)
        );
    }

    #[test]
    fn request_parser_rejects_noncanonical_bound_spelling_without_consuming_request() {
        let raw = named_request(
            1,
            SPAWN_SESSION_TOOL_NAME,
            serde_json::json!({
                "relationship": {
                    "kind": "Bound",
                    "on_parent_cancelled": "keep_running",
                    "on_parent_stopped": "keep_running",
                },
                "task": TEST_TASK,
            }),
        );
        let error = DelegatedSpawnRequest::parse(
            raw.clone(),
            TEST_TASK.into(),
            ChildRelationshipPolicy::Bound {
                on_parent_stopped: BoundChildAction::KeepRunning,
                on_parent_cancelled: BoundChildAction::KeepRunning,
            },
        )
        .expect_err("relationship spelling is closed and lowercase");

        assert_eq!(
            error.failure(),
            &DelegationRequestFailure::InvalidToolRequestPurpose
        );
        assert_eq!(error.into_request(), raw);
    }

    #[test]
    fn spawn_request_rejects_carried_task_drift() {
        let raw = named_request(
            1,
            SPAWN_SESSION_TOOL_NAME,
            serde_json::json!({
                "relationship": { "kind": "background" },
                "task": TEST_TASK,
            }),
        );
        let error = DelegatedSpawnRequest::parse(
            raw,
            "different task".into(),
            ChildRelationshipPolicy::Background,
        )
        .expect_err("carried task must match the canonical request");

        assert_eq!(
            error.failure(),
            &DelegationRequestFailure::InvalidToolRequestPurpose
        );
    }

    #[test]
    fn request_error_forwards_recoverable_invalid_content() {
        let raw = named_request(
            1,
            SPAWN_SESSION_TOOL_NAME,
            serde_json::json!({
                "relationship": { "kind": "background" },
                "task": "",
            }),
        );
        let error =
            DelegatedSpawnRequest::parse(raw, "".into(), ChildRelationshipPolicy::Background)
                .expect_err("empty task is invalid content");
        let source = std::error::Error::source(&error)
            .and_then(|source| source.downcast_ref::<DelegationContentError>())
            .expect("invalid content is the request error source");

        assert_eq!(source.value(), "");
        assert_eq!(
            source.failure(),
            DelegationContentFailure::Invalid(crate::NonEmptyUnicodeTextFailure::Empty)
        );
    }

    #[test]
    fn spawn_request_checks_tool_purpose_before_task_content() {
        let raw = named_request(
            1,
            AWAIT_SESSION_TOOL_NAME,
            serde_json::json!({
                "child_session_id": session_id(2).as_uuid().to_string(),
                "mode": "foreground",
            }),
        );
        let error = DelegatedSpawnRequest::parse(
            raw.clone(),
            String::new(),
            ChildRelationshipPolicy::Background,
        )
        .expect_err("the wrong operation is rejected before invalid task content");

        assert_eq!(
            error.failure(),
            &DelegationRequestFailure::InvalidToolRequestPurpose
        );
        assert_eq!(error.into_request(), raw);
    }

    #[test]
    fn message_request_checks_tool_purpose_before_message_content() {
        let raw = named_request(
            1,
            AWAIT_SESSION_TOOL_NAME,
            serde_json::json!({
                "child_session_id": session_id(2).as_uuid().to_string(),
                "mode": "foreground",
            }),
        );
        let error = DelegationMessageRequest::parse(raw.clone(), session_id(2), String::new())
            .expect_err("the wrong operation is rejected before invalid message content");

        assert_eq!(
            error.failure(),
            &DelegationRequestFailure::InvalidToolRequestPurpose
        );
        assert_eq!(error.into_request(), raw);
    }

    #[test]
    fn await_request_rejects_carried_child_drift() {
        let child = session_id(2);
        let unrelated_child = session_id(9);
        let awaiting = named_request(
            1,
            AWAIT_SESSION_TOOL_NAME,
            serde_json::json!({
                "child_session_id": child.as_uuid().to_string(),
                "mode": "foreground",
            }),
        );
        assert!(
            DelegationAwaitRequest::parse(
                awaiting,
                unrelated_child,
                DelegationWaitMode::Foreground
            )
            .is_err()
        );
    }

    #[test]
    fn await_request_rejects_carried_mode_drift() {
        let child = session_id(2);
        let awaiting = named_request(
            1,
            AWAIT_SESSION_TOOL_NAME,
            serde_json::json!({
                "child_session_id": child.as_uuid().to_string(),
                "mode": "foreground",
            }),
        );

        assert!(
            DelegationAwaitRequest::parse(awaiting, child, DelegationWaitMode::Background).is_err()
        );
    }

    #[test]
    fn message_request_rejects_carried_peer_drift() {
        let message = content("progress update");
        let unrelated_peer = session_id(9);
        let sending = named_request(
            2,
            SEND_SESSION_MESSAGE_TOOL_NAME,
            serde_json::json!({
                "content": message.as_str(),
                "peer_session_id": session_id(1).as_uuid().to_string(),
            }),
        );

        assert!(
            DelegationMessageRequest::parse(sending, unrelated_peer, message.as_str().into())
                .is_err()
        );
    }

    #[test]
    fn message_request_rejects_carried_content_drift() {
        let message = content("progress update");
        let sending = named_request(
            2,
            SEND_SESSION_MESSAGE_TOOL_NAME,
            serde_json::json!({
                "content": message.as_str(),
                "peer_session_id": session_id(1).as_uuid().to_string(),
            }),
        );

        assert!(DelegationMessageRequest::parse(sending, session_id(1), "drift".into()).is_err());
    }

    /// S18 / INV-010: a live completed call seals its own nonempty result.
    #[test]
    fn s18_inv010_live_completed_call_proves_returned_content() {
        let expected = content("completed child result");
        let completed = completed_turn_fixture(&[expected.as_str()]);
        let terminal = TerminalChildTurn::from_completed(&completed)
            .expect("live completed call seals terminal child evidence");
        let outcome = DelegationOutcome::from_terminal_child(terminal, Some(expected.clone()))
            .expect("sealed live result authenticates its exact content");

        assert_eq!(outcome.kind(), DelegationOutcomeKind::ResultReturned);
        assert_eq!(outcome.content(), Some(&expected));
    }

    /// S18 / INV-010: empty live completion is a typed unavailable result.
    #[test]
    fn s18_inv010_empty_live_completion_produces_unavailable_outcome() {
        let completed = completed_turn_fixture(&[]);
        let terminal = TerminalChildTurn::from_completed(&completed)
            .expect("empty live completion remains terminal child evidence");
        let outcome = DelegationOutcome::from_terminal_child(terminal, None)
            .expect("empty completion derives an explicit failed outcome");

        assert_eq!(outcome.kind(), DelegationOutcomeKind::ChildFailed);
        assert_eq!(
            outcome.reason(),
            DelegationOutcomeReason::ChildResultUnavailable
        );
    }

    /// S18 / INV-010: oversized aggregate live completion is typed unavailable.
    #[test]
    fn s18_inv010_oversized_live_completion_produces_unavailable_outcome() {
        let part = "x".repeat(DelegationContent::MAX_UTF8_BYTES / 2 + 1);
        let completed = completed_turn_fixture(&[part.as_str(), part.as_str()]);
        let terminal = TerminalChildTurn::from_completed(&completed)
            .expect("oversized live completion remains terminal child evidence");
        let outcome = DelegationOutcome::from_terminal_child(terminal, None)
            .expect("oversized completion derives an explicit failed outcome");

        assert_eq!(outcome.kind(), DelegationOutcomeKind::ChildFailed);
        assert_eq!(
            outcome.reason(),
            DelegationOutcomeReason::ChildResultUnavailable
        );
    }

    /// S18 / INV-010: terminal proof authenticates returned content.
    #[test]
    fn s18_inv010_terminal_proof_rejects_fabricated_content() {
        let expected = content("stored result");
        let returned = TerminalChildTurn {
            session: session_id(3),
            turn: turn_id(4),
            kind: TerminalChildTurnKind::Returned,
            reason: DelegationOutcomeReason::ChildCompleted,
            result_digest: Some(delegation_content_digest(&expected)),
        };

        assert_eq!(
            DelegationOutcome::from_terminal_child(returned, Some(content("fabricated"))),
            None
        );
    }

    /// S18 / INV-010: child-origin terminal proof cannot select stopped.
    #[test]
    fn s18_inv010_terminal_proof_cannot_construct_stopped_outcome() {
        let cancelled = TerminalChildTurn {
            session: session_id(3),
            turn: turn_id(4),
            kind: TerminalChildTurnKind::Cancelled,
            reason: DelegationOutcomeReason::ChildCancelled,
            result_digest: None,
        };

        assert_eq!(
            DelegationOutcome::from_terminal_child(cancelled, None)
                .expect("cancelled proof derives one outcome")
                .kind(),
            DelegationOutcomeKind::ChildCancelled
        );
    }

    /// S18 / INV-010: parent-alone authority cannot disposition descendants.
    #[test]
    fn s18_inv010_parent_alone_cannot_construct_descendant_outcome() {
        let authority = parent_termination_authority(DescendantTerminationScope::ParentAlone);

        assert_eq!(
            DelegationOutcome::from_parent_policy(authority, BoundChildAction::Stop),
            None
        );
    }

    /// S18 / INV-010: parent-and-descendants authority admits bound policy.
    #[test]
    fn s18_inv010_parent_and_descendants_constructs_policy_outcome() {
        let authority =
            parent_termination_authority(DescendantTerminationScope::ParentAndDescendants);
        let outcome = DelegationOutcome::from_parent_policy(authority, BoundChildAction::Stop)
            .expect("descendant-scoped authority may apply bound policy");

        assert_eq!(outcome.kind(), DelegationOutcomeKind::ChildStopped);
        assert_eq!(
            outcome.reason(),
            DelegationOutcomeReason::ParentStopped {
                scope: DescendantTerminationScope::ParentAndDescendants,
            }
        );
    }

    /// S18 / INV-010: descendant-scoped cancellation selects its exact reason.
    #[test]
    fn s18_inv010_parent_and_descendants_cancel_constructs_policy_outcome() {
        let authority = ParentTerminationAuthority {
            parent: session_id(1),
            source: ParentTerminationCommandSource::Turn { turn: turn_id(2) },
            command: command_id(3),
            kind: ParentTerminationKind::Cancelled,
            scope: DescendantTerminationScope::ParentAndDescendants,
        };
        let outcome = DelegationOutcome::from_parent_policy(authority, BoundChildAction::Cancel)
            .expect("descendant-scoped cancellation may apply bound policy");

        assert_eq!(outcome.kind(), DelegationOutcomeKind::ChildCancelled);
        assert_eq!(
            outcome.reason(),
            DelegationOutcomeReason::ParentCancelled {
                scope: DescendantTerminationScope::ParentAndDescendants,
            }
        );
    }

    /// S18 / INV-010: goal-stop authority names a generation without a turn.
    #[test]
    fn s18_inv010_goal_termination_authority_never_fabricates_a_turn() {
        let generation = GoalGeneration::new(std::num::NonZeroU64::MIN);
        let authority = ParentTerminationAuthority {
            parent: session_id(1),
            source: ParentTerminationCommandSource::Goal { generation },
            command: command_id(3),
            kind: ParentTerminationKind::Stopped,
            scope: DescendantTerminationScope::ParentAndDescendants,
        };
        let provenance = DelegationProvenance::from_parent_termination(authority);

        assert_eq!(authority.turn(), None);
        assert_eq!(authority.goal_generation(), Some(generation));
        assert_eq!(provenance.parent_command(), Some(authority));
    }

    #[test]
    fn content_bound_reports_exact_utf8_length() {
        let oversized_utf8_bytes = DelegationContent::MAX_UTF8_BYTES + 1;
        let value = "x".repeat(oversized_utf8_bytes);
        let expected_failure = DelegationContentFailure::Oversized {
            utf8_byte_length: oversized_utf8_bytes,
        };
        let error = DelegationContent::try_new(value.clone())
            .expect_err("oversized content must fail without consuming the input");

        assert_eq!(error.failure(), expected_failure);
        assert_eq!(error.value(), value);
        expect!["delegation content is 1048577 bytes; maximum is 1048576"]
            .assert_eq(&error.to_string());
        let (rejected_value, failure) = error.into_parts();
        assert_eq!(rejected_value, value);
        assert_eq!(failure, expected_failure);
    }
}
