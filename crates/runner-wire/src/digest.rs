//! Domain-separated runner digest preimages and checked digest payloads.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use signalbox_domain::{
    CredentialProfileName, ProvisionedWorkspace, RunnerAdvertisement, RunnerCapabilityClass,
    RunnerRepositoryEntry, RunnerSandboxProfile, ToolName,
    WorkspaceCapability as DomainWorkspaceCapability, WorkspaceRecovery, WorkspaceRelativePath,
    WorkspaceRepositoryKey, WorkspaceRevision,
};

use crate::value::{
    BranchName, CanonicalUuid, CapabilityName, Digest, ManifestLifecycle, ProfileName,
    RepositoryKey, SandboxProfile, ValueError, WireToolName, WorkspaceCapability,
};

/// The sole digest encoding version carried by enrollment frames.
pub const DIGEST_VERSION: u64 = 1;
/// Maximum advertised capability classes.
pub const MAX_CAPABILITY_CLASSES: usize = 16;
/// Maximum advertised tools.
pub const MAX_TOOLS: usize = 256;
/// Maximum advertised workspace capabilities; the closed vocabulary has one variant.
pub const MAX_WORKSPACE_CAPABILITIES: usize = 1;
/// Maximum advertised sandbox profiles; the closed vocabulary has two variants.
pub const MAX_SANDBOX_PROFILES: usize = 2;
/// Maximum advertised credential profiles.
pub const MAX_PROFILES: usize = 64;
/// Maximum advertised repository entries.
pub const MAX_REPOSITORIES: usize = RunnerAdvertisement::MAX_REPOSITORIES;
/// Maximum facts retained in one leak page.
pub const MAX_LEAK_PAGE_FACTS: usize = 64;

/// One repository key and its exact optional configured credential requirement.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryEntry {
    /// Checked repository key.
    pub key: RepositoryKey,
    /// Required profile; absence explicitly advertises anonymous HTTPS.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::deserialize_present"
    )]
    pub credential_profile: Option<ProfileName>,
}

/// One complete sorted availability-only runner advertisement.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Advertisement {
    /// Sorted unique capability classes.
    pub capability_classes: Vec<CapabilityName>,
    /// Sorted unique tool names.
    pub tools: Vec<WireToolName>,
    /// Sorted unique workspace capabilities.
    pub workspace_capabilities: Vec<WorkspaceCapability>,
    /// Sorted unique sandbox profiles.
    pub sandbox_profiles: Vec<SandboxProfile>,
    /// Sorted unique credential-profile names.
    pub credential_profiles: Vec<ProfileName>,
    /// Sorted unique repository entries by key.
    pub repositories: Vec<RepositoryEntry>,
}

impl Advertisement {
    /// Checks inventory caps, order, uniqueness, and repository/profile pairing.
    pub fn validate(&self) -> Result<(), ValueError> {
        validate_inventory(&self.capability_classes, MAX_CAPABILITY_CLASSES)?;
        validate_inventory(&self.tools, MAX_TOOLS)?;
        validate_inventory(&self.workspace_capabilities, MAX_WORKSPACE_CAPABILITIES)?;
        validate_inventory(&self.sandbox_profiles, MAX_SANDBOX_PROFILES)?;
        validate_inventory(&self.credential_profiles, MAX_PROFILES)?;
        if self.repositories.len() > MAX_REPOSITORIES
            || self
                .repositories
                .windows(2)
                .any(|pair| pair[0].key >= pair[1].key)
        {
            return Err(ValueError::Inventory);
        }
        if self.repositories.iter().any(|entry| {
            entry
                .credential_profile
                .as_ref()
                .is_some_and(|profile| self.credential_profiles.binary_search(profile).is_err())
        }) {
            return Err(ValueError::Inventory);
        }
        Ok(())
    }

