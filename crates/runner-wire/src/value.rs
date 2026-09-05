//! Checked scalar and closed-vocabulary runner-wire values.

use std::{error::Error, fmt, num::NonZeroU64};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use signalbox_domain::{
    CredentialProfileName, RunnerCapabilityClass, ToolName, WorkspaceRepositoryKey,
};
use uuid::Uuid;

/// One protocol or payload value failed closed validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ValueError {
    /// A UUID was not canonical lowercase hyphenated text.
    CanonicalUuid,
    /// A required positive integer was zero.
    PositiveInteger,
    /// A digest was not 64 lowercase hexadecimal bytes.
    Digest,
    /// A portable checked name was invalid for its domain role.
    PortableName,
    /// A sorted inventory was unordered, duplicated, or over its cap.
    Inventory,
    /// A terminal result bound differed from version one's fixed contract.
    ResultBounds,
    /// A result member violated its domain bound or vocabulary.
    Result,
    /// A structured failure detail violated its exact recursive bounds.
    FailureDetail,
    /// A message combined closed-union members illegally.
    Correlation,
}

impl fmt::Display for ValueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::CanonicalUuid => "UUID is not canonical lowercase hyphenated text",
            Self::PositiveInteger => "integer must be positive",
            Self::Digest => "digest must be 64 lowercase hexadecimal bytes",
            Self::PortableName => "portable name is invalid",
            Self::Inventory => "inventory is not sorted, unique, and within its cap",
            Self::ResultBounds => "result bounds differ from runner-wire version one",
            Self::Result => "terminal result is outside its closed domain shape",
            Self::FailureDetail => "failure detail is outside its recursive bounds",
            Self::Correlation => "correlation union is invalid",
        })
    }
}

impl Error for ValueError {}

/// A canonical lowercase hyphenated UUID on the runner boundary.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CanonicalUuid(Uuid);

impl CanonicalUuid {
    /// Wraps a typed domain identity's UUID.
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Returns the UUID for explicit domain mapping.
    pub const fn into_uuid(self) -> Uuid {
        self.0
    }

    fn parse(value: &str) -> Result<Self, ValueError> {
        let parsed = Uuid::parse_str(value).map_err(|_| ValueError::CanonicalUuid)?;
        if parsed.hyphenated().to_string() != value {
            return Err(ValueError::CanonicalUuid);
        }
        Ok(Self(parsed))
    }
}

impl fmt::Display for CanonicalUuid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.hyphenated().fmt(formatter)
    }
}

impl Serialize for CanonicalUuid {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for CanonicalUuid {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

/// A positive unsigned runner generation, revision, sequence, or page.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct PositiveU64(NonZeroU64);

impl PositiveU64 {
    /// Checks that the boundary integer is positive.
    pub const fn try_new(value: u64) -> Result<Self, ValueError> {
        match NonZeroU64::new(value) {
            Some(value) => Ok(Self(value)),
            None => Err(ValueError::PositiveInteger),
        }
    }

    /// Returns the checked integer.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl<'de> Deserialize<'de> for PositiveU64 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_new(u64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// A canonical lowercase SHA-256 hexadecimal digest.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Digest(String);

impl Digest {
    /// Checks the complete lowercase digest text.
    pub fn try_new(value: String) -> Result<Self, ValueError> {
        let mut decoded = [0_u8; 32];
        if hex::decode_to_slice(&value, &mut decoded).is_ok() && hex::encode(decoded) == value {
            Ok(Self(value))
        } else {
            Err(ValueError::Digest)
        }
    }

    /// Returns the canonical hexadecimal text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn from_sha256(bytes: [u8; 32]) -> Self {
        Self(hex::encode(bytes))
    }
}

impl Serialize for Digest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// A domain-validated portable capability-class name.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct CapabilityName(String);

impl CapabilityName {
    /// Checks and stores the domain spelling.
    pub fn try_new(value: String) -> Result<Self, ValueError> {
        RunnerCapabilityClass::try_new(value.clone())
            .map(|_| Self(value))
            .map_err(|_| ValueError::PortableName)
    }

