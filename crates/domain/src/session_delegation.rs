//! Typed delegated-session relation, messages, outcomes, and wait subject.

use std::{collections::HashSet, num::NonZeroU64};

use sha2::{Digest, Sha256};

use crate::{
    DelegationMessageId, DurableCommandId, GoalGeneration, NonEmptyUnicodeText,
    SessionCreationProvenance, SessionId, ToolRequest, ToolRequestId, TurnId,
};

const SPAWN_SESSION_TOOL_NAME: &str = "spawn_session";
const AWAIT_SESSION_TOOL_NAME: &str = "await_session";
const SEND_SESSION_MESSAGE_TOOL_NAME: &str = "send_session_message";

/// Returns the persisted model-facing child-spawn tool name.
pub const fn spawn_session_tool_name() -> &'static str {
    SPAWN_SESSION_TOOL_NAME
}

/// Returns the persisted model-facing child-result wait tool name.
pub const fn await_session_tool_name() -> &'static str {
    AWAIT_SESSION_TOOL_NAME
}

/// Returns the persisted model-facing bidirectional-message tool name.
pub const fn send_session_message_tool_name() -> &'static str {
    SEND_SESSION_MESSAGE_TOOL_NAME
}

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
    /// A lifecycle stop applied directly to a session with no live turn.
    Lifecycle,
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
            ParentTerminationCommandSource::Goal { .. }
            | ParentTerminationCommandSource::Lifecycle => None,
        }
    }

    pub const fn goal_generation(self) -> Option<GoalGeneration> {
        match self.source {
            ParentTerminationCommandSource::Goal { generation } => Some(generation),
            ParentTerminationCommandSource::Turn { .. }
            | ParentTerminationCommandSource::Lifecycle => None,
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

    /// Seals an execution failure from the exact failed-turn commit candidate.
    /// Unlike an accepted-input scheduling projection, this evidence may name
    /// any turn origin, including a delegated task.
    pub const fn from_failed(value: &crate::FailedModelCallTurn) -> Self {
        Self {
            session: value.session(),
            turn: value.turn(),
            kind: TerminalChildTurnKind::Failed,
            reason: DelegationOutcomeReason::ChildExecutionFailed,
            result_digest: None,
        }
    }

    /// Seals a reconciliation-required child as a failed result whose provider
    /// outcome cannot be made available to its waiting parent.
    pub const fn from_reconciliation_required(
        value: &crate::ReconciliationRequiredModelCallTurn,
    ) -> Self {
        Self {
            session: value.session(),
            turn: value.turn(),
            kind: TerminalChildTurnKind::Failed,
            reason: DelegationOutcomeReason::ChildResultUnavailable,
            result_digest: None,
        }
    }

    /// Seals cancellation from the exact cancelled-turn commit candidate.
    /// Unlike an accepted-input scheduling projection, this evidence may name
    /// any turn origin, including a delegated task.
    pub const fn from_cancelled(value: &crate::CancelledModelCallTurn) -> Self {
        Self {
            session: value.session(),
            turn: value.turn(),
            kind: TerminalChildTurnKind::Cancelled,
            reason: DelegationOutcomeReason::ChildCancelled,
            result_digest: None,
        }
    }

    /// Seals cancellation when an interrupt closed a tool-using response.
    pub const fn from_cancelled_tool_round(value: &crate::CancelledToolRoundModelCallTurn) -> Self {
        Self {
            session: value.session(),
            turn: value.turn(),
            kind: TerminalChildTurnKind::Cancelled,
            reason: DelegationOutcomeReason::ChildCancelled,
            result_digest: None,
        }
    }

    /// Seals a provider refusal from the exact refused-turn commit candidate.
    pub const fn from_refused(value: &crate::RefusedModelCallTurn) -> Self {
        Self {
            session: value.session(),
            turn: value.turn(),
            kind: TerminalChildTurnKind::Failed,
            reason: DelegationOutcomeReason::ChildExecutionFailed,
            result_digest: None,
        }
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
            | crate::SemanticTranscriptEntryPayload::DelegatedTask { .. }
            | crate::SemanticTranscriptEntryPayload::DelegationMessage { .. }
            | crate::SemanticTranscriptEntryPayload::DelegationResult { .. }
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

/// Closed public projection of one event's typed authority.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DelegationProvenanceProjection {
    ToolRequest {
        source_session: SessionId,
        source_turn: TurnId,
        request: ToolRequestId,
    },
    ChildTurn {
        terminal: TerminalChildTurn,
    },
    ParentCommand {
        authority: ParentTerminationAuthority,
    },
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

    /// Projects every closed provenance variant without optional probing.
    pub const fn projection(self) -> DelegationProvenanceProjection {
        match self.kind {
            DelegationProvenanceKind::ToolRequest {
                source_session,
                source_turn,
                request,
                ..
            } => DelegationProvenanceProjection::ToolRequest {
                source_session,
                source_turn,
                request,
            },
            DelegationProvenanceKind::ChildTurn { terminal } => {
                DelegationProvenanceProjection::ChildTurn { terminal }
            }
            DelegationProvenanceKind::ParentCommand { authority } => {
                DelegationProvenanceProjection::ParentCommand { authority }
            }
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

/// Labeled parent and child endpoints used to restore one stored message.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DelegationMessageEndpoints {
    pub parent: SessionId,
    pub child: SessionId,
}

/// One immutable bidirectional message whose content is authoritative.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DelegationMessage {
    id: DelegationMessageId,
    direction: DelegationMessageDirection,
    peer: SessionId,
    content: DelegationContent,
    provenance: DelegationProvenance,
}

impl DelegationMessage {
    /// Reconstitutes one stored message from its canonical logical request.
    ///
    /// The containing [`SessionDelegation`] reconstitution validates the
    /// direction against the immutable parent and child endpoints.
    pub fn reconstitute(
        request: &DelegationMessageRequest,
        id: DelegationMessageId,
        direction: DelegationMessageDirection,
        endpoints: DelegationMessageEndpoints,
    ) -> Option<Self> {
        let endpoints_match = match direction {
            DelegationMessageDirection::ParentToChild => {
                request.request().session() == endpoints.parent && request.peer() == endpoints.child
            }
            DelegationMessageDirection::ChildToParent => {
                request.request().session() == endpoints.child && request.peer() == endpoints.parent
            }
        };
        endpoints_match.then(|| Self {
            id,
            direction,
            peer: request.peer(),
            content: request.content().clone(),
            provenance: DelegationProvenance::from_message(request),
        })
    }

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
    AlreadyTerminal,
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
    provenance: DelegationOutcomeProvenance,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum DelegationOutcomeProvenance {
    ChildTurn(TerminalChildTurn),
    ParentCommand(ParentTerminationAuthority),
}

/// Stored proof source supplied to checked delegation-outcome reconstitution.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DelegationProvenanceReconstitutionInput {
    ChildTurn {
        session: SessionId,
        turn: TurnId,
    },
    ParentTurnCommand {
        session: SessionId,
        turn: TurnId,
        command: DurableCommandId,
    },
    ParentGoalCommand {
        session: SessionId,
        generation: GoalGeneration,
        command: DurableCommandId,
    },
    ParentLifecycleCommand {
        session: SessionId,
        command: DurableCommandId,
    },
}

impl DelegationOutcome {
    /// Derives the complete delivered outcome from a completed child turn's
    /// proof-bearing assistant entries.
    pub fn from_completed_child(value: &crate::CompletedModelCallTurn) -> Self {
        let content = delegation_content_from_live_completed(value);
        let terminal =
            terminal_from_completed_content(value.session(), value.turn(), content.clone());
        Self {
            kind: if content.is_some() {
                DelegationOutcomeKind::ResultReturned
            } else {
                DelegationOutcomeKind::ChildFailed
            },
            content,
            reason: terminal.reason,
            provenance: DelegationOutcomeProvenance::ChildTurn(terminal),
        }
    }

    /// Derives a failed delivered outcome from a failed child turn.
    pub fn from_failed_child(value: &crate::FailedModelCallTurn) -> Self {
        Self::failed_child(TerminalChildTurn::from_failed(value))
    }

    /// Derives an unavailable failed result from a child whose ambiguous model
    /// call required terminal reconciliation.
    pub fn from_reconciliation_required_child(
        value: &crate::ReconciliationRequiredModelCallTurn,
    ) -> Self {
        Self::failed_child(TerminalChildTurn::from_reconciliation_required(value))
    }

    /// Derives a failed delivered outcome from a refused child turn.
    pub fn from_refused_child(value: &crate::RefusedModelCallTurn) -> Self {
        Self::failed_child(TerminalChildTurn::from_refused(value))
    }

    /// Derives a cancelled delivered outcome from a cancelled child turn.
    pub fn from_cancelled_child(value: &crate::CancelledModelCallTurn) -> Self {
        Self::cancelled_child(TerminalChildTurn::from_cancelled(value))
    }

    /// Derives a cancelled delivered outcome from a cancelled tool-round child.
    pub fn from_cancelled_tool_round_child(value: &crate::CancelledToolRoundModelCallTurn) -> Self {
        Self::cancelled_child(TerminalChildTurn::from_cancelled_tool_round(value))
    }

    fn failed_child(terminal: TerminalChildTurn) -> Self {
        Self {
            kind: DelegationOutcomeKind::ChildFailed,
            content: None,
            reason: terminal.reason,
            provenance: DelegationOutcomeProvenance::ChildTurn(terminal),
        }
    }

    fn cancelled_child(terminal: TerminalChildTurn) -> Self {
        Self {
            kind: DelegationOutcomeKind::ChildCancelled,
            content: None,
            reason: terminal.reason,
            provenance: DelegationOutcomeProvenance::ChildTurn(terminal),
        }
    }

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
            provenance: DelegationOutcomeProvenance::ChildTurn(terminal),
        })
    }

    /// Checks complete stored outcome facts before restoring their sealed proof.
    pub fn reconstitute(
        kind: DelegationOutcomeKind,
        content: Option<DelegationContent>,
        reason: DelegationOutcomeReason,
        provenance: DelegationProvenanceReconstitutionInput,
    ) -> Option<Self> {
        match provenance {
            DelegationProvenanceReconstitutionInput::ChildTurn { session, turn } => {
                let (terminal_kind, result_digest) = match (kind, reason, content.as_ref()) {
                    (
                        DelegationOutcomeKind::ResultReturned,
                        DelegationOutcomeReason::ChildCompleted,
                        Some(content),
                    ) => (
                        TerminalChildTurnKind::Returned,
                        Some(delegation_content_digest(content)),
                    ),
                    (
                        DelegationOutcomeKind::ChildFailed,
                        DelegationOutcomeReason::ChildExecutionFailed
                        | DelegationOutcomeReason::ChildResultUnavailable,
                        None,
                    ) => (TerminalChildTurnKind::Failed, None),
                    (
                        DelegationOutcomeKind::ChildCancelled,
                        DelegationOutcomeReason::ChildCancelled,
                        None,
                    ) => (TerminalChildTurnKind::Cancelled, None),
                    _ => return None,
                };
                Self::from_terminal_child(
                    TerminalChildTurn {
                        session,
                        turn,
                        kind: terminal_kind,
                        reason,
                        result_digest,
                    },
                    content,
                )
            }
            DelegationProvenanceReconstitutionInput::ParentTurnCommand {
                session,
                turn,
                command,
            } => Self::reconstitute_parent_outcome(
                kind,
                content,
                reason,
                session,
                ParentTerminationCommandSource::Turn { turn },
                command,
            ),
            DelegationProvenanceReconstitutionInput::ParentGoalCommand {
                session,
                generation,
                command,
            } => Self::reconstitute_parent_outcome(
                kind,
                content,
                reason,
                session,
                ParentTerminationCommandSource::Goal { generation },
                command,
            ),
            DelegationProvenanceReconstitutionInput::ParentLifecycleCommand {
                session,
                command,
            } => Self::reconstitute_parent_outcome(
                kind,
                content,
                reason,
                session,
                ParentTerminationCommandSource::Lifecycle,
                command,
            ),
        }
    }

    fn reconstitute_parent_outcome(
        kind: DelegationOutcomeKind,
        content: Option<DelegationContent>,
        reason: DelegationOutcomeReason,
        parent: SessionId,
        source: ParentTerminationCommandSource,
        command: DurableCommandId,
    ) -> Option<Self> {
        if content.is_some() {
            return None;
        }
        let termination_kind = match reason {
            DelegationOutcomeReason::ParentStopped {
                scope: DescendantTerminationScope::ParentAndDescendants,
            } => ParentTerminationKind::Stopped,
            DelegationOutcomeReason::ParentCancelled {
                scope: DescendantTerminationScope::ParentAndDescendants,
            } => ParentTerminationKind::Cancelled,
            DelegationOutcomeReason::ChildCompleted
            | DelegationOutcomeReason::ChildExecutionFailed
            | DelegationOutcomeReason::ChildResultUnavailable
            | DelegationOutcomeReason::ChildCancelled
            | DelegationOutcomeReason::ParentStopped {
                scope: DescendantTerminationScope::ParentAlone,
            }
            | DelegationOutcomeReason::ParentCancelled {
                scope: DescendantTerminationScope::ParentAlone,
            } => return None,
        };
        let authority = ParentTerminationAuthority {
            parent,
            source,
            command,
            kind: termination_kind,
            scope: DescendantTerminationScope::ParentAndDescendants,
        };
        match kind {
            DelegationOutcomeKind::AlreadyTerminal => Self::from_parent_already_terminal(authority),
            DelegationOutcomeKind::ContinueRunning => {
                Self::from_parent_policy(authority, BoundChildAction::KeepRunning)
            }
            DelegationOutcomeKind::ChildStopped => {
                Self::from_parent_policy(authority, BoundChildAction::Stop)
            }
            DelegationOutcomeKind::ChildCancelled => {
                Self::from_parent_policy(authority, BoundChildAction::Cancel)
            }
            DelegationOutcomeKind::ResultReturned | DelegationOutcomeKind::ChildFailed => None,
        }
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
                    provenance: DelegationOutcomeProvenance::ParentCommand(authority),
                })
            }
        }
    }

    /// Records that an evaluated descendant edge was already terminal.
    ///
    /// The relationship aggregate calls this only after resolving the
    /// relationship's unique immutable child result. That result remains the
    /// authority for the prior terminal state; this disposition records the
    /// exact parent command that evaluated the edge without fabricating a
    /// second child result.
    #[allow(dead_code, reason = "consumed by the stacked delegation aggregate")]
    pub(crate) const fn from_parent_already_terminal(
        authority: ParentTerminationAuthority,
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
                Some(Self {
                    kind: DelegationOutcomeKind::AlreadyTerminal,
                    content: None,
                    reason,
                    provenance: DelegationOutcomeProvenance::ParentCommand(authority),
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
        match self.provenance {
            DelegationOutcomeProvenance::ChildTurn(terminal) => {
                DelegationProvenance::from_terminal_child(terminal)
            }
            DelegationOutcomeProvenance::ParentCommand(authority) => {
                DelegationProvenance::from_parent_termination(authority)
            }
        }
    }

    pub const fn reconstitution_provenance(&self) -> DelegationProvenanceReconstitutionInput {
        match self.provenance {
            DelegationOutcomeProvenance::ChildTurn(terminal) => {
                DelegationProvenanceReconstitutionInput::ChildTurn {
                    session: terminal.session(),
                    turn: terminal.turn(),
                }
            }
            DelegationOutcomeProvenance::ParentCommand(authority) => match authority.source() {
                ParentTerminationCommandSource::Turn { turn } => {
                    DelegationProvenanceReconstitutionInput::ParentTurnCommand {
                        session: authority.parent(),
                        turn,
                        command: authority.command(),
                    }
                }
                ParentTerminationCommandSource::Goal { generation } => {
                    DelegationProvenanceReconstitutionInput::ParentGoalCommand {
                        session: authority.parent(),
                        generation,
                        command: authority.command(),
                    }
                }
                ParentTerminationCommandSource::Lifecycle => {
                    DelegationProvenanceReconstitutionInput::ParentLifecycleCommand {
                        session: authority.parent(),
                        command: authority.command(),
                    }
                }
            },
        }
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
    pub(crate) const fn from_checked_parts(
        awaiting_request: ToolRequestId,
        spawning_request: ToolRequestId,
        child: SessionId,
    ) -> Self {
        Self {
            awaiting_request,
            spawning_request,
            child,
        }
    }

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
    /// Reconstitutes one stored wait from the checked relationship and exact
    /// canonical await request. No live dispatch authority is minted.
    pub fn reconstitute(
        relation: &SessionDelegation,
        awaiting_request: &DelegationAwaitRequest,
    ) -> Option<Self> {
        Self::reconstitute_stored(
            awaiting_request,
            relation.spawning_request,
            relation.parent,
            relation.child,
            awaiting_request.mode(),
        )
    }

    /// Reconstitutes one stored wait from its immutable relationship endpoints
    /// and mode without loading relationship event history.
    pub fn reconstitute_stored(
        awaiting_request: &DelegationAwaitRequest,
        spawning_request: ToolRequestId,
        parent: SessionId,
        child: SessionId,
        mode: DelegationWaitMode,
    ) -> Option<Self> {
        (awaiting_request.request().session() == parent
            && parent != child
            && awaiting_request.request().id() != spawning_request
            && awaiting_request.child() == child
            && awaiting_request.mode() == mode)
            .then_some(Self {
                awaiting_request: awaiting_request.request().id(),
                spawning_request,
                parent,
                child,
                mode,
            })
    }

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

/// One positive contiguous event position per delegation relation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DelegationEventOrdinal(NonZeroU64);

impl DelegationEventOrdinal {
    pub const fn new(value: NonZeroU64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }

    const fn first() -> Self {
        Self(NonZeroU64::MIN)
    }

    fn successor(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

/// One immutable event in a relation's ordered history.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum DelegationEvent {
    Spawned {
        ordinal: DelegationEventOrdinal,
        provenance: DelegationProvenance,
    },
    MessageDelivered {
        ordinal: DelegationEventOrdinal,
        message: DelegationMessage,
    },
    OutcomeRecorded {
        ordinal: DelegationEventOrdinal,
        outcome: DelegationOutcome,
    },
}

impl DelegationEvent {
    pub const fn ordinal(&self) -> DelegationEventOrdinal {
        match self {
            Self::Spawned { ordinal, .. }
            | Self::MessageDelivered { ordinal, .. }
            | Self::OutcomeRecorded { ordinal, .. } => *ordinal,
        }
    }

    pub const fn message(&self) -> Option<&DelegationMessage> {
        match self {
            Self::MessageDelivered { message, .. } => Some(message),
            Self::Spawned { .. } | Self::OutcomeRecorded { .. } => None,
        }
    }

    pub const fn outcome(&self) -> Option<&DelegationOutcome> {
        match self {
            Self::OutcomeRecorded { outcome, .. } => Some(outcome),
            Self::Spawned { .. } | Self::MessageDelivered { .. } => None,
        }
    }
}

/// Scheduling-visible relation lifecycle.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DelegationLifecycle {
    Active,
    Terminal,
}

/// Complete canonical facts required to reconstitute one stored relation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionDelegationReconstitutionInput {
    spawning_request: DelegatedSpawnRequest,
    child: SessionId,
    child_turn: TurnId,
    events: Vec<DelegationEvent>,
}

impl SessionDelegationReconstitutionInput {
    pub fn new(
        spawning_request: DelegatedSpawnRequest,
        child: SessionId,
        child_turn: TurnId,
        events: Vec<DelegationEvent>,
    ) -> Self {
        Self {
            spawning_request,
            child,
            child_turn,
            events,
        }
    }

    pub const fn spawning_request(&self) -> &DelegatedSpawnRequest {
        &self.spawning_request
    }

    pub const fn child(&self) -> SessionId {
        self.child
    }

    pub const fn child_turn(&self) -> TurnId {
        self.child_turn
    }

    pub fn events(&self) -> &[DelegationEvent] {
        &self.events
    }

    /// Validates the complete ordered history without authorizing a new
    /// spawn, message, outcome, or wait effect.
    pub fn reconstitute(self) -> Result<SessionDelegation, SessionDelegationReconstitutionError> {
        SessionDelegation::reconstitute(self)
    }
}

/// Why stored relationship facts could not form one canonical aggregate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionDelegationReconstitutionFailure {
    SameSession,
    MissingSpawnEvent,
    NoncontiguousEventOrdinal,
    InvalidSpawnEvent,
    InvalidMessageProvenance,
    DuplicateMessageIdentity,
    DuplicateMessageRequest,
    DuplicateOutcomeAuthority,
    OutcomeReasonMismatch,
    EventAfterTerminal,
}

/// Rejected reconstitution retaining every canonical input fact unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionDelegationReconstitutionError {
    input: Box<SessionDelegationReconstitutionInput>,
    failure: SessionDelegationReconstitutionFailure,
}

