//! Positive imported record and transcript positions for `docs/spec/conversation-import.md`.

use std::hash::Hash;

macro_rules! positive_position {
    ($(#[$documentation:meta])* $name:ident) => {
        $(#[$documentation])*
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(u64);

        impl $name {
            /// Reconstitutes a position from a positive ordinal.
            pub const fn try_from_u64(value: u64) -> Option<Self> {
                if value == 0 { None } else { Some(Self(value)) }
            }

            /// Returns the positive ordinal.
            pub const fn as_u64(self) -> u64 {
                self.0
            }

            /// Returns the first position.
            pub const fn first() -> Self {
                Self(1)
            }

            /// Returns the next position or `None` after `u64::MAX`.
            pub const fn checked_next(self) -> Option<Self> {
                match self.0.checked_add(1) {
                    Some(value) => Some(Self(value)),
                    None => None,
                }
            }
        }
    };
}

positive_position!(
    /// One physical raw source-record position.
    ImportedRawRecordPosition
);
positive_position!(
    /// One normalized entry position inside a raw source record.
    ImportedRecordEntryPosition
);
positive_position!(
    /// One normalized imported entry position across the conversation.
    ImportedTranscriptPosition
);