    /// Converts checked wire inventories into the domain advertisement authority.
    pub fn try_into_domain(self) -> Result<RunnerAdvertisement, ValueError> {
        self.validate()?;
        let classes = self
            .capability_classes
            .into_iter()
            .map(|value| RunnerCapabilityClass::try_new(value.as_str().to_owned()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| ValueError::PortableName)?;
        let tools = self
            .tools
            .into_iter()
            .map(|value| ToolName::try_new(value.as_str().to_owned()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| ValueError::PortableName)?;
        let profiles = self
            .credential_profiles
            .into_iter()
            .map(|value| CredentialProfileName::try_new(value.as_str().to_owned()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| ValueError::PortableName)?;
        let workspaces = self
            .workspace_capabilities
            .into_iter()
            .map(|value| match value {
                WorkspaceCapability::WorktreePerSession => {
                    DomainWorkspaceCapability::WorktreePerSession
                }
            });
        let sandboxes = self.sandbox_profiles.into_iter().map(|value| match value {
            SandboxProfile::Ambient => RunnerSandboxProfile::Ambient,
            SandboxProfile::WorkspaceRestricted => RunnerSandboxProfile::WorkspaceRestricted,
        });
        let repositories = self
            .repositories
            .into_iter()
            .map(|entry| {
                let key = WorkspaceRepositoryKey::try_new(entry.key.as_str().to_owned())
                    .map_err(|_| ValueError::PortableName)?;
                let profile = entry
                    .credential_profile
                    .map(|profile| CredentialProfileName::try_new(profile.as_str().to_owned()))
                    .transpose()
                    .map_err(|_| ValueError::PortableName)?;
                Ok(RunnerRepositoryEntry::new(key, profile))
            })
            .collect::<Result<Vec<_>, ValueError>>()?;
        Ok(RunnerAdvertisement::new(
            classes,
            tools,
            profiles,
            workspaces,
            sandboxes,
            repositories,
        ))
    }
}

impl TryFrom<&RunnerAdvertisement> for Advertisement {
    type Error = ValueError;

    fn try_from(value: &RunnerAdvertisement) -> Result<Self, Self::Error> {
        let advertisement = Self {
            capability_classes: value
                .classes()
                .map(|value| CapabilityName::try_new(value.as_str().to_owned()))
                .collect::<Result<_, _>>()?,
            tools: value
                .tools()
                .map(|value| WireToolName::try_new(value.as_str().to_owned()))
                .collect::<Result<_, _>>()?,
            workspace_capabilities: value
                .workspaces()
                .map(|value| match value {
                    DomainWorkspaceCapability::WorktreePerSession => {
                        WorkspaceCapability::WorktreePerSession
                    }
                })
                .collect(),
            sandbox_profiles: value
                .sandboxes()
                .map(|value| match value {
                    RunnerSandboxProfile::Ambient => SandboxProfile::Ambient,
                    RunnerSandboxProfile::WorkspaceRestricted => {
                        SandboxProfile::WorkspaceRestricted
                    }
                })
                .collect(),
            credential_profiles: value
                .profiles()
                .map(|value| ProfileName::try_new(value.as_str().to_owned()))
                .collect::<Result<_, _>>()?,
            repositories: value
                .repositories()
                .map(|entry| {
                    Ok(RepositoryEntry {
                        key: RepositoryKey::try_new(entry.key().as_str().to_owned())?,
                        credential_profile: entry
                            .credential_profile()
                            .map(|profile| ProfileName::try_new(profile.as_str().to_owned()))
                            .transpose()?,
                    })
                })
                .collect::<Result<_, ValueError>>()?,
        };
        advertisement.validate()?;
        Ok(advertisement)
    }
}

/// Closed recoverable Git state in a workspace manifest.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Recovery {
    /// Detached exact commit.
    Commit {
        /// Full lowercase Git object identity.
        revision: String,
    },
    /// Validated branch at an exact object identity.
    Branch {
        /// Name without `refs/heads/`.
        name: BranchName,
        /// Full lowercase Git object identity.
        revision: String,
    },
    /// Validated branch whose first commit has not yet been born.
    UnbornBranch {
        /// Name without `refs/heads/`.
        name: BranchName,
    },
}

impl Recovery {
    fn validate(&self) -> Result<(), ValueError> {
        match self {
            Self::Commit { revision } => WorkspaceRevision::try_new(revision.clone())
                .map(|_| ())
                .map_err(|_| ValueError::Result),
            Self::Branch { name, revision } => {
                WorkspaceRevision::try_new(revision.clone()).map_err(|_| ValueError::Result)?;
                Ok(())
            }
            Self::UnbornBranch { .. } => Ok(()),
        }
    }
}

/// Complete workspace-manifest digest input, excluding its resulting identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceManifest {
    /// Monotonic manifest lifecycle.
    pub lifecycle: ManifestLifecycle,
    /// Stable manifest identity across lifecycle changes.
    pub manifest_id: CanonicalUuid,
    /// Owning session.
    pub session: CanonicalUuid,
    /// Positive placement revision.
    pub placement_revision: crate::value::PositiveU64,
    /// Cleanup-owning runner.
    pub runner: CanonicalUuid,
    /// Repository key, absent for a private root.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::deserialize_present"
    )]
    pub repository: Option<RepositoryKey>,
    /// Canonical clone-URL digest, absent for a private root.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::deserialize_present"
    )]
    pub canonical_clone_url_digest: Option<Digest>,
    /// Independently optional credential-profile name.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::deserialize_present"
    )]
    pub credential_profile: Option<ProfileName>,
    /// Exact sandbox profile.
    pub sandbox_profile: SandboxProfile,
    /// Runner-root-relative workspace path.
    pub relative_path: String,
    /// Recovery facts, absent for a private root.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::deserialize_present"
    )]
    pub recovery: Option<Recovery>,
}

