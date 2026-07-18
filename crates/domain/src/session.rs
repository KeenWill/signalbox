//! Session creation cause and transcript ancestry values.
//!
//! ADR-0003 (`docs/decisions/0003-session-creation-and-transcript-ancestry.md`)
//! is the normative specification. Every session records two required,
//! independent, immutable creation facts: a creation cause answering why the
//! session exists, and a transcript ancestry answering where its initial
//! semantic conversation context came from. This module represents those
//! facts as pure values, together with the typed [`CreateSession`] caller
//! payload, its baseline pre-commit candidate, and its purpose-specific
//! reconstitution boundary. Durable storage and selection of a real frontier
//! from source-session history remain later-slice work.

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
/// # Comparison payload
///
/// Structural equality is the ADR-0001 durable-command comparison payload:
/// every caller-supplied semantic field except the command identifier itself.
/// Two creation payloads that differ only in `command_id` therefore compare
/// equal, matching the sibling [`crate::DeliveryRequest`] payload, which omits
/// command identity entirely. The replay/deduplication boundary looks up the
/// claimed identifier separately and compares canonical payloads: equal replay
/// returns the recorded result, while the same identifier arriving with a
/// different provenance or defaults payload is conflicting reuse.
///
/// # Scope
///
/// This is neither a wire message nor a committed command handling. It omits
/// session identity minting, owner authority, command deduplication and
/// replay, atomic validation of the provenance pair before acknowledgement,
/// persistence, and acknowledgement.
#[derive(Clone, Copy, Debug)]
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
    ///     let _ = SessionCreationProvenance::new(cause, ancestry, defaults);
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

/// ADR-0001: the durable-command comparison payload is every caller-supplied
/// semantic field except the identifier itself, so equality and hashing cover
/// the provenance facts and the defaults payload but not the command identity.
impl PartialEq for CreateSession {
    fn eq(&self, other: &Self) -> bool {
        self.provenance == other.provenance
            && self.initial_configuration_defaults == other.initial_configuration_defaults
    }
}

impl Eq for CreateSession {}

impl std::hash::Hash for CreateSession {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.provenance.hash(state);
        self.initial_configuration_defaults.hash(state);
    }
}

/// The canonical initial state of one session and its defaults.
///
/// This pure value does not claim that a transaction committed. It is carried
/// by [`PreparedCreateSession`] before persistence and by
/// [`ReconstitutedSessionCreation`] only after complete durable facts validate.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct InitialSession {
    id: SessionId,
    provenance: SessionCreationProvenance,
    configuration_defaults: VersionedSessionConfigurationDefaults,
}

impl InitialSession {
    /// Returns the hub-minted session identity.
    pub const fn id(&self) -> SessionId {
        self.id
    }

    /// Returns the immutable creation provenance.
    pub const fn provenance(&self) -> SessionCreationProvenance {
        self.provenance
    }

    /// Returns defaults version one established by creation.
    pub const fn configuration_defaults(&self) -> &VersionedSessionConfigurationDefaults {
        &self.configuration_defaults
    }
}

/// The terminal typed result recorded when `CreateSession` is applied.
///
/// The field is private and there is no constructor from a raw session
/// identity. Live preparation and complete reconstitution are its only
/// producers. The value records a result suitable for replay; possessing a
/// pre-commit value does not claim that persistence occurred.
///
/// ```compile_fail
/// use signalbox_domain::{CreateSessionAppliedResult, SessionId};
///
/// fn a_raw_session_id_is_not_an_applied_result(session: SessionId) {
///     let _ = CreateSessionAppliedResult { session };
/// }
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CreateSessionAppliedResult {
    session: SessionId,
}

impl CreateSessionAppliedResult {
    /// Returns the exact session identity created by the applied command.
    pub const fn session(&self) -> SessionId {
        self.session
    }
}

/// A sealed baseline creation candidate for one future atomic transaction.
///
/// Construction consumes the canonical command and accepts a session identity
/// minted by application orchestration. Private fields prevent independently
/// cross-wiring the command, initial state, and applied result. This value is
/// not evidence of a database commit or command claim.
#[derive(Clone, Copy, Debug)]
pub struct PreparedCreateSession {
    command: CreateSession,
    session: InitialSession,
    applied_result: CreateSessionAppliedResult,
}

impl PreparedCreateSession {
    /// Borrows the exact canonical command to claim in the future transaction.
    pub const fn command(&self) -> &CreateSession {
        &self.command
    }

    /// Borrows the exact initial session state to persist.
    pub const fn session(&self) -> &InitialSession {
        &self.session
    }