impl SessionDelegationReconstitutionError {
    pub const fn input(&self) -> &SessionDelegationReconstitutionInput {
        &self.input
    }

    pub const fn failure(&self) -> SessionDelegationReconstitutionFailure {
        self.failure
    }

    pub fn into_parts(
        self,
    ) -> (
        SessionDelegationReconstitutionInput,
        SessionDelegationReconstitutionFailure,
    ) {
        (*self.input, self.failure)
    }
}

impl std::fmt::Display for SessionDelegationReconstitutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "delegation {} reconstitution failed: {:?}",
            self.input.spawning_request.request().id().as_uuid(),
            self.failure
        )
    }
}

impl std::error::Error for SessionDelegationReconstitutionError {}

/// One exact parent/child relationship keyed by its spawning request.
///
/// Construction is intentionally sealed inside this module until the
/// persistence stack can admit a spawn from the complete, locked parent
/// relationship inventory. Callers cannot substitute a count or partial slice
/// for that admission proof.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionDelegation {
    spawning_request: ToolRequestId,
    parent: SessionId,
    child: SessionId,
    child_turn: TurnId,
    task: DelegationContent,
    policy: ChildRelationshipPolicy,
    lifecycle: DelegationLifecycle,
    events: Vec<DelegationEvent>,
}

impl SessionDelegation {
    fn reconstitute(
        input: SessionDelegationReconstitutionInput,
    ) -> Result<Self, SessionDelegationReconstitutionError> {
        let lifecycle = match validate_reconstituted_history(&input) {
            Ok(lifecycle) => lifecycle,
            Err(failure) => return Err(reconstitution_error(input, failure)),
        };
        let SessionDelegationReconstitutionInput {
            spawning_request,
            child,
            child_turn,
            events,
        } = input;
        let DelegatedSpawnRequest {
            request,
            task,
            policy,
        } = spawning_request;
        Ok(Self {
            spawning_request: request.id(),
            parent: request.session(),
            child,
            child_turn,
            task,
            policy,
            lifecycle,
            events,
        })
    }

    #[cfg(test)]
    fn spawn_fixture(
        spawning_request: DelegatedSpawnRequest,
        child: SessionId,
        child_turn: TurnId,
    ) -> Result<Self, DelegationTransitionError> {
        let parent = spawning_request.request().session();
        if parent == child {
            let request_id = spawning_request.request().id();
            return Err(DelegationTransitionError {
                spawning_request: request_id,
                failure: DelegationTransitionFailure::SameSession,
                rejected: Some(Box::new(RejectedDelegationTransition::Spawn {
                    request: spawning_request,
                    child,
                    child_turn,
                })),
            });
        }
        let provenance = DelegationProvenance::from_spawn(&spawning_request);
        Ok(Self {
            spawning_request: spawning_request.request().id(),
            parent,
            child,
            child_turn,
            task: spawning_request.task().clone(),
            policy: spawning_request.policy(),
            lifecycle: DelegationLifecycle::Active,
            events: vec![DelegationEvent::Spawned {
                ordinal: DelegationEventOrdinal::first(),
                provenance,
            }],
        })
    }

    pub const fn spawning_request(&self) -> ToolRequestId {
        self.spawning_request
    }

    pub const fn parent(&self) -> SessionId {
        self.parent
    }

    pub const fn child(&self) -> SessionId {
        self.child
    }

    pub const fn child_turn(&self) -> TurnId {
        self.child_turn
    }

    pub const fn task(&self) -> &DelegationContent {
        &self.task
    }

    pub const fn policy(&self) -> ChildRelationshipPolicy {
        self.policy
    }

    pub const fn lifecycle(&self) -> DelegationLifecycle {
        self.lifecycle
    }

    pub fn events(&self) -> &[DelegationEvent] {
        &self.events
    }

    /// Returns the child session's immutable delegated/no-ancestry provenance.
    pub const fn child_creation_provenance(&self) -> SessionCreationProvenance {
        SessionCreationProvenance::delegated(self.spawning_request)
    }

    pub fn register_wait(
        &self,
        awaiting_request: &DelegationAwaitRequest,
        dispatch: &crate::ToolDispatchAuthority,
    ) -> Result<DelegationWait, DelegationTransitionError> {
        if !dispatch_matches(awaiting_request.request(), dispatch)
            || awaiting_request.request().session() != self.parent
            || awaiting_request.request().id() == self.spawning_request
            || awaiting_request.child() != self.child
        {
            return Err(self.fail(DelegationTransitionFailure::InvalidProvenance));
        }
        Ok(DelegationWait {
            awaiting_request: awaiting_request.request().id(),
            spawning_request: self.spawning_request,
            parent: self.parent,
            child: self.child,
            mode: awaiting_request.mode(),
        })
    }

