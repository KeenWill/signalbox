//! Session-template identity, version, digest, and provenance values.
//!
//! The normative specification is `docs/spec/configuration-and-credentials.md`.

use core::fmt;

use sha2::{Digest, Sha256};

use crate::{
    AnthropicServiceTier, CodexCliServiceTier, DangerousToolAutoApproval, FastMode,
    FastModeOverlay, ModelSelectionRequest, ModelSettingsOverlay, OpenAiServiceTier,
    ReasoningLevel, ServiceTier, SessionConfigurationDefaults, SettingOverlay,
    ValidatedModelSettings,
};

/// One validated daemon-configured session-template name.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionTemplateName(String);

impl SessionTemplateName {
    /// The maximum admitted UTF-8 byte length.
    pub const MAX_UTF8_BYTES: usize = 128;

    /// Validates the closed lowercase ASCII template-name grammar.
    pub fn try_new(value: String) -> Result<Self, SessionTemplateNameError> {
        let first_is_admitted = value
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
        let failure = if value.is_empty() {
            Some(SessionTemplateNameFailure::Empty)
        } else if value.len() > Self::MAX_UTF8_BYTES {
            Some(SessionTemplateNameFailure::TooLong { bytes: value.len() })
        } else if !first_is_admitted {
            Some(SessionTemplateNameFailure::InvalidFirstByte)
        } else if value.bytes().any(|byte| {
            !byte.is_ascii_lowercase() && !byte.is_ascii_digit() && !b"._-".contains(&byte)
        }) {
            Some(SessionTemplateNameFailure::InvalidByte)
        } else {
            None
        };
        match failure {
            Some(failure) => Err(SessionTemplateNameError { value, failure }),
            None => Ok(Self(value)),
        }
    }

    /// Borrows the exact admitted name.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the exact admitted name.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Debug for SessionTemplateName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("SessionTemplateName")
            .field(&self.0)
            .finish()
    }
}

/// Why a session-template name was not admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionTemplateNameFailure {
    /// The name was empty.
    Empty,
    /// The name exceeded its byte bound.
    TooLong {
        /// The observed UTF-8 byte count.
        bytes: usize,
    },
    /// The first byte was not a lowercase ASCII letter or digit.
    InvalidFirstByte,
    /// A byte was outside lowercase ASCII letters, digits, dot, dash, and
    /// underscore.
    InvalidByte,
}

/// Failed template-name construction retaining the rejected value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionTemplateNameError {
    value: String,
    failure: SessionTemplateNameFailure,
}

impl SessionTemplateNameError {
    /// Borrows the rejected value.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Returns why the value was rejected.
    pub const fn failure(&self) -> SessionTemplateNameFailure {
        self.failure
    }

    /// Returns the rejected value and failure.
    pub fn into_parts(self) -> (String, SessionTemplateNameFailure) {
        (self.value, self.failure)
    }
}

impl fmt::Display for SessionTemplateNameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid session-template name: {:?}",
            self.failure
        )
    }
}

impl std::error::Error for SessionTemplateNameError {}

/// One positive operator-assigned template bundle version.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionTemplateVersion(u64);