    /// Returns the exact terminal applied result to record atomically.
    pub const fn applied_result(&self) -> CreateSessionAppliedResult {
        self.applied_result
    }

    /// Consumes the sealed candidate into its correlated transaction inputs.
    pub const fn into_parts(self) -> (CreateSession, InitialSession, CreateSessionAppliedResult) {
        (self.command, self.session, self.applied_result)
    }
}

/// Why a canonical command cannot yet form the baseline creation candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CreateSessionPreparationFailure {
    /// Trusted production and validation of a source transcript frontier is
    /// not available in this slice.
    TranscriptAncestryUnavailable,
}

/// A failed pre-commit preparation retaining every supplied input unchanged.
///
/// This is not an authoritative command rejection and does not claim the
/// durable command identity.
#[derive(Clone, Debug)]
pub struct CreateSessionPreparationError {
    session: SessionId,
    command: CreateSession,
    failure: CreateSessionPreparationFailure,
}

impl CreateSessionPreparationError {
    /// Returns why no baseline candidate was formed.
    pub const fn failure(&self) -> CreateSessionPreparationFailure {
        self.failure
    }

    /// Borrows the unchanged canonical command.
    pub const fn command(&self) -> &CreateSession {
        &self.command
    }

    /// Returns the unchanged supplied session identity.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns all unchanged preparation inputs and the failure.
    pub fn into_parts(self) -> (SessionId, CreateSession, CreateSessionPreparationFailure) {
        (self.session, self.command, self.failure)
    }
}

impl CreateSession {
    /// Prepares the owner-initiated, no-ancestry baseline for one transaction.
    ///
    /// A single-source command remains a canonical command value but cannot
    /// be handled until a trusted transcript-frontier producer validates its
    /// source boundary. That case returns every input unchanged and is not a
    /// terminal rejected command result.
    pub fn prepare(
        self,
        session: SessionId,
    ) -> Result<PreparedCreateSession, CreateSessionPreparationError> {
        match (self.provenance.cause(), self.provenance.ancestry()) {
            (SessionCreationCause::OwnerInitiated, TranscriptAncestry::None) => {}
            (SessionCreationCause::OwnerInitiated, TranscriptAncestry::SingleSource { .. }) => {
                return Err(CreateSessionPreparationError {
                    session,
                    command: self,
                    failure: CreateSessionPreparationFailure::TranscriptAncestryUnavailable,
                });
            }
        }

        let initial_session = InitialSession {
            id: session,
            provenance: self.provenance,
            configuration_defaults: self.establish_initial_defaults(),
        };
        Ok(PreparedCreateSession {
            command: self,
            session: initial_session,
            applied_result: CreateSessionAppliedResult { session },
        })
    }
}

/// Complete checked inputs for reconstituting one applied session creation.
///
/// These are domain values rather than rows or nullable storage shapes. The
/// result session and the defaults row's owning session are each supplied
/// separately from the session record identity so the domain can reject a
/// cross-wired applied result or a defaults row belonging to another session.
#[derive(Clone, Copy, Debug)]
pub struct CreateSessionReconstitutionInput {
    command: CreateSession,
    result_session: SessionId,
    session: SessionId,
    provenance: SessionCreationProvenance,
    defaults_session: SessionId,
    defaults_version: crate::SessionConfigurationDefaultsVersion,
    defaults: SessionConfigurationDefaults,
}

impl CreateSessionReconstitutionInput {
    /// Supplies the complete typed facts required by this purpose-specific
    /// reconstitution seam.
    pub const fn new(
        command: CreateSession,
        result_session: SessionId,
        session: SessionId,
        provenance: SessionCreationProvenance,
        defaults_session: SessionId,
        defaults_version: crate::SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
    ) -> Self {
        Self {
            command,
            result_session,
            session,
            provenance,
            defaults_session,
            defaults_version,
            defaults,
        }
    }

    /// Borrows the reconstructed canonical command record.
    pub const fn command(&self) -> &CreateSession {
        &self.command
    }

    /// Returns the session identity recorded in the applied result.
    pub const fn result_session(&self) -> SessionId {
        self.result_session
    }

    /// Returns the identity recorded by the session aggregate.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the immutable provenance recorded by the session aggregate.
    pub const fn provenance(&self) -> SessionCreationProvenance {
        self.provenance
    }

    /// Returns the session that owns the stored initial defaults row.
    pub const fn defaults_session(&self) -> SessionId {
        self.defaults_session
    }