impl WorkspaceManifest {
    /// Checks the repository/private-root cross-shape and all checked domain values.
    pub fn validate(&self) -> Result<(), ValueError> {
        WorkspaceRelativePath::try_new(self.relative_path.clone())
            .map_err(|_| ValueError::Result)?;
        let repository_bound = (
            self.repository.is_some(),
            self.canonical_clone_url_digest.is_some(),
            self.recovery.is_some(),
        );
        if !matches!(repository_bound, (true, true, true) | (false, false, false)) {
            return Err(ValueError::Correlation);
        }
        if self.repository.is_none() && self.credential_profile.is_some() {
            return Err(ValueError::Correlation);
        }
        if let Some(recovery) = &self.recovery {
            recovery.validate()?;
        }
        Ok(())
    }

    /// Projects provisioned domain facts with the caller's exact lifecycle.
    pub fn from_domain(
        lifecycle: ManifestLifecycle,
        workspace: &ProvisionedWorkspace,
    ) -> Result<Self, ValueError> {
        let recovery = workspace.recovery.as_ref().map(|value| match value {
            WorkspaceRecovery::Commit { revision } => Recovery::Commit {
                revision: revision.as_str().to_owned(),
            },
            WorkspaceRecovery::Branch { name, revision } => Recovery::Branch {
                name: BranchName::try_new(name.as_str().to_owned())?,
                revision: revision.as_str().to_owned(),
            },
            WorkspaceRecovery::UnbornBranch { name } => Recovery::UnbornBranch {
                name: BranchName::try_new(name.as_str().to_owned())?,
            },
        });
        let manifest = Self {
            lifecycle,
            manifest_id: CanonicalUuid::from_uuid(workspace.manifest_id.into_uuid()),
            session: CanonicalUuid::from_uuid(workspace.session.into_uuid()),
            placement_revision: crate::value::PositiveU64::try_new(
                workspace.placement_revision.get(),
            )?,
            runner: CanonicalUuid::from_uuid(workspace.runner.into_uuid()),
            repository: workspace
                .repository
                .as_ref()
                .map(|value| RepositoryKey::try_new(value.as_str().to_owned()))
                .transpose()?,
            canonical_clone_url_digest: workspace
                .canonical_clone_url_digest
                .as_ref()
                .map(|value| Digest::try_new(value.as_str().to_owned()))
                .transpose()?,
            credential_profile: workspace
                .credential_profile
                .as_ref()
                .map(|value| ProfileName::try_new(value.as_str().to_owned()))
                .transpose()?,
            sandbox_profile: match workspace.sandbox {
                RunnerSandboxProfile::Ambient => SandboxProfile::Ambient,
                RunnerSandboxProfile::WorkspaceRestricted => SandboxProfile::WorkspaceRestricted,
            },
            relative_path: workspace.relative_path.as_str().to_owned(),
            recovery,
        };
        manifest.validate()?;
        Ok(manifest)
    }
}

