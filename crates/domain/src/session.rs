//! Session creation cause and transcript ancestry values.
//!
//! ADR-0003 (`docs/decisions/0003-session-creation-and-transcript-ancestry.md`)
//! is the normative specification. Every session records two required,
//! independent, immutable creation facts: a creation cause answering why the
//! session exists, and a transcript ancestry answering where its initial
//! semantic conversation context came from. This module represents those
//! facts as pure values, together with the typed [`CreateSession`] caller
//! payload that pairs them with the initial model-selection defaults ADR-0027
//! versions: atomic creation-time validation, durable storage, and selection
//! of a real frontier from source-session history are aggregate and
//! later-slice work.

use crate::{
    DurableCommandId, SessionConfigurationDefaults, SessionId,
    VersionedSessionConfigurationDefaults,
};

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

/// The complete typed caller payload that creates one session.
///
/// The payload carries the owner-global durable command identity, both
/// required independent creation-provenance facts, and one complete
/// unversioned model-selection defaults value. Session creation establishes
/// the first immutable defaults version through
/// [`Self::establish_initial_defaults`], so the caller cannot supply a
/// version of its own:
///
/// ```compile_fail
/// use signalbox_domain::{
///     CreateSession, DurableCommandId, SessionCreationProvenance,
///     VersionedSessionConfigurationDefaults,
/// };
///
/// fn a_versioned_value_is_not_a_creation_payload(
///     command_id: DurableCommandId,
///     provenance: SessionCreationProvenance,
///     defaults: VersionedSessionConfigurationDefaults,
/// ) {
///     let _ = CreateSession::new(command_id, provenance, defaults);
/// }
/// ```
///
/// # Scope
///
/// This is neither a wire message nor a committed command handling. It omits
/// session identity minting, owner authority, command deduplication and
/// replay, atomic validation of the provenance pair before acknowledgement,
/// persistence, and acknowledgement.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CreateSession {
    command_id: DurableCommandId,
    provenance: SessionCreationProvenance,
    initial_configuration_defaults: SessionConfigurationDefaults,
}

impl CreateSession {
    /// Creates the complete payload from its command identity, provenance
    /// facts, and unversioned initial defaults value.
    pub const fn new(
        command_id: DurableCommandId,
        provenance: SessionCreationProvenance,
        initial_configuration_defaults: SessionConfigurationDefaults,
    ) -> Self {
        Self {
            command_id,
            provenance,
            initial_configuration_defaults,
        }
    }

    /// Returns the owner-global durable command identity claimed by this
    /// payload.
    pub const fn command_id(&self) -> DurableCommandId {
        self.command_id
    }

    /// Returns the two required independent creation facts.
    pub const fn provenance(&self) -> SessionCreationProvenance {
        self.provenance
    }

    /// Returns the complete unversioned initial defaults payload.
    pub const fn initial_configuration_defaults(&self) -> SessionConfigurationDefaults {
        self.initial_configuration_defaults
    }

    /// Establishes the first immutable defaults version this creation
    /// installs.
    ///
    /// The result is always [`VersionedSessionConfigurationDefaults::establish`]
    /// applied to the carried payload, so session creation establishes
    /// version one. S01 / INV-003: the established defaults are operationally
    /// associated with the session but are not a third creation-provenance
    /// fact:
    ///
    /// ```compile_fail
    /// use signalbox_domain::{
    ///     SessionConfigurationDefaults, SessionCreationCause,
    ///     SessionCreationProvenance, TranscriptAncestry,
    /// };
    ///
    /// fn defaults_are_not_a_provenance_fact(
    ///     cause: SessionCreationCause,
    ///     ancestry: TranscriptAncestry,
    ///     defaults: SessionConfigurationDefaults,
    /// ) {
    ///     let _ = SessionCreationProvenance {
    ///         cause,
    ///         ancestry,
    ///         defaults,
    ///     };
    /// }
    /// ```
    ///
    /// A later explicit replacement installs the next version without
    /// rewriting creation cause, transcript ancestry, or already accepted
    /// work.
    pub const fn establish_initial_defaults(&self) -> VersionedSessionConfigurationDefaults {
        VersionedSessionConfigurationDefaults::establish(self.initial_configuration_defaults)
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
        CreateSession, SessionCreationCause, SessionCreationProvenance, TranscriptAncestry,
        test_frontier,
    };
    use crate::test_support::{command_id, direct, session_id};
    use crate::{
        ModelSelectionRequest, SessionConfigurationDefaults, SessionConfigurationDefaultsVersion,
        VersionedSessionConfigurationDefaults,
    };

