//! Runner credential grant for `docs/spec/runner-protocol.md`.

use super::catalog::{
    CredentialToolApproval, RunnerSandboxProfile, RunnerToolEffectClass,
    RunnerToolPermissionOverride, RunnerToolPermissionOverrides,
};
use super::enrollment::ValidatedRunnerRegistration;
use super::names::{CredentialProfileName, RunnerDomainError, RunnerGeneration};
use super::placement::{PinnedRunnerPlacement, RunnerCredentialGrantLineage};
use crate::{RunnerId, SessionId, ToolName};
use std::{collections::BTreeMap, collections::BTreeSet};

/// Active or terminally revoked credential grant.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CredentialProfileGrantState {
    /// The credential profile grant may authorize tool dispatch.
    Active,
    /// The credential profile grant is terminally revoked.
    Revoked,
}

/// Daemon grant snapshot for one runner-local profile.
#[derive(Debug, Eq, PartialEq)]
pub struct CredentialProfileGrant {
    pub(super) session: SessionId,
    pub(super) runner: RunnerId,
    pub(super) revision: RunnerGeneration,
    pub(super) profile: CredentialProfileName,
    pub(super) tools: BTreeSet<ToolName>,
    pub(super) approvals: BTreeMap<ToolName, CredentialToolApproval>,
    pub(super) state: CredentialProfileGrantState,
}

impl CredentialProfileGrant {
    fn matches_binding(
        &self,
        session: SessionId,
        runner: RunnerId,
        profile: &CredentialProfileName,
    ) -> bool {
        self.session == session && self.runner == runner && &self.profile == profile
    }

    pub(super) fn matches_selection(
        &self,
        session: SessionId,
        runner: RunnerId,
        profile: &CredentialProfileName,
    ) -> bool {
        self.state == CredentialProfileGrantState::Active
            && self.matches_binding(session, runner, profile)
    }

    /// Returns the credential grant lifecycle state.
    pub const fn state(&self) -> CredentialProfileGrantState {
        self.state
    }

    /// Returns the credential grant revision.
    pub const fn revision(&self) -> RunnerGeneration {
        self.revision
    }

    /// Returns the exact runner and revision that issued the grant.
    pub const fn lineage(&self) -> RunnerCredentialGrantLineage {
        RunnerCredentialGrantLineage {
            runner: self.runner,
            revision: self.revision,
        }
    }

    /// Returns the runner-local profile named by this grant.
    pub const fn profile(&self) -> &CredentialProfileName {
        &self.profile
    }

    /// Returns the owning session identity.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the runner identity.
    pub const fn runner(&self) -> RunnerId {
        self.runner
    }

    /// Iterates the tools authorized by this grant.
    pub fn tools(&self) -> impl Iterator<Item = &ToolName> {
        self.tools.iter()
    }

    /// Iterates the explicit tool approval overrides.
    pub fn approvals(&self) -> impl Iterator<Item = (&ToolName, CredentialToolApproval)> {
        self.approvals
            .iter()
            .map(|(tool, approval)| (tool, *approval))
    }

    fn reconstitution_facts(&self) -> CredentialProfileGrantReconstitutionInput {
        CredentialProfileGrantReconstitutionInput {
            session: self.session,
            runner: self.runner,
            revision: self.revision,
            profile: self.profile.clone(),
            tools: self.tools.clone(),
            approvals: self.approvals.clone(),
            state: self.state,
        }
    }

    pub(super) fn authorization_for(
        &self,
        session: SessionId,
        runner: RunnerId,
        profile: &CredentialProfileName,
        tool: &ToolName,
    ) -> Result<CredentialDispatchAuthorization, RunnerDomainError> {
        if self.state != CredentialProfileGrantState::Active {
            return Err(RunnerDomainError::GrantRevoked);
        }
        if self.session != session
            || self.runner != runner
            || &self.profile != profile
            || !self.tools.contains(tool)
        {
            return Err(RunnerDomainError::ToolUnavailable);
        }
        Ok(CredentialDispatchAuthorization {
            session: self.session,
            runner: self.runner,
            grant_revision: self.revision,
            profile: self.profile.clone(),
            tool: tool.clone(),
            approval: self.approvals[tool],
        })
    }

