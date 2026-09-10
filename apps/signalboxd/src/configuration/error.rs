use super::model_routing::ModelAdapter;
use signalbox_domain::ModelSelectionRequest;
use std::{error::Error, fmt, sync::Arc};

/// Sanitized static-configuration failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HubModelConfigurationError {
    /// The configuration file could not be read as UTF-8 text.
    Read,
    /// The content was not a TOML document.
    InvalidDocument,
    /// The document version is absent or unsupported.
    UnsupportedVersion,
    /// One or more required deployment numeric-bound fields were absent.
    MissingNumericBounds {
        /// Every absent field, in schema order.
        fields: Vec<&'static str>,
    },
    /// One required deployment numeric bound had the wrong type or spelling.
    InvalidNumericBound {
        /// The rejected field.
        field: &'static str,
    },
    /// No nonempty model-definition array exists.
    MissingModels,
    /// No nonempty static adapter mapping table exists.
    MissingAdapterMappings,
    /// No nonempty credential-profile billing registry exists.
    MissingCredentialProfiles,
    /// One credential profile appeared more than once.
    DuplicateCredentialProfile {
        /// Exact repeated profile name.
        credential_profile: Arc<str>,
    },
    /// A credential profile declared no supported billing kind.
    InvalidBillingKind,
    /// A profile's `billing_kind` contradicts the authentication its delivery
    /// establishes.
    ///
    /// Both spellings are carried because a refusal naming only the profile
    /// leaves an operator to rediscover which of its two fields to edit.
    DisagreeingCredentialBillingKind {
        /// Exact profile name whose two fields disagree.
        credential_profile: Arc<str>,
        /// Exact delivery spelling that fixes the authentication kind.
        delivery: Arc<str>,
        /// Exact billing kind the profile declared alongside it.
        billing_kind: Arc<str>,
    },
    /// A credential profile named no delivery, or its delivery's own fields
    /// were absent or malformed.
    InvalidCredentialDelivery,
    /// Required GitHub delivery field is absent or invalid.
    InvalidGithubCredentialField {
        /// Exact field to correct.
        field: &'static str,
    },
    /// One member's Codex home failed path/directory admission.
    InvalidCredentialHome {
        /// Non-secret profile reference identifying the failed member.
        credential_profile: Arc<str>,
        /// Closed startup failure class; never path or auth material.
        failure: crate::credential_pools::CredentialHomeAdmissionFailure,
    },
    /// A credential profile named a delivery its adapter does not admit.
    UnsupportedCredentialDelivery {
        /// Build-provided adapter whose admitted deliveries were checked.
        adapter: ModelAdapter,
        /// Exact delivery spelling that adapter does not admit.
        delivery: Arc<str>,
    },
    /// A credential profile named a delivery the grammar admits but no surface
    /// in this build supplies.
    UndeliveredCredentialDelivery {
        /// Exact delivery spelling no composed surface honors.
        delivery: Arc<str>,
    },
    /// No nonempty credential pool array exists.
    MissingCredentialPools,
    /// One credential pool appeared more than once.
    DuplicateCredentialPool {
        /// Exact repeated pool name.
        credential_pool: Arc<str>,
    },
    /// A credential pool declared no members.
    EmptyCredentialPool {
        /// Exact pool name that admitted no member.
        credential_pool: Arc<str>,
    },
    /// One profile appeared twice among a pool's members.
    DuplicatePoolMember {
        /// Exact pool name carrying the repetition.
        credential_pool: Arc<str>,
        /// Exact repeated profile name.
        credential_profile: Arc<str>,
    },
    /// A pool member named no declared credential profile.
    UnknownPoolMemberProfile {
        /// Exact pool name carrying the member.
        credential_pool: Arc<str>,
        /// Exact profile spelling absent from the profile registry.
        credential_profile: Arc<str>,
    },
    /// An adapter mapping named no declared credential pool.
    UnknownCredentialPool {
        /// Exact family key whose mapping named the pool.
        model_family: Arc<str>,
        /// Exact pool spelling absent from the pool registry.
        credential_pool: Arc<str>,
    },
    /// A pool's members carried different adapters, or a mapping's adapter
    /// disagreed with its pool's.
    ConflictingPoolAdapters {
        /// Exact pool name carrying the disagreement.
        credential_pool: Arc<str>,
    },
    /// A pool member's priority was absent, zero, or outside `u32`.
    InvalidMemberPriority {
        /// Exact pool name carrying the member.
        credential_pool: Arc<str>,
    },
    /// A pool named no supported tie-break or exhaustion behavior.
    InvalidCredentialPoolPolicy,
    /// A pool trigger named no supported action.
    UnknownCredentialPoolAction,
    /// A pool trigger carried an action that cause does not admit.
    InadmissibleCredentialPoolAction {
        /// Exact trigger key whose configured action it does not admit.
        trigger: Arc<str>,
    },
    /// A headroom reserve was outside zero through ninety-nine percent.
    InvalidHeadroomReserve,
    /// A pool's selection depends on remaining capacity its adapter does not
    /// report, so the setting could never take effect.
    UnobservedCapacityPolicy {
        /// Exact pool name carrying the unobservable setting.
        credential_pool: Arc<str>,
    },
    /// The daemon tool mapping registry was incomplete or malformed.
    InvalidToolMappings,
    /// Mapped daemon tools were configured without the required Git identity.
    MissingGitIdentityConfiguration,
    /// The daemon Git identity table was malformed or unsafe.
    InvalidGitIdentityConfiguration,
    /// Mapped daemon tools were configured without their process settings.
    MissingDaemonToolSettings,
    /// The daemon tool process-settings table was malformed or unsafe.
    InvalidDaemonToolSettings,
    /// The tool approval wait settings table was malformed.
    InvalidToolSettings,
    /// The per-tool approval posture table was malformed.
    InvalidToolApprovalPostures,
    /// The approval-judge selection table was malformed.
    InvalidApprovalJudge,
    /// The configured approval judge names no direct model selection.
    DanglingApprovalJudgeSelection,
    /// One daemon tool family appeared more than once.
    DuplicateToolFamily,
    /// The required compaction configuration table is absent.
    MissingCompaction,
    /// An unrecognized root or table field was present.
    UnknownField,
    /// A required field had the wrong TOML type or was absent.
    InvalidField,
    /// A configured identity was not a UUID.
    InvalidIdentity,
    /// A mapping named no adapter implementation provided by this build.
    UnsupportedAdapter {
        /// Exact adapter spelling from the rejected mapping.
        adapter: Arc<str>,
    },
    /// One model family appeared more than once in the mapping table.
    DuplicateModelFamily {
        /// Exact repeated family key.
        model_family: Arc<str>,
    },
    /// A model named no entry in the static mapping table.
    UnmappedModelFamily {
        /// Exact family key absent from the table.
        model_family: Arc<str>,
    },
    /// One provider-native model spelling was routed to different adapters.
    ConflictingProviderModelRoute,
    /// A Codex mapping exists without its required process configuration.
    MissingCodexCliConfiguration,
    /// Codex paths were malformed, relative, or named no existing directory.
    InvalidCodexCliConfiguration,
    /// A Claude mapping exists without its required process configuration.
    MissingClaudeCliConfiguration,
    /// Claude paths were malformed, relative, or named no existing directory.
    InvalidClaudeCliConfiguration,
    /// The named Claude MCP bridge executable is on no absolute PATH entry.
    UnresolvedClaudeMcpBridgeExecutable,
    /// The provider-native model spelling was empty or padded.
    InvalidProviderModel,
    /// Only part of a model's five-field versioned rate set was declared.
    IncompleteBillingRates,
    /// A billing rate was not a bounded nonnegative decimal string.
    InvalidBillingRate,
    /// An output or context token limit was zero or outside `u32`.
    InvalidLimit,
    /// The compaction prompt was empty, oversized, or contained NUL.
    InvalidCompactionPrompt,
    /// The optional conversation-import byte bound was absent, zero, or invalid.
    InvalidConversationImportLimit,
    /// The optional blob-store registry or its routes were malformed.
    InvalidBlobStorageConfiguration,
    /// The optional web-fetch table was malformed or named an invalid origin.
    InvalidWebFetchPolicy,
    /// The optional version-one repository-watch section was malformed.
    InvalidRepositoryWatchConfiguration,
    /// The optional version-one workspace-instruction section was malformed.
    InvalidWorkspaceInstructionConfiguration,
    /// The convergence sweep names no loaded session template.
    UnknownConvergenceSweepTemplate {
        /// Exact missing template name.
        template: String,
    },
    /// One structured repository-watch rule failed closed validation.
    InvalidRepositoryWatchRule {
        /// Stable operator-assigned rule identity.
        rule: String,
        /// Safe domain or template-validation diagnostic.
        reason: String,
    },
    /// Two repository-watch entries normalized to the same repository.
    DuplicateWatchedRepository,
    /// Two signal-reviewer spellings normalized to the same login.
    DuplicateSignalReviewer,
    /// Two repository-watch polling credentials or webhook secrets resolve to
    /// the same file reference.
    DuplicateRepositoryWatchCredentialFile,
    /// Two webhook-enabled repositories named the same positive GitHub hook ID.
    DuplicateRepositoryWatchWebhookHookId,
    /// One direct selection appeared more than once.
    DuplicateSelection,
    /// One per-model settings capability record was malformed.
    InvalidModelCapabilities,
    /// A global, named-profile, or model-profile settings declaration was malformed.
    InvalidModelSettingsConfiguration,
    /// The model catalog exceeded the process-protocol capability bound.
    TooManyModels,
    /// One target was assigned conflicting runtime meanings.
    ConflictingTarget,
    /// The aliases field was not an array of tables.
    InvalidAliases,
    /// The deployment alias catalog exceeded the process-protocol bound.
    TooManyAliases,
    /// One alias appeared more than once.
    DuplicateAlias,
    /// An alias selected no configured direct model.
    DanglingAlias,
}

