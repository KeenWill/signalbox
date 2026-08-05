//! Model-facing session-delegation tools over an injected nonblocking port.

use std::{error::Error, fmt, future::Future, num::NonZeroU64};

use signalbox_application::{
    ClassifyOperatorFailure, CompiledTool, CompiledToolCatalog, CorrelatedDurableToolCompletion,
    CorrelatedToolExecutorEvidence, OperatorFailureClass, ToolArgumentValidator,
    ToolExecutionInvocation, ToolExecutorEvidence,
};
use signalbox_domain::{
    BoundChildAction, ChildRelationshipPolicy, DelegatedSpawnRequest, DelegationAwaitRequest,
    DelegationContent, DelegationEvent, DelegationEventOrdinal, DelegationMessageDirection,
    DelegationMessageId, DelegationMessageRequest, DelegationOutcome, DelegationOutcomeKind,
    DelegationOutcomeReason, DelegationProvenance, DelegationRequestError, DelegationWait,
    DelegationWaitMode, NormalizedToolArguments, SessionDelegation, SessionId,
    ToolAttemptDispatchCorrelation, ToolDispatchAuthority, ToolEffectClass,
    ToolExecutionErrorDetail, ToolPermissionDefault, ToolRequest, ToolRequestId, ToolResultText,
};
use signalbox_model_provider_runtime::render_delegation_outcome;
use signalbox_tool_contract::{
    ToolContract, ToolContractCompileError, compile_contract_definition,
};

/// Model-facing child-spawn tool name.
pub const SPAWN_SESSION_NAME: &str = "spawn_session";
/// Model-facing child-result wait tool name.
pub const AWAIT_SESSION_NAME: &str = "await_session";
/// Model-facing bidirectional-message tool name.
pub const SEND_SESSION_MESSAGE_NAME: &str = "send_session_message";
/// Stable session-delegation registry names in declaration order.
pub const SESSION_DELEGATION_TOOL_NAMES: [&str; 3] = [
    SPAWN_SESSION_NAME,
    AWAIT_SESSION_NAME,
    SEND_SESSION_MESSAGE_NAME,
];
/// Exact UTF-8 byte ceiling owned by delegated content.
pub const MAX_DELEGATION_CONTENT_BYTES: usize = DelegationContent::MAX_UTF8_BYTES;

const UUID_TEXT_BYTES: usize = 36;
const INVALID_ARGUMENTS_DETAIL: &str = "invalid bounded session-delegation arguments";
const REJECTED_DETAIL: &str = "session-delegation request rejected";

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum BoundChildActionArguments {
    KeepRunning,
    Stop,
    Cancel,
}

