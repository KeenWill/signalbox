use super::{
    DaemonToolsConstructionError,
    session_workspace_roots::{
        SESSION_WORKSPACE_COMPOSITION_DETAIL, SESSION_WORKSPACE_OBJECT_FORMAT_DETAIL,
        SESSION_WORKSPACE_REPLACED_DETAIL, SESSION_WORKSPACE_SHARED_DETAIL,
        SESSION_WORKSPACE_UNRESOLVABLE_DETAIL, SESSION_WORKSPACE_UNVERIFIABLE_CONFIGURED_DETAIL,
    },
};
use signalbox_domain::ToolExecutionErrorDetail;

/// Why one session's workspace-bound tools could not be composed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SessionWorkspaceFailure {
    /// The derived root, its repository layout, or its supervisor binding was
    /// rejected by the family that binds it.
    Composition(DaemonToolsConstructionError),
    /// The derived repository selects another object identifier format than the
    /// one the process-lifetime catalog compiled its Git validators against.
    ObjectFormatDisagreement,
    /// The derived path could not be classified, is not a directory, or has
    /// gone away under a session that already bound it.
    UnresolvableRoot,
    /// The derived root is the same directory as the configured root or as
    /// another session's, so binding it would defeat the isolation the
    /// derivation exists to establish.
    SharedRootIdentity,
    /// A different directory now stands at the pathname this session bound.
    ReplacedRootIdentity,
    /// The configured root's own directories could not be captured, so whether
    /// this session's root is one of them could not be decided.
    UnverifiableConfiguredRoot,
}

impl SessionWorkspaceFailure {
    /// Names the failure for startup-free runtime telemetry.
    pub(super) const fn discriminant(self) -> &'static str {
        match self {
            Self::Composition(_) => "composition_rejected",
            Self::ObjectFormatDisagreement => "object_format_disagreement",
            Self::UnresolvableRoot => "derived_root_unresolvable",
            Self::SharedRootIdentity => "derived_root_shared",
            Self::ReplacedRootIdentity => "derived_root_replaced",
            Self::UnverifiableConfiguredRoot => "configured_root_unverifiable",
        }
    }
}

/// Sanitized details naming why a session's workspace-bound tools are
/// unavailable.
///
/// The reason travels in the tool result rather than in a second operator
/// event: the tool loop already emits one failed-attempt event at its single
/// admission site, and a closed discriminant in the durable result is better
/// provenance than a log line beside it. Each value is a fixed string naming a
/// closed reason, so nothing about the deployment's paths reaches the model.
#[derive(Clone, Debug)]
pub(super) struct SessionWorkspaceFailureDetails {
    composition: ToolExecutionErrorDetail,
    object_format: ToolExecutionErrorDetail,
    unresolvable_root: ToolExecutionErrorDetail,
    shared_root: ToolExecutionErrorDetail,
    replaced_root: ToolExecutionErrorDetail,
    unverifiable_configured_root: ToolExecutionErrorDetail,
}

impl SessionWorkspaceFailureDetails {
    pub(super) fn try_new() -> Result<Self, DaemonToolsConstructionError> {
        let detail = |value: &str| {
            ToolExecutionErrorDetail::try_new(value.to_owned())
                .map_err(|_| DaemonToolsConstructionError::SessionWorkspaceDetail)
        };
        Ok(Self {
            composition: detail(SESSION_WORKSPACE_COMPOSITION_DETAIL)?,
            object_format: detail(SESSION_WORKSPACE_OBJECT_FORMAT_DETAIL)?,
            unresolvable_root: detail(SESSION_WORKSPACE_UNRESOLVABLE_DETAIL)?,
            shared_root: detail(SESSION_WORKSPACE_SHARED_DETAIL)?,
            replaced_root: detail(SESSION_WORKSPACE_REPLACED_DETAIL)?,
            unverifiable_configured_root: detail(SESSION_WORKSPACE_UNVERIFIABLE_CONFIGURED_DETAIL)?,
        })
    }

    /// Names the closed reason one failure carries into the tool result.
    pub(super) fn detail(&self, failure: SessionWorkspaceFailure) -> ToolExecutionErrorDetail {
        match failure {
            SessionWorkspaceFailure::Composition(_) => self.composition.clone(),
            SessionWorkspaceFailure::ObjectFormatDisagreement => self.object_format.clone(),
            SessionWorkspaceFailure::UnresolvableRoot => self.unresolvable_root.clone(),
            SessionWorkspaceFailure::SharedRootIdentity => self.shared_root.clone(),
            SessionWorkspaceFailure::ReplacedRootIdentity => self.replaced_root.clone(),
            SessionWorkspaceFailure::UnverifiableConfiguredRoot => {
                self.unverifiable_configured_root.clone()
            }
        }
    }
}