    /// Returns the stored initial defaults version.
    pub const fn defaults_version(&self) -> crate::SessionConfigurationDefaultsVersion {
        self.defaults_version
    }

    /// Returns the stored initial defaults value.
    pub const fn defaults(&self) -> SessionConfigurationDefaults {
        self.defaults
    }

    /// Reconstructs the complete canonical creation without replaying effects.
    pub fn reconstitute(
        self,
    ) -> Result<ReconstitutedSessionCreation, CreateSessionReconstitutionError> {
        let fail = |failure| CreateSessionReconstitutionError {
            input: Box::new(self),
            failure,
        };

        if self.session != self.result_session {
            return Err(fail(
                CreateSessionReconstitutionFailure::SessionResultMismatch,
            ));
        }
        if self.command.provenance() != self.provenance {
            return Err(fail(CreateSessionReconstitutionFailure::ProvenanceMismatch));
        }
        if self.defaults_session != self.session {
            return Err(fail(
                CreateSessionReconstitutionFailure::DefaultsSessionMismatch,
            ));
        }
        match (self.provenance.cause(), self.provenance.ancestry()) {
            (SessionCreationCause::OwnerInitiated, TranscriptAncestry::None) => {}
            (SessionCreationCause::OwnerInitiated, TranscriptAncestry::SingleSource { .. }) => {
                return Err(fail(
                    CreateSessionReconstitutionFailure::TranscriptAncestryUnavailable,
                ));
            }
        }
        if self.defaults_version != crate::SessionConfigurationDefaultsVersion::first() {
            return Err(fail(
                CreateSessionReconstitutionFailure::DefaultsVersionIsNotFirst,
            ));
        }
        if self.command.initial_configuration_defaults() != self.defaults {
            return Err(fail(CreateSessionReconstitutionFailure::DefaultsMismatch));
        }

        Ok(ReconstitutedSessionCreation {
            command: self.command,
            session: InitialSession {
                id: self.session,
                provenance: self.provenance,
                configuration_defaults: VersionedSessionConfigurationDefaults::establish(
                    self.defaults,
                ),
            },
            applied_result: CreateSessionAppliedResult {
                session: self.result_session,
            },
        })
    }
}

/// Why complete typed durable facts cannot reconstruct session creation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CreateSessionReconstitutionFailure {
    /// The applied result names a different session from the session record.
    SessionResultMismatch,
    /// The stored creation provenance differs from the canonical command.
    ProvenanceMismatch,
    /// The stored initial defaults row belongs to a different session.
    DefaultsSessionMismatch,
    /// Trusted source-frontier production is unavailable for this slice.
    TranscriptAncestryUnavailable,
    /// Session creation did not establish defaults version one.
    DefaultsVersionIsNotFirst,
    /// The stored initial defaults differ from the canonical command payload.
    DefaultsMismatch,
}

/// A failed reconstitution retaining the complete unchanged typed input.
#[derive(Clone, Debug)]
pub struct CreateSessionReconstitutionError {
    input: Box<CreateSessionReconstitutionInput>,
    failure: CreateSessionReconstitutionFailure,
}

impl CreateSessionReconstitutionError {
    /// Returns why the complete projection could not be reconstructed.
    pub const fn failure(&self) -> CreateSessionReconstitutionFailure {
        self.failure
    }

    /// Borrows the complete unchanged input.
    pub const fn input(&self) -> &CreateSessionReconstitutionInput {
        &self.input
    }

    /// Returns the complete unchanged input and failure.
    pub fn into_parts(
        self,
    ) -> (
        CreateSessionReconstitutionInput,
        CreateSessionReconstitutionFailure,
    ) {
        (*self.input, self.failure)
    }
}

/// One complete session creation reconstructed from matching durable facts.
///
/// This is distinct from [`PreparedCreateSession`]: it authorizes no insert,
/// effect, identity generation, or command claim.
#[derive(Clone, Copy, Debug)]
pub struct ReconstitutedSessionCreation {
    command: CreateSession,
    session: InitialSession,
    applied_result: CreateSessionAppliedResult,
}

impl ReconstitutedSessionCreation {
    /// Borrows the reconstructed canonical command.
    pub const fn command(&self) -> &CreateSession {
        &self.command
    }

    /// Borrows the reconstructed initial session state.
    pub const fn session(&self) -> &InitialSession {
        &self.session
    }

