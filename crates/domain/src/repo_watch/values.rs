//! Repository watch values for `docs/spec/repo-watch.md`.

use regex::Regex;
use std::{error::Error, fmt, hash::Hash, hash::Hasher, num::NonZeroU64};

const MAX_REPOSITORY_BYTES: usize = 201;
const MAX_BRANCH_BYTES: usize = 255;
const MAX_LOGIN_BASE_BYTES: usize = 39;
const BOT_LOGIN_SUFFIX: &str = "[bot]";
const MAX_LOGIN_BYTES: usize = MAX_LOGIN_BASE_BYTES + BOT_LOGIN_SUFFIX.len();
const MAX_LABEL_BYTES: usize = 200;
const MAX_LABEL_CHARACTERS: usize = 50;
const MAX_NAME_BYTES: usize = 256;
const MAX_REACTION_BYTES: usize = 64;
const MAX_RULE_ID_BYTES: usize = 128;
const MAX_PATTERN_BYTES: usize = 1_024;
const MAX_TITLE_BYTES: usize = 1_024;
const MAX_BODY_BYTES: usize = 262_144;

/// Why one repository-watch text value was refused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchTextError {
    Empty,
    ContainsNull,
    TooLong { bytes: usize, maximum: usize },
    TooManyCharacters { characters: usize, maximum: usize },
    Malformed,
    UnanchoredPattern,
    InvalidPattern { reason: String },
}

impl fmt::Display for RepoWatchTextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("repository-watch value is empty"),
            Self::ContainsNull => formatter.write_str("repository-watch value contains U+0000"),
            Self::TooLong { bytes, maximum } => write!(
                formatter,
                "repository-watch value has {bytes} bytes; maximum is {maximum}"
            ),
            Self::TooManyCharacters {
                characters,
                maximum,
            } => write!(
                formatter,
                "repository-watch value has {characters} characters; maximum is {maximum}"
            ),
            Self::Malformed => formatter.write_str("repository-watch value has an invalid shape"),
            Self::UnanchoredPattern => {
                formatter.write_str("repository-watch regex must be anchored with ^ and $")
            }
            Self::InvalidPattern { reason } => {
                write!(formatter, "repository-watch regex is invalid: {reason}")
            }
        }
    }
}

impl Error for RepoWatchTextError {}

fn validate_text(value: &str, maximum: usize) -> Result<(), RepoWatchTextError> {
    if value.is_empty() {
        Err(RepoWatchTextError::Empty)
    } else if value.contains('\0') {
        Err(RepoWatchTextError::ContainsNull)
    } else if value.len() > maximum {
        Err(RepoWatchTextError::TooLong {
            bytes: value.len(),
            maximum,
        })
    } else {
        Ok(())
    }
}

macro_rules! bounded_text {
    ($(#[$meta:meta])* $name:ident, $maximum:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            pub fn try_new(value: String) -> Result<Self, RepoWatchTextError> {
                validate_text(&value, $maximum)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_string(self) -> String {
                self.0
            }
        }
    };
}

/// A GitHub repository in canonical `namespace/name` spelling.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RepositorySlug(String);