impl From<BoundChildActionArguments> for BoundChildAction {
    fn from(value: BoundChildActionArguments) -> Self {
        match value {
            BoundChildActionArguments::KeepRunning => Self::KeepRunning,
            BoundChildActionArguments::Stop => Self::Stop,
            BoundChildActionArguments::Cancel => Self::Cancel,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ChildRelationshipArguments {
    Background,
    Bound {
        on_parent_stopped: BoundChildActionArguments,
        on_parent_cancelled: BoundChildActionArguments,
    },
}

impl From<ChildRelationshipArguments> for ChildRelationshipPolicy {
    fn from(value: ChildRelationshipArguments) -> Self {
        match value {
            ChildRelationshipArguments::Background => Self::Background,
            ChildRelationshipArguments::Bound {
                on_parent_stopped,
                on_parent_cancelled,
            } => Self::Bound {
                on_parent_stopped: on_parent_stopped.into(),
                on_parent_cancelled: on_parent_cancelled.into(),
            },
        }
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SpawnSessionArguments {
    #[schemars(length(min = 1, max = MAX_DELEGATION_CONTENT_BYTES))]
    task: String,
    relationship: ChildRelationshipArguments,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum DelegationWaitModeArguments {
    Foreground,
    Background,
}

impl From<DelegationWaitModeArguments> for DelegationWaitMode {
    fn from(value: DelegationWaitModeArguments) -> Self {
        match value {
            DelegationWaitModeArguments::Foreground => Self::Foreground,
            DelegationWaitModeArguments::Background => Self::Background,
        }
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct AwaitSessionArguments {
    #[schemars(length(min = UUID_TEXT_BYTES, max = UUID_TEXT_BYTES))]
    child_session_id: String,
    mode: DelegationWaitModeArguments,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SendSessionMessageArguments {
    #[schemars(length(min = UUID_TEXT_BYTES, max = UUID_TEXT_BYTES))]
    peer_session_id: String,
    #[schemars(length(min = 1, max = MAX_DELEGATION_CONTENT_BYTES))]
    content: String,
}

struct SpawnSessionContract;

impl ToolContract for SpawnSessionContract {
    type Arguments = SpawnSessionArguments;
    const NAME: &'static str = SPAWN_SESSION_NAME;
    const DESCRIPTION: &'static str = "Creates a delegated child session for a bounded task with a parent-selected lifecycle relationship.";
}

struct AwaitSessionContract;

impl ToolContract for AwaitSessionContract {
    type Arguments = AwaitSessionArguments;
    const NAME: &'static str = AWAIT_SESSION_NAME;
    const DESCRIPTION: &'static str =
        "Registers foreground or background delivery of one related child session's result.";
}

struct SendSessionMessageContract;

impl ToolContract for SendSessionMessageContract {
    type Arguments = SendSessionMessageArguments;
    const NAME: &'static str = SEND_SESSION_MESSAGE_NAME;
    const DESCRIPTION: &'static str =
        "Sends bounded content to the other endpoint of one session-delegation relationship.";
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionDelegationToolKind {
    Spawn,
    Await,
    SendMessage,
}

impl SessionDelegationToolKind {
    const ALL: [Self; 3] = [Self::Spawn, Self::Await, Self::SendMessage];

    fn definition(self) -> Result<signalbox_application::ToolDefinition, ToolContractCompileError> {
        match self {
            Self::Spawn => compile_contract_definition::<SpawnSessionContract>(
                ToolPermissionDefault::Auto,
                ToolEffectClass::ExternalEffect,
            ),
            Self::Await => compile_contract_definition::<AwaitSessionContract>(
                ToolPermissionDefault::Auto,
                ToolEffectClass::EffectFree,
            ),
            Self::SendMessage => compile_contract_definition::<SendSessionMessageContract>(
                ToolPermissionDefault::Auto,
                ToolEffectClass::ExternalEffect,
            ),
        }
    }
}

/// One spawn receipt derived from the admitted relationship.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnSessionReceipt {
    tool_request: ToolRequestId,
    child: SessionId,
    policy: ChildRelationshipPolicy,
}

impl SpawnSessionReceipt {
    /// Projects a receipt only when the aggregate matches the sealed request.
    pub fn from_relation(
        request: &DelegatedSpawnRequest,
        relation: &SessionDelegation,
    ) -> Option<Self> {
        let spawn_provenance_matches = relation.events().first().is_some_and(|event| {
            matches!(
                event,
                DelegationEvent::Spawned { provenance, .. }
                    if provenance.tool_request()
                        == Some((
                            request.request().session(),
                            request.request().turn(),
                            request.request().id(),
                        ))
            )
        });
        (relation.spawning_request() == request.request().id()
            && relation.parent() == request.request().session()
            && relation.task() == request.task()
            && relation.policy() == request.policy()
            && spawn_provenance_matches)
            .then_some(Self {
                tool_request: relation.spawning_request(),
                child: relation.child(),
                policy: relation.policy(),
            })
    }

    /// Returns the exact spawning tool request.
    pub const fn tool_request(&self) -> ToolRequestId {
        self.tool_request
    }

    /// Returns the admitted child session.
    pub const fn child(&self) -> SessionId {
        self.child
    }

    /// Returns the immutable relationship policy.
    pub const fn policy(&self) -> ChildRelationshipPolicy {
        self.policy
    }
}

/// One background delivery-registration receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AwaitSessionReceipt {
    tool_request: ToolRequestId,
    child: SessionId,
    mode: DelegationWaitMode,
}

impl AwaitSessionReceipt {
    /// Projects a receipt only from the exact sealed request and wait.
    pub fn from_wait(request: &DelegationAwaitRequest, wait: DelegationWait) -> Option<Self> {
        (request.request().id() == wait.awaiting_request()
            && request.request().session() == wait.parent()
            && request.child() == wait.child()
            && request.mode() == wait.mode())
        .then_some(Self {
            tool_request: wait.awaiting_request(),
            child: wait.child(),
            mode: wait.mode(),
        })
    }

    /// Returns the exact await tool request.
    pub const fn tool_request(self) -> ToolRequestId {
        self.tool_request
    }

    /// Returns the related child.
    pub const fn child(self) -> SessionId {
        self.child
    }

    /// Returns the registered wait mode.
    pub const fn mode(self) -> DelegationWaitMode {
        self.mode
    }
}

/// One message receipt derived from the admitted relationship event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionMessageReceipt {
    tool_request: ToolRequestId,
    message: DelegationMessageId,
    direction: DelegationMessageDirection,
    ordinal: DelegationEventOrdinal,
    delivery_sequence: NonZeroU64,
}

impl SessionMessageReceipt {
    /// Projects a receipt only when the relation event matches the request.
    pub fn from_relation_event(
        request: &DelegationMessageRequest,
        relation: &SessionDelegation,
        event: &DelegationEvent,
        delivery_sequence: NonZeroU64,
    ) -> Option<Self> {
        let message = event.message()?;
        let source = request.request().session();
        let expected_direction =
            if source == relation.parent() && request.peer() == relation.child() {
                DelegationMessageDirection::ParentToChild
            } else if source == relation.child() && request.peer() == relation.parent() {
                DelegationMessageDirection::ChildToParent
            } else {
                return None;
            };
        let provenance = message.provenance().tool_request()?;
        (relation.events().contains(event)
            && provenance
                == (
                    request.request().session(),
                    request.request().turn(),
                    request.request().id(),
                )
            && message.content() == request.content()
            && message.direction() == expected_direction)
            .then_some(Self {
                tool_request: request.request().id(),
                message: message.id(),
                direction: message.direction(),
                ordinal: event.ordinal(),
                delivery_sequence,
            })
    }

    /// Returns the exact sending tool request.
    pub const fn tool_request(self) -> ToolRequestId {
        self.tool_request
    }

    /// Returns the durable message identity.
    pub const fn message(self) -> DelegationMessageId {
        self.message
    }

    /// Returns the relationship direction.
    pub const fn direction(self) -> DelegationMessageDirection {
        self.direction
    }

    /// Returns the durable relation-event ordinal.
    pub const fn ordinal(self) -> DelegationEventOrdinal {
        self.ordinal
    }

    /// Returns the recipient's durable delivery-stream position.
    pub const fn delivery_sequence(self) -> NonZeroU64 {
        self.delivery_sequence
    }
}

/// A terminal child result selected for delivery to one exact parent wait.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveredChildResult {
    wait: DelegationWait,
    outcome: DelegationOutcome,
}

impl DeliveredChildResult {
    /// Admits only a deliverable outcome event from the exact validated relation.
    pub fn try_new(
        wait: DelegationWait,
        relation: &SessionDelegation,
        event: &DelegationEvent,
    ) -> Result<Self, DeliveredChildResultError> {
        let Some(outcome) = event.outcome() else {
            return Err(DeliveredChildResultError {
                wait,
                event: Box::new(event.clone()),
            });
        };
        let valid_kind = match outcome.kind() {
            DelegationOutcomeKind::ResultReturned
            | DelegationOutcomeKind::ChildFailed
            | DelegationOutcomeKind::ChildStopped
            | DelegationOutcomeKind::ChildCancelled => true,
            DelegationOutcomeKind::AlreadyTerminal | DelegationOutcomeKind::ContinueRunning => {
                false
            }
        };
        if wait.mode() != DelegationWaitMode::Foreground
            || wait.spawning_request() != relation.spawning_request()
            || wait.parent() != relation.parent()
            || wait.child() != relation.child()
            || relation.lifecycle() != signalbox_domain::DelegationLifecycle::Terminal
            || !valid_kind
            || !relation.events().iter().any(|candidate| candidate == event)
        {
            return Err(DeliveredChildResultError {
                wait,
                event: Box::new(event.clone()),
            });
        }
        Ok(Self {
            wait,
            outcome: outcome.clone(),
        })
    }

    /// Returns the exact foreground wait that selected this delivery.
    pub const fn wait(&self) -> DelegationWait {
        self.wait
    }

    /// Returns the child whose terminal result is delivered.
    pub const fn child(&self) -> SessionId {
        self.wait.child()
    }

    /// Returns the closed terminal outcome kind.
    pub const fn kind(&self) -> DelegationOutcomeKind {
        self.outcome.kind()
    }

    /// Borrows returned content when this is a successful child result.
    pub const fn content(&self) -> Option<&DelegationContent> {
        self.outcome.content()
    }

    /// Returns the typed lifecycle reason.
    pub const fn reason(&self) -> DelegationOutcomeReason {
        self.outcome.reason()
    }

    /// Returns the typed terminal authority projection.
    pub const fn provenance(&self) -> DelegationProvenance {
        self.outcome.provenance()
    }
}

/// A nonterminal or cross-wired relationship event was offered for delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveredChildResultError {
    wait: DelegationWait,
    event: Box<DelegationEvent>,
}

impl DeliveredChildResultError {
    /// Returns the unchanged rejected input.
    pub fn into_parts(self) -> (DelegationWait, DelegationEvent) {
        (self.wait, *self.event)
    }
}

impl fmt::Display for DeliveredChildResultError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("delegation event is not deliverable for the selected relationship")
    }
}

impl Error for DeliveredChildResultError {}

/// Applied effect or a definitive checked refusal from the durable boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionDelegationPortOutcome<Value> {
    /// The effect was applied or equally replayed.
    Applied(Value),
    /// Domain or durable admission definitively refused the request.
    Rejected,
}

/// Nonblocking result of registering or observing one child wait.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AwaitSessionPortOutcome {
    /// Background delivery was registered or equally replayed.
    BackgroundRegistered(AwaitSessionReceipt),
    /// A foreground result already existed and is deliverable immediately.
    Delivered(DeliveredChildResult),
    /// The foreground wait and physical-attempt closure were atomically parked.
    ForegroundPending(DelegationWait),
    /// Domain or durable admission definitively refused the request.
    Rejected,
}

/// Nonblocking durable boundary for the session-delegation tool family.
pub trait SessionDelegationPort: Send {
    /// Sanitized adapter failure returned when no trustworthy result exists.
    type Error: ClassifyOperatorFailure;