impl SessionTemplateVersion {
    /// Constructs a version from its positive ordinal.
    pub const fn try_from_u64(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    /// Returns the positive ordinal.
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// The domain-separated SHA-256 digest of one resolved template bundle.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct SessionTemplateContentDigest([u8; 32]);

impl SessionTemplateContentDigest {
    /// Derives the digest from the version and exact copied defaults bundle.
    pub fn derive(
        version: SessionTemplateVersion,
        defaults: &SessionConfigurationDefaults,
    ) -> Option<Self> {
        let prompt = defaults.system_prompt()?;
        let mut digest = Sha256::new();
        update_frame(&mut digest, b"signalbox/session-template/content-digest/v2");
        update_frame(&mut digest, &version.as_u64().to_be_bytes());
        match defaults.model() {
            ModelSelectionRequest::Direct(selection) => {
                update_frame(&mut digest, b"direct");
                update_frame(&mut digest, selection.as_uuid().as_bytes());
            }
            ModelSelectionRequest::Alias(alias) => {
                update_frame(&mut digest, b"alias");
                update_frame(&mut digest, alias.as_uuid().as_bytes());
            }
        }
        let approval = match defaults.dangerous_tool_auto_approval() {
            DangerousToolAutoApproval::Disabled => b"disabled".as_slice(),
            DangerousToolAutoApproval::ApproveAll => b"approve_all".as_slice(),
        };
        update_frame(&mut digest, approval);
        update_frame(&mut digest, prompt.as_str().as_bytes());
        update_frame(
            &mut digest,
            &model_settings_digest(defaults.model_settings()),
        );
        Some(Self(digest.finalize().into()))
    }

    /// Reconstitutes an exact stored digest.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the exact digest bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for SessionTemplateContentDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionTemplateContentDigest([digest])")
    }
}

fn update_frame(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

fn model_settings_digest(settings: ValidatedModelSettings) -> [u8; 32] {
    let mut digest = Sha256::new();
    update_frame(&mut digest, b"signalbox/model-settings/snapshot-digest/v1");
    let precedence = settings.precedence();
    update_model_settings_layer(&mut digest, b"per_call", precedence.per_call());
    update_model_settings_layer(&mut digest, b"session", precedence.session());
    update_model_settings_layer(&mut digest, b"profile", precedence.profile());
    update_model_settings_layer(&mut digest, b"global_default", precedence.global_default());
    match settings.validated_for() {
        Some(selection) => {
            update_frame(&mut digest, b"validated_selection");
            update_frame(&mut digest, selection.as_uuid().as_bytes());
        }
        None => update_frame(&mut digest, b"unbound_provider_defaults"),
    }
    digest.finalize().into()
}

fn update_model_settings_layer(digest: &mut Sha256, name: &[u8], overlay: ModelSettingsOverlay) {
    update_frame(digest, name);
    update_frame(digest, reasoning_overlay_name(overlay.reasoning_level()));
    update_frame(digest, fast_mode_overlay_name(overlay.fast_mode()));
    update_frame(digest, service_tier_overlay_name(overlay.service_tier()));
}

const fn reasoning_overlay_name(value: SettingOverlay<ReasoningLevel>) -> &'static [u8] {
    match value {
        SettingOverlay::Inherit => b"inherit",
        SettingOverlay::ProviderDefault => b"provider_default",
        SettingOverlay::Value(ReasoningLevel::None) => b"none",
        SettingOverlay::Value(ReasoningLevel::Minimal) => b"minimal",
        SettingOverlay::Value(ReasoningLevel::Low) => b"low",
        SettingOverlay::Value(ReasoningLevel::Medium) => b"medium",
        SettingOverlay::Value(ReasoningLevel::High) => b"high",
        SettingOverlay::Value(ReasoningLevel::XHigh) => b"xhigh",
        SettingOverlay::Value(ReasoningLevel::Max) => b"max",
        SettingOverlay::Value(ReasoningLevel::Ultra) => b"ultra",
    }
}

const fn fast_mode_overlay_name(value: FastModeOverlay) -> &'static [u8] {
    match value {
        FastModeOverlay::Inherit => b"inherit",
        FastModeOverlay::Value(FastMode::Disabled) => b"disabled",
        FastModeOverlay::Value(FastMode::Enabled) => b"enabled",
    }
}

const fn service_tier_overlay_name(value: SettingOverlay<ServiceTier>) -> &'static [u8] {
    match value {
        SettingOverlay::Inherit => b"inherit",
        SettingOverlay::ProviderDefault => b"provider_default",
        SettingOverlay::Value(ServiceTier::Anthropic(AnthropicServiceTier::Auto)) => {
            b"anthropic:auto"
        }
        SettingOverlay::Value(ServiceTier::Anthropic(AnthropicServiceTier::StandardOnly)) => {
            b"anthropic:standard_only"
        }
        SettingOverlay::Value(ServiceTier::OpenAi(OpenAiServiceTier::Auto)) => b"openai:auto",
        SettingOverlay::Value(ServiceTier::OpenAi(OpenAiServiceTier::Default)) => b"openai:default",
        SettingOverlay::Value(ServiceTier::OpenAi(OpenAiServiceTier::Flex)) => b"openai:flex",
        SettingOverlay::Value(ServiceTier::OpenAi(OpenAiServiceTier::Scale)) => b"openai:scale",
        SettingOverlay::Value(ServiceTier::OpenAi(OpenAiServiceTier::Priority)) => {
            b"openai:priority"
        }
        SettingOverlay::Value(ServiceTier::OpenAi(OpenAiServiceTier::Fast)) => b"openai:fast",
        SettingOverlay::Value(ServiceTier::CodexCli(CodexCliServiceTier::Default)) => {
            b"codex_cli:default"
        }
        SettingOverlay::Value(ServiceTier::CodexCli(CodexCliServiceTier::Priority)) => {
            b"codex_cli:priority"
        }
        SettingOverlay::Value(ServiceTier::CodexCli(CodexCliServiceTier::Flex)) => {
            b"codex_cli:flex"
        }
    }
}

/// Immutable provenance for one template bundle copied at session creation.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SessionTemplateProvenance {
    name: SessionTemplateName,
    content_digest: SessionTemplateContentDigest,
}

impl SessionTemplateProvenance {
    /// Pairs the configured name with the exact copied-content digest.
    pub const fn new(
        name: SessionTemplateName,
        content_digest: SessionTemplateContentDigest,
    ) -> Self {
        Self {
            name,
            content_digest,
        }
    }

    /// Borrows the configured template name.
    pub const fn name(&self) -> &SessionTemplateName {
        &self.name
    }

    /// Returns the exact copied-content digest.
    pub const fn content_digest(&self) -> SessionTemplateContentDigest {
        self.content_digest
    }
}
