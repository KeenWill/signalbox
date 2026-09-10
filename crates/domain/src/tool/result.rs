//! Tool result for `docs/spec/tool-loop.md`.

use crate::{ToolAttemptId, ToolRequestId};

pub(super) const MAX_TOOL_RESULT_TEXT_BYTES: usize = 1024 * 1024;

/// The implemented result-content algebra for one terminal tool attempt.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ToolResultContent {
    /// Exact bounded UTF-8 text, including the empty value.
    Text(ToolResultText),
    /// Bounded descriptive text and authenticated image evidence.
    Media {
        /// Model-visible summary subject to the ordinary context text bound.
        text: ToolResultText,
        /// Immutable image validation evidence retained with this attempt.
        reference: crate::ToolMediaReference,
    },
}

/// Exact bounded tool-result text.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ToolResultText(String);

impl ToolResultText {
    /// Checks the admission bound and rejects U+0000 without rewriting.
    pub fn try_new(value: String) -> Result<Self, ToolResultTextError> {
        let failure = if value.len() > MAX_TOOL_RESULT_TEXT_BYTES {
            Some(ToolResultTextFailure::TooLarge { bytes: value.len() })
        } else if value.contains('\0') {
            Some(ToolResultTextFailure::ContainsNull)
        } else {
            None
        };
        match failure {
            Some(failure) => Err(ToolResultTextError { value, failure }),
            None => Ok(Self(value)),
        }
    }

    /// Borrows exact admitted text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns exact admitted text.
    pub fn into_string(self) -> String {
        self.0
    }
}

/// Why tool-result text was not admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolResultTextFailure {
    /// The text exceeded the result bound.
    TooLarge {
        /// The observed UTF-8 byte count.
        bytes: usize,
    },
    /// The text contained U+0000.
    ContainsNull,
}

/// Failed result-text construction retaining the rejected value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolResultTextError {
    value: String,
    failure: ToolResultTextFailure,
}

impl ToolResultTextError {
    /// Borrows the rejected text.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Returns the admission failure.
    pub const fn failure(&self) -> ToolResultTextFailure {
        self.failure
    }

    /// Returns the rejected text and failure.
    pub fn into_parts(self) -> (String, ToolResultTextFailure) {
        (self.value, self.failure)
    }
}

/// One durable logical resolution referenced by semantic history.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ToolRequestResolution {
    /// Execution evidence lives on the exact attempt.
    Executed {
        /// The terminal physical attempt.
        attempt: ToolAttemptId,
    },
    /// Approval evidence lives on the request-bound decision.
    Denied {
        /// The denied logical request.
        request: ToolRequestId,
    },
    /// The request could not be admitted before dispatch.
    ClosedInadmissible {
        /// The closed logical request.
        request: ToolRequestId,
    },
    /// The turn ended while the request remained undecided.
    ClosedByTurnEnd {
        /// The closed logical request.
        request: ToolRequestId,
    },
}

/// The closed reason for a request resolved before dispatch.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ToolInadmissibleReason {
    /// The proposal followed the configured number admitted from this response.
    ProposalLimitExceeded {
        /// Maximum admitted proposals in this response.
        limit: u64,
    },
    /// The provider argument payload exceeded its configured byte limit.
    ArgumentBytesExceeded {
        /// Maximum admitted argument bytes.
        limit: u64,
        /// Observed argument bytes before the preview was bounded.
        bytes: u64,
    },
    /// The runner placement was lost before any lease offer or executor dispatch.
    PlacementLost,
}