    /// Creates or equally replays one delegated child and relationship.
    fn spawn_session(
        &mut self,
        request: DelegatedSpawnRequest,
        dispatch: ToolDispatchAuthority,
    ) -> impl Future<Output = Result<SessionDelegationPortOutcome<SpawnSessionReceipt>, Self::Error>>
    + Send;

    /// Registers or observes one wait without waiting for child completion.
    ///
    /// Before returning [`AwaitSessionPortOutcome::ForegroundPending`], the port
    /// commits the wait registration, physical-attempt closure, and turn
    /// `AwaitingChild` state in one durable transaction. The returned handoff
    /// stops local execution; it does not authorize a later parking write.
    fn await_session(
        &mut self,
        request: DelegationAwaitRequest,
        dispatch: ToolDispatchAuthority,
    ) -> impl Future<Output = Result<AwaitSessionPortOutcome, Self::Error>> + Send;

    /// Appends or equally replays one relationship message.
    fn send_session_message(
        &mut self,
        request: DelegationMessageRequest,
        dispatch: ToolDispatchAuthority,
    ) -> impl Future<
        Output = Result<SessionDelegationPortOutcome<SessionMessageReceipt>, Self::Error>,
    > + Send;
}

/// A static session-delegation tool family could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionDelegationToolsConstructionError {
    /// One static name was invalid.
    Name,
    /// One static schema was invalid.
    Schema,
    /// One static sanitized error detail was invalid.
    ErrorDetail,
    /// The compiled catalog unexpectedly contained a duplicate.
    Duplicate,
}

