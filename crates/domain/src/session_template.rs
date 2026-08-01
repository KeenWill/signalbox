//! Session-template identity, version, digest, and provenance values.
//!
//! The normative specification is `docs/spec/configuration-and-credentials.md`.

use core::fmt;

use sha2::{Digest, Sha256};

use crate::{DangerousToolAutoApproval, ModelSelectionRequest, SessionConfigurationDefaults};

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
        update_frame(&mut digest, b"signalbox/session-template/content-digest/v1");
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