    /// Returns the checked text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for CapabilityName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// A checked failure-detail name with the portable catalog-key grammar.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct DetailName(String);

impl DetailName {
    /// Checks the exact bounded catalog-key spelling.
    pub fn try_new(value: String) -> Result<Self, ValueError> {
        WorkspaceRepositoryKey::try_new(value.clone())
            .map(|_| Self(value))
            .map_err(|_| ValueError::PortableName)
    }

    /// Returns the checked text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for DetailName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}
/// A domain-validated tool name.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct WireToolName(String);

impl WireToolName {
    /// Checks and stores the domain spelling.
    pub fn try_new(value: String) -> Result<Self, ValueError> {
        ToolName::try_new(value.clone())
            .map(|_| Self(value))
            .map_err(|_| ValueError::PortableName)
    }

    /// Returns the checked text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for WireToolName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// A domain-validated credential-profile name.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ProfileName(String);

impl ProfileName {
    /// Checks and stores the domain spelling.
    pub fn try_new(value: String) -> Result<Self, ValueError> {
        CredentialProfileName::try_new(value.clone())
            .map(|_| Self(value))
            .map_err(|_| ValueError::PortableName)
    }

    /// Returns the checked text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ProfileName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// A domain-validated repository key.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct RepositoryKey(String);

impl RepositoryKey {
    /// Checks and stores the domain spelling.
    pub fn try_new(value: String) -> Result<Self, ValueError> {
        WorkspaceRepositoryKey::try_new(value.clone())
            .map(|_| Self(value))
            .map_err(|_| ValueError::PortableName)
    }

    /// Returns the checked text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for RepositoryKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Closed runner sandbox profiles.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxProfile {
    /// Supervision without filesystem confinement.
    Ambient,
    /// Access limited to the selected writable root.
    WorkspaceRestricted,
}

/// Closed runner workspace capabilities.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceCapability {
    /// One repository worktree per session placement.
    WorktreePerSession,
}

/// Closed three-way runner effect classes.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    /// Externally effect-free work.
    Pure,
    /// Repeatable external work.
    Idempotent,
    /// Work whose repetition is not known safe.
    SideEffecting,
}

/// Closed workspace-manifest lifecycle vocabulary.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestLifecycle {
    /// Prepared content is not published.
    Staging,
    /// Published content awaits daemon recording.
    Ready,
    /// The daemon durably recorded the workspace.
    Active,
    /// Release was accepted before trash rename.
    Releasing,
}

/// Closed runner tool execution errors.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionErrorKind {
    /// No checked tool matched.
    UnknownTool,
    /// Arguments were invalid.
    InvalidArguments,
    /// Execution definitively failed.
    ExecutionFailed,
    /// Successful content exceeded its bound.
    ResultTooLarge,
    /// Restart lost effect-free work.
    CrashLost,
}

/// Version-one fixed successful result UTF-8 bound.
pub const SUCCESS_TEXT_BYTES: u64 = 1_048_576;
/// Version-one fixed known-failure detail UTF-8 bound.
pub const FAILURE_DETAIL_BYTES: u64 = 4_096;

/// Exact fixed result bounds carried by every lease offer.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultBounds {
    /// Maximum successful UTF-8 result bytes.
    pub success_text_bytes: u64,
    /// Maximum known-failure detail UTF-8 bytes.
    pub failure_detail_bytes: u64,
}

impl ResultBounds {
    /// Returns the only admitted version-one pair.
    pub const fn version_one() -> Self {
        Self {
            success_text_bytes: SUCCESS_TEXT_BYTES,
            failure_detail_bytes: FAILURE_DETAIL_BYTES,
        }
    }

