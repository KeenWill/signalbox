//! Session delegation request for `docs/spec/sessions-and-transcript.md`.

use super::content::{DelegationContent, DelegationContentError};
use super::vocabulary::{BoundChildAction, ChildRelationshipPolicy, DelegationWaitMode};
use crate::{SessionId, ToolRequest};

pub(super) const SPAWN_SESSION_TOOL_NAME: &str = "spawn_session";
pub(super) const AWAIT_SESSION_TOOL_NAME: &str = "await_session";
pub(super) const SEND_SESSION_MESSAGE_TOOL_NAME: &str = "send_session_message";

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
    pub(super) request: ToolRequest,
    pub(super) task: DelegationContent,
    pub(super) policy: ChildRelationshipPolicy,
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

pub(super) fn relationship_argument(policy: ChildRelationshipPolicy) -> serde_json::Value {
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

pub(super) const fn wait_mode_argument(mode: DelegationWaitMode) -> &'static str {
    match mode {
        DelegationWaitMode::Foreground => "foreground",
        DelegationWaitMode::Background => "background",
    }
}
