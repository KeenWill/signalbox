//! Tool policy for `docs/spec/tool-loop.md`.

use super::request::ToolRequest;
use crate::{DurableCommandId, ModelCallId, ToolRequestId};

/// The dangerous blanket-auto posture frozen into one turn.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DangerousToolAutoApproval {
    /// Registry defaults and fail-closed confirmation remain authoritative.
    Disabled,
    /// Every proposal is automatically approved under explicit blanket provenance.
    ApproveAll,
}

/// Registry permission behavior for one declared tool.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ToolPermissionDefault {
    /// Policy automatically approves the request.
    Auto,
    /// A user decision is required.
    Confirm,
    /// A user decision is required even under blanket automatic approval.
    AlwaysConfirm,
}

/// Deployment-selected approval authority for one exact tool.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ToolApprovalPosture {
    /// Policy may approve without a decision event.
    Auto,
    /// A delegate model may decide or escalate to the user.
    Delegated,
    /// Only the user may approve or deny.
    Human,
}

/// Crash-relevant physical effect classification.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ToolEffectClass {
    /// Crash loss is known not to have caused an external effect.
    EffectFree,
    /// Crash loss may have caused an externally visible effect.
    ExternalEffect,
}

/// Closed additive provenance for one approval decision.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ToolDecisionSource {
    /// An applied user-global durable command.
    UserCommand,
    /// Registry policy selected automatic approval.
    PolicyAuto,
    /// The frozen dangerous session blanket selected automatic approval.
    SessionBlanket,
    /// Reserved for a future exact per-tool session override.
    SessionOverride,
    /// A checked delegate-model decision.
    Delegate,
    /// Runtime controls denied suppressed arguments or an expired human wait.
    RuntimeSafety,
    /// A committed session closure denied a parked request before interrupting
    /// the live turn.
    LifecycleClosure,
    /// A user-recorded one-shot override of a delegate denial supplied approval
    /// when the session re-proposed the denied command.
    UserOverride,
}

impl ToolDecisionSource {
    pub(crate) const fn requires_ordered_prefix(self) -> bool {
        match self {
            Self::UserCommand | Self::Delegate => true,
            Self::PolicyAuto
            | Self::SessionBlanket
            | Self::SessionOverride
            | Self::RuntimeSafety
            | Self::LifecycleClosure
            | Self::UserOverride => false,
        }
    }
}

/// Who made one explicit approval decision.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ToolApprovalDecider {
    /// The user acted through the named durable command.
    User {
        /// Exact command provenance.
        command: DurableCommandId,
    },
    /// The named configured model acted through the recorded call.
    Delegate {
        /// Exact direct model selection used by the judge.
        model: crate::DirectModelSelection,
        /// Dedicated recorded judge call.
        call: ModelCallId,
    },
    /// The user pre-approved the re-proposed command by overriding one exact
    /// delegate denial through the named durable command.
    UserOverride {
        /// Exact override-command provenance.
        command: DurableCommandId,
        /// The delegate-denied request whose recorded override was consumed.
        denied_request: ToolRequestId,
    },
}

/// One checked delegate rationale retained verbatim.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ToolDecisionRationale(String);

impl ToolDecisionRationale {
    /// Maximum admitted UTF-8 byte length.
    pub const MAX_UTF8_BYTES: usize = 4096;

    /// Admits nonempty bounded text without U+0000.
    pub fn try_new(value: String) -> Result<Self, ToolDecisionRationaleError> {
        if value.is_empty() || value.len() > Self::MAX_UTF8_BYTES || value.contains('\0') {
            Err(ToolDecisionRationaleError { value })
        } else {
            Ok(Self(value))
        }
    }

    /// Borrows the exact rationale.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the exact rationale.
    pub fn into_string(self) -> String {
        self.0
    }
}

/// A delegate rationale was empty, oversized, or contained U+0000.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolDecisionRationaleError {
    value: String,
}

impl ToolDecisionRationaleError {
    /// Borrows the rejected value.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Returns the rejected value.
    pub fn into_value(self) -> String {
        self.value
    }
}

impl std::fmt::Display for ToolDecisionRationaleError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "tool decision rationale must be nonempty, at most {} bytes, and contain no U+0000",
            ToolDecisionRationale::MAX_UTF8_BYTES
        )
    }
}

impl std::error::Error for ToolDecisionRationaleError {}

/// Closed result vocabulary emitted by an approval judge.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DelegateApprovalRecommendation {
    /// Permit this exact request.
    Approve,
    /// Permanently deny this exact request.
    Deny,
    /// Leave the request parked for the user.
    EscalateToHuman,
}

/// One authority-checked delegate result with complete model provenance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegateToolApproval {
    pub(super) request: ToolRequestId,
    posture: ToolApprovalPosture,
    pub(super) model: crate::DirectModelSelection,
    pub(super) call: ModelCallId,
    pub(super) recommendation: DelegateApprovalRecommendation,
    pub(super) rationale: ToolDecisionRationale,
}

