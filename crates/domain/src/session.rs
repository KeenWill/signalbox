//! Session creation cause and transcript ancestry values.
//!
//! ADR-0003 (`docs/decisions/0003-session-creation-and-transcript-ancestry.md`)
//! is the normative specification. Every session records two required,
//! independent, immutable creation facts: a creation cause answering why the
//! session exists, and a transcript ancestry answering where its initial
//! semantic conversation context came from. This module represents those
//! facts as pure values: atomic creation-time validation, durable storage,
//! and selection of a real frontier from source-session history are aggregate
//! and later-slice work.

use crate::SessionId;

/// Why one session exists.
///
/// The first implementable cause is owner-initiated. Application-initiated,
/// scheduled, delegated, and any other causes are reserved extension examples
/// rather than valid baseline values: the ADR that enables one must add a
/// typed variant carrying the exact durable initiating domain identity, so
/// this type contains no uninhabitable placeholders. S18 / INV-003: a
/// reserved example is not constructible:
///
/// ```compile_fail
/// use signalbox_domain::SessionCreationCause;
///
/// let _ = SessionCreationCause::Delegated;
/// ```
///
/// and an unstructured string is not a substitute for a typed variant:
///
/// ```compile_fail
/// use signalbox_domain::SessionCreationCause;
///
/// let _: SessionCreationCause = "delegated".into();
/// ```
///
/// S01 / S17 / INV-003: no cause variant implies or carries ancestry:
///
/// ```compile_fail
/// use signalbox_domain::{SessionCreationCause, TranscriptAncestry};
///
/// fn a_cause_cannot_carry_ancestry(ancestry: TranscriptAncestry) {
///     let _ = SessionCreationCause::OwnerInitiated { ancestry };
/// }
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionCreationCause {
    /// The owner started this conversation.
    OwnerInitiated,
}

/// Identifies one exact immutable source boundary in semantic history.
///
/// A transcript frontier is related to, but need not share the storage
/// representation of, the per-model-call context frontier. The boundary
/// representation inside semantic history remains undecided, so this value is
/// opaque: equality compares exact boundaries, and no public constructor or
/// raw-part conversion exists:
///
/// ```compile_fail
/// use signalbox_domain::TranscriptFrontier;
///
/// fn a_raw_token_is_not_a_source_boundary<T>(token: T) {
///     let _ = TranscriptFrontier { boundary: token };
/// }
/// ```
///
/// The slice that fixes semantic-history boundaries supplies the trusted
/// producer that selects and validates a frontier from a real source session.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TranscriptFrontier {
    boundary: uuid::Uuid,
}

/// Where one session's initial semantic conversation context came from.
///
/// Ancestry is either none or exactly one immutable source consisting of a
/// source session and an exact source transcript frontier. [`Self::None`]
/// explicitly means that no prior session transcript supplied initial
/// semantic context; it does not mean the session lacks task input,
/// configuration, or a creation cause. Signalbox never infers ancestry from
/// related-session links, task briefs, copied text, or delegation.
///
/// S17 / INV-003: ancestry never implies a creation cause and no variant
/// carries one:
///
/// ```compile_fail
/// use signalbox_domain::{SessionCreationCause, TranscriptAncestry};
///
/// fn ancestry_cannot_carry_a_cause(cause: SessionCreationCause) {
///     let _ = TranscriptAncestry::None { cause };
/// }
/// ```
///
/// INV-030: the value is immutable and has no update operations; later
/// source-session changes cannot rewrite it. Multiple-source ancestry and
/// merge remain reserved future decision scope.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TranscriptAncestry {
    /// No prior session transcript supplied initial semantic context.
    None,
    /// Exactly one immutable source supplied initial semantic context.
    SingleSource {
        /// The session whose transcript seeded this session's initial
        /// context.
        source_session: SessionId,
        /// The exact immutable boundary selected within the source
        /// transcript.
        source_frontier: TranscriptFrontier,
    },
}

/// The two required, independent, immutable creation facts for one session.
///
/// Cause and ancestry vary independently and neither can be omitted. S01 /
/// S17 / INV-003: one fact alone is not creation provenance:
///
/// ```compile_fail
/// use signalbox_domain::{SessionCreationCause, SessionCreationProvenance};
///
/// fn a_cause_alone_is_not_provenance(cause: SessionCreationCause) {
///     let _: SessionCreationProvenance = cause.into();
/// }
/// ```
///
/// ```compile_fail
/// use signalbox_domain::{SessionCreationProvenance, TranscriptAncestry};
///
/// fn ancestry_alone_is_not_provenance(ancestry: TranscriptAncestry) {
///     let _: SessionCreationProvenance = ancestry.into();
/// }
/// ```
///
/// This value claims nothing about validation or durability: atomic
/// creation-time validation of the pair before acknowledgement is aggregate
/// work.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SessionCreationProvenance {
    cause: SessionCreationCause,
    ancestry: TranscriptAncestry,
}