    fn defaults(value: u128) -> SessionConfigurationDefaults {
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(direct(value)))
    }

    fn owner_initiated_empty() -> SessionCreationProvenance {
        SessionCreationProvenance::new(
            SessionCreationCause::OwnerInitiated,
            TranscriptAncestry::None,
        )
    }

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

    /// S01 / INV-003: the creation payload couples the durable command
    /// identity, both independent provenance facts, and one complete
    /// unversioned defaults payload.
    #[test]
    fn s01_inv003_create_session_couples_command_provenance_and_defaults() {
        let provenance = owner_initiated_empty();
        let create = CreateSession::new(command_id(1), provenance, defaults(2));

        assert_eq!(create.command_id(), command_id(1));
        assert_eq!(create.provenance(), provenance);
        assert_eq!(create.initial_configuration_defaults(), defaults(2));
    }

    /// S01: session creation establishes exactly version one of the carried
    /// model-selection defaults payload.
    #[test]
    fn s01_creation_establishes_version_one_of_the_carried_defaults() {
        let create = CreateSession::new(command_id(1), owner_initiated_empty(), defaults(2));

        let established = create.establish_initial_defaults();

        assert_eq!(
            established,
            VersionedSessionConfigurationDefaults::establish(defaults(2))
        );
        assert_eq!(
            established.version(),
            SessionConfigurationDefaultsVersion::first()
        );
        assert_eq!(*established.defaults(), defaults(2));
    }

    /// S01 / S17 / INV-003: initial defaults never join the provenance facts,
    /// and replacing established defaults installs a later version while both
    /// provenance facts compare unchanged.
    #[test]
    fn s01_s17_inv003_defaults_are_not_a_third_provenance_fact() {
        let provenance = owner_initiated_empty();
        let first = CreateSession::new(command_id(1), provenance, defaults(2));
        let second = CreateSession::new(command_id(1), provenance, defaults(3));

        assert_ne!(first, second);
        assert_eq!(first.provenance(), second.provenance());

        let replaced = first
            .establish_initial_defaults()
            .replace(defaults(4))
            .expect("version one must have a next version");
        assert_eq!(
            Some(replaced.version()),
            SessionConfigurationDefaultsVersion::first().checked_next()
        );
        assert_eq!(first.provenance(), provenance);
    }

    /// S01 / S17 / INV-012: canonical payload comparison includes the command
    /// identity, both provenance facts, and the defaults payload.
    #[test]
    fn s01_s17_inv012_create_session_payload_equality_is_structural() {
        let fork = SessionCreationProvenance::new(
            SessionCreationCause::OwnerInitiated,
            TranscriptAncestry::SingleSource {
                source_session: session_id(1),
                source_frontier: test_frontier(2),
            },
        );
        let create = CreateSession::new(command_id(3), owner_initiated_empty(), defaults(4));

        assert_eq!(
            create,
            CreateSession::new(command_id(3), owner_initiated_empty(), defaults(4))
        );
        assert_ne!(
            create,
            CreateSession::new(command_id(5), owner_initiated_empty(), defaults(4))
        );
        assert_ne!(create, CreateSession::new(command_id(3), fork, defaults(4)));
        assert_ne!(
            create,
            CreateSession::new(command_id(3), owner_initiated_empty(), defaults(6))
        );
    }
}