impl DelegateToolApproval {
    /// Checks the recommendation against the request's frozen posture.
    pub fn try_new(
        request: &ToolRequest,
        model: crate::DirectModelSelection,
        call: ModelCallId,
        recommendation: DelegateApprovalRecommendation,
        rationale: ToolDecisionRationale,
    ) -> Result<Self, DelegateToolApprovalError> {
        let permitted = match request.approval_posture() {
            ToolApprovalPosture::Delegated => true,
            ToolApprovalPosture::Human => {
                recommendation == DelegateApprovalRecommendation::EscalateToHuman
            }
            ToolApprovalPosture::Auto => false,
        };
        if !permitted {
            return Err(DelegateToolApprovalError {
                posture: request.approval_posture(),
                recommendation,
            });
        }
        Ok(Self {
            request: request.id(),
            posture: request.approval_posture(),
            model,
            call,
            recommendation,
            rationale,
        })
    }

    /// Returns the exact request judged.
    pub const fn request(&self) -> ToolRequestId {
        self.request
    }

    pub(crate) const fn posture(&self) -> ToolApprovalPosture {
        self.posture
    }

    /// Returns the direct model selection used by the judge.
    pub const fn model(&self) -> crate::DirectModelSelection {
        self.model
    }

    /// Returns the dedicated judge call.
    pub const fn call(&self) -> ModelCallId {
        self.call
    }

    /// Returns the checked recommendation.
    pub const fn recommendation(&self) -> DelegateApprovalRecommendation {
        self.recommendation
    }

    /// Borrows the exact judge rationale.
    pub const fn rationale(&self) -> &ToolDecisionRationale {
        &self.rationale
    }
}

/// A delegate recommendation exceeded the request's frozen authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DelegateToolApprovalError {
    posture: ToolApprovalPosture,
    recommendation: DelegateApprovalRecommendation,
}

impl DelegateToolApprovalError {
    /// Returns the frozen posture that rejected the recommendation.
    pub const fn posture(self) -> ToolApprovalPosture {
        self.posture
    }

    /// Returns the rejected recommendation.
    pub const fn recommendation(self) -> DelegateApprovalRecommendation {
        self.recommendation
    }
}

impl std::fmt::Display for DelegateToolApprovalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "delegate recommendation {:?} exceeds {:?} approval-posture authority",
            self.recommendation, self.posture
        )
    }
}

impl std::error::Error for DelegateToolApprovalError {}

/// One checked optional denial explanation.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ToolDenialReason(pub(super) String);

impl ToolDenialReason {
    /// Maximum admitted UTF-8 byte length.
    pub const MAX_UTF8_BYTES: usize = 1024;

    /// Checks length, surrounding POSIX whitespace, and control characters.
    pub fn try_new(value: String) -> Result<Self, ToolDenialReasonError> {
        let failure = if value.is_empty() {
            Some(ToolDenialReasonFailure::Empty)
        } else if value.len() > Self::MAX_UTF8_BYTES {
            Some(ToolDenialReasonFailure::TooLong { bytes: value.len() })
        } else if has_surrounding_posix_whitespace(&value) {
            Some(ToolDenialReasonFailure::SurroundingWhitespace)
        } else {
            value
                .chars()
                .any(char::is_control)
                .then_some(ToolDenialReasonFailure::ContainsControl)
        };
        match failure {
            Some(failure) => Err(ToolDenialReasonError { value, failure }),
            None => Ok(Self(value)),
        }
    }

    /// Borrows the exact checked reason.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the exact checked reason.
    pub fn into_string(self) -> String {
        self.0
    }

    /// Derives the deterministic denial reason carried by a delegate denial.
    ///
    /// A rationale admits control characters and up to
    /// [`ToolDecisionRationale::MAX_UTF8_BYTES`] bytes, so this conversion is
    /// lossy exactly where the two bounds disagree: control characters become
    /// spaces, edge characters the reason validator forbids are trimmed, and
    /// the text is cut to [`Self::MAX_UTF8_BYTES`] on a character boundary.
    /// After control mapping the only forbidden edge character left is the
    /// space itself, so admissible non-POSIX edge whitespace such as NBSP is
    /// preserved verbatim. A rationale that is entirely control characters
    /// and spaces derives no reason.
    pub fn from_rationale(rationale: &ToolDecisionRationale) -> Option<Self> {
        let sanitized = rationale
            .as_str()
            .chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .collect::<String>();
        let mut trimmed = sanitized.trim_matches(' ');
        while trimmed.len() > Self::MAX_UTF8_BYTES {
            let mut cut = Self::MAX_UTF8_BYTES;
            while !trimmed.is_char_boundary(cut) {
                cut -= 1;
            }
            trimmed = trimmed[..cut].trim_end_matches(' ');
        }
        (!trimmed.is_empty()).then(|| Self(String::from(trimmed)))
    }
}

/// Why a denial reason is unsafe or outside its bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolDenialReasonFailure {
    /// A present reason cannot be empty.
    Empty,
    /// The reason exceeds the admission bound.
    TooLong {
        /// The observed UTF-8 byte count.
        bytes: usize,
    },
    /// Leading or trailing POSIX whitespace was present.
    SurroundingWhitespace,
    /// At least one Unicode control scalar was present.
    ContainsControl,
}

/// Failed denial-reason construction retaining the rejected value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolDenialReasonError {
    value: String,
    failure: ToolDenialReasonFailure,
}

fn has_surrounding_posix_whitespace(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
        || value
            .as_bytes()
            .last()
            .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
}

impl ToolDenialReasonError {
    /// Borrows the rejected value.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Returns the validation failure.
    pub const fn failure(&self) -> ToolDenialReasonFailure {
        self.failure
    }

    /// Returns the rejected value and failure.
    pub fn into_parts(self) -> (String, ToolDenialReasonFailure) {
        (self.value, self.failure)
    }
}
