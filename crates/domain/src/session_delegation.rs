//! Typed delegated-session relation, messages, outcomes, and wait subject.

use sha2::{Digest, Sha256};

use crate::{
    DelegationMessageId, DurableCommandId, NonEmptyUnicodeText, SessionId, ToolRequest,
    ToolRequestId, TurnId,
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
    /// Shared admission bound for delegated tasks, messages, and results.
    pub const MAX_UTF8_BYTES: usize = 1_048_576;

    pub fn try_new(value: String) -> Result<Self, DelegationContentError> {
        if value.len() > Self::MAX_UTF8_BYTES {
            return Err(DelegationContentError::Oversized {
                utf8_byte_length: value.len(),
            });
        }
        NonEmptyUnicodeText::try_new(value)
            .map(Self)
            .map_err(|error| DelegationContentError::Invalid(error.failure()))
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
            return Err(DelegationContentError::Oversized { utf8_byte_length });
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
pub enum DelegationContentError {
    Invalid(crate::NonEmptyUnicodeTextFailure),
    Oversized { utf8_byte_length: usize },
}

impl std::fmt::Display for DelegationContentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(failure) => write!(f, "invalid delegation content: {failure:?}"),
            Self::Oversized { utf8_byte_length } => write!(
                f,
                "delegation content is {utf8_byte_length} bytes; maximum is {}",
                DelegationContent::MAX_UTF8_BYTES
            ),
        }
    }
}

impl std::error::Error for DelegationContentError {}

/// Why a tool request could not be sealed as one delegation operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

    pub const fn failure(&self) -> DelegationRequestFailure {
        self.failure
    }

    pub fn into_request(self) -> ToolRequest {
        *self.request
    }
}

impl std::fmt::Display for DelegationRequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.failure {
            DelegationRequestFailure::InvalidToolRequestPurpose => {
                f.write_str("tool request is not the exact canonical delegation operation")
            }
            DelegationRequestFailure::InvalidContent(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for DelegationRequestError {}

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
        policy: ChildRelationshipPolicy,
    ) -> Result<Self, DelegationRequestError> {
        let value = parse_arguments(&request, SPAWN_SESSION_TOOL_NAME)?;
        let task_text = value
            .get("task")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| invalid_request(request.clone()))?;
        let task = DelegationContent::try_new(task_text.to_owned()).map_err(|failure| {
            DelegationRequestError {
                request: Box::new(request.clone()),
                failure: DelegationRequestFailure::InvalidContent(failure),
            }
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
        content: DelegationContent,
    ) -> Result<Self, DelegationRequestError> {
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
    pub fn from_scheduling(
        value: &crate::AcceptedInputTurnSchedulingProjection,
        reason: DelegationOutcomeReason,
        assistant_text: &[crate::AssistantText],
    ) -> Option<Self> {
        let (kind, result_digest) = match (value.status(), reason) {
            (
                crate::AcceptedInputTurnSchedulingStatus::TerminalCompleted,
                DelegationOutcomeReason::ChildCompleted,
            ) => {
                let content = DelegationContent::from_assistant_text(assistant_text).ok()?;
                (
                    TerminalChildTurnKind::Returned,
                    Some(delegation_content_digest(&content)),
                )
            }
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
    ChildCancelled,
    ParentStopped { scope: DescendantTerminationScope },
    ParentCancelled { scope: DescendantTerminationScope },
}

/// Every child disposition, including an explicit no-change evaluation.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum DelegationOutcome {
    ResultReturned {
        content: DelegationContent,
        reason: DelegationOutcomeReason,
        provenance: DelegationProvenance,
    },
    ChildFailed {
        reason: DelegationOutcomeReason,
        provenance: DelegationProvenance,
    },
    ChildStopped {
        reason: DelegationOutcomeReason,
        provenance: DelegationProvenance,
    },
    ChildCancelled {
        reason: DelegationOutcomeReason,
        provenance: DelegationProvenance,
    },
    ContinueRunning {
        reason: DelegationOutcomeReason,
        provenance: DelegationProvenance,
    },
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
        NormalizedToolArguments, ToolCallProposal, ToolName, ToolRequestOrdinal,
        test_support::{model_call_id, session_id, tool_request_id, turn_id},
    };

    const TEST_TASK: &str = "inspect delegated work";

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
        let request = DelegatedSpawnRequest::parse(raw, ChildRelationshipPolicy::Background)
            .expect("canonical spawn request");
        let provenance = DelegationProvenance::from_spawn(&request);

        assert_eq!(request.task(), &content(TEST_TASK));
        assert_eq!(request.policy(), ChildRelationshipPolicy::Background);
        assert_eq!(provenance.tool_request(), Some(expected_identity));
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
        let request = DelegationMessageRequest::parse(raw, peer, message.clone())
            .expect("canonical message request");

        assert_eq!(request.peer(), peer);
        assert_eq!(request.content(), &message);
        assert_eq!(
            DelegationProvenance::from_message(&request).tool_request(),
            Some(expected_identity)
        );
    }

    #[test]
    fn request_parser_rejects_noncanonical_arguments_without_consuming_request() {
        let raw = named_request(
            1,
            SPAWN_SESSION_TOOL_NAME,
            serde_json::json!({
                "relationship": { "kind": "background" },
                "task": TEST_TASK,
                "unknown": true,
            }),
        );
        let error = DelegatedSpawnRequest::parse(raw.clone(), ChildRelationshipPolicy::Background)
            .expect_err("unknown fields are noncanonical");

        assert_eq!(
            error.failure(),
            DelegationRequestFailure::InvalidToolRequestPurpose
        );
        assert_eq!(error.into_request(), raw);
    }

    #[test]
    fn content_bound_reports_exact_utf8_length() {
        let value = "x".repeat(DelegationContent::MAX_UTF8_BYTES + 1);
        let error = DelegationContent::try_new(value).expect_err("oversized content must fail");

        assert_eq!(
            error,
            DelegationContentError::Oversized {
                utf8_byte_length: DelegationContent::MAX_UTF8_BYTES + 1,
            }
        );
        assert_eq!(
            error.to_string(),
            "delegation content is 1048577 bytes; maximum is 1048576"
        );
    }
}