    pub(super) fn replace_for(
        self,
        registration: &ValidatedRunnerRegistration,
        profile: CredentialProfileName,
        tools: impl IntoIterator<Item = ToolName>,
        sandbox: RunnerSandboxProfile,
        permission_overrides: &RunnerToolPermissionOverrides,
    ) -> Result<CredentialProfileGrantReplacement, RunnerDomainError> {
        if self.state != CredentialProfileGrantState::Active {
            return Err(RunnerDomainError::InvalidState);
        }
        if self.runner != registration.runner {
            return Err(RunnerDomainError::CorrelationMismatch);
        }
        let revision = self
            .revision
            .checked_next()
            .ok_or(RunnerDomainError::GenerationExhausted)?;
        let replacement = build_grant(
            self.session,
            revision,
            registration,
            profile,
            tools,
            RunnerApprovalPolicy {
                sandbox,
                permission_overrides,
            },
            CredentialProfileGrantState::Active,
        )?;
        Ok(CredentialProfileGrantReplacement {
            change: CredentialProfileChange {
                session: self.session,
                prior_revision: self.revision,
                replacement_revision: revision,
                before_profile: self.profile,
                after_profile: replacement.profile.clone(),
                before_tools: self.tools,
                after_tools: replacement.tools.clone(),
            },
            grant: replacement,
        })
    }

    /// Transitions the value to its terminal revoked state.
    pub fn revoke(mut self) -> Result<Self, RunnerDomainError> {
        if self.state != CredentialProfileGrantState::Active {
            return Err(RunnerDomainError::InvalidState);
        }
        self.state = CredentialProfileGrantState::Revoked;
        Ok(self)
    }

    /// Reconstitutes a credential grant against its expected session and registration.
    pub fn reconstitute(
        input: CredentialProfileGrantReconstitutionInput,
        expected_session: SessionId,
        registration: &ValidatedRunnerRegistration,
        sandbox: RunnerSandboxProfile,
        permission_overrides: &RunnerToolPermissionOverrides,
    ) -> Result<Self, RunnerDomainError> {
        if input.session != expected_session {
            return Err(RunnerDomainError::CorruptStoredFacts);
        }
        let checked = build_grant(
            input.session,
            input.revision,
            registration,
            input.profile,
            input.tools,
            RunnerApprovalPolicy {
                sandbox,
                permission_overrides,
            },
            input.state,
        )?;
        if checked.runner == input.runner && checked.approvals == input.approvals {
            Ok(checked)
        } else {
            Err(RunnerDomainError::CorruptStoredFacts)
        }
    }
}

/// Complete credential-grant facts loaded from canonical storage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialProfileGrantReconstitutionInput {
    /// The owning session identity.
    pub session: SessionId,
    /// The runner that issued the stored grant.
    pub runner: RunnerId,
    /// The stored credential grant revision.
    pub revision: RunnerGeneration,
    /// The runner-local credential profile name.
    pub profile: CredentialProfileName,
    /// The exact tools authorized by the stored grant.
    pub tools: BTreeSet<ToolName>,
    /// The exact per-tool approval postures.
    pub approvals: BTreeMap<ToolName, CredentialToolApproval>,
    /// The stored domain state.
    pub state: CredentialProfileGrantState,
}

/// Complete before-and-after credential-grant facts from runner replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerCredentialGrantChange {
    /// The complete credential grant before replacement, if one existed.
    pub before: Option<CredentialProfileGrantReconstitutionInput>,
    /// The complete credential grant after replacement, if one exists.
    pub after: Option<CredentialProfileGrantReconstitutionInput>,
}

pub(super) fn successor_grant(
    session: SessionId,
    before: &PinnedRunnerPlacement,
    after: &PinnedRunnerPlacement,
    registration: &ValidatedRunnerRegistration,
    prior: Option<CredentialProfileGrant>,
) -> Result<
    (
        Option<CredentialProfileGrant>,
        Option<RunnerCredentialGrantChange>,
    ),
    RunnerDomainError,
> {
    let prior = match (
        before.credential_profile.as_ref(),
        before.grant_lineage,
        prior,
    ) {
        (Some(profile), Some(lineage), Some(prior))
            if prior.matches_binding(session, before.runner, profile)
                && prior.lineage() == lineage =>
        {
            Some(prior)
        }
        (None, Some(lineage), Some(prior))
            if prior.session == session
                && prior.state == CredentialProfileGrantState::Revoked
                && prior.lineage() == lineage =>
        {
            Some(prior)
        }
        (None, None, None) => None,
        _ => return Err(RunnerDomainError::CorrelationMismatch),
    };
    let before_facts = prior
        .as_ref()
        .map(CredentialProfileGrant::reconstitution_facts);
    let grant = match prior {
        None => after
            .credential_profile
            .clone()
            .map(|profile| {
                build_grant(
                    session,
                    RunnerGeneration::one(),
                    registration,
                    profile,
                    registration.tool_names().cloned(),
                    RunnerApprovalPolicy {
                        sandbox: after.sandbox,
                        permission_overrides: &after.permission_overrides,
                    },
                    CredentialProfileGrantState::Active,
                )
            })
            .transpose()?,
        Some(prior) => {
            let revision = prior
                .revision
                .checked_next()
                .ok_or(RunnerDomainError::GenerationExhausted)?;
            match after.credential_profile.clone() {
                Some(profile) => Some(build_grant(
                    session,
                    revision,
                    registration,
                    profile,
                    registration.tool_names().cloned(),
                    RunnerApprovalPolicy {
                        sandbox: after.sandbox,
                        permission_overrides: &after.permission_overrides,
                    },
                    CredentialProfileGrantState::Active,
                )?),
                None => Some(CredentialProfileGrant {
                    revision,
                    state: CredentialProfileGrantState::Revoked,
                    ..prior
                }),
            }
        }
    };
    let after_facts = grant
        .as_ref()
        .map(CredentialProfileGrant::reconstitution_facts);
    let grant_change =
        (before_facts.is_some() || after_facts.is_some()).then_some(RunnerCredentialGrantChange {
            before: before_facts,
            after: after_facts,
        });
    Ok((grant, grant_change))
}