/// Closed startup workspace-leak fact kinds.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeakFactKind {
    /// No protected manifest is known.
    UnknownManifest,
    /// Retired workspace remains present.
    RetiredPresent,
    /// Protected and observed facts disagree.
    ManifestConflict,
    /// Cleanup failed.
    CleanupFailed,
    /// Startup reconciliation remains incomplete.
    Unreconciled,
}

/// One sorted workspace leak fact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeakFact {
    /// Closed classification.
    pub kind: LeakFactKind,
    /// Runner-root-relative locator.
    pub locator: String,
    /// Manifest or entry digest.
    pub entry_digest: Digest,
    /// Session when known.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::deserialize_present"
    )]
    pub session: Option<CanonicalUuid>,
    /// Placement revision when known.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::deserialize_present"
    )]
    pub placement_revision: Option<crate::value::PositiveU64>,
}

impl Ord for LeakFact {
    fn cmp(&self, other: &Self) -> Ordering {
        compare_leak_facts(self, other)
    }
}

impl PartialOrd for LeakFact {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl LeakFact {
    /// Checks the relative locator.
    pub fn validate(&self) -> Result<(), ValueError> {
        WorkspaceRelativePath::try_new(self.locator.clone())
            .map(|_| ())
            .map_err(|_| ValueError::Result)
    }
}

/// Computes the canonical advertisement digest.
pub fn advertisement_digest(value: &Advertisement) -> Result<Digest, ValueError> {
    value.validate()?;
    let mut fields = Encoder::new("advertisement");
    fields.inventory(value.capability_classes.iter().map(CapabilityName::as_str));
    fields.inventory(value.tools.iter().map(WireToolName::as_str));
    fields.inventory(value.workspace_capabilities.iter().map(workspace_token));
    fields.inventory(value.sandbox_profiles.iter().map(sandbox_token));
    fields.inventory(value.credential_profiles.iter().map(ProfileName::as_str));
    let records = value
        .repositories
        .iter()
        .map(repository_record)
        .collect::<Vec<_>>();
    fields.inventory_bytes(records.iter().map(Vec::as_slice));
    Ok(fields.finish())
}

/// Computes the canonical validated clone-URL digest.
pub fn clone_url_digest(canonical_url: &str) -> Digest {
    let mut fields = Encoder::new("clone-url");
    fields.field(canonical_url.as_bytes());
    fields.finish()
}

/// Computes the canonical workspace-manifest digest.
pub fn workspace_manifest_digest(value: &WorkspaceManifest) -> Result<Digest, ValueError> {
    value.validate()?;
    let mut fields = Encoder::new("workspace-manifest");
    fields.field(manifest_lifecycle_token(value.lifecycle).as_bytes());
    fields.field(value.manifest_id.to_string().as_bytes());
    fields.field(value.session.to_string().as_bytes());
    fields.field(&value.placement_revision.get().to_be_bytes());
    fields.field(value.runner.to_string().as_bytes());
    fields.optional(value.repository.as_ref().map(|v| v.as_str().as_bytes()));
    fields.optional(
        value
            .canonical_clone_url_digest
            .as_ref()
            .map(|v| v.as_str().as_bytes()),
    );
    fields.optional(
        value
            .credential_profile
            .as_ref()
            .map(|v| v.as_str().as_bytes()),
    );
    fields.field(sandbox_token(&value.sandbox_profile).as_bytes());
    fields.field(value.relative_path.as_bytes());
    let recovery = value.recovery.as_ref().map(recovery_record);
    fields.optional(recovery.as_deref());
    Ok(fields.finish())
}

/// Computes a digest over a complete sorted leak report.
pub fn leak_report_digest(facts: &[LeakFact]) -> Result<Digest, ValueError> {
    validate_leak_facts(facts)?;
    facts.iter().try_for_each(LeakFact::validate)?;
    let records = facts.iter().map(leak_record).collect::<Vec<_>>();
    let mut fields = Encoder::new("leak-report");
    fields.inventory_bytes(records.iter().map(Vec::as_slice));
    Ok(fields.finish())
}

/// Labeled exact input to one leak-page digest.
#[derive(Clone, Copy, Debug)]
pub struct LeakPageDigestInput<'a> {
    /// Active registration revision.
    pub registration_revision: crate::value::PositiveU64,
    /// Complete report digest.
    pub report_digest: &'a Digest,
    /// Positive page number.
    pub page: crate::value::PositiveU64,
    /// Prior page digest, absent exactly on page one.
    pub prior_page_digest: Option<&'a Digest>,
    /// Whether this page completes the report.
    pub final_page: bool,
    /// Canonically sorted page facts.
    pub facts: &'a [LeakFact],
}