impl fmt::Display for SessionDelegationToolsConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Name => "session-delegation static tool name is invalid",
            Self::Schema => "session-delegation static tool schema is invalid",
            Self::ErrorDetail => "session-delegation static error detail is invalid",
            Self::Duplicate => "session-delegation tool catalog contains a duplicate",
        })
    }
}

impl Error for SessionDelegationToolsConstructionError {}

/// Compiled session-delegation catalog and matching nonblocking executor.
#[derive(Clone, Debug)]
pub struct SessionDelegationTools<Port> {
    catalog: CompiledToolCatalog,
    executor: SessionDelegationExecutor<Port>,
}

impl<Port> SessionDelegationTools<Port> {
    /// Compiles the three automatic daemon-local delegation tools.
    pub fn try_new(port: Port) -> Result<Self, SessionDelegationToolsConstructionError> {
        let invalid_arguments_detail = detail(INVALID_ARGUMENTS_DETAIL)?;
        let rejected_detail = detail(REJECTED_DETAIL)?;
        let compiled = SessionDelegationToolKind::ALL
            .into_iter()
            .map(|kind| {
                let definition = kind.definition().map_err(map_contract_error)?;
                Ok(CompiledTool::new(
                    definition,
                    SessionDelegationArgumentValidator {
                        kind,
                        detail: invalid_arguments_detail.clone(),
                    },
                ))
            })
            .collect::<Result<Vec<_>, SessionDelegationToolsConstructionError>>()?;
        let catalog = CompiledToolCatalog::try_new(compiled)
            .map_err(|_| SessionDelegationToolsConstructionError::Duplicate)?;
        Ok(Self {
            catalog,
            executor: SessionDelegationExecutor {
                port,
                rejected_detail,
            },
        })
    }

    /// Separates declaration and execution roles.
    pub fn into_parts(self) -> (CompiledToolCatalog, SessionDelegationExecutor<Port>) {
        (self.catalog, self.executor)
    }
}

fn detail(
    value: &str,
) -> Result<ToolExecutionErrorDetail, SessionDelegationToolsConstructionError> {
    ToolExecutionErrorDetail::try_new(value.to_owned())
        .map_err(|_| SessionDelegationToolsConstructionError::ErrorDetail)
}

fn map_contract_error(error: ToolContractCompileError) -> SessionDelegationToolsConstructionError {
    match error {
        ToolContractCompileError::Name => SessionDelegationToolsConstructionError::Name,
        ToolContractCompileError::Schema => SessionDelegationToolsConstructionError::Schema,
    }
}

#[derive(Clone, Debug)]
struct SessionDelegationArgumentValidator {
    kind: SessionDelegationToolKind,
    detail: ToolExecutionErrorDetail,
}

impl ToolArgumentValidator for SessionDelegationArgumentValidator {
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail> {
        decode_arguments(self.kind, arguments)
            .map(|_| ())
            .map_err(|_| self.detail.clone())
    }
}