impl SessionCreationProvenance {
    /// Pairs the two required independent creation facts.
    pub const fn new(cause: SessionCreationCause, ancestry: TranscriptAncestry) -> Self {
        Self { cause, ancestry }
    }

    /// Returns why this session exists.
    pub const fn cause(&self) -> SessionCreationCause {
        self.cause
    }

    /// Returns where this session's initial semantic context came from.
    pub const fn ancestry(&self) -> TranscriptAncestry {
        self.ancestry
    }
}

#[cfg(test)]
const fn test_frontier(value: u128) -> TranscriptFrontier {
    TranscriptFrontier {
        boundary: uuid::Uuid::from_u128(value),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SessionCreationCause, SessionCreationProvenance, TranscriptAncestry, test_frontier,
    };
    use crate::test_support::session_id;

    /// S01 / INV-003: an owner-initiated session with explicitly empty
    /// ancestry is complete creation provenance for an empty conversation.
    #[test]
    fn s01_inv003_owner_initiated_with_no_ancestry_is_complete_provenance() {
        let provenance = SessionCreationProvenance::new(
            SessionCreationCause::OwnerInitiated,
            TranscriptAncestry::None,
        );

        assert_eq!(provenance.cause(), SessionCreationCause::OwnerInitiated);
        assert_eq!(provenance.ancestry(), TranscriptAncestry::None);
    }

    /// S17 / INV-003 / INV-030: an owner-created fork records the exact
    /// immutable source session and source frontier it was seeded from.
    #[test]
    fn s17_inv003_inv030_fork_provenance_records_exact_source_and_frontier() {
        let source_session = session_id(1);
        let source_frontier = test_frontier(2);
        let provenance = SessionCreationProvenance::new(
            SessionCreationCause::OwnerInitiated,
            TranscriptAncestry::SingleSource {
                source_session,
                source_frontier,
            },
        );

        assert_eq!(provenance.cause(), SessionCreationCause::OwnerInitiated);
        let TranscriptAncestry::SingleSource {
            source_session: carried_session,
            source_frontier: carried_frontier,
        } = provenance.ancestry()
        else {
            panic!("fork provenance must retain its single ancestry source");
        };
        assert_eq!(carried_session, source_session);
        assert_eq!(carried_frontier, source_frontier);
    }

    /// S01 / S17 / INV-003: the same owner-initiated cause pairs with empty
    /// and single-source ancestry, so neither fact is a proxy for the other.
    #[test]
    fn s01_s17_inv003_cause_and_ancestry_vary_independently() {
        let empty = SessionCreationProvenance::new(
            SessionCreationCause::OwnerInitiated,
            TranscriptAncestry::None,
        );
        let fork = SessionCreationProvenance::new(
            SessionCreationCause::OwnerInitiated,
            TranscriptAncestry::SingleSource {
                source_session: session_id(1),
                source_frontier: test_frontier(2),
            },
        );

        assert_eq!(empty.cause(), fork.cause());
        assert_ne!(empty.ancestry(), fork.ancestry());
        assert_ne!(empty, fork);
    }

    /// S17 / INV-030: ancestry equality is exact over both the source session
    /// and the source frontier, and an explicit empty ancestry never equals a
    /// single-source one.
    #[test]
    fn s17_inv030_ancestry_equality_is_exact_over_source_and_frontier() {
        let ancestry = TranscriptAncestry::SingleSource {
            source_session: session_id(1),
            source_frontier: test_frontier(2),
        };
        let same_source = TranscriptAncestry::SingleSource {
            source_session: session_id(1),
            source_frontier: test_frontier(2),
        };
        let different_session = TranscriptAncestry::SingleSource {
            source_session: session_id(3),
            source_frontier: test_frontier(2),
        };
        let different_frontier = TranscriptAncestry::SingleSource {
            source_session: session_id(1),
            source_frontier: test_frontier(4),
        };

        assert_eq!(ancestry, same_source);
        assert_ne!(ancestry, different_session);
        assert_ne!(ancestry, different_frontier);
        assert_ne!(ancestry, TranscriptAncestry::None);
    }
}