/// Computes a digest over one exact leak page.
pub fn leak_page_digest(input: LeakPageDigestInput<'_>) -> Result<Digest, ValueError> {
    let LeakPageDigestInput {
        registration_revision,
        report_digest,
        page,
        prior_page_digest,
        final_page,
        facts,
    } = input;
    if facts.len() > MAX_LEAK_PAGE_FACTS || (!final_page && facts.len() != MAX_LEAK_PAGE_FACTS) {
        return Err(ValueError::Inventory);
    }
    validate_leak_facts(facts)?;
    facts.iter().try_for_each(LeakFact::validate)?;
    let records = facts.iter().map(leak_record).collect::<Vec<_>>();
    let mut fields = Encoder::new("leak-page");
    fields.field(&registration_revision.get().to_be_bytes());
    fields.field(report_digest.as_str().as_bytes());
    fields.field(&page.get().to_be_bytes());
    fields.optional(prior_page_digest.map(|value| value.as_str().as_bytes()));
    fields.field(&[u8::from(final_page)]);
    fields.inventory_bytes(records.iter().map(Vec::as_slice));
    Ok(fields.finish())
}

fn validate_leak_facts(facts: &[LeakFact]) -> Result<(), ValueError> {
    if facts
        .windows(2)
        .any(|pair| compare_leak_facts(&pair[0], &pair[1]) != Ordering::Less)
    {
        Err(ValueError::Inventory)
    } else {
        Ok(())
    }
}

fn compare_leak_facts(left: &LeakFact, right: &LeakFact) -> Ordering {
    left.locator
        .as_bytes()
        .cmp(right.locator.as_bytes())
        .then_with(|| left.kind.cmp(&right.kind))
        .then_with(|| {
            left.entry_digest
                .as_str()
                .as_bytes()
                .cmp(right.entry_digest.as_str().as_bytes())
        })
        .then_with(|| compare_optional_uuid(left.session, right.session))
        .then_with(|| {
            left.placement_revision
                .map(crate::value::PositiveU64::get)
                .cmp(&right.placement_revision.map(crate::value::PositiveU64::get))
        })
}

fn compare_optional_uuid(left: Option<CanonicalUuid>, right: Option<CanonicalUuid>) -> Ordering {
    match (left, right) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(left), Some(right)) => left
            .to_string()
            .as_bytes()
            .cmp(right.to_string().as_bytes()),
    }
}

fn validate_inventory<T: Ord>(values: &[T], max: usize) -> Result<(), ValueError> {
    if values.len() > max || values.windows(2).any(|pair| pair[0] >= pair[1]) {
        Err(ValueError::Inventory)
    } else {
        Ok(())
    }
}

fn workspace_token(value: &WorkspaceCapability) -> &'static str {
    match value {
        WorkspaceCapability::WorktreePerSession => "worktree_per_session",
    }
}