#[derive(Debug)]
enum DecodedArguments {
    Spawn {
        task: String,
        policy: ChildRelationshipPolicy,
    },
    Await {
        child: SessionId,
        mode: DelegationWaitMode,
    },
    SendMessage {
        peer: SessionId,
        content: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InvalidSessionDelegationArguments;

fn decode_arguments(
    kind: SessionDelegationToolKind,
    arguments: &NormalizedToolArguments,
) -> Result<DecodedArguments, InvalidSessionDelegationArguments> {
    match kind {
        SessionDelegationToolKind::Spawn => {
            let decoded: SpawnSessionArguments = serde_json::from_str(arguments.as_str())
                .map_err(|_| InvalidSessionDelegationArguments)?;
            DelegationContent::try_new(decoded.task.clone())
                .map_err(|_| InvalidSessionDelegationArguments)?;
            Ok(DecodedArguments::Spawn {
                task: decoded.task,
                policy: decoded.relationship.into(),
            })
        }
        SessionDelegationToolKind::Await => {
            let decoded: AwaitSessionArguments = serde_json::from_str(arguments.as_str())
                .map_err(|_| InvalidSessionDelegationArguments)?;
            Ok(DecodedArguments::Await {
                child: canonical_session_id(&decoded.child_session_id)?,
                mode: decoded.mode.into(),
            })
        }
        SessionDelegationToolKind::SendMessage => {
            let decoded: SendSessionMessageArguments = serde_json::from_str(arguments.as_str())
                .map_err(|_| InvalidSessionDelegationArguments)?;
            DelegationContent::try_new(decoded.content.clone())
                .map_err(|_| InvalidSessionDelegationArguments)?;
            Ok(DecodedArguments::SendMessage {
                peer: canonical_session_id(&decoded.peer_session_id)?,
                content: decoded.content,
            })
        }
    }
}

fn canonical_session_id(value: &str) -> Result<SessionId, InvalidSessionDelegationArguments> {
    if value.len() != UUID_TEXT_BYTES {
        return Err(InvalidSessionDelegationArguments);
    }
    let parsed = uuid::Uuid::parse_str(value).map_err(|_| InvalidSessionDelegationArguments)?;
    if parsed.hyphenated().to_string() != value {
        return Err(InvalidSessionDelegationArguments);
    }
    Ok(SessionId::from_uuid(parsed))
}

fn kind_for_name(name: &str) -> Option<SessionDelegationToolKind> {
    match name {
        SPAWN_SESSION_NAME => Some(SessionDelegationToolKind::Spawn),
        AWAIT_SESSION_NAME => Some(SessionDelegationToolKind::Await),
        SEND_SESSION_MESSAGE_NAME => Some(SessionDelegationToolKind::SendMessage),
        _ => None,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SessionDelegationOperation {
    Spawn(DelegatedSpawnRequest),
    Await(DelegationAwaitRequest),
    SendMessage(DelegationMessageRequest),
}

fn decode_operation(
    request: &ToolRequest,
) -> Result<SessionDelegationOperation, SessionDelegationRequestDecodeError> {
    let kind = kind_for_name(request.name().as_str())
        .ok_or_else(|| SessionDelegationRequestDecodeError::invalid_arguments(request.clone()))?;
    let decoded = decode_arguments(kind, request.arguments())
        .map_err(|_| SessionDelegationRequestDecodeError::invalid_arguments(request.clone()))?;
    match decoded {
        DecodedArguments::Spawn { task, policy } => {
            DelegatedSpawnRequest::parse(request.clone(), task, policy)
                .map(SessionDelegationOperation::Spawn)
                .map_err(SessionDelegationRequestDecodeError::sealed)
        }
        DecodedArguments::Await { child, mode } => {
            DelegationAwaitRequest::parse(request.clone(), child, mode)
                .map(SessionDelegationOperation::Await)
                .map_err(SessionDelegationRequestDecodeError::sealed)
        }
        DecodedArguments::SendMessage { peer, content } => {
            DelegationMessageRequest::parse(request.clone(), peer, content)
                .map(SessionDelegationOperation::SendMessage)
                .map_err(SessionDelegationRequestDecodeError::sealed)
        }
    }
}

/// Why one exact logical request could not enter the delegation executor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionDelegationRequestDecodeFailure {
    /// The name or bounded typed arguments were invalid.
    InvalidArguments,
    /// Domain sealing rejected canonical request purpose or content.
    Sealed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SessionDelegationRequestDecodeErrorKind {
    InvalidArguments(Box<ToolRequest>),
    Sealed(DelegationRequestError),
}

/// Failed request decoding retaining the unchanged logical request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionDelegationRequestDecodeError {
    kind: SessionDelegationRequestDecodeErrorKind,
}

impl SessionDelegationRequestDecodeError {
    fn invalid_arguments(request: ToolRequest) -> Self {
        Self {
            kind: SessionDelegationRequestDecodeErrorKind::InvalidArguments(Box::new(request)),
        }
    }

    fn sealed(error: DelegationRequestError) -> Self {
        Self {
            kind: SessionDelegationRequestDecodeErrorKind::Sealed(error),
        }
    }

    /// Returns the closed failure classification.
    pub const fn failure(&self) -> SessionDelegationRequestDecodeFailure {
        match self.kind {
            SessionDelegationRequestDecodeErrorKind::InvalidArguments(_) => {
                SessionDelegationRequestDecodeFailure::InvalidArguments
            }
            SessionDelegationRequestDecodeErrorKind::Sealed(_) => {
                SessionDelegationRequestDecodeFailure::Sealed
            }
        }
    }

    /// Returns the unchanged logical request.
    pub fn into_request(self) -> ToolRequest {
        match self.kind {
            SessionDelegationRequestDecodeErrorKind::InvalidArguments(request) => *request,
            SessionDelegationRequestDecodeErrorKind::Sealed(error) => error.into_request(),
        }
    }
}

impl fmt::Display for SessionDelegationRequestDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            SessionDelegationRequestDecodeErrorKind::InvalidArguments(_) => {
                formatter.write_str("invalid bounded session-delegation arguments")
            }
            SessionDelegationRequestDecodeErrorKind::Sealed(error) => error.fmt(formatter),
        }
    }
}

impl Error for SessionDelegationRequestDecodeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.kind {
            SessionDelegationRequestDecodeErrorKind::Sealed(error) => Some(error),
            SessionDelegationRequestDecodeErrorKind::InvalidArguments(_) => None,
        }
    }
}