    /// Rejects attempted bound negotiation.
    pub const fn validate(self) -> Result<(), ValueError> {
        if self.success_text_bytes == SUCCESS_TEXT_BYTES
            && self.failure_detail_bytes == FAILURE_DETAIL_BYTES
        {
            Ok(())
        } else {
            Err(ValueError::ResultBounds)
        }
    }
}

/// Closed terminal evidence projected from the domain attempt end.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TerminalResult {
    /// Admitted successful text.
    Success {
        /// Exact result text.
        text: String,
    },
    /// Definitive typed failure.
    KnownFailure {
        /// Closed failure kind.
        error_kind: ExecutionErrorKind,
        /// Optional sanitized detail.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "crate::deserialize_present"
        )]
        detail: Option<String>,
    },
    /// External effect outcome cannot be established.
    Ambiguous,
}

struct UniqueObject(serde_json::Map<String, serde_json::Value>);

impl<'de> Deserialize<'de> for UniqueObject {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct UniqueObjectVisitor;

        impl<'de> serde::de::Visitor<'de> for UniqueObjectVisitor {
            type Value = UniqueObject;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an object with unique member names")
            }

            fn visit_map<A>(self, mut entries: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut object = serde_json::Map::new();
                while let Some((key, value)) = entries.next_entry()? {
                    if object.insert(key, value).is_some() {
                        return Err(serde::de::Error::custom(
                            "terminal result contains a duplicate member",
                        ));
                    }
                }
                Ok(UniqueObject(object))
            }
        }

        deserializer.deserialize_map(UniqueObjectVisitor)
    }
}

impl<'de> Deserialize<'de> for TerminalResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut object = UniqueObject::deserialize(deserializer)?.0;
        let kind = object
            .remove("kind")
            .and_then(|value| value.as_str().map(str::to_owned))
            .ok_or_else(|| serde::de::Error::custom("terminal result requires a string kind"))?;
        match kind.as_str() {
            "success" if object.len() == 1 => {
                let text = object
                    .remove("text")
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .ok_or_else(|| serde::de::Error::custom("success requires exact text"))?;
                Ok(Self::Success { text })
            }
            "known_failure" if object.len() == 1 || object.len() == 2 => {
                let error_kind = object
                    .remove("error_kind")
                    .ok_or_else(|| serde::de::Error::custom("known failure requires error_kind"))?;
                let error_kind = ExecutionErrorKind::deserialize(error_kind)
                    .map_err(serde::de::Error::custom)?;
                let detail = object
                    .remove("detail")
                    .map(|value| {
                        value
                            .as_str()
                            .map(str::to_owned)
                            .ok_or_else(|| serde::de::Error::custom("present detail must be text"))
                    })
                    .transpose()?;
                if object.is_empty() {
                    Ok(Self::KnownFailure { error_kind, detail })
                } else {
                    Err(serde::de::Error::custom(
                        "known failure has an unknown member",
                    ))
                }
            }
            "ambiguous" if object.is_empty() => Ok(Self::Ambiguous),
            "success" | "known_failure" | "ambiguous" => Err(serde::de::Error::custom(
                "terminal result members do not match its kind",
            )),
            _ => Err(serde::de::Error::custom("terminal result kind is unknown")),
        }
    }
}

impl TerminalResult {
    /// Enforces the domain result and error-detail bounds without rewriting.
    pub fn validate(&self) -> Result<(), ValueError> {
        match self {
            Self::Success { text }
                if text.len() <= SUCCESS_TEXT_BYTES as usize && !text.contains('\0') =>
            {
                Ok(())
            }
            Self::KnownFailure { detail: None, .. } => Ok(()),
            Self::KnownFailure {
                detail: Some(detail),
                ..
            } if !detail.is_empty()
                && detail.len() <= FAILURE_DETAIL_BYTES as usize
                && !has_posix_edge_whitespace(detail)
                && !detail.chars().any(char::is_control) =>
            {
                Ok(())
            }
            Self::Success { .. } | Self::KnownFailure { .. } => Err(ValueError::Result),
            Self::Ambiguous => Ok(()),
        }
    }
}

fn has_posix_edge_whitespace(value: &str) -> bool {
    let is_posix = |byte: &u8| matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r');
    value.as_bytes().first().is_some_and(is_posix) || value.as_bytes().last().is_some_and(is_posix)
}