pub(super) fn resolve_runner_approval(
    effect: RunnerToolEffectClass,
    sandbox: RunnerSandboxProfile,
    permission_overrides: &RunnerToolPermissionOverrides,
    tool: &ToolName,
) -> CredentialToolApproval {
    match permission_overrides.get(tool) {
        Some(RunnerToolPermissionOverride::Auto) => CredentialToolApproval::Automatic,
        Some(RunnerToolPermissionOverride::Confirm) => CredentialToolApproval::SessionPolicy,
        None if sandbox == RunnerSandboxProfile::WorkspaceRestricted => {
            CredentialToolApproval::Automatic
        }
        None if effect == RunnerToolEffectClass::Pure => CredentialToolApproval::Automatic,
        None => CredentialToolApproval::SessionPolicy,
    }
}

pub(super) struct RunnerApprovalPolicy<'a> {
    pub(super) sandbox: RunnerSandboxProfile,
    pub(super) permission_overrides: &'a RunnerToolPermissionOverrides,
}

pub(super) fn build_grant(
    session: SessionId,
    revision: RunnerGeneration,
    registration: &ValidatedRunnerRegistration,
    profile: CredentialProfileName,
    tools: impl IntoIterator<Item = ToolName>,
    policy: RunnerApprovalPolicy<'_>,
    state: CredentialProfileGrantState,
) -> Result<CredentialProfileGrant, RunnerDomainError> {
    registration
        .profile(&profile)
        .ok_or(RunnerDomainError::CredentialProfileUnavailable)?;
    let tools: BTreeSet<_> = tools.into_iter().collect();
    if tools.iter().any(|tool| registration.tool(tool).is_none()) {
        return Err(RunnerDomainError::ToolUnavailable);
    }
    let approvals = tools
        .iter()
        .map(|tool| {
            let declaration = &registration.tools[tool];
            (
                tool.clone(),
                resolve_runner_approval(
                    declaration.effect,
                    policy.sandbox,
                    policy.permission_overrides,
                    tool,
                ),
            )
        })
        .collect();
    Ok(CredentialProfileGrant {
        session,
        runner: registration.runner,
        revision,
        profile,
        tools,
        approvals,
        state,
    })
}

/// Exact future-dispatch authority resolved from a tool/profile pair.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialDispatchAuthorization {
    /// The owning session identity.
    pub session: SessionId,
    /// The runner authorized to execute the dispatch.
    pub runner: RunnerId,
    /// The credential grant revision authorizing dispatch.
    pub grant_revision: RunnerGeneration,
    /// The runner-local credential profile name.
    pub profile: CredentialProfileName,
    /// The exact tool name.
    pub tool: ToolName,
    /// The approval posture applied to the tool dispatch.
    pub approval: CredentialToolApproval,
}

/// Successful forward-only credential grant replacement.
#[derive(Debug, Eq, PartialEq)]
pub struct CredentialProfileGrantReplacement {
    /// The resulting credential grant, when the selection requires one.
    pub grant: CredentialProfileGrant,
    /// The complete before-and-after change facts.
    pub change: CredentialProfileChange,
}

/// Complete before-and-after profile and tool facts for grant replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialProfileChange {
    /// The owning session identity.
    pub session: SessionId,
    /// The placement or grant revision before replacement.
    pub prior_revision: RunnerGeneration,
    /// The placement or grant revision after replacement.
    pub replacement_revision: RunnerGeneration,
    /// The credential profile before replacement.
    pub before_profile: CredentialProfileName,
    /// The credential profile after replacement.
    pub after_profile: CredentialProfileName,
    /// The granted tool set before replacement.
    pub before_tools: BTreeSet<ToolName>,
    /// The granted tool set after replacement.
    pub after_tools: BTreeSet<ToolName>,
}