    pub fn deliver_message(
        mut self,
        sending_request: DelegationMessageRequest,
        id: DelegationMessageId,
        dispatch: &crate::ToolDispatchAuthority,
    ) -> Result<(Self, DelegationEvent), DelegationTransitionError> {
        if !dispatch_matches(sending_request.request(), dispatch) {
            return Err(Self::reject_message(
                self,
                sending_request,
                id,
                DelegationTransitionFailure::InvalidProvenance,
            ));
        }
        let source = sending_request.request().session();
        let direction = if source == self.parent && sending_request.peer() == self.child {
            DelegationMessageDirection::ParentToChild
        } else if source == self.child && sending_request.peer() == self.parent {
            DelegationMessageDirection::ChildToParent
        } else {
            return Err(Self::reject_message(
                self,
                sending_request,
                id,
                DelegationTransitionFailure::InvalidProvenance,
            ));
        };
        if sending_request.request().id() == self.spawning_request {
            return Err(Self::reject_message(
                self,
                sending_request,
                id,
                DelegationTransitionFailure::InvalidProvenance,
            ));
        }
        let provenance = DelegationProvenance::from_message(&sending_request);
        if let Some(existing) = self
            .events
            .iter()
            .find(|event| {
                event.message().is_some_and(|message| {
                    message
                        .provenance()
                        .tool_request()
                        .is_some_and(|(_, _, request)| request == sending_request.request().id())
                })
            })
            .cloned()
        {
            if existing.message().is_some_and(|message| {
                message.direction() == direction
                    && message.content() == sending_request.content()
                    && message.provenance() == provenance
            }) {
                return Ok((self, existing));
            }
            return Err(Self::reject_message(
                self,
                sending_request,
                id,
                DelegationTransitionFailure::ConflictingMessageReplay,
            ));
        }
        if self
            .events
            .iter()
            .any(|event| event.message().is_some_and(|message| message.id() == id))
        {
            return Err(Self::reject_message(
                self,
                sending_request,
                id,
                DelegationTransitionFailure::DuplicateMessageIdentity,
            ));
        }
        let ordinal = match self.next_ordinal() {
            Ok(ordinal)
                if self.lifecycle == DelegationLifecycle::Terminal || ordinal.get() < u64::MAX =>
            {
                ordinal
            }
            Ok(_) => {
                return Err(Self::reject_message(
                    self,
                    sending_request,
                    id,
                    DelegationTransitionFailure::EventOrdinalExhausted,
                ));
            }
            Err(failure) => return Err(Self::reject_message(self, sending_request, id, failure)),
        };
        let event = DelegationEvent::MessageDelivered {
            ordinal,
            message: DelegationMessage {
                id,
                direction,
                peer: sending_request.peer(),
                content: sending_request.content().clone(),
                provenance,
            },
        };
        self.events.push(event.clone());
        Ok((self, event))
    }

    pub fn record_outcome(
        mut self,
        outcome: DelegationOutcome,
    ) -> Result<Self, DelegationTransitionError> {
        let provenance = outcome.provenance();
        if let Some(authority) = provenance.parent_command()
            && let Some(existing) = self
                .events
                .iter()
                .filter_map(DelegationEvent::outcome)
                .find(|existing| {
                    existing
                        .provenance()
                        .parent_command()
                        .is_some_and(|existing| existing.command() == authority.command())
                })
        {
            if existing == &outcome {
                return Ok(self);
            }
            return Err(Self::reject_outcome(
                self,
                outcome,
                DelegationTransitionFailure::DuplicateOutcomeAuthority,
            ));
        }
        if let Some(existing) = self
            .events
            .iter()
            .filter_map(DelegationEvent::outcome)
            .find(|existing| existing.provenance() == provenance)
        {
            if existing == &outcome {
                return Ok(self);
            }
            return Err(Self::reject_outcome(
                self,
                outcome,
                DelegationTransitionFailure::DuplicateOutcomeAuthority,
            ));
        }
        let records_terminal_evaluation = outcome.kind() == DelegationOutcomeKind::AlreadyTerminal;
        if self.lifecycle != DelegationLifecycle::Active
            && (!records_terminal_evaluation || !has_child_terminal_outcome(&self))
        {
            return Err(Self::reject_outcome(
                self,
                outcome,
                DelegationTransitionFailure::AlreadyTerminal,
            ));
        }
        if self.lifecycle == DelegationLifecycle::Active && records_terminal_evaluation {
            return Err(Self::reject_outcome(
                self,
                outcome,
                DelegationTransitionFailure::OutcomeReasonMismatch,
            ));
        }
        if !outcome_matches_relation(DelegationRelationIdentity::from_relation(&self), &outcome) {
            return Err(Self::reject_outcome(
                self,
                outcome,
                DelegationTransitionFailure::OutcomeReasonMismatch,
            ));
        }
        let remains_active = match outcome.kind() {
            DelegationOutcomeKind::ContinueRunning => true,
            DelegationOutcomeKind::ResultReturned
            | DelegationOutcomeKind::ChildFailed
            | DelegationOutcomeKind::ChildStopped
            | DelegationOutcomeKind::ChildCancelled
            | DelegationOutcomeKind::AlreadyTerminal => false,
        };
        let ordinal = match self.next_ordinal() {
            Ok(ordinal) => ordinal,
            Err(failure) => return Err(Self::reject_outcome(self, outcome, failure)),
        };
        self.events
            .push(DelegationEvent::OutcomeRecorded { ordinal, outcome });
        if !remains_active {
            self.lifecycle = DelegationLifecycle::Terminal;
        }
        Ok(self)
    }

    /// Applies one descendant-scoped parent termination using the immutable
    /// relationship policy and current edge lifecycle.
    pub fn record_parent_termination(
        self,
        authority: ParentTerminationAuthority,
    ) -> Result<Self, DelegationTransitionError> {
        if authority.parent() != self.parent {
            return Err(Self::reject_parent_termination(
                self,
                authority,
                DelegationTransitionFailure::InvalidProvenance,
            ));
        }
        if authority.scope() == DescendantTerminationScope::ParentAlone {
            return Err(Self::reject_parent_termination(
                self,
                authority,
                DelegationTransitionFailure::DescendantsNotSelected,
            ));
        }
        if self
            .events
            .iter()
            .filter_map(DelegationEvent::outcome)
            .any(|outcome| outcome.provenance().parent_command() == Some(authority))
        {
            return Ok(self);
        }
        let outcome = if self.lifecycle == DelegationLifecycle::Terminal {
            DelegationOutcome::from_parent_already_terminal(authority)
        } else {
            let action = match self.policy {
                ChildRelationshipPolicy::Background => BoundChildAction::KeepRunning,
                ChildRelationshipPolicy::Bound {
                    on_parent_stopped,
                    on_parent_cancelled,
                } => match authority.kind() {
                    ParentTerminationKind::Stopped => on_parent_stopped,
                    ParentTerminationKind::Cancelled => on_parent_cancelled,
                },
            };
            DelegationOutcome::from_parent_policy(authority, action)
        };
        let Some(outcome) = outcome else {
            return Err(Self::reject_parent_termination(
                self,
                authority,
                DelegationTransitionFailure::DescendantsNotSelected,
            ));
        };
        self.record_outcome(outcome)
    }

    fn next_ordinal(&self) -> Result<DelegationEventOrdinal, DelegationTransitionFailure> {
        let Some(last_event) = self.events.last() else {
            return Err(DelegationTransitionFailure::MissingSpawnEvent);
        };
        last_event
            .ordinal()
            .successor()
            .ok_or(DelegationTransitionFailure::EventOrdinalExhausted)
    }

    fn fail(&self, failure: DelegationTransitionFailure) -> DelegationTransitionError {
        DelegationTransitionError {
            spawning_request: self.spawning_request,
            failure,
            rejected: None,
        }
    }

    fn reject_message(
        relation: Self,
        request: DelegationMessageRequest,
        id: DelegationMessageId,
        failure: DelegationTransitionFailure,
    ) -> DelegationTransitionError {
        DelegationTransitionError {
            spawning_request: relation.spawning_request,
            failure,
            rejected: Some(Box::new(RejectedDelegationTransition::DeliverMessage {
                relation,
                request,
                id,
            })),
        }
    }

    fn reject_outcome(
        relation: Self,
        outcome: DelegationOutcome,
        failure: DelegationTransitionFailure,
    ) -> DelegationTransitionError {
        DelegationTransitionError {
            spawning_request: relation.spawning_request,
            failure,
            rejected: Some(Box::new(RejectedDelegationTransition::RecordOutcome {
                relation,
                outcome,
            })),
        }
    }

    fn reject_parent_termination(
        relation: Self,
        authority: ParentTerminationAuthority,
        failure: DelegationTransitionFailure,
    ) -> DelegationTransitionError {
        DelegationTransitionError {
            spawning_request: relation.spawning_request,
            failure,
            rejected: Some(Box::new(
                RejectedDelegationTransition::RecordParentTermination {
                    relation,
                    authority,
                },
            )),
        }
    }
}

fn reconstitution_error(
    input: SessionDelegationReconstitutionInput,
    failure: SessionDelegationReconstitutionFailure,
) -> SessionDelegationReconstitutionError {
    SessionDelegationReconstitutionError {
        input: Box::new(input),
        failure,
    }
}

#[derive(Clone, Copy)]
struct DelegationRelationIdentity {
    spawning_request: ToolRequestId,
    parent: SessionId,
    child: SessionId,
    child_turn: TurnId,
    policy: ChildRelationshipPolicy,
}

impl DelegationRelationIdentity {
    const fn from_relation(relation: &SessionDelegation) -> Self {
        Self {
            spawning_request: relation.spawning_request,
            parent: relation.parent,
            child: relation.child,
            child_turn: relation.child_turn,
            policy: relation.policy,
        }
    }
}

fn validate_reconstituted_history(
    input: &SessionDelegationReconstitutionInput,
) -> Result<DelegationLifecycle, SessionDelegationReconstitutionFailure> {
    let identity = DelegationRelationIdentity {
        spawning_request: input.spawning_request.request().id(),
        parent: input.spawning_request.request().session(),
        child: input.child,
        child_turn: input.child_turn,
        policy: input.spawning_request.policy(),
    };
    if identity.parent == identity.child {
        return Err(SessionDelegationReconstitutionFailure::SameSession);
    }
    let Some(first) = input.events.first() else {
        return Err(SessionDelegationReconstitutionFailure::MissingSpawnEvent);
    };
    if first
        != &(DelegationEvent::Spawned {
            ordinal: DelegationEventOrdinal::first(),
            provenance: DelegationProvenance::from_spawn(&input.spawning_request),
        })
    {
        return Err(SessionDelegationReconstitutionFailure::InvalidSpawnEvent);
    }

    let mut lifecycle = DelegationLifecycle::Active;
    let mut seen_message_ids = HashSet::new();
    let mut seen_message_requests = HashSet::new();
    let mut seen_parent_commands = HashSet::new();
    let mut seen_outcome_provenance = HashSet::new();
    let mut seen_child_terminal = false;
    for (index, event) in input.events.iter().enumerate() {
        let expected = u64::try_from(index)
            .ok()
            .and_then(|index| index.checked_add(1));
        if expected != Some(event.ordinal().get()) {
            return Err(SessionDelegationReconstitutionFailure::NoncontiguousEventOrdinal);
        }
        match event {
            DelegationEvent::Spawned { .. } if index == 0 => {}
            DelegationEvent::Spawned { .. } => {
                return Err(SessionDelegationReconstitutionFailure::InvalidSpawnEvent);
            }
            DelegationEvent::MessageDelivered { message, .. } => {
                validate_reconstituted_message(
                    identity,
                    message,
                    &mut seen_message_ids,
                    &mut seen_message_requests,
                )?;
            }
            DelegationEvent::OutcomeRecorded { outcome, .. } => {
                lifecycle = validate_reconstituted_outcome(
                    identity,
                    lifecycle,
                    outcome,
                    &mut seen_parent_commands,
                    &mut seen_outcome_provenance,
                    &mut seen_child_terminal,
                )?;
            }
        }
    }
    Ok(lifecycle)
}

fn validate_reconstituted_message(
    relation: DelegationRelationIdentity,
    message: &DelegationMessage,
    seen_ids: &mut HashSet<DelegationMessageId>,
    seen_requests: &mut HashSet<ToolRequestId>,
) -> Result<(), SessionDelegationReconstitutionFailure> {
    let (source, request, purpose) = match message.provenance.kind {
        DelegationProvenanceKind::ToolRequest {
            source_session,
            request,
            purpose,
            ..
        } => (source_session, request, purpose),
        DelegationProvenanceKind::ChildTurn { .. }
        | DelegationProvenanceKind::ParentCommand { .. } => {
            return Err(SessionDelegationReconstitutionFailure::InvalidMessageProvenance);
        }
    };
    let endpoints_match = match message.direction {
        DelegationMessageDirection::ParentToChild => {
            source == relation.parent && message.peer == relation.child
        }
        DelegationMessageDirection::ChildToParent => {
            source == relation.child && message.peer == relation.parent
        }
    };
    if purpose != DelegationToolRequestPurpose::SendMessage
        || request == relation.spawning_request
        || !endpoints_match
    {
        return Err(SessionDelegationReconstitutionFailure::InvalidMessageProvenance);
    }
    if !seen_ids.insert(message.id()) {
        return Err(SessionDelegationReconstitutionFailure::DuplicateMessageIdentity);
    }
    if !seen_requests.insert(request) {
        return Err(SessionDelegationReconstitutionFailure::DuplicateMessageRequest);
    }
    Ok(())
}

