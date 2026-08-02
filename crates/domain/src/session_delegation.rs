//! Typed delegated-session relation, messages, outcomes, and wait subject.

use std::num::NonZeroU64;

use sha2::{Digest, Sha256};

use crate::{
    DelegationMessageId, DurableCommandId, NonEmptyUnicodeText, SessionCreationProvenance,
    SessionId, ToolRequest, ToolRequestId, TurnId,
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

/// Exact applied parent termination authority.
///
/// Raw identities cannot construct this proof. The scheduling slice supplies
/// it only from the exact applied stop or cancellation command result; this
/// foundation slice keeps that producer sealed.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ParentTerminationAuthority {
    parent: SessionId,
    turn: TurnId,
    command: DurableCommandId,
    kind: ParentTerminationKind,
    scope: DescendantTerminationScope,
}

impl ParentTerminationAuthority {
    pub const fn parent(self) -> SessionId {
        self.parent
    }

    pub const fn turn(self) -> TurnId {
        self.turn
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
        let task = DelegationContent::try_new(task).map_err(|failure| DelegationRequestError {
            request: Box::new(request.clone()),
            failure: DelegationRequestFailure::InvalidContent(failure),
        })?;
        let value = parse_arguments(&request, SPAWN_SESSION_TOOL_NAME)?;
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
        let content =
            DelegationContent::try_new(content).map_err(|failure| DelegationRequestError {
                request: Box::new(request.clone()),
                failure: DelegationRequestFailure::InvalidContent(failure),
            })?;
        let value = parse_arguments(&request, SEND_SESSION_MESSAGE_TOOL_NAME)?;
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
        let (kind, result_digest) = match (value.status(), reason) {
            (
                crate::AcceptedInputTurnSchedulingStatus::TerminalFailed
                | crate::AcceptedInputTurnSchedulingStatus::TerminalRefused,
                DelegationOutcomeReason::ChildExecutionFailed,
            ) => (TerminalChildTurnKind::Failed, None),
            (
                crate::AcceptedInputTurnSchedulingStatus::TerminalCancelled,
                DelegationOutcomeReason::ChildCancelled,
            ) => (TerminalChildTurnKind::Cancelled, None),
            (
                crate::AcceptedInputTurnSchedulingStatus::Queued
                | crate::AcceptedInputTurnSchedulingStatus::Active
                | crate::AcceptedInputTurnSchedulingStatus::TerminalCompleted
                | crate::AcceptedInputTurnSchedulingStatus::TerminalFailed
                | crate::AcceptedInputTurnSchedulingStatus::TerminalRefused
                | crate::AcceptedInputTurnSchedulingStatus::TerminalCancelled
                | crate::AcceptedInputTurnSchedulingStatus::TerminalReconciliationRequired,
                _,
            ) => return None,
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
        let crate::SemanticTranscriptEntryPayload::AssistantText {
            producing_call,
            value: text,
        } = entry.payload()
        else {
            return None;
        };
        if entry.source_session() != value.session() || *producing_call != value.call().id() {
            return None;
        }
        assistant_text.push(text.clone());
    }
    DelegationContent::from_assistant_text(&assistant_text).ok()
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

    pub const fn parent_command(&self) -> Option<(SessionId, TurnId, DurableCommandId)> {
        match self.kind {
            DelegationProvenanceKind::ParentCommand { authority } => {
                Some((authority.parent(), authority.turn(), authority.command()))
            }
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
        let kind = match (terminal.kind, terminal.reason, content.as_ref()) {
            (
                TerminalChildTurnKind::Returned,
                DelegationOutcomeReason::ChildCompleted,
                Some(content),
            ) if terminal.result_digest == Some(delegation_content_digest(content)) => {
                DelegationOutcomeKind::ResultReturned
            }
            (
                TerminalChildTurnKind::Failed,
                DelegationOutcomeReason::ChildExecutionFailed
                | DelegationOutcomeReason::ChildResultUnavailable,
                None,
            ) => DelegationOutcomeKind::ChildFailed,
            (TerminalChildTurnKind::Cancelled, DelegationOutcomeReason::ChildCancelled, None) => {
                DelegationOutcomeKind::ChildCancelled
            }
            _ => return None,
        };
        Some(Self {
            kind,
            content,
            reason: terminal.reason,
            provenance: DelegationProvenance::from_terminal_child(terminal),
        })
    }

    /// Derives a policy disposition from exact applied parent authority.
    pub const fn from_parent_policy(
        authority: ParentTerminationAuthority,
        action: BoundChildAction,
    ) -> Option<Self> {
        if matches!(authority.scope, DescendantTerminationScope::ParentAlone) {
            return None;
        }
        let reason = match authority.kind {
            ParentTerminationKind::Stopped => DelegationOutcomeReason::ParentStopped {
                scope: authority.scope,
            },
            ParentTerminationKind::Cancelled => DelegationOutcomeReason::ParentCancelled {
                scope: authority.scope,
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

/// One exact parent/child relationship keyed by its spawning request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionDelegation {
    spawning_request: ToolRequestId,
    parent: SessionId,
    child: SessionId,
    task: DelegationContent,
    policy: ChildRelationshipPolicy,
    lifecycle: DelegationLifecycle,
    events: Vec<DelegationEvent>,
}

impl SessionDelegation {
    pub fn spawn(
        spawning_request: DelegatedSpawnRequest,
        child: SessionId,
    ) -> Result<Self, DelegationTransitionError> {
        let parent = spawning_request.request().session();
        if parent == child {
            return Err(DelegationTransitionError {
                spawning_request: spawning_request.request().id(),
                failure: DelegationTransitionFailure::SameSession,
                rejected: None,
            });
        }
        let provenance = DelegationProvenance::from_spawn(&spawning_request);
        Ok(Self {
            spawning_request: spawning_request.request().id(),
            parent,
            child,
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

    pub const fn child_creation_provenance(&self) -> SessionCreationProvenance {
        SessionCreationProvenance::delegated(self.spawning_request)
    }

    pub fn register_wait(
        &self,
        awaiting_request: &DelegationAwaitRequest,
    ) -> Result<DelegationWait, DelegationTransitionError> {
        if awaiting_request.request().session() != self.parent
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
    ) -> Result<(Self, DelegationEvent), DelegationTransitionError> {
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
            Ok(ordinal) => ordinal,
            Err(failure) => return Err(Self::reject_message(self, sending_request, id, failure)),
        };
        let event = DelegationEvent::MessageDelivered {
            ordinal,
            message: DelegationMessage {
                id,
                direction,
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
        if self.lifecycle != DelegationLifecycle::Active {
            return Err(Self::reject_outcome(
                self,
                outcome,
                DelegationTransitionFailure::AlreadyTerminal,
            ));
        }
        if !outcome_matches_relation(&self, &outcome) {
            return Err(Self::reject_outcome(
                self,
                outcome,
                DelegationTransitionFailure::OutcomeReasonMismatch,
            ));
        }
        let remains_active = outcome.kind() == DelegationOutcomeKind::ContinueRunning;
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
}

fn outcome_matches_relation(relation: &SessionDelegation, outcome: &DelegationOutcome) -> bool {
    let reason = outcome.reason();
    let child_matches = || {
        outcome
            .provenance()
            .child_turn()
            .is_some_and(|(child, _)| child == relation.child)
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
    }
}

fn parent_outcome_matches(
    relation: &SessionDelegation,
    outcome: &DelegationOutcome,
    reason: DelegationOutcomeReason,
    expected_action: BoundChildAction,
) -> bool {
    let authority_matches = match (outcome.provenance().kind, reason) {
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
    };
    authority_matches && descendant_action(relation.policy, reason) == Some(expected_action)
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
    DuplicateMessageIdentity,
    ConflictingMessageReplay,
    DuplicateOutcomeAuthority,
    OutcomeReasonMismatch,
    EventOrdinalExhausted,
}

/// Unchanged aggregate and exact attempted input from a rejected consuming transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RejectedDelegationTransition {
    DeliverMessage {
        relation: SessionDelegation,
        request: DelegationMessageRequest,
        id: DelegationMessageId,
    },
    RecordOutcome {
        relation: SessionDelegation,
        outcome: DelegationOutcome,
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
        NormalizedToolArguments, ToolCallProposal, ToolName, ToolRequestOrdinal,
        model_execution::tests::completed_turn_fixture,
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
            turn: turn_id(2),
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
        NormalizedToolArguments, ToolCallProposal, ToolName, ToolRequestOrdinal,
        model_execution::tests::completed_turn_fixture,
        test_support::{
            command_id, delegation_message_id, model_call_id, session_id, tool_request_id, turn_id,
        },
    };

    const TEST_TASK: &str = "inspect aggregate work";

    /// Canonical aggregate request fixture: `source` selects the source
    /// session, while `request_seed` independently derives request +1000, turn
    /// +10, call +20, and ordinal zero.
    fn named_request(
        source: u128,
        request_seed: u128,
        name: &str,
        arguments: serde_json::Value,
    ) -> ToolRequest {
        ToolRequest::from_model_proposal(
            tool_request_id(request_seed + 1000),
            session_id(source),
            turn_id(request_seed + 10),
            model_call_id(request_seed + 20),
            ToolRequestOrdinal::from_u32(0),
            ToolCallProposal::new(
                ToolName::try_new(name.into()).expect("valid fixture name"),
                NormalizedToolArguments::try_from_provider_text(arguments.to_string())
                    .expect("valid fixture arguments"),
            ),
        )
    }

    fn spawn_request(
        parent: u128,
        request_seed: u128,
        policy: ChildRelationshipPolicy,
    ) -> DelegatedSpawnRequest {
        let relationship = relationship_argument(policy);
        DelegatedSpawnRequest::parse(
            named_request(
                parent,
                request_seed,
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
        parent: u128,
        request_seed: u128,
        child: SessionId,
        mode: DelegationWaitMode,
    ) -> DelegationAwaitRequest {
        DelegationAwaitRequest::parse(
            named_request(
                parent,
                request_seed,
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
        source: u128,
        request_seed: u128,
        peer: SessionId,
        value: &str,
    ) -> DelegationMessageRequest {
        DelegationMessageRequest::parse(
            named_request(
                source,
                request_seed,
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

    fn relation(parent: u128, child: u128, policy: ChildRelationshipPolicy) -> SessionDelegation {
        SessionDelegation::spawn(spawn_request(parent, 1, policy), session_id(child))
            .expect("fixture parent and child are distinct")
    }

    fn parent_authority(
        parent: u128,
        kind: ParentTerminationKind,
        scope: DescendantTerminationScope,
    ) -> ParentTerminationAuthority {
        ParentTerminationAuthority {
            parent: session_id(parent),
            turn: turn_id(5),
            command: command_id(6),
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

    fn assert_standard_error<T: std::error::Error>() {}

    #[test]
    fn delegation_public_errors_implement_standard_error_contract() {
        assert_standard_error::<DelegationContentError>();
        assert_standard_error::<DelegationRequestError>();
        assert_standard_error::<DelegationTransitionError>();
    }

    /// S18 / INV-003 / INV-010: spawn retains sealed request facts and no ancestry.
    #[test]
    fn s18_inv003_inv010_aggregate_spawn_retains_policy_task_and_provenance() {
        let policy = ChildRelationshipPolicy::Bound {
            on_parent_stopped: BoundChildAction::Stop,
            on_parent_cancelled: BoundChildAction::Cancel,
        };
        let request = spawn_request(2, 1, policy);
        let spawning_request = request.request().id();
        let relation = SessionDelegation::spawn(request, session_id(3)).expect("distinct child");

        assert_eq!(relation.spawning_request(), spawning_request);
        assert_eq!(relation.task(), &content(TEST_TASK));
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

    /// S18 / INV-010: foreground wait retains the exact child subject.
    #[test]
    fn s18_inv010_foreground_registration_yields_exact_child_wait() {
        let relation = relation(2, 3, ChildRelationshipPolicy::Background);
        let awaiting = await_request(2, 2, relation.child(), DelegationWaitMode::Foreground);
        let expected_request = awaiting.request().id();
        let wait = relation
            .register_wait(&awaiting)
            .expect("parent may await its exact child");
        let subject = wait
            .foreground_subject()
            .expect("foreground wait retains turn subject");

        assert_eq!(subject.awaiting_request(), expected_request);
        assert_eq!(subject.spawning_request(), relation.spawning_request());
        assert_eq!(subject.child(), relation.child());
    }

    /// S18 / INV-010: background wait releases the parent turn subject.
    #[test]
    fn s18_inv010_background_registration_has_no_child_wait() {
        let relation = relation(2, 3, ChildRelationshipPolicy::Background);
        let awaiting = await_request(2, 2, relation.child(), DelegationWaitMode::Background);
        let wait = relation
            .register_wait(&awaiting)
            .expect("parent may await its exact child");

        assert_eq!(wait.mode(), DelegationWaitMode::Background);
        assert_eq!(wait.foreground_subject(), None);
    }

    /// S18 / INV-010: a typed await for another child cannot cross relations.
    #[test]
    fn s18_inv010_wait_registration_rejects_relation_child_cross_wiring() {
        let relation = relation(2, 3, ChildRelationshipPolicy::Background);
        let awaiting = await_request(2, 2, session_id(9), DelegationWaitMode::Background);
        let error = relation
            .register_wait(&awaiting)
            .expect_err("another child cannot reuse this relation");

        assert_eq!(
            error.failure(),
            DelegationTransitionFailure::InvalidProvenance
        );
    }

    /// S18 / INV-010: one request cannot both spawn and await a child.
    #[test]
    fn s18_inv010_wait_registration_requires_distinct_parent_work() {
        let relation = relation(2, 3, ChildRelationshipPolicy::Background);
        let awaiting = await_request(2, 1, relation.child(), DelegationWaitMode::Background);
        let error = relation
            .register_wait(&awaiting)
            .expect_err("spawn request identity cannot also register a wait");

        assert_eq!(
            error.failure(),
            DelegationTransitionFailure::InvalidProvenance
        );
    }

    /// S18 / INV-010: messages are relation-directed and request-provenanced.
    #[test]
    fn s18_inv010_messages_are_bidirectional_and_ordered() {
        let relation = relation(2, 3, ChildRelationshipPolicy::Background);
        let parent_message = message_request(2, 3, relation.child(), "parent update");
        let child_message = message_request(3, 4, relation.parent(), "child update");
        let (relation, first) = relation
            .deliver_message(parent_message, delegation_message_id(5))
            .expect("parent message is related");
        let (relation, second) = relation
            .deliver_message(child_message, delegation_message_id(6))
            .expect("child message is related");

        assert_eq!(first.ordinal().get(), 2);
        assert_eq!(second.ordinal().get(), 3);
        assert_eq!(
            first.message().expect("message event").direction(),
            DelegationMessageDirection::ParentToChild
        );
        assert_eq!(
            second.message().expect("message event").direction(),
            DelegationMessageDirection::ChildToParent
        );
        assert_eq!(relation.events().len(), 3);
    }

    /// S18 / INV-010: a typed message for another peer returns exact inputs.
    #[test]
    fn s18_inv010_message_rejects_relation_peer_cross_wiring_and_returns_input() {
        let relation = relation(2, 3, ChildRelationshipPolicy::Background);
        let request = message_request(2, 3, session_id(9), "misdirected");
        let id = delegation_message_id(5);
        let error = relation
            .clone()
            .deliver_message(request.clone(), id)
            .expect_err("another peer cannot cross this relation");
        let (returned_relation, returned_request, returned_id) = rejected_message(error);

        assert_eq!(returned_relation, relation);
        assert_eq!(returned_request, request);
        assert_eq!(returned_id, id);
    }

    /// S18 / INV-012: one logical message request appends at most one event.
    #[test]
    fn s18_inv012_message_request_replay_returns_persisted_event() {
        let relation = relation(2, 3, ChildRelationshipPolicy::Background);
        let request = message_request(2, 3, relation.child(), "once");
        let (relation, first) = relation
            .deliver_message(request.clone(), delegation_message_id(5))
            .expect("first delivery appends");
        let (relation, replay) = relation
            .deliver_message(request, delegation_message_id(9))
            .expect("equal request replay returns persisted event");

        assert_eq!(replay, first);
        assert_eq!(relation.events().len(), 2);
    }

    /// S18 / INV-012: a message identity cannot name another logical request.
    #[test]
    fn s18_inv012_duplicate_message_identity_returns_attempted_request() {
        let relation = relation(2, 3, ChildRelationshipPolicy::Background);
        let first = message_request(2, 3, relation.child(), "first");
        let second = message_request(2, 4, relation.child(), "second");
        let id = delegation_message_id(5);
        let (relation, _) = relation
            .deliver_message(first, id)
            .expect("first identity is unused");
        let error = relation
            .clone()
            .deliver_message(second.clone(), id)
            .expect_err("identity reuse is rejected");
        let (returned_relation, returned_request, returned_id) = rejected_message(error);

        assert_eq!(returned_relation, relation);
        assert_eq!(returned_request, second);
        assert_eq!(returned_id, id);
    }

    /// S18 / INV-010: returned child result terminalizes exactly once.
    #[test]
    fn s18_inv010_returned_result_terminalizes_and_replays() {
        let relation = relation(2, 1, ChildRelationshipPolicy::Background);
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

    /// S18 / INV-010: another child's sealed result returns unchanged.
    #[test]
    fn s18_inv010_returned_result_rejects_foreign_child_proof() {
        let relation = relation(2, 3, ChildRelationshipPolicy::Background);
        let outcome = returned_outcome("foreign child result");
        let error = relation
            .clone()
            .record_outcome(outcome.clone())
            .expect_err("terminal proof belongs to fixture child one");
        let (returned_relation, returned_outcome) = rejected_outcome(error);

        assert_eq!(returned_relation, relation);
        assert_eq!(returned_outcome, outcome);
    }

    /// S18 / INV-010: a terminal result still accepts a late wait registration.
    #[test]
    fn s18_inv010_terminal_result_accepts_late_wait() {
        let relation = relation(2, 1, ChildRelationshipPolicy::Background)
            .record_outcome(returned_outcome("late result"))
            .expect("child result terminalizes relation");
        let awaiting = await_request(2, 2, relation.child(), DelegationWaitMode::Background);
        let wait = relation
            .register_wait(&awaiting)
            .expect("late wait registration remains valid");

        assert_eq!(wait.mode(), DelegationWaitMode::Background);
        assert_eq!(wait.child(), relation.child());
    }

    /// S18 / INV-010: messages remain available after child terminalization.
    #[test]
    fn s18_inv010_message_is_recorded_after_child_terminalizes() {
        let relation = relation(2, 1, ChildRelationshipPolicy::Background)
            .record_outcome(returned_outcome("done"))
            .expect("child result terminalizes relation");
        let request = message_request(2, 3, relation.child(), "afterward");
        let (relation, event) = relation
            .deliver_message(request, delegation_message_id(5))
            .expect("terminal relation still records messages");

        assert_eq!(relation.lifecycle(), DelegationLifecycle::Terminal);
        assert_eq!(event.ordinal().get(), 3);
    }

    /// S19 / INV-010: child cancellation retains child-turn provenance.
    #[test]
    fn s19_inv010_child_cancel_records_child_turn_provenance() {
        let relation = relation(2, 3, ChildRelationshipPolicy::Background);
        let outcome = cancelled_outcome(relation.child());
        let relation = relation
            .record_outcome(outcome)
            .expect("child cancellation belongs to relation");
        let recorded = relation.events()[1].outcome().expect("outcome event");

        assert_eq!(recorded.kind(), DelegationOutcomeKind::ChildCancelled);
        assert_eq!(
            recorded.provenance().child_turn(),
            Some((relation.child(), turn_id(7)))
        );
    }

    /// S19 / INV-010: a bound keep-running action remains active and explicit.
    #[test]
    fn s19_inv010_bound_keep_running_records_no_change() {
        let policy = ChildRelationshipPolicy::Bound {
            on_parent_stopped: BoundChildAction::KeepRunning,
            on_parent_cancelled: BoundChildAction::Cancel,
        };
        let relation = relation(2, 3, policy);
        let authority = parent_authority(
            2,
            ParentTerminationKind::Stopped,
            DescendantTerminationScope::ParentAndDescendants,
        );
        let outcome =
            DelegationOutcome::from_parent_policy(authority, BoundChildAction::KeepRunning)
                .expect("descendant-scoped parent authority");
        let relation = relation
            .record_outcome(outcome)
            .expect("bound keep-running policy matches outcome");

        assert_eq!(relation.lifecycle(), DelegationLifecycle::Active);
        assert_eq!(relation.events().len(), 2);
    }

    /// S19 / INV-012: continue-running replay does not append another event.
    #[test]
    fn s19_inv012_continue_running_replay_is_idempotent() {
        let relation = relation(2, 3, ChildRelationshipPolicy::Background);
        let authority = parent_authority(
            2,
            ParentTerminationKind::Stopped,
            DescendantTerminationScope::ParentAndDescendants,
        );
        let outcome =
            DelegationOutcome::from_parent_policy(authority, BoundChildAction::KeepRunning)
                .expect("descendant-scoped parent authority");
        let relation = relation
            .record_outcome(outcome.clone())
            .expect("first disposition appends");
        let replayed = relation
            .clone()
            .record_outcome(outcome)
            .expect("equal disposition replay is idempotent");

        assert_eq!(replayed, relation);
        assert_eq!(relation.events().len(), 2);
    }

    /// S19 / INV-010: background child survives parent stop explicitly.
    #[test]
    fn s19_inv010_background_child_survives_parent_stop_with_typed_outcome() {
        let relation = relation(2, 3, ChildRelationshipPolicy::Background);
        let authority = parent_authority(
            2,
            ParentTerminationKind::Stopped,
            DescendantTerminationScope::ParentAndDescendants,
        );
        let outcome =
            DelegationOutcome::from_parent_policy(authority, BoundChildAction::KeepRunning)
                .expect("descendant-scoped parent authority");
        let relation = relation
            .record_outcome(outcome)
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

    /// S19 / INV-010: background child survives parent cancellation explicitly.
    #[test]
    fn s19_inv010_background_child_survives_parent_cancel_with_typed_outcome() {
        let relation = relation(2, 3, ChildRelationshipPolicy::Background);
        let authority = parent_authority(
            2,
            ParentTerminationKind::Cancelled,
            DescendantTerminationScope::ParentAndDescendants,
        );
        let outcome =
            DelegationOutcome::from_parent_policy(authority, BoundChildAction::KeepRunning)
                .expect("descendant-scoped parent authority");
        let relation = relation
            .record_outcome(outcome)
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

    /// S19 / INV-010: bound child follows its chosen parent-stop policy.
    #[test]
    fn s19_inv010_bound_child_follows_parent_stop_policy() {
        let policy = ChildRelationshipPolicy::Bound {
            on_parent_stopped: BoundChildAction::Stop,
            on_parent_cancelled: BoundChildAction::Cancel,
        };
        let relation = relation(2, 3, policy);
        let authority = parent_authority(
            2,
            ParentTerminationKind::Stopped,
            DescendantTerminationScope::ParentAndDescendants,
        );
        let outcome = DelegationOutcome::from_parent_policy(authority, BoundChildAction::Stop)
            .expect("descendant-scoped parent authority");
        let relation = relation
            .record_outcome(outcome)
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

    /// S19 / INV-010: bound child follows its chosen parent-cancel policy.
    #[test]
    fn s19_inv010_bound_child_follows_parent_cancel_policy() {
        let policy = ChildRelationshipPolicy::Bound {
            on_parent_stopped: BoundChildAction::Stop,
            on_parent_cancelled: BoundChildAction::Cancel,
        };
        let relation = relation(2, 3, policy);
        let authority = parent_authority(
            2,
            ParentTerminationKind::Cancelled,
            DescendantTerminationScope::ParentAndDescendants,
        );
        let outcome = DelegationOutcome::from_parent_policy(authority, BoundChildAction::Cancel)
            .expect("descendant-scoped parent authority");
        let relation = relation
            .record_outcome(outcome)
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

    /// S19 / INV-010: parent authority cannot override the chosen edge action.
    #[test]
    fn s19_inv010_parent_outcome_rejects_wrong_policy_action() {
        let policy = ChildRelationshipPolicy::Bound {
            on_parent_stopped: BoundChildAction::Stop,
            on_parent_cancelled: BoundChildAction::Cancel,
        };
        let relation = relation(2, 3, policy);
        let authority = parent_authority(
            2,
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

    /// S19 / INV-010: foreign parent authority returns aggregate and outcome.
    #[test]
    fn s19_inv010_parent_outcome_rejects_foreign_termination_authority() {
        let relation = relation(2, 3, ChildRelationshipPolicy::Background);
        let authority = parent_authority(
            9,
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