impl fmt::Display for HubModelConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Self::MissingNumericBounds { fields } = self {
            return write!(
                formatter,
                "model configuration is missing required numeric bounds: {}",
                fields.join(", ")
            );
        }
        if let Self::InvalidNumericBound { field } = self {
            return write!(
                formatter,
                "model configuration contains invalid numeric bound `{field}`"
            );
        }
        if let Self::InvalidRepositoryWatchRule { rule, reason } = self {
            return write!(
                formatter,
                "model configuration contains invalid repository-watch rule `{rule}`: {reason}"
            );
        }
        if let Self::UnknownConvergenceSweepTemplate { template } = self {
            return write!(
                formatter,
                "model configuration names unknown convergence template `{template}`"
            );
        }
        // Startup telemetry formats this value, so the failing member and the
        // closed admission cause must both survive. The path never appears, as
        // `configuration-and-credentials.md` requires.
        if let Self::InvalidCredentialHome {
            credential_profile,
            failure,
        } = self
        {
            return write!(
                formatter,
                "model configuration credential profile `{credential_profile}` names an unavailable Codex credential home: {}",
                failure.cause()
            );
        }
        formatter.write_str(match self {
            Self::Read => "model configuration file could not be read",
            Self::InvalidDocument => "model configuration is not valid TOML",
            Self::UnsupportedVersion => "model configuration version is unsupported",
            Self::MissingNumericBounds { .. } => {
                "model configuration is missing required numeric bounds"
            }
            Self::InvalidNumericBound { .. } => {
                "model configuration contains an invalid numeric bound"
            }
            Self::MissingModels => "model configuration has no model definitions",
            Self::MissingAdapterMappings => "model configuration has no adapter mappings",
            Self::InvalidToolApprovalPostures => {
                "model configuration contains invalid tool approval postures"
            }
            Self::InvalidApprovalJudge => {
                "model configuration contains invalid approval judge settings"
            }
            Self::DanglingApprovalJudgeSelection => {
                "model configuration contains a dangling approval judge selection"
            }
            Self::MissingCredentialProfiles => {
                "model configuration has no credential profile billing registry"
            }
            Self::DuplicateCredentialProfile { .. } => {
                "model configuration repeats a credential profile"
            }
            Self::InvalidBillingKind => {
                "model configuration contains an invalid credential billing kind"
            }
            Self::DisagreeingCredentialBillingKind { .. } => {
                "model configuration declares a billing kind its credential delivery cannot authenticate"
            }
            Self::InvalidGithubCredentialField { field } => return write!(formatter, "GitHub credential field `{field}` is missing or invalid"),
            Self::InvalidCredentialDelivery => {
                "model configuration contains an invalid credential delivery"
            }
            Self::InvalidCredentialHome { .. } => {
                "model configuration contains an unavailable Codex credential home"
            }
            Self::UnsupportedCredentialDelivery { .. } => {
                "model configuration names a credential delivery its adapter does not admit"
            }
            Self::UndeliveredCredentialDelivery { .. } => {
                "model configuration names a credential delivery this build does not supply"
            }
            Self::MissingCredentialPools => "model configuration has no credential pools",
            Self::DuplicateCredentialPool { .. } => "model configuration repeats a credential pool",
            Self::EmptyCredentialPool { .. } => {
                "model configuration contains a credential pool with no members"
            }
            Self::DuplicatePoolMember { .. } => {
                "model configuration repeats a credential pool member"
            }
            Self::UnknownPoolMemberProfile { .. } => {
                "model configuration pools an undeclared credential profile"
            }
            Self::UnknownCredentialPool { .. } => {
                "model configuration names an undeclared credential pool"
            }
            Self::ConflictingPoolAdapters { .. } => {
                "model configuration gives one credential pool conflicting adapters"
            }
            Self::InvalidMemberPriority { .. } => {
                "model configuration contains an invalid credential pool priority"
            }
            Self::InvalidCredentialPoolPolicy => {
                "model configuration contains an invalid credential pool policy"
            }
            Self::UnknownCredentialPoolAction => {
                "model configuration contains an unknown credential pool action"
            }
            Self::InadmissibleCredentialPoolAction { .. } => {
                "model configuration gives a credential pool trigger an inadmissible action"
            }
            Self::InvalidHeadroomReserve => {
                "model configuration contains an invalid headroom reserve"
            }
            Self::UnobservedCapacityPolicy { .. } => {
                "model configuration depends on provider capacity no adapter reports"
            }
            Self::InvalidToolMappings => {
                "model configuration contains invalid daemon tool mappings"
            }
            Self::MissingGitIdentityConfiguration => {
                "model configuration maps daemon tools without Git identity settings"
            }
            Self::InvalidGitIdentityConfiguration => {
                "model configuration contains invalid Git identity settings"
            }
            Self::MissingDaemonToolSettings => {
                "model configuration maps daemon tools without process settings"
            }
            Self::InvalidDaemonToolSettings => {
                "model configuration contains invalid daemon tool process settings"
            }
            Self::InvalidToolSettings => {
                "model configuration contains invalid tool approval wait settings"
            }
            Self::DuplicateToolFamily => "model configuration repeats a daemon tool family",
            Self::MissingCompaction => "model configuration has no compaction settings",
            Self::UnknownField => "model configuration contains an unknown field",
            Self::InvalidField => "model configuration has a missing or mistyped field",
            Self::InvalidIdentity => "model configuration contains an invalid identity",
            Self::UnsupportedAdapter { .. } => "model configuration names an unsupported adapter",
            Self::DuplicateModelFamily { .. } => {
                "model configuration repeats a model family mapping"
            }
            Self::UnmappedModelFamily { .. } => {
                "model configuration names an unmapped model family"
            }
            Self::ConflictingProviderModelRoute => {
                "model configuration routes one provider model to conflicting adapters"
            }
            Self::MissingCodexCliConfiguration => {
                "model configuration maps Codex CLI without Codex CLI settings"
            }
            Self::InvalidCodexCliConfiguration => {
                "model configuration contains invalid Codex CLI settings"
            }
            Self::MissingClaudeCliConfiguration => {
                "model configuration maps Claude CLI without Claude CLI settings"
            }
            Self::InvalidClaudeCliConfiguration => {
                "model configuration contains invalid Claude CLI settings"
            }
            Self::UnresolvedClaudeMcpBridgeExecutable => {
                "model configuration names an unresolvable Claude MCP bridge executable"
            }
            Self::InvalidProviderModel => "model configuration contains an invalid provider model",
            Self::IncompleteBillingRates => {
                "model configuration contains an incomplete model billing rate set"
            }
            Self::InvalidBillingRate => {
                "model configuration contains an invalid model billing rate"
            }
            Self::InvalidLimit => "model configuration contains an invalid token limit",
            Self::InvalidCompactionPrompt => {
                "model configuration contains an invalid compaction prompt"
            }
            Self::InvalidConversationImportLimit => {
                "model configuration contains an invalid conversation import byte limit"
            }
            Self::InvalidBlobStorageConfiguration => {
                "model configuration contains invalid blob-storage settings"
            }
            Self::InvalidWebFetchPolicy => {
                "model configuration contains an invalid web_fetch egress policy"
            }
            Self::InvalidRepositoryWatchConfiguration => {
                "model configuration contains invalid repository-watch settings"
            }
            Self::InvalidWorkspaceInstructionConfiguration => {
                "model configuration contains invalid workspace-instruction settings"
            }
            Self::UnknownConvergenceSweepTemplate { .. } => {
                "model configuration names an unknown convergence template"
            }
            Self::InvalidRepositoryWatchRule { .. } => {
                "model configuration contains an invalid repository-watch rule"
            }
            Self::DuplicateWatchedRepository => "model configuration repeats a watched repository",
            Self::DuplicateSignalReviewer => {
                "model configuration repeats a repository-watch signal reviewer"
            }
            Self::DuplicateRepositoryWatchCredentialFile => {
                "model configuration repeats a repository-watch credential-file reference"
            }
            Self::DuplicateRepositoryWatchWebhookHookId => {
                "model configuration repeats a repository-watch webhook hook ID"
            }
            Self::DuplicateSelection => "model configuration repeats a direct selection",
            Self::InvalidModelCapabilities => {
                "model configuration contains invalid model capabilities"
            }
            Self::InvalidModelSettingsConfiguration => {
                "model configuration contains invalid model settings layers"
            }
            Self::TooManyModels => "model configuration contains too many models",
            Self::ConflictingTarget => "model configuration gives one target conflicting meaning",
            Self::InvalidAliases => "model aliases are not an array of tables",
            Self::TooManyAliases => "model configuration contains too many aliases",
            Self::DuplicateAlias => "model configuration repeats an alias",
            Self::DanglingAlias => "model configuration contains a dangling alias",
        })
    }
}

impl Error for HubModelConfigurationError {}

/// Typed session-admission rejection for a model absent from the static table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnknownSessionModel {
    /// Exact model request that no configured entry serves.
    pub selection: ModelSelectionRequest,
}

impl fmt::Display for UnknownSessionModel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "session model is not configured: {:?}",
            self.selection
        )
    }
}

impl Error for UnknownSessionModel {}