fn validate_reconstituted_outcome(
    relation: DelegationRelationIdentity,
    lifecycle: DelegationLifecycle,
    outcome: &DelegationOutcome,
    seen_parent_commands: &mut HashSet<DurableCommandId>,
    seen_provenance: &mut HashSet<DelegationProvenance>,
    seen_child_terminal: &mut bool,
) -> Result<DelegationLifecycle, SessionDelegationReconstitutionFailure> {
    if let Some(authority) = outcome.provenance().parent_command()
        && !seen_parent_commands.insert(authority.command())
    {
        return Err(SessionDelegationReconstitutionFailure::DuplicateOutcomeAuthority);
    }
    if !seen_provenance.insert(outcome.provenance()) {
        return Err(SessionDelegationReconstitutionFailure::DuplicateOutcomeAuthority);
    }
    let records_terminal_evaluation = outcome.kind() == DelegationOutcomeKind::AlreadyTerminal;
    if lifecycle == DelegationLifecycle::Terminal
        && (!records_terminal_evaluation || !*seen_child_terminal)
    {
        return Err(SessionDelegationReconstitutionFailure::EventAfterTerminal);
    }
    if lifecycle == DelegationLifecycle::Active && records_terminal_evaluation {
        return Err(SessionDelegationReconstitutionFailure::OutcomeReasonMismatch);
    }
    if !outcome_matches_relation(relation, outcome) {
        return Err(SessionDelegationReconstitutionFailure::OutcomeReasonMismatch);
    }
    if matches!(
        outcome.kind(),
        DelegationOutcomeKind::ResultReturned
            | DelegationOutcomeKind::ChildFailed
            | DelegationOutcomeKind::ChildStopped
            | DelegationOutcomeKind::ChildCancelled
    ) {
        *seen_child_terminal = true;
    }
    match outcome.kind() {
        DelegationOutcomeKind::ContinueRunning => Ok(DelegationLifecycle::Active),
        DelegationOutcomeKind::ResultReturned
        | DelegationOutcomeKind::ChildFailed
        | DelegationOutcomeKind::ChildStopped
        | DelegationOutcomeKind::ChildCancelled
        | DelegationOutcomeKind::AlreadyTerminal => Ok(DelegationLifecycle::Terminal),
    }
}

fn dispatch_matches(request: &ToolRequest, dispatch: &crate::ToolDispatchAuthority) -> bool {
    dispatch.request() == request
}

fn has_child_terminal_outcome(relation: &SessionDelegation) -> bool {
    relation
        .events
        .iter()
        .filter_map(DelegationEvent::outcome)
        .any(|outcome| {
            matches!(
                outcome.kind(),
                DelegationOutcomeKind::ResultReturned
                    | DelegationOutcomeKind::ChildFailed
                    | DelegationOutcomeKind::ChildStopped
                    | DelegationOutcomeKind::ChildCancelled
            )
        })
}

fn outcome_matches_relation(
    relation: DelegationRelationIdentity,
    outcome: &DelegationOutcome,
) -> bool {
    let reason = outcome.reason();
    let child_matches = || {
        outcome
            .provenance()
            .child_turn()
            .is_some_and(|(child, turn)| child == relation.child && turn == relation.child_turn)
    };
    match outcome.kind() {
        DelegationOutcomeKind::ResultReturned => {
            reason == DelegationOutcomeReason::ChildCompleted && child_matches()
        }
        DelegationOutcomeKind::ChildFailed => {
            matches!(
                reason,
                DelegationOutcomeReason::ChildExecutionFailed
                    | DelegationOutcomeReason::ChildResultUnavailable
            ) && child_matches()
        }
        DelegationOutcomeKind::ChildStopped => {
            parent_outcome_matches(relation, outcome, reason, BoundChildAction::Stop)
        }
        DelegationOutcomeKind::ChildCancelled => {
            if reason == DelegationOutcomeReason::ChildCancelled {
                child_matches()
            } else {
                parent_outcome_matches(relation, outcome, reason, BoundChildAction::Cancel)
            }
        }
        DelegationOutcomeKind::ContinueRunning => {
            parent_outcome_matches(relation, outcome, reason, BoundChildAction::KeepRunning)
        }
        DelegationOutcomeKind::AlreadyTerminal => {
            parent_evaluation_matches(relation, outcome, reason)
        }
    }
}

fn parent_outcome_matches(
    relation: DelegationRelationIdentity,
    outcome: &DelegationOutcome,
    reason: DelegationOutcomeReason,
    expected_action: BoundChildAction,
) -> bool {
    parent_evaluation_matches(relation, outcome, reason)
        && descendant_action(relation.policy, reason) == Some(expected_action)
}

fn parent_evaluation_matches(
    relation: DelegationRelationIdentity,
    outcome: &DelegationOutcome,
    reason: DelegationOutcomeReason,
) -> bool {
    match (outcome.provenance().kind, reason) {
        (
            DelegationProvenanceKind::ParentCommand { authority },
            DelegationOutcomeReason::ParentStopped {
                scope: DescendantTerminationScope::ParentAndDescendants,
            },
        ) => {
            authority.parent() == relation.parent
                && authority.kind() == ParentTerminationKind::Stopped
                && authority.scope() == DescendantTerminationScope::ParentAndDescendants
        }
        (
            DelegationProvenanceKind::ParentCommand { authority },
            DelegationOutcomeReason::ParentCancelled {
                scope: DescendantTerminationScope::ParentAndDescendants,
            },
        ) => {
            authority.parent() == relation.parent
                && authority.kind() == ParentTerminationKind::Cancelled
                && authority.scope() == DescendantTerminationScope::ParentAndDescendants
        }
        _ => false,
    }
}

fn descendant_action(
    policy: ChildRelationshipPolicy,
    reason: DelegationOutcomeReason,
) -> Option<BoundChildAction> {
    match (policy, reason) {
        (
            ChildRelationshipPolicy::Background,
            DelegationOutcomeReason::ParentStopped {
                scope: DescendantTerminationScope::ParentAndDescendants,
            }
            | DelegationOutcomeReason::ParentCancelled {
                scope: DescendantTerminationScope::ParentAndDescendants,
            },
        ) => Some(BoundChildAction::KeepRunning),
        (
            ChildRelationshipPolicy::Bound {
                on_parent_stopped, ..
            },
            DelegationOutcomeReason::ParentStopped {
                scope: DescendantTerminationScope::ParentAndDescendants,
            },
        ) => Some(on_parent_stopped),
        (
            ChildRelationshipPolicy::Bound {
                on_parent_cancelled,
                ..
            },
            DelegationOutcomeReason::ParentCancelled {
                scope: DescendantTerminationScope::ParentAndDescendants,
            },
        ) => Some(on_parent_cancelled),
        (
            ChildRelationshipPolicy::Background | ChildRelationshipPolicy::Bound { .. },
            DelegationOutcomeReason::ChildCompleted
            | DelegationOutcomeReason::ChildExecutionFailed
            | DelegationOutcomeReason::ChildResultUnavailable
            | DelegationOutcomeReason::ChildCancelled
            | DelegationOutcomeReason::ParentStopped {
                scope: DescendantTerminationScope::ParentAlone,
            }
            | DelegationOutcomeReason::ParentCancelled {
                scope: DescendantTerminationScope::ParentAlone,
            },
        ) => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DelegationTransitionFailure {
    SameSession,
    AlreadyTerminal,
    MissingSpawnEvent,
    InvalidProvenance,
    DescendantsNotSelected,
    DuplicateMessageIdentity,
    ConflictingMessageReplay,
    DuplicateOutcomeAuthority,
    OutcomeReasonMismatch,
    EventOrdinalExhausted,
}

/// Unchanged aggregate and exact attempted input from a rejected consuming transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RejectedDelegationTransition {
    Spawn {
        request: DelegatedSpawnRequest,
        child: SessionId,
        child_turn: TurnId,
    },
    DeliverMessage {
        relation: SessionDelegation,
        request: DelegationMessageRequest,
        id: DelegationMessageId,
    },
    RecordOutcome {
        relation: SessionDelegation,
        outcome: DelegationOutcome,
    },
    RecordParentTermination {
        relation: SessionDelegation,
        authority: ParentTerminationAuthority,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegationTransitionError {
    spawning_request: ToolRequestId,
    failure: DelegationTransitionFailure,
    rejected: Option<Box<RejectedDelegationTransition>>,
}

impl DelegationTransitionError {
    pub const fn failure(&self) -> DelegationTransitionFailure {
        self.failure
    }

    pub const fn spawning_request(&self) -> ToolRequestId {
        self.spawning_request
    }

    pub fn into_rejected(self) -> Option<RejectedDelegationTransition> {
        self.rejected.map(|rejected| *rejected)
    }
}

impl std::fmt::Display for DelegationTransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "delegation {} transition failed: {:?}",
            self.spawning_request.as_uuid(),
            self.failure
        )
    }
}