/// Decodes only a canonical foreground await for the scheduler intercept.
pub fn foreground_await_request(request: &ToolRequest) -> Option<DelegationAwaitRequest> {
    match decode_operation(request).ok()? {
        SessionDelegationOperation::Await(awaiting)
            if awaiting.mode() == DelegationWaitMode::Foreground =>
        {
            Some(awaiting)
        }
        SessionDelegationOperation::Spawn(_)
        | SessionDelegationOperation::Await(_)
        | SessionDelegationOperation::SendMessage(_) => None,
    }
}

/// Exact scheduling handoff for a durably registered foreground child wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ForegroundAwaitPending {
    correlation: ToolAttemptDispatchCorrelation,
    wait: DelegationWait,
}

impl ForegroundAwaitPending {
    /// Returns the dispatch correlation whose physical attempt is already closed.
    pub const fn correlation(self) -> ToolAttemptDispatchCorrelation {
        self.correlation
    }

    /// Returns the exact durable child-wait subject.
    pub const fn wait(self) -> DelegationWait {
        self.wait
    }
}

/// Exact scheduling handoff for an already-delivered foreground child result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForegroundAwaitDelivered {
    correlation: ToolAttemptDispatchCorrelation,
    result: DeliveredChildResult,
}

impl ForegroundAwaitDelivered {
    /// Returns the issued physical dispatch correlation.
    pub const fn correlation(&self) -> ToolAttemptDispatchCorrelation {
        self.correlation
    }

    /// Borrows the typed child outcome selected by the exact foreground wait.
    pub const fn result(&self) -> &DeliveredChildResult {
        &self.result
    }

    /// Returns the typed child outcome selected by the exact foreground wait.
    pub fn into_result(self) -> DeliveredChildResult {
        self.result
    }
}

/// Nonblocking executor result: ordinary evidence or a typed scheduler handoff.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionDelegationExecutionDisposition {
    /// Ordinary completed or known-failed tool evidence bound to its dispatch.
    Completed(CorrelatedToolExecutorEvidence),
    /// Terminal evidence already committed atomically with its delegation effect.
    DurableCompletion(CorrelatedDurableToolCompletion),
    /// An already-delivered foreground result retaining its typed outcome.
    ForegroundDelivered(ForegroundAwaitDelivered),
    /// A foreground wait registered without retaining a physical future.
    ForegroundPending(ForegroundAwaitPending),
}

/// Executor for the three session-delegation operations.
#[derive(Clone, Debug)]
pub struct SessionDelegationExecutor<Port> {
    port: Port,
    rejected_detail: ToolExecutionErrorDetail,
}

impl<Port> SessionDelegationExecutor<Port> {
    /// Returns the injected port for explicit ownership handoff.
    pub fn into_port(self) -> Port {
        self.port
    }
}

/// Failure inside the session-delegation executor.
#[derive(Debug)]
pub enum SessionDelegationExecutorError<PortError> {
    /// Executor decoding disagreed with catalog validation.
    ArgumentValidationDrift(SessionDelegationRequestDecodeError),
    /// The injected port failed without trustworthy result evidence.
    Port(PortError),
    /// The port returned evidence unrelated to the exact request or wait mode.
    PortContract,
    /// Compact model-facing result encoding unexpectedly failed.
    ResultEncoding,
}

impl<PortError> fmt::Display for SessionDelegationExecutorError<PortError>
where
    PortError: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ArgumentValidationDrift(error) => error.fmt(formatter),
            Self::Port(error) => error.fmt(formatter),
            Self::PortContract => formatter.write_str("session-delegation port contract failed"),
            Self::ResultEncoding => {
                formatter.write_str("session-delegation result encoding failed")
            }
        }
    }
}

impl<PortError> Error for SessionDelegationExecutorError<PortError>
where
    PortError: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ArgumentValidationDrift(error) => Some(error),
            Self::Port(error) => Some(error),
            Self::PortContract | Self::ResultEncoding => None,
        }
    }
}

impl<PortError> ClassifyOperatorFailure for SessionDelegationExecutorError<PortError>
where
    PortError: ClassifyOperatorFailure,
{
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Port(error) => error.operator_failure_class(),
            Self::ArgumentValidationDrift(_) | Self::PortContract | Self::ResultEncoding => {
                OperatorFailureClass::CallerOrHubBug
            }
        }
    }
}

enum UnboundExecutionDisposition {
    Completed(ToolExecutorEvidence),
    DurableCompletion(ToolExecutorEvidence),
    ForegroundDelivered(DeliveredChildResult),
    ForegroundPending(DelegationWait),
}