impl RepositorySlug {
    pub fn try_new(mut value: String) -> Result<Self, RepoWatchTextError> {
        validate_text(&value, MAX_REPOSITORY_BYTES)?;
        let mut parts = value.split('/');
        let namespace = parts.next().unwrap_or_default();
        let repository = parts.next().unwrap_or_default();
        if !valid_repository_segment(namespace)
            || !valid_repository_segment(repository)
            || parts.next().is_some()
        {
            return Err(RepoWatchTextError::Malformed);
        }
        value.make_ascii_lowercase();
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

fn valid_repository_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// One exact repository branch name admitted by Git's ref-name grammar.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BranchName(String);

impl BranchName {
    pub fn try_new(mut value: String) -> Result<Self, RepoWatchTextError> {
        if let Some(name) = value.strip_prefix("refs/heads/") {
            value = name.to_owned();
        }
        validate_text(&value, MAX_BRANCH_BYTES)?;
        let invalid_component = value.split('/').any(|component| {
            component.is_empty() || component.starts_with('.') || component.ends_with(".lock")
        });
        if value == "@"
            || value.starts_with('-')
            || value.ends_with('.')
            || value.contains("..")
            || value.contains("@{")
            || value.bytes().any(|byte| {
                byte <= 0x20
                    || byte == 0x7f
                    || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
            })
            || invalid_component
        {
            return Err(RepoWatchTextError::Malformed);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}
/// One exact repository label name.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LabelName(String);

impl LabelName {
    pub fn try_new(value: String) -> Result<Self, RepoWatchTextError> {
        validate_text(&value, MAX_LABEL_BYTES)?;
        let characters = value.chars().count();
        if characters > MAX_LABEL_CHARACTERS {
            return Err(RepoWatchTextError::TooManyCharacters {
                characters,
                maximum: MAX_LABEL_CHARACTERS,
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}
/// One checked GitHub human, managed-user, or App-bot actor login.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RepoWatchAuthorLogin(String);

impl RepoWatchAuthorLogin {
    pub fn try_new(mut value: String) -> Result<Self, RepoWatchTextError> {
        validate_text(&value, MAX_LOGIN_BYTES)?;
        value.make_ascii_lowercase();
        let base = value.strip_suffix(BOT_LOGIN_SUFFIX).unwrap_or(&value);
        let valid = !base.is_empty()
            && base.len() <= MAX_LOGIN_BASE_BYTES
            && !base.starts_with('-')
            && !base.ends_with('-')
            && !base.contains("--")
            && base
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
        if !valid {
            return Err(RepoWatchTextError::Malformed);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}
bounded_text!(/// One check-run name.
    CheckRunName, MAX_NAME_BYTES);
bounded_text!(/// One workflow name.
    WorkflowName, MAX_NAME_BYTES);
bounded_text!(/// One reaction content spelling retained as event evidence.
    ReactionContent, MAX_REACTION_BYTES);
/// One stable operator-defined rule name.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RepoWatchRuleId(String);

impl RepoWatchRuleId {
    pub fn try_new(value: String) -> Result<Self, RepoWatchTextError> {
        validate_text(&value, MAX_RULE_ID_BYTES)?;
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(RepoWatchTextError::Malformed);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}
bounded_text!(/// One stable provider review-thread identifier.
    ReviewThreadId, MAX_NAME_BYTES);
bounded_text!(/// One exact pull-request title.
    PullRequestTitle, MAX_TITLE_BYTES);

/// One possibly empty, bounded pull-request body.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PullRequestBody(String);

impl PullRequestBody {
    pub fn try_new(value: String) -> Result<Self, RepoWatchTextError> {
        if value.contains('\0') {
            Err(RepoWatchTextError::ContainsNull)
        } else if value.len() > MAX_BODY_BYTES {
            Err(RepoWatchTextError::TooLong {
                bytes: value.len(),
                maximum: MAX_BODY_BYTES,
            })
        } else {
            Ok(Self(value))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

/// One exact Git commit object identifier.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CommitSha(String);

impl CommitSha {
    pub fn try_new(value: String) -> Result<Self, RepoWatchTextError> {
        if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(RepoWatchTextError::Malformed);
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

/// One positive pull-request number.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PullRequestNumber(NonZeroU64);

impl PullRequestNumber {
    pub const fn new(value: NonZeroU64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// One positive provider object identifier.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GitHubObjectId(NonZeroU64);

impl GitHubObjectId {
    pub const fn new(value: NonZeroU64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// One positive GitHub Actions attempt number within a workflow run.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RepoWatchWorkflowRunAttempt(NonZeroU64);

impl RepoWatchWorkflowRunAttempt {
    pub const fn new(value: NonZeroU64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// One bounded, anchored, linear-time regular expression.
#[derive(Clone)]
pub struct RepoWatchPattern {
    source: String,
    compiled: Regex,
}

impl fmt::Debug for RepoWatchPattern {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("RepoWatchPattern")
            .field(&self.source)
            .finish()
    }
}

impl PartialEq for RepoWatchPattern {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
    }
}

impl Eq for RepoWatchPattern {}

impl Hash for RepoWatchPattern {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.source.hash(state);
    }
}

impl RepoWatchPattern {
    pub const MAX_UTF8_BYTES: usize = MAX_PATTERN_BYTES;

    pub fn try_new(value: String) -> Result<Self, RepoWatchTextError> {
        validate_text(&value, MAX_PATTERN_BYTES)?;
        if !value.starts_with('^') || !value.ends_with('$') {
            return Err(RepoWatchTextError::UnanchoredPattern);
        }
        let compiled = Regex::new(&format!(r"\A(?:{value})\z")).map_err(|error| {
            RepoWatchTextError::InvalidPattern {
                reason: error.to_string(),
            }
        })?;
        Ok(Self {
            source: value,
            compiled,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.source
    }

    pub fn is_match(&self, candidate: &str) -> bool {
        self.compiled.is_match(candidate)
    }
}

/// Version of one durable rule shape.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RepoWatchRuleVersion(NonZeroU64);

impl RepoWatchRuleVersion {
    pub const V1: Self = Self(NonZeroU64::MIN);

    /// The revision, or `None` beyond the durable signed 64-bit range.
    ///
    /// Storage records a revision as a signed 64-bit integer, so a larger
    /// value has no durable representation. Refusing it here keeps every
    /// constructed revision persistable, instead of admitting a rule whose
    /// reconciliation would later report caller input as storage corruption.
    pub const fn new(value: NonZeroU64) -> Option<Self> {
        if value.get() > i64::MAX.unsigned_abs() {
            return None;
        }
        Some(Self(value))
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }
}