impl std::error::Error for DelegationTransitionError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        InitialToolApproval, NormalizedToolArguments, ToolCallProposal, ToolName,
        ToolRequestOrdinal,
        model_execution::{cancelled_turn_fixture, completed_turn_fixture, failed_turn_fixture},
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
    fn provenance_projection_exposes_each_closed_authority_variant() {
        let spawn = DelegatedSpawnRequest::parse(
            named_request(
                1,
                SPAWN_SESSION_TOOL_NAME,
                serde_json::json!({
                    "relationship": { "kind": "background" },
                    "task": TEST_TASK,
                }),
            ),
            TEST_TASK.into(),
            ChildRelationshipPolicy::Background,
        )
        .expect("canonical spawn request");
        let terminal = TerminalChildTurn::from_failed(&failed_turn_fixture());
        let authority =
            parent_termination_authority(DescendantTerminationScope::ParentAndDescendants);

        assert_eq!(
            DelegationProvenance::from_spawn(&spawn).projection(),
            DelegationProvenanceProjection::ToolRequest {
                source_session: spawn.request().session(),
                source_turn: spawn.request().turn(),
                request: spawn.request().id(),
            }
        );
        assert_eq!(
            DelegationProvenance::from_terminal_child(terminal).projection(),
            DelegationProvenanceProjection::ChildTurn { terminal }
        );
        assert_eq!(
            DelegationProvenance::from_parent_termination(authority).projection(),
            DelegationProvenanceProjection::ParentCommand { authority }
        );
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

    /// S18: a live completed call seals its own nonempty result.
    #[test]
    fn s18_live_completed_call_proves_returned_content() {
        let expected = content("completed child result");
        let completed = completed_turn_fixture(&[expected.as_str()]);
        let terminal = TerminalChildTurn::from_completed(&completed)
            .expect("live completed call seals terminal child evidence");
        let outcome = DelegationOutcome::from_terminal_child(terminal, Some(expected.clone()))
            .expect("sealed live result authenticates its exact content");
        let directly_derived = DelegationOutcome::from_completed_child(&completed);

        assert_eq!(outcome.kind(), DelegationOutcomeKind::ResultReturned);
        assert_eq!(outcome.content(), Some(&expected));
        assert_eq!(directly_derived, outcome);
    }

    /// S18: empty live completion is a typed unavailable result.
    #[test]
    fn s18_empty_live_completion_produces_unavailable_outcome() {
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

    /// S18: failure evidence can name a delegated-task-origin turn.
    #[test]
    fn s18_failed_turn_proves_its_exact_origin_agnostic_identity() {
        let failed = failed_turn_fixture();
        let expected_identity = (failed.session(), failed.turn());
        let terminal = TerminalChildTurn::from_failed(&failed);
        let outcome = DelegationOutcome::from_terminal_child(terminal, None)
            .expect("sealed failed turn derives a child failure");
        let directly_derived = DelegationOutcome::from_failed_child(&failed);

        assert_eq!((terminal.session(), terminal.turn()), expected_identity);
        assert_eq!(outcome.kind(), DelegationOutcomeKind::ChildFailed);
        assert_eq!(directly_derived, outcome);
    }

    /// S18: cancellation evidence can name a delegated-task-origin turn.
    #[test]
    fn s18_cancelled_turn_proves_its_exact_origin_agnostic_identity() {
        let cancelled = cancelled_turn_fixture();
        let expected_identity = (cancelled.session(), cancelled.turn());
        let terminal = TerminalChildTurn::from_cancelled(&cancelled);
        let outcome = DelegationOutcome::from_terminal_child(terminal, None)
            .expect("sealed cancelled turn derives child cancellation");
        let directly_derived = DelegationOutcome::from_cancelled_child(&cancelled);

        assert_eq!((terminal.session(), terminal.turn()), expected_identity);
        assert_eq!(outcome.kind(), DelegationOutcomeKind::ChildCancelled);
        assert_eq!(directly_derived, outcome);
    }

    /// S18: oversized aggregate live completion is typed unavailable.
    #[test]
    fn s18_oversized_live_completion_produces_unavailable_outcome() {
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

    /// S18: terminal proof authenticates returned content.
    #[test]
    fn s18_terminal_proof_rejects_fabricated_content() {
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

    /// S18: child-origin terminal proof cannot select stopped.
    #[test]
    fn s18_terminal_proof_cannot_construct_stopped_outcome() {
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

    /// S18: parent-alone authority cannot disposition descendants.
    #[test]
    fn s18_parent_alone_cannot_construct_descendant_outcome() {
        let authority = parent_termination_authority(DescendantTerminationScope::ParentAlone);

        assert_eq!(
            DelegationOutcome::from_parent_policy(authority, BoundChildAction::Stop),
            None
        );
    }

    /// S18: parent-and-descendants authority admits bound policy.
    #[test]
    fn s18_parent_and_descendants_constructs_policy_outcome() {
        let authority =
            parent_termination_authority(DescendantTerminationScope::ParentAndDescendants);
        let outcome = DelegationOutcome::from_parent_policy(authority, BoundChildAction::Stop)
            .expect("descendant-scoped authority may apply bound policy");

        assert_eq!(outcome.kind(), DelegationOutcomeKind::ChildStopped);
        assert_eq!(
            outcome.reason(),
            DelegationOutcomeReason::ParentStopped {
                scope: authority.scope(),
            }
        );
    }

    /// S18: an already-terminal edge records its evaluating command.
    #[test]
    fn s18_already_terminal_edge_has_typed_command_disposition() {
        let authority =
            parent_termination_authority(DescendantTerminationScope::ParentAndDescendants);
        let outcome = DelegationOutcome::from_parent_already_terminal(authority)
            .expect("descendant-scoped authority records terminal-edge evaluation");

        assert_eq!(outcome.kind(), DelegationOutcomeKind::AlreadyTerminal);
        assert_eq!(
            outcome.reason(),
            DelegationOutcomeReason::ParentStopped {
                scope: authority.scope(),
            }
        );
        assert_eq!(outcome.provenance().parent_command(), Some(authority));
    }

    /// S18: descendant-scoped cancellation selects its exact reason.
    #[test]
    fn s18_parent_and_descendants_cancel_constructs_policy_outcome() {
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

    /// S18: goal-stop authority names a generation without a turn.
    #[test]
    fn s18_goal_termination_authority_never_fabricates_a_turn() {
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

#[cfg(test)]
mod aggregate_tests {
    use super::*;
    use crate::{
        ApprovedToolRequest, InitialToolApproval, ModelCallId, NormalizedToolArguments,
        ToolApprovalResolutionReconstitutionInput, ToolCallProposal, ToolEffectClass, ToolName,
        ToolRequestOrdinal,
        model_execution::completed_turn_fixture,
        test_support::{
            command_id, delegation_message_id, model_call_id, session_id, tool_attempt_id,
            tool_request_id, turn_attempt_id, turn_id,
        },
    };

    const TEST_TASK: &str = "inspect aggregate work";

    #[derive(Clone, Copy)]
    enum RequestFixture {
        Spawn,
        Await,
        ParentMessage,
        ChildMessage,
        AnotherParentMessage,
    }

    impl RequestFixture {
        /// Canonical request identities are deliberately decorrelated across
        /// each named fixture. Reusing one fixture is the explicit way a test
        /// requests the same tool-request authority.
        fn identities(self) -> (ToolRequestId, TurnId, ModelCallId) {
            match self {
                Self::Spawn => (tool_request_id(101), turn_id(43), model_call_id(89)),
                Self::Await => (tool_request_id(103), turn_id(41), model_call_id(83)),
                Self::ParentMessage => (tool_request_id(107), turn_id(37), model_call_id(79)),
                Self::ChildMessage => (tool_request_id(109), turn_id(31), model_call_id(73)),
                Self::AnotherParentMessage => {
                    (tool_request_id(113), turn_id(29), model_call_id(71))
                }
            }
        }

        fn source(self) -> SessionId {
            match self {
                Self::ChildMessage => session_id(3),
                Self::Spawn | Self::Await | Self::ParentMessage | Self::AnotherParentMessage => {
                    session_id(2)
                }
            }
        }
    }

    /// Canonical aggregate request fixture. Each named logical purpose owns its
    /// source and fixed, decorrelated identity.
    fn named_request(
        fixture: RequestFixture,
        name: &str,
        arguments: serde_json::Value,
    ) -> ToolRequest {
        let (request, turn, call) = fixture.identities();
        ToolRequest::from_model_proposal(
            request,
            fixture.source(),
            turn,
            call,
            ToolRequestOrdinal::from_u32(0),
            ToolCallProposal::new(
                ToolName::try_new(name.into()).expect("valid fixture name"),
                NormalizedToolArguments::try_from_provider_text(arguments.to_string())
                    .expect("valid fixture arguments"),
            ),
            InitialToolApproval::Confirm,
        )
    }

    fn dispatch_for(request: &ToolRequest) -> crate::ToolDispatchAuthority {
        let approval = ToolApprovalResolutionReconstitutionInput::policy_auto(request.id())
            .reconstitute()
            .expect("fixture policy approves its exact request");
        let approved = ApprovedToolRequest::try_from_resolution(request.clone(), approval)
            .expect("fixture approval binds its exact request");
        let authorized = approved
            .prepare_attempt(
                tool_attempt_id(127),
                turn_attempt_id(131),
                ToolEffectClass::EffectFree,
            )
            .authorize()
            .expect("prepared fixture dispatch authorizes");
        crate::ToolDispatchAuthority::try_new(request.clone(), &authorized)
            .expect("fixture dispatch binds its exact request")
    }

    fn spawn_request(policy: ChildRelationshipPolicy) -> DelegatedSpawnRequest {
        let relationship = relationship_argument(policy);
        DelegatedSpawnRequest::parse(
            named_request(
                RequestFixture::Spawn,
                SPAWN_SESSION_TOOL_NAME,
                serde_json::json!({
                    "relationship": relationship,
                    "task": TEST_TASK,
                }),
            ),
            TEST_TASK.into(),
            policy,
        )
        .expect("canonical aggregate spawn request")
    }

    fn await_request(
        fixture: RequestFixture,
        child: SessionId,
        mode: DelegationWaitMode,
    ) -> DelegationAwaitRequest {
        DelegationAwaitRequest::parse(
            named_request(
                fixture,
                AWAIT_SESSION_TOOL_NAME,
                serde_json::json!({
                    "child_session_id": child.as_uuid().to_string(),
                    "mode": wait_mode_argument(mode),
                }),
            ),
            child,
            mode,
        )
        .expect("canonical aggregate await request")
    }

    fn message_request(
        fixture: RequestFixture,
        peer: SessionId,
        value: &str,
    ) -> DelegationMessageRequest {
        DelegationMessageRequest::parse(
            named_request(
                fixture,
                SEND_SESSION_MESSAGE_TOOL_NAME,
                serde_json::json!({
                    "content": value,
                    "peer_session_id": peer.as_uuid().to_string(),
                }),
            ),
            peer,
            value.into(),
        )
        .expect("canonical aggregate message request")
    }

    fn content(value: &str) -> DelegationContent {
        DelegationContent::try_new(value.into()).expect("bounded fixture content")
    }

    fn relation(policy: ChildRelationshipPolicy) -> SessionDelegation {
        SessionDelegation::spawn_fixture(spawn_request(policy), session_id(3), turn_id(7))
            .expect("fixture parent and child are distinct")
    }

    fn completed_child_relation(policy: ChildRelationshipPolicy) -> SessionDelegation {
        SessionDelegation::spawn_fixture(spawn_request(policy), session_id(1), turn_id(3))
            .expect("completed-turn fixture child is distinct from parent")
    }

    /// S18: restart restores one complete active history.
    #[test]
    fn relation_reconstitution_round_trips_complete_active_history() {
        let policy = ChildRelationshipPolicy::Background;
        let spawning = spawn_request(policy);
        let relation =
            SessionDelegation::spawn_fixture(spawning.clone(), session_id(3), turn_id(7))
                .expect("fixture parent and child are distinct");
        let sending = message_request(RequestFixture::ParentMessage, relation.child(), "progress");
        let dispatch = dispatch_for(sending.request());
        let (relation, _) = relation
            .deliver_message(sending, delegation_message_id(5), &dispatch)
            .expect("canonical message extends the relation");
        let input = SessionDelegationReconstitutionInput::new(
            spawning,
            relation.child(),
            relation.child_turn(),
            relation.events().to_vec(),
        );

        let reconstituted = input
            .reconstitute()
            .expect("complete stored history reconstitutes");

        assert_eq!(reconstituted, relation);
    }

    /// S18: restart derives lifecycle from terminal history.
    #[test]
    fn relation_reconstitution_derives_terminal_lifecycle_from_history() {
        let policy = ChildRelationshipPolicy::Background;
        let spawning = spawn_request(policy);
        let relation = completed_child_relation(policy)
            .record_outcome(returned_outcome("done"))
            .expect("the exact child result terminalizes the relation");
        let input = SessionDelegationReconstitutionInput::new(
            spawning,
            relation.child(),
            relation.child_turn(),
            relation.events().to_vec(),
        );

        let reconstituted = input
            .reconstitute()
            .expect("complete terminal history reconstitutes");

        assert_eq!(reconstituted, relation);
        assert_eq!(reconstituted.lifecycle(), relation.lifecycle());
    }

    /// S18: restart rejects a gap in relationship history.
    #[test]
    fn relation_reconstitution_rejects_noncontiguous_history() {
        let policy = ChildRelationshipPolicy::Background;
        let spawning = spawn_request(policy);
        let relation =
            SessionDelegation::spawn_fixture(spawning.clone(), session_id(3), turn_id(7))
                .expect("fixture parent and child are distinct");
        let sending = message_request(RequestFixture::ParentMessage, relation.child(), "progress");
        let message = DelegationMessage::reconstitute(
            &sending,
            delegation_message_id(5),
            DelegationMessageDirection::ParentToChild,
            DelegationMessageEndpoints {
                parent: relation.parent(),
                child: relation.child(),
            },
        )
        .expect("fixture endpoints match the stored direction");
        let events = vec![
            relation.events()[0].clone(),
            DelegationEvent::MessageDelivered {
                ordinal: DelegationEventOrdinal::new(
                    NonZeroU64::new(3).expect("fixture ordinal is positive"),
                ),
                message,
            },
        ];
        let input = SessionDelegationReconstitutionInput::new(
            spawning,
            relation.child(),
            relation.child_turn(),
            events,
        );

        let error = input
            .reconstitute()
            .expect_err("a skipped event ordinal is corrupt");

        assert_eq!(
            error.failure(),
            SessionDelegationReconstitutionFailure::NoncontiguousEventOrdinal
        );
    }

    /// S18: restart rejects cross-wired message provenance.
    #[test]
    fn relation_reconstitution_rejects_cross_wired_message_direction() {
        let policy = ChildRelationshipPolicy::Background;
        let spawning = spawn_request(policy);
        let relation =
            SessionDelegation::spawn_fixture(spawning.clone(), session_id(3), turn_id(7))
                .expect("fixture parent and child are distinct");
        let sending = message_request(RequestFixture::ChildMessage, relation.parent(), "progress");
        let message = DelegationMessage::reconstitute(
            &sending,
            delegation_message_id(5),
            DelegationMessageDirection::ParentToChild,
            DelegationMessageEndpoints {
                parent: relation.child(),
                child: relation.parent(),
            },
        )
        .expect("the swapped endpoint fixture admits its own direction");
        let input = SessionDelegationReconstitutionInput::new(
            spawning,
            relation.child(),
            relation.child_turn(),
            vec![
                relation.events()[0].clone(),
                DelegationEvent::MessageDelivered {
                    ordinal: DelegationEventOrdinal::new(
                        NonZeroU64::new(2).expect("fixture ordinal is positive"),
                    ),
                    message,
                },
            ],
        );

        let error = input
            .reconstitute()
            .expect_err("the stored direction must match both endpoints");

        assert_eq!(
            error.failure(),
            SessionDelegationReconstitutionFailure::InvalidMessageProvenance
        );
    }

    /// S18: restart rejects two parent authorities that
    /// reuse one durable command identity.
    #[test]
    fn s18_reconstitution_rejects_reused_parent_command_identity() {
        let policy = ChildRelationshipPolicy::Background;
        let spawning = spawn_request(policy);
        let relation =
            SessionDelegation::spawn_fixture(spawning.clone(), session_id(3), turn_id(7))
                .expect("fixture parent and child are distinct");
        let first_authority = parent_authority(
            TerminationAuthoritySource::Parent,
            ParentTerminationKind::Stopped,
            DescendantTerminationScope::ParentAndDescendants,
        );
        let conflicting_authority = ParentTerminationAuthority {
            parent: first_authority.parent(),
            source: ParentTerminationCommandSource::Turn { turn: turn_id(11) },
            command: first_authority.command(),
            kind: ParentTerminationKind::Cancelled,
            scope: first_authority.scope(),
        };
        let first =
            DelegationOutcome::from_parent_policy(first_authority, BoundChildAction::KeepRunning)
                .expect("background policy records typed survival");
        let conflicting = DelegationOutcome::from_parent_policy(
            conflicting_authority,
            BoundChildAction::KeepRunning,
        )
        .expect("background policy records typed survival");
        let input = SessionDelegationReconstitutionInput::new(
            spawning,
            relation.child(),
            relation.child_turn(),
            vec![
                relation.events()[0].clone(),
                DelegationEvent::OutcomeRecorded {
                    ordinal: DelegationEventOrdinal::new(
                        NonZeroU64::new(2).expect("fixture ordinal is positive"),
                    ),
                    outcome: first,
                },
                DelegationEvent::OutcomeRecorded {
                    ordinal: DelegationEventOrdinal::new(
                        NonZeroU64::new(3).expect("fixture ordinal is positive"),
                    ),
                    outcome: conflicting,
                },
            ],
        );

        let error = input
            .reconstitute()
            .expect_err("one command identity cannot denote two authorities");

        assert_eq!(
            error.failure(),
            SessionDelegationReconstitutionFailure::DuplicateOutcomeAuthority
        );
    }

    /// S18: restart binds waits to one request and relation.
    #[test]
    fn wait_reconstitution_uses_exact_relation_and_request() {
        let relation = relation(ChildRelationshipPolicy::Background);
        let awaiting = await_request(
            RequestFixture::Await,
            relation.child(),
            DelegationWaitMode::Foreground,
        );
        let dispatch = dispatch_for(awaiting.request());
        let recorded = relation
            .register_wait(&awaiting, &dispatch)
            .expect("live registration validates");

        let reconstituted = DelegationWait::reconstitute(&relation, &awaiting)
            .expect("stored exact request validates without dispatch authority");

        assert_eq!(reconstituted, recorded);
    }

    /// S18: stored waits cannot reconstitute a self relationship.
    #[test]
    fn s18_wait_reconstitution_rejects_same_session_endpoints() {
        let relation = relation(ChildRelationshipPolicy::Background);
        let awaiting = await_request(
            RequestFixture::Await,
            relation.parent(),
            DelegationWaitMode::Foreground,
        );

        assert_eq!(
            DelegationWait::reconstitute_stored(
                &awaiting,
                relation.spawning_request(),
                relation.parent(),
                relation.parent(),
                DelegationWaitMode::Foreground,
            ),
            None
        );
    }

    /// S18: immutable endpoint facts reconstitute an exact
    /// wait without loading the relationship event stream.
    #[test]
    fn wait_reconstitution_uses_stored_endpoints_and_mode() {
        let relation = relation(ChildRelationshipPolicy::Background);
        let awaiting = await_request(
            RequestFixture::Await,
            relation.child(),
            DelegationWaitMode::Foreground,
        );
        let expected = DelegationWait::reconstitute(&relation, &awaiting)
            .expect("aggregate-backed reconstitution validates");

        let reconstituted = DelegationWait::reconstitute_stored(
            &awaiting,
            relation.spawning_request(),
            relation.parent(),
            relation.child(),
            DelegationWaitMode::Foreground,
        )
        .expect("stored endpoint facts validate");

        assert_eq!(reconstituted, expected);
    }

    #[derive(Clone, Copy)]
    enum TerminationAuthoritySource {
        Parent,
        ForeignParent,
    }

    impl TerminationAuthoritySource {
        fn session(self) -> SessionId {
            match self {
                Self::Parent => session_id(2),
                Self::ForeignParent => session_id(9),
            }
        }
    }

    fn parent_authority(
        authority_source: TerminationAuthoritySource,
        kind: ParentTerminationKind,
        scope: DescendantTerminationScope,
    ) -> ParentTerminationAuthority {
        let command_source = match kind {
            ParentTerminationKind::Stopped => ParentTerminationCommandSource::Goal {
                generation: GoalGeneration::new(NonZeroU64::MIN),
            },
            ParentTerminationKind::Cancelled => {
                ParentTerminationCommandSource::Turn { turn: turn_id(5) }
            }
        };
        ParentTerminationAuthority {
            parent: authority_source.session(),
            source: command_source,
            command: command_id(6),
            kind,
            scope,
        }
    }

    fn later_parent_authority(
        kind: ParentTerminationKind,
        scope: DescendantTerminationScope,
    ) -> ParentTerminationAuthority {
        let command_source = match kind {
            ParentTerminationKind::Stopped => ParentTerminationCommandSource::Goal {
                generation: GoalGeneration::new(NonZeroU64::new(2).expect("two is positive")),
            },
            ParentTerminationKind::Cancelled => {
                ParentTerminationCommandSource::Turn { turn: turn_id(11) }
            }
        };
        ParentTerminationAuthority {
            parent: session_id(2),
            source: command_source,
            command: command_id(13),
            kind,
            scope,
        }
    }

    fn returned_outcome(value: &str) -> DelegationOutcome {
        let content = content(value);
        let completed = completed_turn_fixture(&[value]);
        let terminal = TerminalChildTurn::from_completed(&completed)
            .expect("live completion seals terminal evidence");
        DelegationOutcome::from_terminal_child(terminal, Some(content))
            .expect("exact live content authenticates its outcome")
    }

    fn cancelled_outcome(child: SessionId) -> DelegationOutcome {
        let terminal = TerminalChildTurn {
            session: child,
            turn: turn_id(7),
            kind: TerminalChildTurnKind::Cancelled,
            reason: DelegationOutcomeReason::ChildCancelled,
            result_digest: None,
        };
        DelegationOutcome::from_terminal_child(terminal, None)
            .expect("cancelled terminal evidence seals its outcome")
    }

    #[test]
    fn delegation_outcome_reconstitution_round_trips_child_evidence() {
        let returned = returned_outcome("checked result");
        let reconstituted_returned = DelegationOutcome::reconstitute(
            returned.kind(),
            returned.content().cloned(),
            returned.reason(),
            returned.reconstitution_provenance(),
        );
        let cancelled = cancelled_outcome(session_id(3));
        let reconstituted_cancelled = DelegationOutcome::reconstitute(
            cancelled.kind(),
            cancelled.content().cloned(),
            cancelled.reason(),
            cancelled.reconstitution_provenance(),
        );
        assert_eq!(reconstituted_returned, Some(returned));
        assert_eq!(reconstituted_cancelled, Some(cancelled));
    }

    #[test]
    fn delegation_outcome_reconstitution_round_trips_parent_command_evidence() {
        let stopped_authority = parent_authority(
            TerminationAuthoritySource::Parent,
            ParentTerminationKind::Stopped,
            DescendantTerminationScope::ParentAndDescendants,
        );
        let stopped =
            DelegationOutcome::from_parent_policy(stopped_authority, BoundChildAction::KeepRunning)
                .expect("descendant-scoped stopped authority evaluates policy");
        let cancelled_authority = parent_authority(
            TerminationAuthoritySource::Parent,
            ParentTerminationKind::Cancelled,
            DescendantTerminationScope::ParentAndDescendants,
        );
        let cancelled =
            DelegationOutcome::from_parent_policy(cancelled_authority, BoundChildAction::Cancel)
                .expect("descendant-scoped cancelled authority evaluates policy");
        assert_eq!(
            DelegationOutcome::reconstitute(
                stopped.kind(),
                stopped.content().cloned(),
                stopped.reason(),
                stopped.reconstitution_provenance(),
            ),
            Some(stopped)
        );
        assert_eq!(
            DelegationOutcome::reconstitute(
                cancelled.kind(),
                cancelled.content().cloned(),
                cancelled.reason(),
                cancelled.reconstitution_provenance(),
            ),
            Some(cancelled)
        );
    }

    #[test]
    fn delegation_outcome_reconstitution_rejects_cross_wired_shapes() {
        let child = DelegationProvenanceReconstitutionInput::ChildTurn {
            session: session_id(3),
            turn: turn_id(7),
        };
        let parent = DelegationProvenanceReconstitutionInput::ParentTurnCommand {
            session: session_id(2),
            turn: turn_id(5),
            command: command_id(6),
        };
        assert_eq!(
            DelegationOutcome::reconstitute(
                DelegationOutcomeKind::ResultReturned,
                Some(content("result")),
                DelegationOutcomeReason::ChildCompleted,
                parent,
            ),
            None
        );
        assert_eq!(
            DelegationOutcome::reconstitute(
                DelegationOutcomeKind::ChildStopped,
                None,
                DelegationOutcomeReason::ParentStopped {
                    scope: DescendantTerminationScope::ParentAlone,
                },
                parent,
            ),
            None
        );
        assert_eq!(
            DelegationOutcome::reconstitute(
                DelegationOutcomeKind::ChildFailed,
                None,
                DelegationOutcomeReason::ChildCancelled,
                child,
            ),
            None
        );
    }

    #[track_caller]
    fn rejected_spawn(
        error: DelegationTransitionError,
    ) -> (DelegatedSpawnRequest, SessionId, TurnId) {
        let Some(RejectedDelegationTransition::Spawn {
            request,
            child,
            child_turn,
        }) = error.into_rejected()
        else {
            panic!("spawn rejection retains typed request, child, and delegated-task turn");
        };
        (request, child, child_turn)
    }

    #[track_caller]
    fn rejected_message(
        error: DelegationTransitionError,
    ) -> (
        SessionDelegation,
        DelegationMessageRequest,
        DelegationMessageId,
    ) {
        let Some(RejectedDelegationTransition::DeliverMessage {
            relation,
            request,
            id,
        }) = error.into_rejected()
        else {
            panic!("message rejection retains aggregate and typed request");
        };
        (relation, request, id)
    }

    #[track_caller]
    fn rejected_outcome(
        error: DelegationTransitionError,
    ) -> (SessionDelegation, DelegationOutcome) {
        let Some(RejectedDelegationTransition::RecordOutcome { relation, outcome }) =
            error.into_rejected()
        else {
            panic!("outcome rejection retains aggregate and opaque outcome");
        };
        (relation, outcome)
    }

    #[track_caller]
    fn rejected_parent_termination(
        error: DelegationTransitionError,
    ) -> (SessionDelegation, ParentTerminationAuthority) {
        let Some(RejectedDelegationTransition::RecordParentTermination {
            relation,
            authority,
        }) = error.into_rejected()
        else {
            panic!("parent-termination rejection retains aggregate and authority");
        };
        (relation, authority)
    }

    fn assert_standard_error<T: std::error::Error>() {}

    #[test]
    fn delegation_public_errors_implement_standard_error_contract() {
        assert_standard_error::<DelegationContentError>();
        assert_standard_error::<DelegationRequestError>();
        assert_standard_error::<DelegationTransitionError>();
    }

    /// S18: spawn retains the exact sealed request facts
    /// and derives delegated creation without ancestry.
    #[test]
    fn s18_aggregate_spawn_retains_policy_task_and_provenance() {
        let policy = ChildRelationshipPolicy::Bound {
            on_parent_stopped: BoundChildAction::Stop,
            on_parent_cancelled: BoundChildAction::Cancel,
        };
        let request = spawn_request(policy);
        let spawning_request = request.request().id();
        let task = request.task().clone();
        let child_turn = turn_id(7);
        let relation = SessionDelegation::spawn_fixture(request, session_id(3), child_turn)
            .expect("distinct child");

        assert_eq!(relation.spawning_request(), spawning_request);
        assert_eq!(relation.child_turn(), child_turn);
        assert_eq!(relation.task(), &task);
        assert_eq!(relation.policy(), policy);
        assert_eq!(relation.events()[0].ordinal().get(), 1);
        assert_eq!(
            relation.child_creation_provenance().cause(),
            crate::SessionCreationCause::Delegated { spawning_request }
        );
        assert_eq!(
            relation.child_creation_provenance().ancestry(),
            crate::TranscriptAncestry::None
        );
    }

    /// S18: a session cannot delegate to itself, and rejection is lossless.
    #[test]
    fn s18_same_session_spawn_rejection_returns_exact_inputs() {
        let request = spawn_request(ChildRelationshipPolicy::Background);
        let child = session_id(2);
        let child_turn = turn_id(7);
        let error = SessionDelegation::spawn_fixture(request.clone(), child, child_turn)
            .expect_err("a child must be distinct from its parent");

        assert_eq!(error.failure(), DelegationTransitionFailure::SameSession);
        let (returned_request, returned_child, returned_child_turn) = rejected_spawn(error);
        assert_eq!(returned_request, request);
        assert_eq!(returned_child, child);
        assert_eq!(returned_child_turn, child_turn);
    }

    /// S18: foreground wait retains the exact child subject.
    #[test]
    fn s18_foreground_registration_yields_exact_child_wait() {
        let relation = relation(ChildRelationshipPolicy::Background);
        let awaiting = await_request(
            RequestFixture::Await,
            relation.child(),
            DelegationWaitMode::Foreground,
        );
        let expected_request = awaiting.request().id();
        let dispatch = dispatch_for(awaiting.request());
        let wait = relation
            .register_wait(&awaiting, &dispatch)
            .expect("parent may await its exact child");
        let subject = wait
            .foreground_subject()
            .expect("foreground wait retains turn subject");

        assert_eq!(subject.awaiting_request(), expected_request);
        assert_eq!(subject.spawning_request(), relation.spawning_request());
        assert_eq!(subject.child(), relation.child());
    }

    /// S18: background wait releases the parent turn subject.
    #[test]
    fn s18_background_registration_has_no_child_wait() {
        let relation = relation(ChildRelationshipPolicy::Background);
        let awaiting = await_request(
            RequestFixture::Await,
            relation.child(),
            DelegationWaitMode::Background,
        );
        let dispatch = dispatch_for(awaiting.request());
        let wait = relation
            .register_wait(&awaiting, &dispatch)
            .expect("parent may await its exact child");

        assert_eq!(wait.mode(), DelegationWaitMode::Background);
        assert_eq!(wait.foreground_subject(), None);
    }

    /// S18: a typed await for another child cannot cross relations.
    #[test]
    fn s18_wait_registration_rejects_relation_child_cross_wiring() {
        let relation = relation(ChildRelationshipPolicy::Background);
        let awaiting = await_request(
            RequestFixture::Await,
            session_id(9),
            DelegationWaitMode::Background,
        );
        let dispatch = dispatch_for(awaiting.request());
        let error = relation
            .register_wait(&awaiting, &dispatch)
            .expect_err("another child cannot reuse this relation");

        assert_eq!(
            error.failure(),
            DelegationTransitionFailure::InvalidProvenance
        );
    }

    /// S18: one request cannot both spawn and await a child.
    #[test]
    fn s18_wait_registration_requires_distinct_parent_work() {
        let relation = relation(ChildRelationshipPolicy::Background);
        let awaiting = await_request(
            RequestFixture::Spawn,
            relation.child(),
            DelegationWaitMode::Background,
        );
        let dispatch = dispatch_for(awaiting.request());
        let error = relation
            .register_wait(&awaiting, &dispatch)
            .expect_err("spawn request identity cannot also register a wait");

        assert_eq!(
            error.failure(),
            DelegationTransitionFailure::InvalidProvenance
        );
    }

    /// S18: wait registration requires its exact in-flight dispatch.
    #[test]
    fn s18_wait_registration_rejects_foreign_dispatch_authority() {
        let relation = relation(ChildRelationshipPolicy::Background);
        let awaiting = await_request(
            RequestFixture::Await,
            relation.child(),
            DelegationWaitMode::Background,
        );
        let other_request = message_request(
            RequestFixture::ParentMessage,
            relation.child(),
            "other dispatched work",
        );
        let foreign_dispatch = dispatch_for(other_request.request());
        let error = relation
            .register_wait(&awaiting, &foreign_dispatch)
            .expect_err("another dispatch cannot authorize this wait");

        assert_eq!(
            error.failure(),
            DelegationTransitionFailure::InvalidProvenance
        );
    }

    /// S18: dispatch authority binds the complete await request.
    #[test]
    fn s18_wait_registration_rejects_same_identity_argument_drift() {
        let relation = relation(ChildRelationshipPolicy::Background);
        let dispatched = await_request(
            RequestFixture::Await,
            relation.child(),
            DelegationWaitMode::Background,
        );
        let drifted = await_request(
            RequestFixture::Await,
            relation.child(),
            DelegationWaitMode::Foreground,
        );
        let dispatch = dispatch_for(dispatched.request());
        let error = relation
            .register_wait(&drifted, &dispatch)
            .expect_err("same identities cannot substitute different await arguments");

        assert_eq!(
            error.failure(),
            DelegationTransitionFailure::InvalidProvenance
        );
    }

    /// S18: each relation peer derives one exact message direction.
    #[test]
    fn s18_messages_are_bidirectional() {
        let relation = relation(ChildRelationshipPolicy::Background);
        let parent_message = message_request(
            RequestFixture::ParentMessage,
            relation.child(),
            "parent update",
        );
        let child_message = message_request(
            RequestFixture::ChildMessage,
            relation.parent(),
            "child update",
        );
        let parent_dispatch = dispatch_for(parent_message.request());
        let child_dispatch = dispatch_for(child_message.request());
        let (relation, first) = relation
            .deliver_message(parent_message, delegation_message_id(5), &parent_dispatch)
            .expect("parent message is related");
        let (_relation, second) = relation
            .deliver_message(child_message, delegation_message_id(6), &child_dispatch)
            .expect("child message is related");

        assert_eq!(
            first.message().expect("message event").direction(),
            DelegationMessageDirection::ParentToChild
        );
        assert_eq!(
            second.message().expect("message event").direction(),
            DelegationMessageDirection::ChildToParent
        );
    }

    /// S18: distinct message deliveries receive contiguous ordinals.
    #[test]
    fn s18_message_delivery_ordinals_are_contiguous() {
        let relation = relation(ChildRelationshipPolicy::Background);
        let parent_message = message_request(
            RequestFixture::ParentMessage,
            relation.child(),
            "parent update",
        );
        let child_message = message_request(
            RequestFixture::ChildMessage,
            relation.parent(),
            "child update",
        );
        let parent_dispatch = dispatch_for(parent_message.request());
        let child_dispatch = dispatch_for(child_message.request());
        let (relation, first) = relation
            .deliver_message(parent_message, delegation_message_id(5), &parent_dispatch)
            .expect("parent message is related");
        let (_relation, second) = relation
            .deliver_message(child_message, delegation_message_id(6), &child_dispatch)
            .expect("child message is related");

        assert_eq!(first.ordinal().get(), 2);
        assert_eq!(second.ordinal().get(), 3);
    }

    /// S18: nonterminal messages preserve the final
    /// relationship ordinal for a typed terminal outcome.
    #[test]
    fn s18_message_reserves_terminal_event_ordinal() {
        let policy = ChildRelationshipPolicy::Background;
        let spawning = spawn_request(policy);
        let mut relation = relation(policy);
        relation.events = vec![DelegationEvent::Spawned {
            ordinal: DelegationEventOrdinal::new(
                NonZeroU64::new(u64::MAX - 1).expect("fixture ordinal is positive"),
            ),
            provenance: DelegationProvenance::from_spawn(&spawning),
        }];
        let request = message_request(
            RequestFixture::ParentMessage,
            relation.child(),
            "reserved terminal position",
        );
        let dispatch = dispatch_for(request.request());
        let error = relation
            .deliver_message(request, delegation_message_id(5), &dispatch)
            .expect_err("the final ordinal remains reserved for terminal evidence");

        assert_eq!(
            error.failure(),
            DelegationTransitionFailure::EventOrdinalExhausted
        );
    }

    /// S18: a typed message for another peer returns exact inputs.
    #[test]
    fn s18_message_rejects_relation_peer_cross_wiring_and_returns_input() {
        let relation = relation(ChildRelationshipPolicy::Background);
        let request = message_request(RequestFixture::ParentMessage, session_id(9), "misdirected");
        let id = delegation_message_id(5);
        let dispatch = dispatch_for(request.request());
        let error = relation
            .clone()
            .deliver_message(request.clone(), id, &dispatch)
            .expect_err("another peer cannot cross this relation");
        let (returned_relation, returned_request, returned_id) = rejected_message(error);

        assert_eq!(returned_relation, relation);
        assert_eq!(returned_request, request);
        assert_eq!(returned_id, id);
    }

    /// S18: message delivery requires its exact in-flight dispatch.
    #[test]
    fn s18_message_rejects_foreign_dispatch_and_returns_input() {
        let relation = relation(ChildRelationshipPolicy::Background);
        let request = message_request(
            RequestFixture::ParentMessage,
            relation.child(),
            "dispatched message",
        );
        let other_request = await_request(
            RequestFixture::Await,
            relation.child(),
            DelegationWaitMode::Background,
        );
        let foreign_dispatch = dispatch_for(other_request.request());
        let id = delegation_message_id(5);
        let error = relation
            .clone()
            .deliver_message(request.clone(), id, &foreign_dispatch)
            .expect_err("another dispatch cannot authorize this message");
        let (returned_relation, returned_request, returned_id) = rejected_message(error);

        assert_eq!(returned_relation, relation);
        assert_eq!(returned_request, request);
        assert_eq!(returned_id, id);
    }

    /// S18: dispatch authority binds the complete message request.
    #[test]
    fn s18_message_rejects_same_identity_content_drift() {
        let relation = relation(ChildRelationshipPolicy::Background);
        let dispatched = message_request(
            RequestFixture::ParentMessage,
            relation.child(),
            "authorized content",
        );
        let drifted = message_request(
            RequestFixture::ParentMessage,
            relation.child(),
            "substituted content",
        );
        let dispatch = dispatch_for(dispatched.request());
        let id = delegation_message_id(5);
        let error = relation
            .clone()
            .deliver_message(drifted.clone(), id, &dispatch)
            .expect_err("same identities cannot substitute different message content");
        let (returned_relation, returned_request, returned_id) = rejected_message(error);

        assert_eq!(returned_relation, relation);
        assert_eq!(returned_request, drifted);
        assert_eq!(returned_id, id);
    }

    /// S18: one logical message request appends at most one event.
    #[test]
    fn s18_message_request_replay_returns_persisted_event() {
        let relation = relation(ChildRelationshipPolicy::Background);
        let request = message_request(RequestFixture::ParentMessage, relation.child(), "once");
        let dispatch = dispatch_for(request.request());
        let (relation, first) = relation
            .deliver_message(request.clone(), delegation_message_id(5), &dispatch)
            .expect("first delivery appends");
        let (relation, replay) = relation
            .deliver_message(request, delegation_message_id(9), &dispatch)
            .expect("equal request replay returns persisted event");

        assert_eq!(replay, first);
        assert_eq!(relation.events().len(), 2);
    }

    /// S18: a message identity cannot name another logical request.
    #[test]
    fn s18_duplicate_message_identity_returns_attempted_request() {
        let relation = relation(ChildRelationshipPolicy::Background);
        let first = message_request(RequestFixture::ParentMessage, relation.child(), "first");
        let second = message_request(
            RequestFixture::AnotherParentMessage,
            relation.child(),
            "second",
        );
        let id = delegation_message_id(5);
        let first_dispatch = dispatch_for(first.request());
        let second_dispatch = dispatch_for(second.request());
        let (relation, _) = relation
            .deliver_message(first, id, &first_dispatch)
            .expect("first identity is unused");
        let error = relation
            .clone()
            .deliver_message(second.clone(), id, &second_dispatch)
            .expect_err("identity reuse is rejected");
        let (returned_relation, returned_request, returned_id) = rejected_message(error);

        assert_eq!(returned_relation, relation);
        assert_eq!(returned_request, second);
        assert_eq!(returned_id, id);
    }

    /// S18: a replay cannot change content under one request authority.
    #[test]
    fn s18_conflicting_message_replay_reports_code_and_returns_inputs() {
        let relation = relation(ChildRelationshipPolicy::Background);
        let first = message_request(RequestFixture::ParentMessage, relation.child(), "first");
        let conflicting =
            message_request(RequestFixture::ParentMessage, relation.child(), "changed");
        let first_dispatch = dispatch_for(first.request());
        let conflicting_dispatch = dispatch_for(conflicting.request());
        let conflicting_id = delegation_message_id(6);
        let (relation, _) = relation
            .deliver_message(first, delegation_message_id(5), &first_dispatch)
            .expect("first request authority appends");
        let error = relation
            .clone()
            .deliver_message(conflicting.clone(), conflicting_id, &conflicting_dispatch)
            .expect_err("one request authority cannot carry changed content");

        assert_eq!(
            error.failure(),
            DelegationTransitionFailure::ConflictingMessageReplay
        );
        let (returned_relation, returned_request, returned_id) = rejected_message(error);
        assert_eq!(returned_relation, relation);
        assert_eq!(returned_request, conflicting);
        assert_eq!(returned_id, conflicting_id);
    }

    /// S18: returned child result terminalizes exactly once.
    #[test]
    fn s18_returned_result_terminalizes_and_replays() {
        let relation = completed_child_relation(ChildRelationshipPolicy::Background);
        let outcome = returned_outcome("child result");
        let relation = relation
            .record_outcome(outcome.clone())
            .expect("exact child result is related");
        let replayed = relation
            .clone()
            .record_outcome(outcome)
            .expect("equal outcome replay is idempotent");

        assert_eq!(relation.lifecycle(), DelegationLifecycle::Terminal);
        assert_eq!(replayed, relation);
        assert_eq!(relation.events().len(), 2);
    }

    /// S18: terminal evidence must name this spawn's delegated-task turn.
    #[test]
    fn s18_result_rejects_a_later_child_turn() {
        let relation = completed_child_relation(ChildRelationshipPolicy::Background);
        let returned = content("later turn result");
        let terminal = TerminalChildTurn {
            session: relation.child(),
            turn: turn_id(7),
            kind: TerminalChildTurnKind::Returned,
            reason: DelegationOutcomeReason::ChildCompleted,
            result_digest: Some(delegation_content_digest(&returned)),
        };
        let outcome = DelegationOutcome::from_terminal_child(terminal, Some(returned))
            .expect("the later turn has independently valid terminal evidence");
        let error = relation
            .clone()
            .record_outcome(outcome.clone())
            .expect_err("another child turn cannot satisfy this spawn");

        assert_eq!(
            error.failure(),
            DelegationTransitionFailure::OutcomeReasonMismatch
        );
        let (returned_relation, returned_outcome) = rejected_outcome(error);

        assert_eq!(returned_relation, relation);
        assert_eq!(returned_outcome, outcome);
    }

    /// S18: later descendant evaluation is explicit on a child-terminal edge.
    #[test]
    fn s18_child_terminal_edge_records_parent_command_disposition() {
        let relation = completed_child_relation(ChildRelationshipPolicy::Background)
            .record_outcome(returned_outcome("terminal result"))
            .expect("returned result terminalizes relation");
        let authority = parent_authority(
            TerminationAuthoritySource::Parent,
            ParentTerminationKind::Stopped,
            DescendantTerminationScope::ParentAndDescendants,
        );
        let relation = relation
            .record_parent_termination(authority)
            .expect("a child result authenticates the terminal edge");
        let recorded = relation.events().last().and_then(DelegationEvent::outcome);

        assert_eq!(relation.lifecycle(), DelegationLifecycle::Terminal);
        assert_eq!(
            recorded.map(DelegationOutcome::kind),
            Some(DelegationOutcomeKind::AlreadyTerminal)
        );
        assert_eq!(
            recorded.map(DelegationOutcome::provenance),
            Some(DelegationProvenance::from_parent_termination(authority))
        );
        assert_eq!(relation.events().len(), 3);
    }

    /// S18: a prior policy terminal result remains explicit on re-evaluation.
    #[test]
    fn s18_policy_terminal_edge_records_later_command_disposition() {
        let policy = ChildRelationshipPolicy::Bound {
            on_parent_stopped: BoundChildAction::Stop,
            on_parent_cancelled: BoundChildAction::Cancel,
        };
        let first_authority = parent_authority(
            TerminationAuthoritySource::Parent,
            ParentTerminationKind::Stopped,
            DescendantTerminationScope::ParentAndDescendants,
        );
        let relation = relation(policy)
            .record_parent_termination(first_authority)
            .expect("the first policy disposition terminalizes the edge");
        let later_authority = later_parent_authority(
            ParentTerminationKind::Stopped,
            DescendantTerminationScope::ParentAndDescendants,
        );
        let relation = relation
            .record_parent_termination(later_authority)
            .expect("a policy terminal result authenticates later evaluation");
        let recorded = relation.events().last().and_then(DelegationEvent::outcome);

        assert_eq!(relation.lifecycle(), DelegationLifecycle::Terminal);
        assert_eq!(
            recorded.map(DelegationOutcome::kind),
            Some(DelegationOutcomeKind::AlreadyTerminal)
        );
        assert_eq!(
            recorded.map(DelegationOutcome::provenance),
            Some(DelegationProvenance::from_parent_termination(
                later_authority
            ))
        );
        assert_eq!(relation.events().len(), 3);
    }

    /// S18: parent-alone scope never evaluates a child edge.
    #[test]
    fn s18_parent_alone_transition_returns_exact_unevaluated_inputs() {
        let relation = relation(ChildRelationshipPolicy::Background);
        let authority = parent_authority(
            TerminationAuthoritySource::Parent,
            ParentTerminationKind::Stopped,
            DescendantTerminationScope::ParentAlone,
        );
        let error = relation
            .clone()
            .record_parent_termination(authority)
            .expect_err("parent-alone scope does not evaluate descendants");

        assert_eq!(
            error.failure(),
            DelegationTransitionFailure::DescendantsNotSelected
        );
        let (returned_relation, returned_authority) = rejected_parent_termination(error);
        assert_eq!(returned_relation, relation);
        assert_eq!(returned_authority, authority);
    }

    /// S18: a different authority cannot append after terminalization.
    #[test]
    fn s18_already_terminal_rejection_reports_code_and_returns_inputs() {
        let relation = completed_child_relation(ChildRelationshipPolicy::Background)
            .record_outcome(returned_outcome("terminal result"))
            .expect("returned result terminalizes relation");
        let outcome = cancelled_outcome(relation.child());
        let error = relation
            .clone()
            .record_outcome(outcome.clone())
            .expect_err("another terminal authority cannot append");

        assert_eq!(
            error.failure(),
            DelegationTransitionFailure::AlreadyTerminal
        );
        let (returned_relation, returned_outcome) = rejected_outcome(error);
        assert_eq!(returned_relation, relation);
        assert_eq!(returned_outcome, outcome);
    }

    /// S18: one authority cannot replay with a different outcome.
    #[test]
    fn s18_duplicate_outcome_authority_reports_code_and_returns_inputs() {
        let policy = ChildRelationshipPolicy::Bound {
            on_parent_stopped: BoundChildAction::Stop,
            on_parent_cancelled: BoundChildAction::Cancel,
        };
        let relation = relation(policy);
        let authority = parent_authority(
            TerminationAuthoritySource::Parent,
            ParentTerminationKind::Stopped,
            DescendantTerminationScope::ParentAndDescendants,
        );
        let first = DelegationOutcome::from_parent_policy(authority, BoundChildAction::Stop)
            .expect("descendant authority admits the chosen stop policy");
        let conflicting =
            DelegationOutcome::from_parent_policy(authority, BoundChildAction::Cancel)
                .expect("the same authority can express the conflicting attempted action");
        let relation = relation
            .record_outcome(first)
            .expect("chosen stop policy terminalizes relation");
        let error = relation
            .clone()
            .record_outcome(conflicting.clone())
            .expect_err("one authority cannot select two outcomes");

        assert_eq!(
            error.failure(),
            DelegationTransitionFailure::DuplicateOutcomeAuthority
        );
        let (returned_relation, returned_outcome) = rejected_outcome(error);
        assert_eq!(returned_relation, relation);
        assert_eq!(returned_outcome, conflicting);
    }

    /// S18: another child's sealed result returns unchanged.
    #[test]
    fn s18_returned_result_rejects_foreign_child_proof() {
        let relation = relation(ChildRelationshipPolicy::Background);
        let outcome = returned_outcome("foreign child result");
        let error = relation
            .clone()
            .record_outcome(outcome.clone())
            .expect_err("terminal proof belongs to fixture child one");
        let (returned_relation, returned_outcome) = rejected_outcome(error);

        assert_eq!(returned_relation, relation);
        assert_eq!(returned_outcome, outcome);
    }

    /// S18: a terminal result still accepts a late wait registration.
    #[test]
    fn s18_terminal_result_accepts_late_wait() {
        let relation = completed_child_relation(ChildRelationshipPolicy::Background)
            .record_outcome(returned_outcome("late result"))
            .expect("child result terminalizes relation");
        let awaiting = await_request(
            RequestFixture::Await,
            relation.child(),
            DelegationWaitMode::Background,
        );
        let dispatch = dispatch_for(awaiting.request());
        let wait = relation
            .register_wait(&awaiting, &dispatch)
            .expect("late wait registration remains valid");

        assert_eq!(wait.mode(), DelegationWaitMode::Background);
        assert_eq!(wait.child(), relation.child());
    }

    /// S18: messages remain available after child terminalization.
    #[test]
    fn s18_message_is_recorded_after_child_terminalizes() {
        let relation = completed_child_relation(ChildRelationshipPolicy::Background)
            .record_outcome(returned_outcome("done"))
            .expect("child result terminalizes relation");
        let request = message_request(RequestFixture::ParentMessage, relation.child(), "afterward");
        let dispatch = dispatch_for(request.request());
        let (relation, event) = relation
            .deliver_message(request, delegation_message_id(5), &dispatch)
            .expect("terminal relation still records messages");

        assert_eq!(relation.lifecycle(), DelegationLifecycle::Terminal);
        assert_eq!(event.ordinal().get(), 3);
    }

    /// S19: child cancellation retains child-turn provenance.
    #[test]
    fn s19_child_cancel_records_child_turn_provenance() {
        let relation = relation(ChildRelationshipPolicy::Background);
        let outcome = cancelled_outcome(relation.child());
        let relation = relation
            .record_outcome(outcome)
            .expect("child cancellation belongs to relation");
        let recorded = relation.events()[1].outcome().expect("outcome event");

        assert_eq!(recorded.kind(), DelegationOutcomeKind::ChildCancelled);
        assert_eq!(
            recorded.provenance().child_turn(),
            Some((relation.child(), relation.child_turn()))
        );
    }

    /// S19: a bound keep-running action remains active and explicit.
    #[test]
    fn s19_bound_keep_running_records_no_change() {
        let policy = ChildRelationshipPolicy::Bound {
            on_parent_stopped: BoundChildAction::KeepRunning,
            on_parent_cancelled: BoundChildAction::Cancel,
        };
        let relation = relation(policy);
        let authority = parent_authority(
            TerminationAuthoritySource::Parent,
            ParentTerminationKind::Stopped,
            DescendantTerminationScope::ParentAndDescendants,
        );
        let relation = relation
            .record_parent_termination(authority)
            .expect("bound keep-running policy matches outcome");

        assert_eq!(relation.lifecycle(), DelegationLifecycle::Active);
        assert_eq!(relation.events().len(), 2);
    }

    /// S19: continue-running replay does not append another event.
    #[test]
    fn s19_continue_running_replay_is_idempotent() {
        let relation = relation(ChildRelationshipPolicy::Background);
        let authority = parent_authority(
            TerminationAuthoritySource::Parent,
            ParentTerminationKind::Stopped,
            DescendantTerminationScope::ParentAndDescendants,
        );
        let relation = relation
            .record_parent_termination(authority)
            .expect("first disposition appends");
        let replayed = relation
            .clone()
            .record_parent_termination(authority)
            .expect("equal disposition replay is idempotent");

        assert_eq!(replayed, relation);
        assert_eq!(relation.events().len(), 2);
    }

    /// S19: background child survives parent stop explicitly.
    #[test]
    fn s19_background_child_survives_parent_stop_with_typed_outcome() {
        let relation = relation(ChildRelationshipPolicy::Background);
        let authority = parent_authority(
            TerminationAuthoritySource::Parent,
            ParentTerminationKind::Stopped,
            DescendantTerminationScope::ParentAndDescendants,
        );
        let relation = relation
            .record_parent_termination(authority)
            .expect("background policy records survival");

        assert_eq!(relation.lifecycle(), DelegationLifecycle::Active);
        assert_eq!(
            relation.events()[1]
                .outcome()
                .expect("outcome event")
                .kind(),
            DelegationOutcomeKind::ContinueRunning
        );
    }

    /// S19: background child survives parent cancellation explicitly.
    #[test]
    fn s19_background_child_survives_parent_cancel_with_typed_outcome() {
        let relation = relation(ChildRelationshipPolicy::Background);
        let authority = parent_authority(
            TerminationAuthoritySource::Parent,
            ParentTerminationKind::Cancelled,
            DescendantTerminationScope::ParentAndDescendants,
        );
        let relation = relation
            .record_parent_termination(authority)
            .expect("background policy records survival");

        assert_eq!(relation.lifecycle(), DelegationLifecycle::Active);
        assert_eq!(
            relation.events()[1]
                .outcome()
                .expect("outcome event")
                .reason(),
            DelegationOutcomeReason::ParentCancelled {
                scope: DescendantTerminationScope::ParentAndDescendants,
            }
        );
    }

    /// S19: bound child follows its chosen parent-stop policy.
    #[test]
    fn s19_bound_child_follows_parent_stop_policy() {
        let policy = ChildRelationshipPolicy::Bound {
            on_parent_stopped: BoundChildAction::Stop,
            on_parent_cancelled: BoundChildAction::Cancel,
        };
        let relation = relation(policy);
        let authority = parent_authority(
            TerminationAuthoritySource::Parent,
            ParentTerminationKind::Stopped,
            DescendantTerminationScope::ParentAndDescendants,
        );
        let relation = relation
            .record_parent_termination(authority)
            .expect("bound stop policy matches outcome");

        assert_eq!(relation.lifecycle(), DelegationLifecycle::Terminal);
        assert_eq!(
            relation.events()[1]
                .outcome()
                .expect("outcome event")
                .kind(),
            DelegationOutcomeKind::ChildStopped
        );
    }

    /// S19: bound child follows its chosen parent-cancel policy.
    #[test]
    fn s19_bound_child_follows_parent_cancel_policy() {
        let policy = ChildRelationshipPolicy::Bound {
            on_parent_stopped: BoundChildAction::Stop,
            on_parent_cancelled: BoundChildAction::Cancel,
        };
        let relation = relation(policy);
        let authority = parent_authority(
            TerminationAuthoritySource::Parent,
            ParentTerminationKind::Cancelled,
            DescendantTerminationScope::ParentAndDescendants,
        );
        let relation = relation
            .record_parent_termination(authority)
            .expect("bound cancel policy matches outcome");

        assert_eq!(relation.lifecycle(), DelegationLifecycle::Terminal);
        assert_eq!(
            relation.events()[1]
                .outcome()
                .expect("outcome event")
                .kind(),
            DelegationOutcomeKind::ChildCancelled
        );
    }

    /// S19: parent authority cannot override the chosen edge action.
    #[test]
    fn s19_parent_outcome_rejects_wrong_policy_action() {
        let policy = ChildRelationshipPolicy::Bound {
            on_parent_stopped: BoundChildAction::Stop,
            on_parent_cancelled: BoundChildAction::Cancel,
        };
        let relation = relation(policy);
        let authority = parent_authority(
            TerminationAuthoritySource::Parent,
            ParentTerminationKind::Stopped,
            DescendantTerminationScope::ParentAndDescendants,
        );
        let outcome = DelegationOutcome::from_parent_policy(authority, BoundChildAction::Cancel)
            .expect("descendant-scoped parent authority");
        let error = relation
            .clone()
            .record_outcome(outcome.clone())
            .expect_err("spawn policy rejects a different action");
        let (returned_relation, returned_outcome) = rejected_outcome(error);

        assert_eq!(returned_relation, relation);
        assert_eq!(returned_outcome, outcome);
    }

    /// S19: foreign parent authority returns aggregate and outcome.
    #[test]
    fn s19_parent_outcome_rejects_foreign_termination_authority() {
        let relation = relation(ChildRelationshipPolicy::Background);
        let authority = parent_authority(
            TerminationAuthoritySource::ForeignParent,
            ParentTerminationKind::Stopped,
            DescendantTerminationScope::ParentAndDescendants,
        );
        let outcome =
            DelegationOutcome::from_parent_policy(authority, BoundChildAction::KeepRunning)
                .expect("descendant-scoped foreign authority");
        let error = relation
            .clone()
            .record_outcome(outcome.clone())
            .expect_err("foreign parent cannot disposition relation");
        let (returned_relation, returned_outcome) = rejected_outcome(error);

        assert_eq!(returned_relation, relation);
        assert_eq!(returned_outcome, outcome);
    }
}