    /// Returns the reconstructed recorded applied result.
    pub const fn applied_result(&self) -> CreateSessionAppliedResult {
        self.applied_result
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
        CreateSession, CreateSessionPreparationFailure, CreateSessionReconstitutionFailure,
        CreateSessionReconstitutionInput, SessionCreationCause, SessionCreationProvenance,
        TranscriptAncestry, test_frontier,
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

    /// S01 / S17 / INV-012: the canonical comparison payload is every
    /// caller-supplied semantic field except the command identifier itself, so
    /// payloads that differ only in `command_id` compare equal (equal replay),
    /// while any provenance or defaults difference is a distinct payload
    /// (conflicting reuse of one identifier is then detectable).
    #[test]
    fn s01_s17_inv012_create_session_comparison_payload_excludes_command_id() {
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
        assert_eq!(
            create,
            CreateSession::new(command_id(5), owner_initiated_empty(), defaults(4))
        );
        assert_ne!(create, CreateSession::new(command_id(3), fork, defaults(4)));
        assert_ne!(
            create,
            CreateSession::new(command_id(3), owner_initiated_empty(), defaults(6))
        );
    }

    /// S01 / INV-003 / INV-008 / INV-012: preparation seals the exact
    /// command, hub-supplied session, independent provenance, defaults version
    /// one, and matching replay result without claiming a commit.
    #[test]
    fn s01_inv003_inv008_inv012_preparation_couples_complete_creation() {
        let create = CreateSession::new(command_id(1), owner_initiated_empty(), defaults(2));

        let prepared = create
            .prepare(session_id(3))
            .expect("the empty owner-initiated baseline is preparable");

        assert_eq!(prepared.command().command_id(), command_id(1));
        assert_eq!(prepared.command(), &create);
        assert_eq!(prepared.session().id(), session_id(3));
        assert_eq!(prepared.session().provenance(), owner_initiated_empty());
        assert_eq!(
            prepared.session().configuration_defaults().version(),
            SessionConfigurationDefaultsVersion::first()
        );
        assert_eq!(
            prepared.session().configuration_defaults().defaults(),
            &defaults(2)
        );
        assert_eq!(prepared.applied_result().session(), session_id(3));

        let (carried_command, carried_session, carried_result) = prepared.into_parts();
        assert_eq!(carried_command.command_id(), command_id(1));
        assert_eq!(carried_session.id(), carried_result.session());
    }

    /// S17: until trusted transcript-frontier production exists, a
    /// single-source command yields no candidate or terminal command result
    /// and returns the command and minted identity unchanged.
    #[test]
    fn s17_unavailable_ancestry_is_a_nonclaiming_preparation_failure() {
        let provenance = SessionCreationProvenance::new(
            SessionCreationCause::OwnerInitiated,
            TranscriptAncestry::SingleSource {
                source_session: session_id(1),
                source_frontier: test_frontier(2),
            },
        );
        let create = CreateSession::new(command_id(3), provenance, defaults(4));

        let error = create
            .prepare(session_id(5))
            .expect_err("unvalidated source ancestry cannot form a candidate");

        assert_eq!(
            error.failure(),
            CreateSessionPreparationFailure::TranscriptAncestryUnavailable
        );
        assert_eq!(error.session(), session_id(5));
        assert_eq!(error.command().command_id(), command_id(3));
        assert_eq!(error.command().provenance(), provenance);
        let (session, command, failure) = error.into_parts();
        assert_eq!(session, session_id(5));
        assert_eq!(command.command_id(), command_id(3));
        assert_eq!(command.provenance(), provenance);
        assert_eq!(
            failure,
            CreateSessionPreparationFailure::TranscriptAncestryUnavailable
        );
    }

    /// S01 / INV-003 / INV-008 / INV-012: complete matching durable facts
    /// reconstruct the same canonical initial session and typed replay result
    /// without producing a pre-commit candidate.
    #[test]
    fn s01_inv003_inv008_inv012_matching_creation_reconstitutes_whole() {
        let create = CreateSession::new(command_id(1), owner_initiated_empty(), defaults(2));
        let input = CreateSessionReconstitutionInput::new(
            create,
            session_id(3),
            session_id(3),
            owner_initiated_empty(),
            session_id(3),
            SessionConfigurationDefaultsVersion::first(),
            defaults(2),
        );

        let reconstituted = input
            .reconstitute()
            .expect("complete matching creation facts must reconstruct");

        assert_eq!(reconstituted.command().command_id(), command_id(1));
        assert_eq!(reconstituted.command(), &create);
        assert_eq!(reconstituted.session().id(), session_id(3));
        assert_eq!(
            reconstituted.session().provenance(),
            owner_initiated_empty()
        );
        assert_eq!(
            reconstituted.session().configuration_defaults().version(),
            SessionConfigurationDefaultsVersion::first()
        );
        assert_eq!(
            reconstituted.session().configuration_defaults().defaults(),
            &defaults(2)
        );
        assert_eq!(reconstituted.applied_result().session(), session_id(3));
    }

    /// S01 / INV-003 / INV-008 / INV-012: every cross-wired session, result,
    /// provenance, or defaults shape fails closed and retains the complete
    /// unchanged typed projection.
    #[test]
    fn s01_inv003_inv008_inv012_reconstitution_rejects_cross_wired_facts() {
        let create = CreateSession::new(command_id(1), owner_initiated_empty(), defaults(2));
        let second_version = SessionConfigurationDefaultsVersion::first()
            .checked_next()
            .expect("version two exists");
        let fork = SessionCreationProvenance::new(
            SessionCreationCause::OwnerInitiated,
            TranscriptAncestry::SingleSource {
                source_session: session_id(10),
                source_frontier: test_frontier(11),
            },
        );

        let cases = [
            (
                CreateSessionReconstitutionInput::new(
                    create,
                    session_id(4),
                    session_id(3),
                    owner_initiated_empty(),
                    session_id(3),
                    SessionConfigurationDefaultsVersion::first(),
                    defaults(2),
                ),
                CreateSessionReconstitutionFailure::SessionResultMismatch,
            ),
            (
                CreateSessionReconstitutionInput::new(
                    CreateSession::new(command_id(1), fork, defaults(2)),
                    session_id(3),
                    session_id(3),
                    owner_initiated_empty(),
                    session_id(3),
                    SessionConfigurationDefaultsVersion::first(),
                    defaults(2),
                ),
                CreateSessionReconstitutionFailure::ProvenanceMismatch,
            ),
            (
                CreateSessionReconstitutionInput::new(
                    CreateSession::new(command_id(1), fork, defaults(2)),
                    session_id(3),
                    session_id(3),
                    fork,
                    session_id(3),
                    SessionConfigurationDefaultsVersion::first(),
                    defaults(2),
                ),
                CreateSessionReconstitutionFailure::TranscriptAncestryUnavailable,
            ),
            (
                CreateSessionReconstitutionInput::new(
                    create,
                    session_id(3),
                    session_id(3),
                    owner_initiated_empty(),
                    session_id(9),
                    SessionConfigurationDefaultsVersion::first(),
                    defaults(2),
                ),
                CreateSessionReconstitutionFailure::DefaultsSessionMismatch,
            ),
            (
                CreateSessionReconstitutionInput::new(
                    create,
                    session_id(3),
                    session_id(3),
                    owner_initiated_empty(),
                    session_id(3),
                    second_version,
                    defaults(2),
                ),
                CreateSessionReconstitutionFailure::DefaultsVersionIsNotFirst,
            ),
            (
                CreateSessionReconstitutionInput::new(
                    create,
                    session_id(3),
                    session_id(3),
                    owner_initiated_empty(),
                    session_id(3),
                    SessionConfigurationDefaultsVersion::first(),
                    defaults(5),
                ),
                CreateSessionReconstitutionFailure::DefaultsMismatch,
            ),
        ];

        for (input, expected_failure) in cases {
            let expected_command_id = input.command().command_id();
            let expected_result_session = input.result_session();
            let expected_session = input.session();
            let expected_provenance = input.provenance();
            let expected_defaults_session = input.defaults_session();
            let expected_version = input.defaults_version();
            let expected_defaults = input.defaults();

            let error = input
                .reconstitute()
                .expect_err("cross-wired durable facts must fail closed");

            assert_eq!(error.failure(), expected_failure);
            assert_eq!(error.input().command().command_id(), expected_command_id);
            assert_eq!(error.input().result_session(), expected_result_session);
            assert_eq!(error.input().session(), expected_session);
            assert_eq!(error.input().provenance(), expected_provenance);
            assert_eq!(error.input().defaults_session(), expected_defaults_session);
            assert_eq!(error.input().defaults_version(), expected_version);
            assert_eq!(error.input().defaults(), expected_defaults);
            let (returned, failure) = error.into_parts();
            assert_eq!(returned.command().command_id(), expected_command_id);
            assert_eq!(returned.result_session(), expected_result_session);
            assert_eq!(returned.session(), expected_session);
            assert_eq!(returned.provenance(), expected_provenance);
            assert_eq!(returned.defaults_session(), expected_defaults_session);
            assert_eq!(returned.defaults_version(), expected_version);
            assert_eq!(returned.defaults(), expected_defaults);
            assert_eq!(failure, expected_failure);
        }
    }
}