impl<Port> SessionDelegationExecutor<Port>
where
    Port: SessionDelegationPort,
{
    /// Executes one invocation through a port that never waits on child work.
    pub async fn execute_nonblocking(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<SessionDelegationExecutionDisposition, SessionDelegationExecutorError<Port::Error>>
    {
        let operation = decode_operation(invocation.request())
            .map_err(SessionDelegationExecutorError::ArgumentValidationDrift)?;
        let dispatch = invocation.dispatch_authority().clone();
        let correlation = dispatch.correlation();
        match self.execute_operation(operation, dispatch).await? {
            UnboundExecutionDisposition::Completed(evidence) => Ok(
                SessionDelegationExecutionDisposition::Completed(invocation.bind(evidence)),
            ),
            UnboundExecutionDisposition::DurableCompletion(
                ToolExecutorEvidence::CompletedText(_),
            ) => Ok(SessionDelegationExecutionDisposition::DurableCompletion(
                invocation.durable_completion(),
            )),
            UnboundExecutionDisposition::DurableCompletion(_) => {
                Err(SessionDelegationExecutorError::PortContract)
            }
            UnboundExecutionDisposition::ForegroundDelivered(result) => {
                Ok(SessionDelegationExecutionDisposition::ForegroundDelivered(
                    ForegroundAwaitDelivered {
                        correlation,
                        result,
                    },
                ))
            }
            UnboundExecutionDisposition::ForegroundPending(wait) => {
                Ok(SessionDelegationExecutionDisposition::ForegroundPending(
                    ForegroundAwaitPending { correlation, wait },
                ))
            }
        }
    }

    async fn execute_operation(
        &mut self,
        operation: SessionDelegationOperation,
        dispatch: ToolDispatchAuthority,
    ) -> Result<UnboundExecutionDisposition, SessionDelegationExecutorError<Port::Error>> {
        match operation {
            SessionDelegationOperation::Spawn(request) => {
                let expected_request = request.request().id();
                let expected_policy = request.policy();
                let result = self
                    .port
                    .spawn_session(request, dispatch)
                    .await
                    .map_err(SessionDelegationExecutorError::Port)?;
                match result {
                    SessionDelegationPortOutcome::Applied(receipt)
                        if receipt.tool_request() == expected_request
                            && receipt.policy() == expected_policy =>
                    {
                        completed(encode_spawn_receipt(receipt)?)
                    }
                    SessionDelegationPortOutcome::Rejected => self.rejected(),
                    SessionDelegationPortOutcome::Applied(_) => {
                        Err(SessionDelegationExecutorError::PortContract)
                    }
                }
            }
            SessionDelegationOperation::Await(request) => {
                let expected_request = request.request().id();
                let expected_parent = request.request().session();
                let expected_child = request.child();
                let expected_mode = request.mode();
                let result = self
                    .port
                    .await_session(request, dispatch)
                    .await
                    .map_err(SessionDelegationExecutorError::Port)?;
                match result {
                    AwaitSessionPortOutcome::BackgroundRegistered(receipt)
                        if expected_mode == DelegationWaitMode::Background
                            && receipt.tool_request() == expected_request
                            && receipt.child() == expected_child
                            && receipt.mode() == expected_mode =>
                    {
                        durably_completed(encode_await_receipt(receipt)?)
                    }
                    AwaitSessionPortOutcome::Delivered(result)
                        if expected_mode == DelegationWaitMode::Foreground
                            && result.wait().awaiting_request() == expected_request
                            && result.wait().parent() == expected_parent
                            && result.wait().child() == expected_child
                            && result.wait().mode() == expected_mode =>
                    {
                        Ok(UnboundExecutionDisposition::ForegroundDelivered(result))
                    }
                    AwaitSessionPortOutcome::ForegroundPending(wait)
                        if expected_mode == DelegationWaitMode::Foreground
                            && wait.awaiting_request() == expected_request
                            && wait.parent() == expected_parent
                            && wait.child() == expected_child
                            && wait.mode() == expected_mode =>
                    {
                        Ok(UnboundExecutionDisposition::ForegroundPending(wait))
                    }
                    AwaitSessionPortOutcome::Rejected => self.rejected(),
                    AwaitSessionPortOutcome::BackgroundRegistered(_)
                    | AwaitSessionPortOutcome::Delivered(_)
                    | AwaitSessionPortOutcome::ForegroundPending(_) => {
                        Err(SessionDelegationExecutorError::PortContract)
                    }
                }
            }
            SessionDelegationOperation::SendMessage(request) => {
                let expected_request = request.request().id();
                let result = self
                    .port
                    .send_session_message(request, dispatch)
                    .await
                    .map_err(SessionDelegationExecutorError::Port)?;
                match result {
                    SessionDelegationPortOutcome::Applied(receipt)
                        if receipt.tool_request() == expected_request =>
                    {
                        durably_completed(encode_message_receipt(receipt)?)
                    }
                    SessionDelegationPortOutcome::Rejected => self.rejected(),
                    SessionDelegationPortOutcome::Applied(_) => {
                        Err(SessionDelegationExecutorError::PortContract)
                    }
                }
            }
        }
    }

    fn rejected(
        &self,
    ) -> Result<UnboundExecutionDisposition, SessionDelegationExecutorError<Port::Error>> {
        Ok(UnboundExecutionDisposition::Completed(
            ToolExecutorEvidence::KnownFailed {
                detail: Some(self.rejected_detail.clone()),
            },
        ))
    }
}

fn completed<PortError>(
    result: String,
) -> Result<UnboundExecutionDisposition, SessionDelegationExecutorError<PortError>> {
    let result = ToolResultText::try_new(result)
        .map_err(|_| SessionDelegationExecutorError::ResultEncoding)?
        .into_string();
    Ok(UnboundExecutionDisposition::Completed(
        ToolExecutorEvidence::CompletedText(result),
    ))
}

fn durably_completed<PortError>(
    result: String,
) -> Result<UnboundExecutionDisposition, SessionDelegationExecutorError<PortError>> {
    let result = ToolResultText::try_new(result)
        .map_err(|_| SessionDelegationExecutorError::ResultEncoding)?
        .into_string();
    Ok(UnboundExecutionDisposition::DurableCompletion(
        ToolExecutorEvidence::CompletedText(result),
    ))
}

#[derive(serde::Serialize)]
struct SpawnReceiptOutput {
    result: &'static str,
    tool_request_id: String,
    child_session_id: String,
    relationship: RelationshipOutput,
}

#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RelationshipOutput {
    Background,
    Bound {
        on_parent_stopped: &'static str,
        on_parent_cancelled: &'static str,
    },
}