fn sandbox_token(value: &SandboxProfile) -> &'static str {
    match value {
        SandboxProfile::Ambient => "ambient",
        SandboxProfile::WorkspaceRestricted => "workspace_restricted",
    }
}

fn manifest_lifecycle_token(value: ManifestLifecycle) -> &'static str {
    match value {
        ManifestLifecycle::Staging => "staging",
        ManifestLifecycle::Ready => "ready",
        ManifestLifecycle::Active => "active",
        ManifestLifecycle::Releasing => "releasing",
    }
}

fn repository_record(value: &RepositoryEntry) -> Vec<u8> {
    let mut record = Vec::new();
    push_field(&mut record, value.key.as_str().as_bytes());
    push_optional(
        &mut record,
        value
            .credential_profile
            .as_ref()
            .map(|profile| profile.as_str().as_bytes()),
    );
    record
}

fn recovery_record(value: &Recovery) -> Vec<u8> {
    let mut record = Vec::new();
    match value {
        Recovery::Commit { revision } => {
            push_field(&mut record, b"commit");
            push_field(&mut record, revision.as_bytes());
        }
        Recovery::Branch { name, revision } => {
            push_field(&mut record, b"branch");
            push_field(&mut record, name.as_str().as_bytes());
            push_field(&mut record, revision.as_bytes());
        }
        Recovery::UnbornBranch { name } => {
            push_field(&mut record, b"unborn_branch");
            push_field(&mut record, name.as_str().as_bytes());
        }
    }
    record
}

fn leak_record(value: &LeakFact) -> Vec<u8> {
    let mut record = Vec::new();
    let kind = match value.kind {
        LeakFactKind::UnknownManifest => "unknown_manifest",
        LeakFactKind::RetiredPresent => "retired_present",
        LeakFactKind::ManifestConflict => "manifest_conflict",
        LeakFactKind::CleanupFailed => "cleanup_failed",
        LeakFactKind::Unreconciled => "unreconciled",
    };
    push_field(&mut record, kind.as_bytes());
    push_field(&mut record, value.locator.as_bytes());
    push_field(&mut record, value.entry_digest.as_str().as_bytes());
    let session = value.session.map(|session| session.to_string());
    push_optional(&mut record, session.as_deref().map(str::as_bytes));
    let placement = value
        .placement_revision
        .map(|revision| revision.get().to_be_bytes());
    push_optional(&mut record, placement.as_ref().map(<[u8; 8]>::as_slice));
    record
}

struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    fn new(kind: &str) -> Self {
        Self {
            bytes: format!("sbx-digest-v1:{kind}:").into_bytes(),
        }
    }

    fn field(&mut self, value: &[u8]) {
        push_field(&mut self.bytes, value);
    }

    fn optional(&mut self, value: Option<&[u8]>) {
        push_optional(&mut self.bytes, value);
    }

    fn inventory<'a>(&mut self, values: impl IntoIterator<Item = &'a str>) {
        self.inventory_bytes(values.into_iter().map(str::as_bytes));
    }

    fn inventory_bytes<'a>(&mut self, values: impl IntoIterator<Item = &'a [u8]>) {
        let values = values.into_iter().collect::<Vec<_>>();
        let mut inventory = Vec::new();
        inventory.extend_from_slice(&(values.len() as u64).to_be_bytes());
        values
            .into_iter()
            .for_each(|value| push_field(&mut inventory, value));
        self.field(&inventory);
    }

    fn finish(self) -> Digest {
        let bytes: [u8; 32] = Sha256::digest(self.bytes).into();
        Digest::from_sha256(bytes)
    }
}

fn push_field(target: &mut Vec<u8>, value: &[u8]) {
    target.extend_from_slice(&(value.len() as u64).to_be_bytes());
    target.extend_from_slice(value);
}

fn push_optional(target: &mut Vec<u8>, value: Option<&[u8]>) {
    match value {
        Some(value) => {
            target.push(1);
            push_field(target, value);
        }
        None => target.push(0),
    }
}
