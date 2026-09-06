//! Tool name for `docs/spec/tool-loop.md`.

const MAX_TOOL_NAME_BYTES: usize = 64;

/// One checked model-facing tool name.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ToolName(String);

impl ToolName {
    /// Checks the closed baseline spelling without rewriting it.
    pub fn try_new(value: String) -> Result<Self, ToolNameError> {
        let failure = if value.is_empty() {
            Some(ToolNameFailure::Empty)
        } else if value.len() > MAX_TOOL_NAME_BYTES {
            Some(ToolNameFailure::TooLong { bytes: value.len() })
        } else {
            value
                .char_indices()
                .find(|(_, character)| {
                    !(character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
                })
                .map(
                    |(byte_index, character)| ToolNameFailure::InvalidCharacter {
                        byte_index,
                        character,
                    },
                )
        };

        match failure {
            Some(failure) => Err(ToolNameError { value, failure }),
            None => Ok(Self(value)),
        }
    }

    /// Borrows the exact checked spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the exact checked spelling.
    pub fn into_string(self) -> String {
        self.0
    }
}

/// Why a proposed tool name is outside the baseline spelling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolNameFailure {
    /// No name was supplied.
    Empty,
    /// The UTF-8 spelling exceeds the baseline bound.
    TooLong {
        /// The observed UTF-8 byte count.
        bytes: usize,
    },
    /// One scalar is outside ASCII alphanumeric, underscore, and hyphen.
    InvalidCharacter {
        /// Its UTF-8 byte offset.
        byte_index: usize,
        /// The rejected scalar.
        character: char,
    },
}

/// Failed tool-name construction retaining the rejected value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolNameError {
    value: String,
    failure: ToolNameFailure,
}

impl ToolNameError {
    /// Borrows the rejected spelling.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Returns the exact validation failure.
    pub const fn failure(&self) -> ToolNameFailure {
        self.failure
    }

    /// Returns the rejected spelling and failure.
    pub fn into_parts(self) -> (String, ToolNameFailure) {
        (self.value, self.failure)
    }
}