fn relationship_output(policy: ChildRelationshipPolicy) -> RelationshipOutput {
    match policy {
        ChildRelationshipPolicy::Background => RelationshipOutput::Background,
        ChildRelationshipPolicy::Bound {
            on_parent_stopped,
            on_parent_cancelled,
        } => RelationshipOutput::Bound {
            on_parent_stopped: action_output(on_parent_stopped),
            on_parent_cancelled: action_output(on_parent_cancelled),
        },
    }
}

const fn action_output(action: BoundChildAction) -> &'static str {
    match action {
        BoundChildAction::KeepRunning => "keep_running",
        BoundChildAction::Stop => "stop",
        BoundChildAction::Cancel => "cancel",
    }
}

fn encode_spawn_receipt<PortError>(
    receipt: SpawnSessionReceipt,
) -> Result<String, SessionDelegationExecutorError<PortError>> {
    encode_json(&SpawnReceiptOutput {
        result: "session_spawned",
        tool_request_id: receipt.tool_request().as_uuid().to_string(),
        child_session_id: receipt.child().as_uuid().to_string(),
        relationship: relationship_output(receipt.policy()),
    })
}

#[derive(serde::Serialize)]
struct AwaitReceiptOutput {
    result: &'static str,
    tool_request_id: String,
    child_session_id: String,
    mode: &'static str,
}

fn encode_await_receipt<PortError>(
    receipt: AwaitSessionReceipt,
) -> Result<String, SessionDelegationExecutorError<PortError>> {
    let mode = match receipt.mode() {
        DelegationWaitMode::Foreground => "foreground",
        DelegationWaitMode::Background => "background",
    };
    encode_json(&AwaitReceiptOutput {
        result: "session_await_registered",
        tool_request_id: receipt.tool_request().as_uuid().to_string(),
        child_session_id: receipt.child().as_uuid().to_string(),
        mode,
    })
}

#[derive(serde::Serialize)]
struct MessageReceiptOutput {
    result: &'static str,
    tool_request_id: String,
    message_id: String,
    direction: &'static str,
    ordinal: u64,
    delivery_sequence: u64,
}

fn encode_message_receipt<PortError>(
    receipt: SessionMessageReceipt,
) -> Result<String, SessionDelegationExecutorError<PortError>> {
    let direction = match receipt.direction() {
        DelegationMessageDirection::ParentToChild => "parent_to_child",
        DelegationMessageDirection::ChildToParent => "child_to_parent",
    };
    encode_json(&MessageReceiptOutput {
        result: "session_message_sent",
        tool_request_id: receipt.tool_request().as_uuid().to_string(),
        message_id: receipt.message().as_uuid().to_string(),
        direction,
        ordinal: receipt.ordinal().get(),
        delivery_sequence: receipt.delivery_sequence().get(),
    })
}

/// Renders delivered child content or a typed terminal outcome for scheduling.
///
/// A successful return is the child's exact delivered content with no JSON
/// envelope, preserving the full tool-result byte budget. Every non-content
/// terminal result is compact JSON retaining outcome, reason, and provenance.
pub fn render_delivered_child_result(
    result: DeliveredChildResult,
) -> Result<String, DeliveredChildResultRenderError> {
    if result.kind() == DelegationOutcomeKind::ResultReturned {
        let content = result.content().ok_or(DeliveredChildResultRenderError)?;
        let rendered = content.as_str().to_owned();
        ToolResultText::try_new(rendered.clone()).map_err(|_| DeliveredChildResultRenderError)?;
        return Ok(rendered);
    }
    match result.kind() {
        DelegationOutcomeKind::ChildFailed
        | DelegationOutcomeKind::ChildStopped
        | DelegationOutcomeKind::ChildCancelled => Ok(render_delegation_outcome(&result.outcome)),
        DelegationOutcomeKind::ResultReturned
        | DelegationOutcomeKind::AlreadyTerminal
        | DelegationOutcomeKind::ContinueRunning => Err(DeliveredChildResultRenderError),
    }
}

/// A typed delivered result could not fit the model-facing result contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeliveredChildResultRenderError;

impl fmt::Display for DeliveredChildResultRenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("delivered child result is not renderable")
    }
}

impl Error for DeliveredChildResultRenderError {}

fn encode_json<PortError>(
    value: &impl serde::Serialize,
) -> Result<String, SessionDelegationExecutorError<PortError>> {
    serde_json::to_string(value).map_err(|_| SessionDelegationExecutorError::ResultEncoding)
}

#[cfg(test)]
mod tests;
