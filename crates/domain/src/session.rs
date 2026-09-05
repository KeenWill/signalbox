//! Session creation cause and transcript ancestry values.
//!
//! The normative specification is `docs/spec/sessions-and-transcript.md`.
//! Every session records two required, independent, immutable creation
//! facts: a creation cause answering why the session exists, and a
//! transcript ancestry answering where its initial semantic conversation
//! context came from. This module represents those facts as pure values,
//! together with the typed [`CreateSession`] caller payload, its baseline
//! pre-commit candidate, and its purpose-specific reconstitution boundary.
//! Durable storage and selection of a real frontier from source-session
//! history remain later-slice work.

use crate::{
    ContextFrontierId, DurableCommandId, ImportedConversationId, ImportedTranscriptFrontier,
    SessionConfigurationDefaults, SessionConfigurationDefaultsVersion, SessionId, SessionPlacement,
    SessionPlacementVersion, SessionTemplateProvenance, VersionedSessionConfigurationDefaults,
    VersionedSessionPlacement,
};

#[derive(Clone, Debug)]
enum SessionCreationDefaults {
    Explicit(SessionConfigurationDefaults),
    Template {
        provenance: SessionTemplateProvenance,
        resolved: SessionConfigurationDefaults,
    },
}

/// Why one session exists.
///
/// Interactive, module-dispatched, and delegated causes are implemented.
/// Application-initiated, scheduled, and any other causes remain reserved
/// extension examples rather than valid baseline values: the specification
/// revision that enables one must add a typed variant carrying the exact
/// durable initiating domain identity, so this type contains no uninhabitable
/// placeholders.
///
/// and an unstructured string is not a substitute for a typed variant:
///
/// ```compile_fail
/// use signalbox_domain::SessionCreationCause;
///
/// let _: SessionCreationCause = "delegated".into();
/// ```
///
/// S01 / S17: no cause variant implies or carries ancestry:
///
/// ```compile_fail
/// use signalbox_domain::{SessionCreationCause, TranscriptAncestry};
///
/// fn a_cause_cannot_carry_ancestry(ancestry: TranscriptAncestry) {
///     let _ = SessionCreationCause::Interactive { ancestry };
/// }
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionCreationCause {
    /// A person started this conversation.
    ///
    /// The imported-frontier creation family records this cause with its
    /// import reference in its own ancestry columns: importing a conversation
    /// is a user-initiated act, so the vocabulary stays closed.
    Interactive,
    /// One exact module dispatch created this session.
    ModuleDispatched {
        /// The dispatching module and its own durable dispatch identity.
        dispatch: crate::ModuleDispatch,
    },
    /// One exact logical tool request spawned this delegated child.
    Delegated {
        /// The parent work to which the child must return its result.
        spawning_request: crate::ToolRequestId,
    },
}

impl SessionCreationCause {
    /// Returns the ownership a creation takes when the command names none.
    pub const fn default_ownership(&self) -> crate::SessionOwnership {
        match self {
            Self::Interactive => crate::SessionOwnership::Unmonitored,
            Self::ModuleDispatched { .. } | Self::Delegated { .. } => {
                crate::SessionOwnership::Owned
            }
        }
    }

    /// Returns the finish condition a creation carries when the command
    /// names none: dispatched work is gated on the dispatch's external gate.
    pub const fn default_finish_condition(&self) -> Option<crate::FinishCondition> {
        match self {
            Self::ModuleDispatched { .. } => Some(crate::FinishCondition::ExternalGate),
            Self::Interactive | Self::Delegated { .. } => None,
        }
    }
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

/// How a new native session relates to one selected imported frontier.
///
/// Both variants leave the imported conversation immutable and create an
/// independent Signalbox session. The distinction records only the client's
/// creation-time intent.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ImportedSessionRelationship {
    /// Continue from the selected imported point.
    Resume,
    /// Branch from the selected imported point.
    Fork,
}

/// Where one session's initial semantic conversation context came from.
///
/// Ancestry is either none, exactly one native source session and transcript
/// frontier, or exactly one imported frontier and its resume/fork
/// relationship. [`Self::None`] explicitly means that no prior transcript
/// supplied initial semantic context; it does not mean the session lacks task
/// input, configuration, or a creation cause. Signalbox never infers ancestry
/// from related-session links, task briefs, copied text, or delegation.
///
/// S17: ancestry never implies a creation cause and no variant
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
/// the value is immutable and has no update operations; later
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
    /// Exactly one immutable imported frontier supplied initial semantic
    /// context.
    ImportedConversation {
        /// The selected inclusive imported entry boundary. Its owning
        /// conversation is carried by the frontier and is not duplicated.
        source_frontier: ImportedTranscriptFrontier,
        /// The client's creation-time relationship to the imported point.
        relationship: ImportedSessionRelationship,
    },
}

/// The two required, independent, immutable creation facts for one session.
///
/// Cause and ancestry vary independently and neither can be omitted. S01 /
/// S17: one fact alone is not creation provenance:
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

    /// Creates delegated provenance without inferring transcript ancestry.
    pub const fn delegated(spawning_request: crate::ToolRequestId) -> Self {
        Self {
            cause: SessionCreationCause::Delegated { spawning_request },
            ancestry: TranscriptAncestry::None,
        }
    }

    /// Creates module-dispatched provenance naming the exact dispatch.
    ///
    /// A dispatched session starts from no prior transcript, so the ancestry
    /// is fixed rather than accepted: a module supplying one would be
    /// inferring semantic history from a dispatch record.
    pub const fn module_dispatched(dispatch: crate::ModuleDispatch) -> Self {
        Self {
            cause: SessionCreationCause::ModuleDispatched { dispatch },
            ancestry: TranscriptAncestry::None,
        }
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
/// The payload carries the user-global durable command identity, both
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
/// Structural equality is the durable-command comparison payload of
/// docs/spec/identity-and-commands.md: every caller-supplied semantic
/// field except the command identifier itself. Two creation payloads that
/// differ only in `command_id` therefore compare equal, matching the
/// sibling [`crate::DeliveryRequest`] payload, which omits command identity
/// entirely. The replay/deduplication boundary looks up the claimed
/// identifier separately and compares canonical payloads: equal replay
/// returns the recorded result, while the same identifier arriving with a
/// different provenance or defaults payload is conflicting reuse.
///
/// # Scope
///
/// This is neither a wire message nor a committed command handling. It omits
/// session identity minting, user authority, command deduplication and
/// replay, atomic validation of the provenance pair before acknowledgement,
/// persistence, and acknowledgement.
#[derive(Clone, Debug)]
pub struct CreateSession {
    command_id: DurableCommandId,
    provenance: SessionCreationProvenance,
    creation_defaults: SessionCreationDefaults,
    placement: SessionPlacement,
    start_gate: crate::StartGate,
    ownership: crate::SessionOwnership,
    finish_condition: Option<crate::FinishCondition>,
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
            creation_defaults: SessionCreationDefaults::Explicit(initial_configuration_defaults),
            placement: SessionPlacement::pathless(),
            start_gate: crate::StartGate::Open,
            ownership: provenance.cause().default_ownership(),
            finish_condition: provenance.cause().default_finish_condition(),
        }
    }

    /// Creates an explicitly placed session.
    pub const fn new_with_placement(
        command_id: DurableCommandId,
        provenance: SessionCreationProvenance,
        initial_configuration_defaults: SessionConfigurationDefaults,
        placement: SessionPlacement,
    ) -> Self {
        Self {
            command_id,
            provenance,
            creation_defaults: SessionCreationDefaults::Explicit(initial_configuration_defaults),
            placement,
            start_gate: crate::StartGate::Open,
            ownership: provenance.cause().default_ownership(),
            finish_condition: provenance.cause().default_finish_condition(),
        }
    }

    /// Creates a template-sourced payload from the resolved copy and its
    /// immutable name/digest provenance.
    pub const fn new_from_template(
        command_id: DurableCommandId,
        provenance: SessionCreationProvenance,
        template_provenance: SessionTemplateProvenance,
        resolved_configuration_defaults: SessionConfigurationDefaults,
    ) -> Self {
        Self {
            command_id,
            provenance,
            creation_defaults: SessionCreationDefaults::Template {
                provenance: template_provenance,
                resolved: resolved_configuration_defaults,
            },
            placement: SessionPlacement::pathless(),
            start_gate: crate::StartGate::Open,
            ownership: provenance.cause().default_ownership(),
            finish_condition: provenance.cause().default_finish_condition(),
        }
    }

    /// Creates a template-sourced session with an explicit placement.
    pub const fn new_from_template_with_placement(
        command_id: DurableCommandId,
        provenance: SessionCreationProvenance,
        template_provenance: SessionTemplateProvenance,
        resolved_configuration_defaults: SessionConfigurationDefaults,
        placement: SessionPlacement,
    ) -> Self {
        Self {
            command_id,
            provenance,
            creation_defaults: SessionCreationDefaults::Template {
                provenance: template_provenance,
                resolved: resolved_configuration_defaults,
            },
            placement,
            start_gate: crate::StartGate::Open,
            ownership: provenance.cause().default_ownership(),
            finish_condition: provenance.cause().default_finish_condition(),
        }
    }

    /// Returns the user-global durable command identity claimed by this
    /// payload.
    pub const fn command_id(&self) -> DurableCommandId {
        self.command_id
    }

    /// Returns the two required independent creation facts.
    pub const fn provenance(&self) -> SessionCreationProvenance {
        self.provenance
    }

    /// Borrows the complete unversioned initial defaults payload.
    pub const fn initial_configuration_defaults(&self) -> &SessionConfigurationDefaults {
        match &self.creation_defaults {
            SessionCreationDefaults::Explicit(defaults)
            | SessionCreationDefaults::Template {
                resolved: defaults, ..
            } => defaults,
        }
    }

    /// Borrows immutable template provenance when creation copied a template.
    pub const fn template_provenance(&self) -> Option<&SessionTemplateProvenance> {
        match &self.creation_defaults {
            SessionCreationDefaults::Explicit(_) => None,
            SessionCreationDefaults::Template { provenance, .. } => Some(provenance),
        }
    }

    /// Borrows the placement pinned by this creation record.
    pub const fn placement(&self) -> &SessionPlacement {
        &self.placement
    }

    /// Installs the lifecycle members: the start gate, the ownership, and
    /// the finish condition an owned session owes.
    pub fn with_lifecycle(
        mut self,
        start_gate: crate::StartGate,
        ownership: crate::SessionOwnership,
        finish_condition: Option<crate::FinishCondition>,
    ) -> Self {
        self.start_gate = start_gate;
        self.ownership = ownership;
        self.finish_condition = finish_condition;
        self
    }

    /// Returns whether the creation holds its start gate.
    pub const fn start_gate(&self) -> crate::StartGate {
        self.start_gate
    }

    /// Returns the ownership the creation establishes.
    pub const fn ownership(&self) -> crate::SessionOwnership {
        self.ownership
    }

    /// Borrows the finish condition the creation declares.
    pub const fn finish_condition(&self) -> Option<&crate::FinishCondition> {
        self.finish_condition.as_ref()
    }

    /// Establishes the first immutable defaults version this creation
    /// installs.
    ///
    /// The result is always [`VersionedSessionConfigurationDefaults::establish`]
    /// applied to the carried payload, so session creation establishes
    /// version one. S01: the established defaults are operationally
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
    pub fn establish_initial_defaults(&self) -> VersionedSessionConfigurationDefaults {
        VersionedSessionConfigurationDefaults::establish(
            self.initial_configuration_defaults().clone(),
        )
    }
}

/// docs/spec/identity-and-commands.md: the durable-command comparison
/// payload is every caller-supplied semantic field except the identifier
/// itself, so equality and hashing cover the provenance facts and the
/// defaults payload but not the command identity.
impl PartialEq for CreateSession {
    fn eq(&self, other: &Self) -> bool {
        self.provenance == other.provenance
            && self.placement == other.placement
            && self.start_gate == other.start_gate
            && self.ownership == other.ownership
            && self.finish_condition == other.finish_condition
            && match (&self.creation_defaults, &other.creation_defaults) {
                (
                    SessionCreationDefaults::Explicit(left),
                    SessionCreationDefaults::Explicit(right),
                ) => left == right,
                (
                    SessionCreationDefaults::Template {
                        provenance: left, ..
                    },
                    SessionCreationDefaults::Template {
                        provenance: right, ..
                    },
                ) => left.name() == right.name(),
                _ => false,
            }
    }
}

impl Eq for CreateSession {}

impl std::hash::Hash for CreateSession {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.provenance.hash(state);
        self.placement.hash(state);
        self.start_gate.hash(state);
        self.ownership.hash(state);
        self.finish_condition.hash(state);
        match &self.creation_defaults {
            SessionCreationDefaults::Explicit(defaults) => {
                0_u8.hash(state);
                defaults.hash(state);
            }
            SessionCreationDefaults::Template { provenance, .. } => {
                1_u8.hash(state);
                provenance.name().hash(state);
            }
        }
    }
}

/// The distinct command payload for creating a native session from imported
/// history.
///
/// This value selects one imported conversation and addressable entry
/// boundary at session-creation time. Import itself never constructs this
/// command or chooses its relationship.
#[derive(Clone, Debug)]
pub struct CreateSessionFromImportedFrontier {
    command_id: DurableCommandId,
    imported_frontier: ImportedTranscriptFrontier,
    relationship: ImportedSessionRelationship,
    initial_configuration_defaults: SessionConfigurationDefaults,
}

impl CreateSessionFromImportedFrontier {
    /// Creates the complete canonical caller payload.
    pub const fn new(
        command_id: DurableCommandId,
        imported_frontier: ImportedTranscriptFrontier,
        relationship: ImportedSessionRelationship,
        initial_configuration_defaults: SessionConfigurationDefaults,
    ) -> Self {
        Self {
            command_id,
            imported_frontier,
            relationship,
            initial_configuration_defaults,
        }
    }

    /// Returns the user-global durable command identity.
    pub const fn command_id(&self) -> DurableCommandId {
        self.command_id
    }

    /// Returns the selected immutable imported conversation.
    pub const fn imported_conversation(&self) -> ImportedConversationId {
        self.imported_frontier.conversation()
    }

    /// Returns the selected inclusive imported entry boundary.
    pub const fn imported_frontier(&self) -> ImportedTranscriptFrontier {
        self.imported_frontier
    }

    /// Returns the client's creation-time relationship to the imported point.
    pub const fn relationship(&self) -> ImportedSessionRelationship {
        self.relationship
    }

    /// Borrows the complete unversioned initial defaults payload.
    pub const fn initial_configuration_defaults(&self) -> &SessionConfigurationDefaults {
        &self.initial_configuration_defaults
    }

    /// Establishes defaults version one for the session this command creates.
    pub fn establish_initial_defaults(&self) -> VersionedSessionConfigurationDefaults {
        VersionedSessionConfigurationDefaults::establish(
            self.initial_configuration_defaults.clone(),
        )
    }
}

/// The durable-command comparison payload excludes only command identity.
impl PartialEq for CreateSessionFromImportedFrontier {
    fn eq(&self, other: &Self) -> bool {
        self.imported_frontier == other.imported_frontier
            && self.relationship == other.relationship
            && self.initial_configuration_defaults == other.initial_configuration_defaults
    }
}

impl Eq for CreateSessionFromImportedFrontier {}

impl std::hash::Hash for CreateSessionFromImportedFrontier {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.imported_frontier.hash(state);
        self.relationship.hash(state);
        self.initial_configuration_defaults.hash(state);
    }
}

/// The exact Signalbox-owned context frontier materialized for one
/// imported-seeded session.
///
/// This one-to-one record is separate from [`TranscriptAncestry`]: ancestry
/// names the immutable external source, while this value names the local
/// context artifact. Its fields are private, and the complete imported-prefix
/// construction and reconstitution seams remain sealed until the imported
/// semantic-entry projection can validate exact membership.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ImportedSessionSeed {
    session: SessionId,
    seed_frontier: ContextFrontierId,
}

impl ImportedSessionSeed {
    pub(crate) const fn from_validated_parts(
        session: SessionId,
        seed_frontier: ContextFrontierId,
    ) -> Self {
        Self {
            session,
            seed_frontier,
        }
    }

    /// Returns the imported-seeded session that owns this one-to-one record.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the exact generated context-frontier identity.
    pub const fn seed_frontier(&self) -> ContextFrontierId {
        self.seed_frontier
    }
}

/// The canonical initial state of one session and its defaults.
///
/// This pure value does not claim that a transaction committed. It is carried
/// by [`PreparedCreateSession`] before persistence and by
/// [`ReconstitutedSessionCreation`] only after complete durable facts validate.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct InitialSession {
    id: SessionId,
    provenance: SessionCreationProvenance,
    template_provenance: Option<SessionTemplateProvenance>,
    configuration_defaults: VersionedSessionConfigurationDefaults,
    placement: VersionedSessionPlacement,
}

impl InitialSession {
    pub(crate) const fn from_validated_imported_creation(
        id: SessionId,
        provenance: SessionCreationProvenance,
        configuration_defaults: VersionedSessionConfigurationDefaults,
    ) -> Self {
        Self {
            id,
            provenance,
            template_provenance: None,
            configuration_defaults,
            placement: VersionedSessionPlacement::initial(SessionPlacement::pathless()),
        }
    }

    /// Returns the hub-minted session identity.
    pub const fn id(&self) -> SessionId {
        self.id
    }

    /// Returns the immutable creation provenance.
    pub const fn provenance(&self) -> SessionCreationProvenance {
        self.provenance
    }

    /// Borrows immutable template provenance when creation copied a template.
    pub const fn template_provenance(&self) -> Option<&SessionTemplateProvenance> {
        self.template_provenance.as_ref()
    }

    /// Returns defaults version one established by creation.
    pub const fn configuration_defaults(&self) -> &VersionedSessionConfigurationDefaults {
        &self.configuration_defaults
    }

    /// Borrows placement event one established by creation.
    pub const fn placement(&self) -> &VersionedSessionPlacement {
        &self.placement
    }
}

/// The complete current session-level domain snapshot.
///
/// docs/spec/sessions-and-transcript.md defines the normative aggregate
/// boundary. A session owns its semantic identity, immutable creation
/// provenance, and the complete current configuration-defaults version
/// selected by the durable pointer. Creation receipts, transcript history,
/// turns, commands, and scheduler facts remain separate purpose-specific
/// values.
///
/// The fields are private and complete checked reconstitution is the only
/// public producer:
///
/// ```compile_fail
/// use signalbox_domain::{
///     Session, SessionCreationProvenance, SessionId,
///     VersionedSessionConfigurationDefaults,
/// };
///
/// fn raw_parts_are_not_a_session(
///     id: SessionId,
///     provenance: SessionCreationProvenance,
///     defaults: VersionedSessionConfigurationDefaults,
/// ) {
///     let _ = Session {
///         id,
///         creation_provenance: provenance,
///         current_configuration_defaults: defaults,
///     };
/// }
/// ```
///
/// A `Session` is an owned snapshot rather than an implicitly duplicated live
/// handle. Callers must clone it deliberately:
///
/// ```compile_fail
/// use signalbox_domain::Session;
///
/// fn consume(_: Session) {}
///
/// fn a_session_snapshot_is_not_copy(session: Session) {
///     consume(session);
///     consume(session);
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Session {
    id: SessionId,
    creation_provenance: SessionCreationProvenance,
    template_provenance: Option<SessionTemplateProvenance>,
    current_configuration_defaults: VersionedSessionConfigurationDefaults,
    current_placement: VersionedSessionPlacement,
}

impl Session {
    pub(crate) const fn from_validated_imported_reconstitution(
        id: SessionId,
        creation_provenance: SessionCreationProvenance,
        current_configuration_defaults: VersionedSessionConfigurationDefaults,
        current_placement: VersionedSessionPlacement,
    ) -> Self {
        Self {
            id,
            creation_provenance,
            template_provenance: None,
            current_configuration_defaults,
            current_placement,
        }
    }

    /// Returns the durable conversation identity.
    pub const fn id(&self) -> SessionId {
        self.id
    }

    /// Returns the complete immutable creation provenance.
    pub const fn creation_provenance(&self) -> SessionCreationProvenance {
        self.creation_provenance
    }

    /// Borrows immutable template provenance when creation copied a template.
    pub const fn template_provenance(&self) -> Option<&SessionTemplateProvenance> {
        self.template_provenance.as_ref()
    }

    /// Borrows the complete defaults version selected as current when this
    /// snapshot was reconstructed.
    pub const fn current_configuration_defaults(&self) -> &VersionedSessionConfigurationDefaults {
        &self.current_configuration_defaults
    }

    /// Borrows the complete placement event selected as current.
    pub const fn current_placement(&self) -> &VersionedSessionPlacement {
        &self.current_placement
    }
}

/// Labeled, independently stored placement facts for current-session reconstitution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionPlacementReconstitutionFacts {
    /// Session identity owning the mutable current-placement pointer.
    pub current_pointer_session: SessionId,
    /// Placement version selected by the current-placement pointer.
    pub current_pointer_version: SessionPlacementVersion,
    /// Session identity owning the selected immutable placement event.
    pub selected_event_session: SessionId,
    /// Complete selected immutable placement event.
    pub selected_event: VersionedSessionPlacement,
}

/// Complete checked inputs for reconstituting one current [`Session`].
///
/// Each independently stored identity and version is retained so the domain
/// can reject a cross-wired requested session, pointer, or defaults record.
/// These are checked domain values rather than SQL rows, nullable
/// discriminators, or framework types.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionReconstitutionInput {
    requested_session: SessionId,
    stored_session: SessionId,
    provenance: SessionCreationProvenance,
    template_provenance: Option<SessionTemplateProvenance>,
    current_defaults_session: SessionId,
    current_defaults_version: SessionConfigurationDefaultsVersion,
    defaults_session: SessionId,
    defaults_version: SessionConfigurationDefaultsVersion,
    defaults: SessionConfigurationDefaults,
    current_placement_session: SessionId,
    current_placement_version: SessionPlacementVersion,
    placement_session: SessionId,
    current_placement: VersionedSessionPlacement,
}

impl SessionReconstitutionInput {
    /// Supplies every independently stored fact required by the current-session
    /// reconstitution seam.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        requested_session: SessionId,
        stored_session: SessionId,
        provenance: SessionCreationProvenance,
        current_defaults_session: SessionId,
        current_defaults_version: SessionConfigurationDefaultsVersion,
        defaults_session: SessionId,
        defaults_version: SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
        placement: SessionPlacementReconstitutionFacts,
    ) -> Self {
        Self::new_with_template_and_placement(
            requested_session,
            stored_session,
            provenance,
            None,
            current_defaults_session,
            current_defaults_version,
            defaults_session,
            defaults_version,
            defaults,
            placement,
        )
    }

    /// Supplies every independently stored fact, including optional template
    /// provenance, required by the current-session reconstitution seam.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_template_provenance(
        requested_session: SessionId,
        stored_session: SessionId,
        provenance: SessionCreationProvenance,
        template_provenance: Option<SessionTemplateProvenance>,
        current_defaults_session: SessionId,
        current_defaults_version: SessionConfigurationDefaultsVersion,
        defaults_session: SessionId,
        defaults_version: SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
        placement: SessionPlacementReconstitutionFacts,
    ) -> Self {
        Self::new_with_template_and_placement(
            requested_session,
            stored_session,
            provenance,
            template_provenance,
            current_defaults_session,
            current_defaults_version,
            defaults_session,
            defaults_version,
            defaults,
            placement,
        )
    }

    /// Supplies complete stored facts including current placement history.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_template_and_placement(
        requested_session: SessionId,
        stored_session: SessionId,
        provenance: SessionCreationProvenance,
        template_provenance: Option<SessionTemplateProvenance>,
        current_defaults_session: SessionId,
        current_defaults_version: SessionConfigurationDefaultsVersion,
        defaults_session: SessionId,
        defaults_version: SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
        placement: SessionPlacementReconstitutionFacts,
    ) -> Self {
        let SessionPlacementReconstitutionFacts {
            current_pointer_session,
            current_pointer_version,
            selected_event_session,
            selected_event,
        } = placement;
        Self {
            requested_session,
            stored_session,
            provenance,
            template_provenance,
            current_defaults_session,
            current_defaults_version,
            defaults_session,
            defaults_version,
            defaults,
            current_placement_session: current_pointer_session,
            current_placement_version: current_pointer_version,
            placement_session: selected_event_session,
            current_placement: selected_event,
        }
    }

    /// Returns the semantic identity requested by the caller.
    pub const fn requested_session(&self) -> SessionId {
        self.requested_session
    }

    /// Returns the identity stored on the session record.
    pub const fn stored_session(&self) -> SessionId {
        self.stored_session
    }

    /// Returns the complete stored immutable creation provenance.
    pub const fn provenance(&self) -> SessionCreationProvenance {
        self.provenance
    }

    /// Borrows the stored optional template provenance.
    pub const fn template_provenance(&self) -> Option<&SessionTemplateProvenance> {
        self.template_provenance.as_ref()
    }

    /// Returns the session identity owning the current-defaults pointer.
    pub const fn current_defaults_session(&self) -> SessionId {
        self.current_defaults_session
    }

    /// Returns the version selected by the current-defaults pointer.
    pub const fn current_defaults_version(&self) -> SessionConfigurationDefaultsVersion {
        self.current_defaults_version
    }

    /// Returns the session identity owning the selected defaults record.
    pub const fn defaults_session(&self) -> SessionId {
        self.defaults_session
    }

    /// Returns the version identity on the selected defaults record.
    pub const fn defaults_version(&self) -> SessionConfigurationDefaultsVersion {
        self.defaults_version
    }

    /// Borrows the complete value on the selected defaults record.
    pub const fn defaults(&self) -> &SessionConfigurationDefaults {
        &self.defaults
    }

    /// Returns the session identity owning the current-placement pointer.
    pub const fn current_placement_session(&self) -> SessionId {
        self.current_placement_session
    }

    /// Returns the version selected by the current-placement pointer.
    pub const fn current_placement_version(&self) -> SessionPlacementVersion {
        self.current_placement_version
    }

    /// Returns the session identity owning the selected placement event.
    pub const fn placement_session(&self) -> SessionId {
        self.placement_session
    }

    /// Borrows the current placement event supplied for reconstitution.
    pub const fn current_placement(&self) -> &VersionedSessionPlacement {
        &self.current_placement
    }

    /// Reconstructs one complete current session without performing I/O,
    /// replay, identity generation, or lifecycle effects.
    pub fn reconstitute(self) -> Result<Session, SessionReconstitutionError> {
        let failure = if self.requested_session != self.stored_session {
            Some(SessionReconstitutionFailure::RequestedSessionMismatch)
        } else if self.current_defaults_session != self.stored_session {
            Some(SessionReconstitutionFailure::CurrentDefaultsSessionMismatch)
        } else if self.defaults_session != self.stored_session {
            Some(SessionReconstitutionFailure::DefaultsSessionMismatch)
        } else if self.current_defaults_version != self.defaults_version {
            Some(SessionReconstitutionFailure::CurrentDefaultsVersionMismatch)
        } else if self.current_placement_session != self.stored_session {
            Some(SessionReconstitutionFailure::CurrentPlacementSessionMismatch)
        } else if self.placement_session != self.stored_session {
            Some(SessionReconstitutionFailure::PlacementSessionMismatch)
        } else if self.current_placement_version != self.current_placement.version() {
            Some(SessionReconstitutionFailure::CurrentPlacementVersionMismatch)
        } else if let Some(failure) = session_provenance_failure(self.provenance) {
            Some(failure)
        } else if matches!(
            self.provenance.cause(),
            SessionCreationCause::Delegated { .. }
        ) && self.template_provenance.is_some()
        {
            Some(SessionReconstitutionFailure::DelegatedTemplateProvenance)
        } else {
            None
        };
        if let Some(failure) = failure {
            return Err(SessionReconstitutionError {
                input: Box::new(self),
                failure,
            });
        }

        Ok(Session {
            id: self.stored_session,
            creation_provenance: self.provenance,
            template_provenance: self.template_provenance,
            current_configuration_defaults: VersionedSessionConfigurationDefaults::reconstitute(
                self.defaults_version,
                self.defaults,
            ),
            current_placement: self.current_placement,
        })
    }
}

const fn session_provenance_failure(
    provenance: SessionCreationProvenance,
) -> Option<SessionReconstitutionFailure> {
    match (provenance.cause(), provenance.ancestry()) {
        (
            SessionCreationCause::Interactive,
            TranscriptAncestry::None | TranscriptAncestry::SingleSource { .. },
        )
        | (SessionCreationCause::Delegated { .. }, TranscriptAncestry::None)
        | (SessionCreationCause::ModuleDispatched { .. }, TranscriptAncestry::None) => None,
        (SessionCreationCause::Interactive, TranscriptAncestry::ImportedConversation { .. }) => {
            Some(SessionReconstitutionFailure::ImportedSessionSeedUnavailable)
        }
        (
            SessionCreationCause::Delegated { .. },
            TranscriptAncestry::SingleSource { .. }
            | TranscriptAncestry::ImportedConversation { .. },
        ) => Some(SessionReconstitutionFailure::DelegatedAncestryMismatch),
        (
            SessionCreationCause::ModuleDispatched { .. },
            TranscriptAncestry::SingleSource { .. }
            | TranscriptAncestry::ImportedConversation { .. },
        ) => Some(SessionReconstitutionFailure::ModuleDispatchedAncestryMismatch),
    }
}

/// Why complete typed durable facts cannot reconstruct a current session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionReconstitutionFailure {
    /// The requested semantic identity differs from the stored session.
    RequestedSessionMismatch,
    /// The current-defaults pointer belongs to another session.
    CurrentDefaultsSessionMismatch,
    /// The selected defaults record belongs to another session.
    DefaultsSessionMismatch,
    /// The pointer and selected defaults record name different versions.
    CurrentDefaultsVersionMismatch,
    /// The current-placement pointer belongs to another session.
    CurrentPlacementSessionMismatch,
    /// The selected placement event belongs to another session.
    PlacementSessionMismatch,
    /// The pointer and selected placement event name different versions.
    CurrentPlacementVersionMismatch,
    /// Imported ancestry requires the separate exact-prefix seed
    /// reconstitution seam.
    ImportedSessionSeedUnavailable,
    /// Delegated creation is independently constrained to no ancestry.
    DelegatedAncestryMismatch,
    /// Delegated creation cannot carry user-selected template provenance.
    DelegatedTemplateProvenance,
    /// Module-dispatched creation is independently constrained to no ancestry.
    ModuleDispatchedAncestryMismatch,
}

/// A failed current-session reconstitution retaining every typed input
/// unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionReconstitutionError {
    input: Box<SessionReconstitutionInput>,
    failure: SessionReconstitutionFailure,
}

impl SessionReconstitutionError {
    /// Returns why the complete projection could not be reconstructed.
    pub const fn failure(&self) -> SessionReconstitutionFailure {
        self.failure
    }

    /// Borrows the complete unchanged input.
    pub const fn input(&self) -> &SessionReconstitutionInput {
        &self.input
    }

    /// Returns the complete unchanged input and failure.
    pub fn into_parts(self) -> (SessionReconstitutionInput, SessionReconstitutionFailure) {
        (*self.input, self.failure)
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
#[derive(Clone, Debug)]
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
    pub fn into_parts(self) -> (CreateSession, InitialSession, CreateSessionAppliedResult) {
        (self.command, self.session, self.applied_result)
    }
}

/// Why a canonical command cannot yet form the baseline creation candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CreateSessionPreparationFailure {
    /// Trusted production and validation of a source transcript frontier is
    /// not available in this slice.
    TranscriptAncestryUnavailable,
    /// Delegated creation belongs to the spawning-request transaction family.
    DelegatedCreationRequiresSpawn,
}

/// A failed pre-commit preparation retaining every supplied input unchanged.
///
/// This is not an authoritative command rejection and does not claim the
/// durable command identity.
#[derive(Clone, Debug)]
pub struct CreateSessionPreparationError {
    session: SessionId,
    command: Box<CreateSession>,
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
        (self.session, *self.command, self.failure)
    }
}

impl CreateSession {
    /// Prepares the user-initiated, no-ancestry baseline for one transaction.
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
            (
                SessionCreationCause::Interactive | SessionCreationCause::ModuleDispatched { .. },
                TranscriptAncestry::None,
            ) => {}
            (
                SessionCreationCause::Interactive | SessionCreationCause::ModuleDispatched { .. },
                TranscriptAncestry::SingleSource { .. }
                | TranscriptAncestry::ImportedConversation { .. },
            ) => {
                return Err(CreateSessionPreparationError {
                    session,
                    command: Box::new(self),
                    failure: CreateSessionPreparationFailure::TranscriptAncestryUnavailable,
                });
            }
            (SessionCreationCause::Delegated { .. }, _) => {
                return Err(CreateSessionPreparationError {
                    session,
                    command: Box::new(self),
                    failure: CreateSessionPreparationFailure::DelegatedCreationRequiresSpawn,
                });
            }
        }

        let initial_session = InitialSession {
            id: session,
            provenance: self.provenance,
            template_provenance: self.template_provenance().cloned(),
            configuration_defaults: self.establish_initial_defaults(),
            placement: VersionedSessionPlacement::initial(self.placement.clone()),
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
#[derive(Clone, Debug)]
pub struct CreateSessionReconstitutionInput {
    command: CreateSession,
    result_session: SessionId,
    session: SessionId,
    provenance: SessionCreationProvenance,
    template_provenance: Option<SessionTemplateProvenance>,
    defaults_session: SessionId,
    defaults_version: crate::SessionConfigurationDefaultsVersion,
    defaults: SessionConfigurationDefaults,
    placement: VersionedSessionPlacement,
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
        Self::new_with_template_and_placement(
            command,
            result_session,
            session,
            provenance,
            None,
            defaults_session,
            defaults_version,
            defaults,
            VersionedSessionPlacement::initial(SessionPlacement::pathless()),
        )
    }

    /// Supplies complete stored facts including optional template provenance.
    #[allow(clippy::too_many_arguments)]
    pub const fn new_with_template_provenance(
        command: CreateSession,
        result_session: SessionId,
        session: SessionId,
        provenance: SessionCreationProvenance,
        template_provenance: Option<SessionTemplateProvenance>,
        defaults_session: SessionId,
        defaults_version: crate::SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
    ) -> Self {
        Self::new_with_template_and_placement(
            command,
            result_session,
            session,
            provenance,
            template_provenance,
            defaults_session,
            defaults_version,
            defaults,
            VersionedSessionPlacement::initial(SessionPlacement::pathless()),
        )
    }

    /// Supplies complete creation facts including placement event one.
    #[allow(clippy::too_many_arguments)]
    pub const fn new_with_template_and_placement(
        command: CreateSession,
        result_session: SessionId,
        session: SessionId,
        provenance: SessionCreationProvenance,
        template_provenance: Option<SessionTemplateProvenance>,
        defaults_session: SessionId,
        defaults_version: crate::SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
        placement: VersionedSessionPlacement,
    ) -> Self {
        Self {
            command,
            result_session,
            session,
            provenance,
            template_provenance,
            defaults_session,
            defaults_version,
            defaults,
            placement,
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

    /// Borrows the template provenance recorded by the session aggregate.
    pub const fn template_provenance(&self) -> Option<&SessionTemplateProvenance> {
        self.template_provenance.as_ref()
    }

    /// Returns the session that owns the stored initial defaults row.
    pub const fn defaults_session(&self) -> SessionId {
        self.defaults_session
    }

    /// Returns the stored initial defaults version.
    pub const fn defaults_version(&self) -> crate::SessionConfigurationDefaultsVersion {
        self.defaults_version
    }

    /// Borrows the stored initial defaults value.
    pub const fn defaults(&self) -> &SessionConfigurationDefaults {
        &self.defaults
    }

    /// Borrows placement event one stored with creation.
    pub const fn placement(&self) -> &VersionedSessionPlacement {
        &self.placement
    }

    /// Reconstructs the complete canonical creation without replaying effects.
    pub fn reconstitute(
        self,
    ) -> Result<ReconstitutedSessionCreation, CreateSessionReconstitutionError> {
        fn fail(
            input: CreateSessionReconstitutionInput,
            failure: CreateSessionReconstitutionFailure,
        ) -> CreateSessionReconstitutionError {
            CreateSessionReconstitutionError {
                input: Box::new(input),
                failure,
            }
        }

        if self.session != self.result_session {
            return Err(fail(
                self,
                CreateSessionReconstitutionFailure::SessionResultMismatch,
            ));
        }
        if self.command.provenance() != self.provenance {
            return Err(fail(
                self,
                CreateSessionReconstitutionFailure::ProvenanceMismatch,
            ));
        }
        if self.command.template_provenance() != self.template_provenance.as_ref() {
            return Err(fail(
                self,
                CreateSessionReconstitutionFailure::TemplateProvenanceMismatch,
            ));
        }
        if self.placement.version() != crate::SessionPlacementVersion::INITIAL
            || self.command.placement() != self.placement.placement()
        {
            return Err(fail(
                self,
                CreateSessionReconstitutionFailure::PlacementMismatch,
            ));
        }
        if self.defaults_session != self.session {
            return Err(fail(
                self,
                CreateSessionReconstitutionFailure::DefaultsSessionMismatch,
            ));
        }
        match (self.provenance.cause(), self.provenance.ancestry()) {
            (
                SessionCreationCause::Interactive | SessionCreationCause::ModuleDispatched { .. },
                TranscriptAncestry::None,
            ) => {}
            (
                SessionCreationCause::Interactive | SessionCreationCause::ModuleDispatched { .. },
                TranscriptAncestry::SingleSource { .. }
                | TranscriptAncestry::ImportedConversation { .. },
            ) => {
                return Err(fail(
                    self,
                    CreateSessionReconstitutionFailure::TranscriptAncestryUnavailable,
                ));
            }
            (SessionCreationCause::Delegated { .. }, _) => {
                return Err(fail(
                    self,
                    CreateSessionReconstitutionFailure::DelegatedCreationRequiresSpawn,
                ));
            }
        }
        if self.defaults_version != crate::SessionConfigurationDefaultsVersion::first() {
            return Err(fail(
                self,
                CreateSessionReconstitutionFailure::DefaultsVersionIsNotFirst,
            ));
        }
        if *self.command.initial_configuration_defaults() != self.defaults {
            return Err(fail(
                self,
                CreateSessionReconstitutionFailure::DefaultsMismatch,
            ));
        }

        Ok(ReconstitutedSessionCreation {
            command: self.command,
            session: InitialSession {
                id: self.session,
                provenance: self.provenance,
                template_provenance: self.template_provenance,
                configuration_defaults: VersionedSessionConfigurationDefaults::establish(
                    self.defaults,
                ),
                placement: self.placement,
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
    /// The session's template name/digest differs from the canonical command.
    TemplateProvenanceMismatch,
    /// Placement event one differs from the canonical creation payload.
    PlacementMismatch,
    /// The stored initial defaults row belongs to a different session.
    DefaultsSessionMismatch,
    /// Trusted source-frontier production is unavailable for this slice.
    TranscriptAncestryUnavailable,
    /// Delegated creation belongs to its distinct spawning-request family.
    DelegatedCreationRequiresSpawn,
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
#[derive(Clone, Debug)]
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
pub(crate) const fn test_frontier(value: u128) -> TranscriptFrontier {
    TranscriptFrontier {
        boundary: uuid::Uuid::from_u128(value),
    }
}

#[cfg(test)]
mod tests {
    use expect_test::expect;
    use signalbox_expect_table::table;

    use super::{
        CreateSession, CreateSessionFromImportedFrontier, CreateSessionPreparationFailure,
        CreateSessionReconstitutionFailure, CreateSessionReconstitutionInput,
        ImportedSessionRelationship, ImportedSessionSeed, SessionCreationCause,
        SessionCreationProvenance, SessionReconstitutionFailure, SessionReconstitutionInput,
        TranscriptAncestry, test_frontier,
    };
    use crate::imported_conversation::test_imported_frontier;
    use crate::test_support::{
        command_id, context_frontier_id, direct, imported_conversation_id,
        imported_transcript_entry_id, session_id, tool_request_id,
    };
    use crate::{
        ImportedTranscriptPosition, ModelSelectionRequest, SessionConfigurationDefaults,
        SessionConfigurationDefaultsVersion, SessionPlacement, SessionPlacementVersion,
        SessionTemplateContentDigest, SessionTemplateName, SessionTemplateProvenance,
        VersionedSessionConfigurationDefaults, VersionedSessionPlacement,
    };

    fn defaults(value: u128) -> SessionConfigurationDefaults {
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(direct(value)))
    }

    fn user_initiated_empty() -> SessionCreationProvenance {
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None)
    }

    /// Canonical spawning-request identity for delegated-session fixtures.
    fn delegated_spawning_request() -> crate::ToolRequestId {
        tool_request_id(2)
    }

    fn template_provenance(name: &str, digest_byte: u8) -> SessionTemplateProvenance {
        SessionTemplateProvenance::new(
            SessionTemplateName::try_new(name.to_owned()).expect("fixture template name is valid"),
            SessionTemplateContentDigest::from_bytes([digest_byte; 32]),
        )
    }

    #[derive(Debug)]
    #[allow(
        dead_code,
        reason = "the table renderer reads every field through the Debug derive"
    )]
    struct ReconstitutionFailureRow {
        perturbed_stored_fact: &'static str,
        failure: String,
    }

    fn matching_session_input(
        session: crate::SessionId,
        provenance: SessionCreationProvenance,
        version: SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
    ) -> SessionReconstitutionInput {
        SessionReconstitutionInput::new(
            session,
            session,
            provenance,
            session,
            version,
            session,
            version,
            defaults,
            crate::SessionPlacementReconstitutionFacts {
                current_pointer_session: session,
                current_pointer_version: SessionPlacementVersion::INITIAL,
                selected_event_session: session,
                selected_event: VersionedSessionPlacement::initial(SessionPlacement::pathless()),
            },
        )
    }

    /// S01: a user-initiated session with explicitly empty
    /// ancestry is complete creation provenance for an empty conversation.
    #[test]
    fn s01_user_initiated_with_no_ancestry_is_complete_provenance() {
        let provenance = SessionCreationProvenance::new(
            SessionCreationCause::Interactive,
            TranscriptAncestry::None,
        );

        assert_eq!(provenance.cause(), SessionCreationCause::Interactive);
        assert_eq!(provenance.ancestry(), TranscriptAncestry::None);
    }

    /// S17: a user-created fork records the exact
    /// immutable source session and source frontier it was seeded from.
    #[test]
    fn s17_fork_provenance_records_exact_source_and_frontier() {
        let source_session = session_id(1);
        let source_frontier = test_frontier(2);
        let provenance = SessionCreationProvenance::new(
            SessionCreationCause::Interactive,
            TranscriptAncestry::SingleSource {
                source_session,
                source_frontier,
            },
        );

        assert_eq!(provenance.cause(), SessionCreationCause::Interactive);
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

    /// S28: imported ancestry retains the exact imported
    /// boundary and resume/fork relationship without duplicating the
    /// conversation identity outside the frontier.
    #[test]
    fn s28_imported_ancestry_records_exact_frontier_and_relationship() {
        let source_frontier = test_imported_frontier(
            imported_conversation_id(1),
            imported_transcript_entry_id(2),
            ImportedTranscriptPosition::try_from_u64(3).expect("positive position"),
        );
        let provenance = SessionCreationProvenance::new(
            SessionCreationCause::Interactive,
            TranscriptAncestry::ImportedConversation {
                source_frontier,
                relationship: ImportedSessionRelationship::Resume,
            },
        );

        let TranscriptAncestry::ImportedConversation {
            source_frontier: carried_frontier,
            relationship,
        } = provenance.ancestry()
        else {
            panic!("imported provenance must retain its selected source");
        };
        assert_eq!(carried_frontier, source_frontier);
        assert_eq!(carried_frontier.conversation(), imported_conversation_id(1));
        assert_eq!(relationship, ImportedSessionRelationship::Resume);
        assert_ne!(
            provenance.ancestry(),
            TranscriptAncestry::ImportedConversation {
                source_frontier,
                relationship: ImportedSessionRelationship::Fork,
            }
        );
    }

    /// S28: the separate seed record retains the exact
    /// session and generated context-frontier identities; equal semantic
    /// content cannot substitute another frontier identity at this boundary.
    #[test]
    fn s28_imported_seed_keeps_exact_local_frontier_identity() {
        let seed = ImportedSessionSeed {
            session: session_id(1),
            seed_frontier: context_frontier_id(2),
        };

        assert_eq!(seed.session(), session_id(1));
        assert_eq!(seed.seed_frontier(), context_frontier_id(2));
        assert_ne!(
            seed,
            ImportedSessionSeed {
                session: session_id(1),
                seed_frontier: context_frontier_id(3),
            }
        );
    }

    /// S28: the baseline current-session reconstitution seam cannot
    /// accept imported ancestry without the separate exact-prefix seed facts.
    #[test]
    fn s28_current_session_requires_imported_seed_reconstitution() {
        let provenance = SessionCreationProvenance::new(
            SessionCreationCause::Interactive,
            TranscriptAncestry::ImportedConversation {
                source_frontier: test_imported_frontier(
                    imported_conversation_id(1),
                    imported_transcript_entry_id(2),
                    ImportedTranscriptPosition::first(),
                ),
                relationship: ImportedSessionRelationship::Fork,
            },
        );
        let input = matching_session_input(
            session_id(3),
            provenance,
            SessionConfigurationDefaultsVersion::first(),
            defaults(4),
        );

        let error = input
            .clone()
            .reconstitute()
            .expect_err("imported ancestry without its exact seed must fail closed");

        assert_eq!(
            error.failure(),
            SessionReconstitutionFailure::ImportedSessionSeedUnavailable
        );
        assert_eq!(error.input(), &input);
    }

    /// S01 / S17: the same user-initiated cause pairs with empty
    /// and single-source ancestry, so neither fact is a proxy for the other.
    #[test]
    fn s01_s17_cause_and_ancestry_vary_independently() {
        let empty = SessionCreationProvenance::new(
            SessionCreationCause::Interactive,
            TranscriptAncestry::None,
        );
        let fork = SessionCreationProvenance::new(
            SessionCreationCause::Interactive,
            TranscriptAncestry::SingleSource {
                source_session: session_id(1),
                source_frontier: test_frontier(2),
            },
        );

        assert_eq!(empty.cause(), fork.cause());
        assert_ne!(empty.ancestry(), fork.ancestry());
        assert_ne!(empty, fork);
    }

    /// S17: ancestry equality is exact over both the source session
    /// and the source frontier, and an explicit empty ancestry never equals a
    /// single-source one.
    #[test]
    fn s17_ancestry_equality_is_exact_over_source_and_frontier() {
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

    /// S01: a complete matching projection
    /// reconstructs one owned current session with exact immutable provenance
    /// and the complete later defaults version selected by the pointer.
    #[test]
    fn s01_matching_current_session_reconstitutes_whole() {
        let version = SessionConfigurationDefaultsVersion::first()
            .checked_next()
            .expect("version two exists");
        let input =
            matching_session_input(session_id(1), user_initiated_empty(), version, defaults(2));

        let session = input
            .reconstitute()
            .expect("complete matching current-session facts must reconstruct");

        assert_eq!(session.id(), session_id(1));
        assert_eq!(session.creation_provenance(), user_initiated_empty());
        assert_eq!(session.current_configuration_defaults().version(), version);
        assert_eq!(
            session.current_configuration_defaults().defaults(),
            &defaults(2)
        );
        assert_eq!(session.clone(), session);

        let changed_defaults =
            matching_session_input(session_id(1), user_initiated_empty(), version, defaults(3))
                .reconstitute()
                .expect("a different complete defaults value must also reconstruct");
        assert_ne!(session, changed_defaults);
    }

    /// the general current-session seam retains a complete typed
    /// single-source provenance value. It does not repeat the narrower live
    /// CreateSession preparation slice's frontier-availability check.
    #[test]
    fn current_session_reconstitution_retains_typed_provenance() {
        let provenance = SessionCreationProvenance::new(
            SessionCreationCause::Interactive,
            TranscriptAncestry::SingleSource {
                source_session: session_id(1),
                source_frontier: test_frontier(2),
            },
        );
        let input = matching_session_input(
            session_id(3),
            provenance,
            SessionConfigurationDefaultsVersion::first(),
            defaults(4),
        );

        let session = input
            .reconstitute()
            .expect("already-typed provenance remains valid at this seam");

        assert_eq!(session.creation_provenance(), provenance);
    }

    /// The complete stored facts backing one current-session projection,
    /// mirroring [`SessionReconstitutionInput::new`] field for field so a
    /// test perturbs exactly the named facts it cares about
    /// (TS-4, TS-5).
    #[derive(Clone)]
    struct CurrentSessionFacts {
        requested_session: crate::SessionId,
        stored_session: crate::SessionId,
        provenance: SessionCreationProvenance,
        template_provenance: Option<SessionTemplateProvenance>,
        current_defaults_session: crate::SessionId,
        current_defaults_version: SessionConfigurationDefaultsVersion,
        defaults_session: crate::SessionId,
        defaults_version: SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
        current_placement_session: crate::SessionId,
        current_placement_version: SessionPlacementVersion,
        placement_session: crate::SessionId,
        placement: VersionedSessionPlacement,
    }

    impl CurrentSessionFacts {
        /// The canonical matching projection: every stored identity is
        /// `session` and the pointer selects the first defaults version.
        fn matching(session: crate::SessionId) -> Self {
            Self {
                requested_session: session,
                stored_session: session,
                provenance: user_initiated_empty(),
                template_provenance: None,
                current_defaults_session: session,
                current_defaults_version: SessionConfigurationDefaultsVersion::first(),
                defaults_session: session,
                defaults_version: SessionConfigurationDefaultsVersion::first(),
                defaults: defaults(3),
                current_placement_session: session,
                current_placement_version: SessionPlacementVersion::INITIAL,
                placement_session: session,
                placement: VersionedSessionPlacement::initial(SessionPlacement::pathless()),
            }
        }

        fn input(self) -> SessionReconstitutionInput {
            SessionReconstitutionInput::new_with_template_and_placement(
                self.requested_session,
                self.stored_session,
                self.provenance,
                self.template_provenance,
                self.current_defaults_session,
                self.current_defaults_version,
                self.defaults_session,
                self.defaults_version,
                self.defaults,
                crate::SessionPlacementReconstitutionFacts {
                    current_pointer_session: self.current_placement_session,
                    current_pointer_version: self.current_placement_version,
                    selected_event_session: self.placement_session,
                    selected_event: self.placement,
                },
            )
        }
    }

    /// Reconstitutes the facts, asserting the rejection returns the complete
    /// unchanged typed projection, and returns the failure.
    #[track_caller]
    fn current_session_reconstitution_failure(
        facts: CurrentSessionFacts,
    ) -> SessionReconstitutionFailure {
        let input = facts.input();
        let error = input
            .clone()
            .reconstitute()
            .expect_err("cross-wired current-session facts must fail closed");
        let failure = error.failure();
        assert_eq!(error.input(), &input);
        let (returned, returned_failure) = error.into_parts();
        assert_eq!(returned, input);
        assert_eq!(returned_failure, failure);
        failure
    }

    /// S18: delegated construction fixes exact cause and no ancestry.
    #[test]
    fn s18_delegated_helper_constructs_no_ancestry() {
        let spawning_request = delegated_spawning_request();
        let provenance = SessionCreationProvenance::delegated(spawning_request);

        assert_eq!(
            provenance.cause(),
            SessionCreationCause::Delegated { spawning_request }
        );
        assert_eq!(provenance.ancestry(), TranscriptAncestry::None);
    }

    /// S18: matching delegated current-session facts retain the
    /// exact spawning request and no transcript ancestry.
    #[test]
    fn s18_current_session_reconstitutes_delegated_no_ancestry() {
        let spawning_request = delegated_spawning_request();
        let provenance = SessionCreationProvenance::delegated(spawning_request);
        let session = CurrentSessionFacts {
            provenance,
            ..CurrentSessionFacts::matching(session_id(1))
        }
        .input()
        .reconstitute()
        .expect("matching delegated current-session facts reconstitute");

        assert_eq!(session.creation_provenance(), provenance);
        assert_eq!(
            session.creation_provenance().cause(),
            SessionCreationCause::Delegated { spawning_request }
        );
        assert_eq!(
            session.creation_provenance().ancestry(),
            TranscriptAncestry::None
        );
    }

    /// S18: delegated creation cannot retain user-selected template
    /// provenance through the public current-session seam.
    #[test]
    fn s18_current_session_rejects_delegated_template_provenance() {
        let failure = current_session_reconstitution_failure(CurrentSessionFacts {
            provenance: SessionCreationProvenance::delegated(delegated_spawning_request()),
            template_provenance: Some(template_provenance("reviewer", 2)),
            ..CurrentSessionFacts::matching(session_id(1))
        });

        assert_eq!(
            failure,
            SessionReconstitutionFailure::DelegatedTemplateProvenance
        );
    }

    /// S18: delegated current sessions reject native ancestry.
    #[test]
    fn s18_current_session_rejects_delegated_native_ancestry() {
        let spawning_request = delegated_spawning_request();
        let provenance = SessionCreationProvenance::new(
            SessionCreationCause::Delegated { spawning_request },
            TranscriptAncestry::SingleSource {
                source_session: session_id(3),
                source_frontier: test_frontier(4),
            },
        );
        let failure = current_session_reconstitution_failure(CurrentSessionFacts {
            provenance,
            ..CurrentSessionFacts::matching(session_id(1))
        });

        assert_eq!(
            failure,
            SessionReconstitutionFailure::DelegatedAncestryMismatch
        );
    }

    /// S18: delegated current sessions reject imported ancestry.
    #[test]
    fn s18_current_session_rejects_delegated_imported_ancestry() {
        let spawning_request = delegated_spawning_request();
        let provenance = SessionCreationProvenance::new(
            SessionCreationCause::Delegated { spawning_request },
            TranscriptAncestry::ImportedConversation {
                source_frontier: test_imported_frontier(
                    imported_conversation_id(3),
                    imported_transcript_entry_id(4),
                    ImportedTranscriptPosition::first(),
                ),
                relationship: ImportedSessionRelationship::Fork,
            },
        );
        let failure = current_session_reconstitution_failure(CurrentSessionFacts {
            provenance,
            ..CurrentSessionFacts::matching(session_id(1))
        });

        assert_eq!(
            failure,
            SessionReconstitutionFailure::DelegatedAncestryMismatch
        );
    }

    /// S01: every requested/stored identity, defaults
    /// pointer/record identity, placement pointer/event identity, or
    /// selected-version mismatch fails closed and
    /// returns the complete unchanged typed projection.
    #[test]
    fn s01_current_session_rejects_cross_wired_facts() {
        let matching = CurrentSessionFacts::matching(session_id(1));
        let second_version = SessionConfigurationDefaultsVersion::first()
            .checked_next()
            .expect("version two exists");
        let second_placement_version = SessionPlacementVersion::INITIAL
            .next()
            .expect("placement version two exists");

        let requested_other_session = current_session_reconstitution_failure(CurrentSessionFacts {
            requested_session: session_id(2),
            ..matching.clone()
        });
        assert_eq!(
            requested_other_session,
            SessionReconstitutionFailure::RequestedSessionMismatch
        );

        let pointer_owned_elsewhere = current_session_reconstitution_failure(CurrentSessionFacts {
            current_defaults_session: session_id(2),
            ..matching.clone()
        });
        assert_eq!(
            pointer_owned_elsewhere,
            SessionReconstitutionFailure::CurrentDefaultsSessionMismatch
        );

        let defaults_owned_elsewhere =
            current_session_reconstitution_failure(CurrentSessionFacts {
                defaults_session: session_id(2),
                ..matching.clone()
            });
        assert_eq!(
            defaults_owned_elsewhere,
            SessionReconstitutionFailure::DefaultsSessionMismatch
        );

        let pointer_and_record_versions_torn =
            current_session_reconstitution_failure(CurrentSessionFacts {
                current_defaults_version: second_version,
                ..matching.clone()
            });
        assert_eq!(
            pointer_and_record_versions_torn,
            SessionReconstitutionFailure::CurrentDefaultsVersionMismatch
        );

        let placement_pointer_owned_elsewhere =
            current_session_reconstitution_failure(CurrentSessionFacts {
                current_placement_session: session_id(2),
                ..matching.clone()
            });
        assert_eq!(
            placement_pointer_owned_elsewhere,
            SessionReconstitutionFailure::CurrentPlacementSessionMismatch
        );

        let placement_event_owned_elsewhere =
            current_session_reconstitution_failure(CurrentSessionFacts {
                placement_session: session_id(2),
                ..matching.clone()
            });
        assert_eq!(
            placement_event_owned_elsewhere,
            SessionReconstitutionFailure::PlacementSessionMismatch
        );

        let placement_pointer_and_event_versions_torn =
            current_session_reconstitution_failure(CurrentSessionFacts {
                current_placement_version: second_placement_version,
                ..matching.clone()
            });
        assert_eq!(
            placement_pointer_and_event_versions_torn,
            SessionReconstitutionFailure::CurrentPlacementVersionMismatch
        );

        expect![[r#"
            ┌──────────────────────────────────┬────────────────────────────────┐
            │ perturbed_stored_fact            │ failure                        │
            ├──────────────────────────────────┼────────────────────────────────┤
            │ requested session differs        │ RequestedSessionMismatch       │
            │ defaults pointer owned elsewhere │ CurrentDefaultsSessionMismatch │
            │ defaults record owned elsewhere  │ DefaultsSessionMismatch        │
            │ pointer and record versions torn │ CurrentDefaultsVersionMismatch │
            └──────────────────────────────────┴────────────────────────────────┘
        "#]]
        .assert_eq(&table([
            ReconstitutionFailureRow {
                perturbed_stored_fact: "requested session differs",
                failure: format!("{requested_other_session:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "defaults pointer owned elsewhere",
                failure: format!("{pointer_owned_elsewhere:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "defaults record owned elsewhere",
                failure: format!("{defaults_owned_elsewhere:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "pointer and record versions torn",
                failure: format!("{pointer_and_record_versions_torn:?}"),
            },
        ]));
    }

    /// S01: the creation payload couples the durable command
    /// identity, both independent provenance facts, and one complete
    /// unversioned defaults payload.
    #[test]
    fn s01_create_session_couples_command_provenance_and_defaults() {
        let provenance = user_initiated_empty();
        let create = CreateSession::new(command_id(1), provenance, defaults(2));

        assert_eq!(create.command_id(), command_id(1));
        assert_eq!(create.provenance(), provenance);
        assert_eq!(create.initial_configuration_defaults(), &defaults(2));
    }

    /// S01: session creation establishes exactly version one of the carried
    /// model-selection defaults payload.
    #[test]
    fn s01_creation_establishes_version_one_of_the_carried_defaults() {
        let create = CreateSession::new(command_id(1), user_initiated_empty(), defaults(2));

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

    /// S01 / S17: initial defaults never join the provenance facts,
    /// and replacing established defaults installs a later version while both
    /// provenance facts compare unchanged.
    #[test]
    fn s01_s17_defaults_are_not_a_third_provenance_fact() {
        let provenance = user_initiated_empty();
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

    /// S01 / S17: the canonical comparison payload is every
    /// caller-supplied semantic field except the command identifier itself, so
    /// payloads that differ only in `command_id` compare equal (equal replay),
    /// while any provenance or defaults difference is a distinct payload
    /// (conflicting reuse of one identifier is then detectable).
    #[test]
    fn s01_s17_create_session_comparison_payload_excludes_command_id() {
        let fork = SessionCreationProvenance::new(
            SessionCreationCause::Interactive,
            TranscriptAncestry::SingleSource {
                source_session: session_id(1),
                source_frontier: test_frontier(2),
            },
        );
        let create = CreateSession::new(command_id(3), user_initiated_empty(), defaults(4));

        assert_eq!(
            create,
            CreateSession::new(command_id(3), user_initiated_empty(), defaults(4))
        );
        assert_eq!(
            create,
            CreateSession::new(command_id(5), user_initiated_empty(), defaults(4))
        );
        assert_ne!(create, CreateSession::new(command_id(3), fork, defaults(4)));
        assert_ne!(
            create,
            CreateSession::new(command_id(3), user_initiated_empty(), defaults(6))
        );
    }

    /// replay of template creation is keyed by requested name, so a
    /// catalog edit under that name remains equal while another name or the
    /// explicit creation mode is conflicting reuse.
    #[test]
    fn template_creation_comparison_uses_name_and_creation_mode() {
        let original = CreateSession::new_from_template(
            command_id(1),
            user_initiated_empty(),
            template_provenance("reviewer", 2),
            defaults(3),
        );
        let edited_same_name = CreateSession::new_from_template(
            command_id(1),
            user_initiated_empty(),
            template_provenance("reviewer", 4),
            defaults(5),
        );
        let different_name = CreateSession::new_from_template(
            command_id(1),
            user_initiated_empty(),
            template_provenance("planner", 2),
            defaults(3),
        );
        let explicit = CreateSession::new(command_id(1), user_initiated_empty(), defaults(3));

        assert_eq!(original, edited_same_name);
        assert_ne!(original, different_name);
        assert_ne!(original, explicit);
    }

    /// preparation copies the resolved bundle and immutable
    /// name/digest provenance into initial session state.
    #[test]
    fn template_preparation_copies_defaults_and_provenance() {
        let provenance = template_provenance("reviewer", 2);
        let create = CreateSession::new_from_template(
            command_id(1),
            user_initiated_empty(),
            provenance.clone(),
            defaults(3),
        );

        let prepared = create
            .prepare(session_id(4))
            .expect("user creation without ancestry prepares");

        assert_eq!(prepared.command().template_provenance(), Some(&provenance));
        assert_eq!(prepared.session().template_provenance(), Some(&provenance));
        assert_eq!(
            prepared.session().configuration_defaults().defaults(),
            &defaults(3)
        );
    }

    /// replay reconstitution requires command and session storage to
    /// repeat the same name and digest exactly.
    #[test]
    fn template_reconstitution_rejects_missing_provenance() {
        let provenance = template_provenance("reviewer", 2);
        let create = CreateSession::new_from_template(
            command_id(1),
            user_initiated_empty(),
            provenance,
            defaults(3),
        );
        let error = CreateSessionReconstitutionInput::new_with_template_provenance(
            create,
            session_id(4),
            session_id(4),
            user_initiated_empty(),
            None,
            session_id(4),
            SessionConfigurationDefaultsVersion::first(),
            defaults(3),
        )
        .reconstitute()
        .expect_err("stored template provenance cannot be absent");

        assert_eq!(
            error.failure(),
            CreateSessionReconstitutionFailure::TemplateProvenanceMismatch
        );
    }

    /// S28: imported-session command comparison excludes
    /// only command identity; its conversation, boundary, relationship, and
    /// defaults all remain replay-significant.
    #[test]
    fn s28_imported_creation_comparison_payload_is_complete() {
        let conversation = imported_conversation_id(1);
        let frontier = test_imported_frontier(
            conversation,
            imported_transcript_entry_id(2),
            ImportedTranscriptPosition::first(),
        );
        let create = CreateSessionFromImportedFrontier::new(
            command_id(3),
            frontier,
            ImportedSessionRelationship::Resume,
            defaults(4),
        );

        assert_eq!(
            create,
            CreateSessionFromImportedFrontier::new(
                command_id(5),
                frontier,
                ImportedSessionRelationship::Resume,
                defaults(4),
            )
        );
        assert_ne!(
            create,
            CreateSessionFromImportedFrontier::new(
                command_id(3),
                test_imported_frontier(
                    imported_conversation_id(6),
                    imported_transcript_entry_id(2),
                    ImportedTranscriptPosition::first(),
                ),
                ImportedSessionRelationship::Resume,
                defaults(4),
            )
        );
        assert_ne!(
            create,
            CreateSessionFromImportedFrontier::new(
                command_id(3),
                frontier,
                ImportedSessionRelationship::Fork,
                defaults(4),
            )
        );
        assert_ne!(
            create,
            CreateSessionFromImportedFrontier::new(
                command_id(3),
                frontier,
                ImportedSessionRelationship::Resume,
                defaults(7),
            )
        );
        assert_eq!(create.command_id(), command_id(3));
        assert_eq!(create.imported_conversation(), conversation);
        assert_eq!(create.imported_frontier(), frontier);
        assert_eq!(create.relationship(), ImportedSessionRelationship::Resume);
        assert_eq!(create.initial_configuration_defaults(), &defaults(4));
        assert_eq!(
            create.establish_initial_defaults().version(),
            SessionConfigurationDefaultsVersion::first()
        );
    }

    /// S01: preparation seals the exact
    /// command, hub-supplied session, independent provenance, defaults version
    /// one, and matching replay result without claiming a commit.
    #[test]
    fn s01_preparation_couples_complete_creation() {
        let create = CreateSession::new(command_id(1), user_initiated_empty(), defaults(2));

        let prepared = create
            .clone()
            .prepare(session_id(3))
            .expect("the empty user-initiated baseline is preparable");

        assert_eq!(prepared.command().command_id(), command_id(1));
        assert_eq!(prepared.command(), &create);
        assert_eq!(prepared.session().id(), session_id(3));
        assert_eq!(prepared.session().provenance(), user_initiated_empty());
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
            SessionCreationCause::Interactive,
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

    /// S18: ordinary CreateSession cannot forge the delegated
    /// creation family owned by the spawning-request transaction.
    #[test]
    fn s18_create_session_rejects_delegated_creation() {
        let command = command_id(3);
        let session = session_id(5);
        let provenance = SessionCreationProvenance::delegated(delegated_spawning_request());
        let create = CreateSession::new(command, provenance, defaults(4));

        let error = create
            .prepare(session)
            .expect_err("ordinary creation cannot claim delegated provenance");

        assert_eq!(
            error.failure(),
            CreateSessionPreparationFailure::DelegatedCreationRequiresSpawn
        );
        assert_eq!(error.session(), session);
        assert_eq!(error.command().command_id(), command);
        assert_eq!(error.command().provenance(), provenance);
    }

    /// S01: complete matching durable facts
    /// reconstruct the same canonical initial session and typed replay result
    /// without producing a pre-commit candidate.
    #[test]
    fn s01_matching_creation_reconstitutes_whole() {
        let create = CreateSession::new(command_id(1), user_initiated_empty(), defaults(2));
        let input = CreateSessionReconstitutionInput::new(
            create.clone(),
            session_id(3),
            session_id(3),
            user_initiated_empty(),
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
        assert_eq!(reconstituted.session().provenance(), user_initiated_empty());
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

    /// The complete stored facts backing one applied creation, mirroring
    /// [`CreateSessionReconstitutionInput::new`] field for field so a test
    /// perturbs exactly the named facts it cares about
    /// (TS-4, TS-5).
    #[derive(Clone)]
    struct CreationFacts {
        command: CreateSession,
        result_session: crate::SessionId,
        session: crate::SessionId,
        provenance: SessionCreationProvenance,
        defaults_session: crate::SessionId,
        defaults_version: SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
    }

    impl CreationFacts {
        /// The canonical stored facts matching an applied `command`: every
        /// stored identity is `session`, the stored provenance and defaults
        /// repeat the command payload, and creation established version one.
        fn matching(command: CreateSession, session: crate::SessionId) -> Self {
            Self {
                result_session: session,
                session,
                provenance: command.provenance(),
                defaults_session: session,
                defaults_version: SessionConfigurationDefaultsVersion::first(),
                defaults: command.initial_configuration_defaults().clone(),
                command,
            }
        }

        fn input(self) -> CreateSessionReconstitutionInput {
            CreateSessionReconstitutionInput::new(
                self.command,
                self.result_session,
                self.session,
                self.provenance,
                self.defaults_session,
                self.defaults_version,
                self.defaults,
            )
        }
    }

    /// Reconstitutes the facts, asserting the rejection retains the complete
    /// unchanged typed projection, and returns the failure.
    #[track_caller]
    fn creation_reconstitution_failure(facts: CreationFacts) -> CreateSessionReconstitutionFailure {
        let error = facts
            .clone()
            .input()
            .reconstitute()
            .expect_err("cross-wired durable facts must fail closed");
        let failure = error.failure();
        assert_creation_input_is_unchanged(error.input(), &facts);
        let (returned, returned_failure) = error.into_parts();
        assert_creation_input_is_unchanged(&returned, &facts);
        assert_eq!(returned_failure, failure);
        failure
    }

    /// S18: ordinary creation reconstitution cannot claim the
    /// delegated provenance family owned by the spawning-request transaction.
    #[test]
    fn s18_creation_reconstitution_rejects_delegated_provenance() {
        let command = CreateSession::new(
            command_id(1),
            SessionCreationProvenance::delegated(delegated_spawning_request()),
            defaults(2),
        );
        let failure =
            creation_reconstitution_failure(CreationFacts::matching(command, session_id(3)));

        assert_eq!(
            failure,
            CreateSessionReconstitutionFailure::DelegatedCreationRequiresSpawn
        );
    }

    #[track_caller]
    fn assert_creation_input_is_unchanged(
        input: &CreateSessionReconstitutionInput,
        facts: &CreationFacts,
    ) {
        assert_eq!(input.command().command_id(), facts.command.command_id());
        assert_eq!(input.command(), &facts.command);
        assert_eq!(input.result_session(), facts.result_session);
        assert_eq!(input.session(), facts.session);
        assert_eq!(input.provenance(), facts.provenance);
        assert_eq!(input.defaults_session(), facts.defaults_session);
        assert_eq!(input.defaults_version(), facts.defaults_version);
        assert_eq!(input.defaults(), &facts.defaults);
    }

    /// S01: every cross-wired session, result,
    /// provenance, or defaults shape fails closed and retains the complete
    /// unchanged typed projection.
    #[test]
    fn s01_reconstitution_rejects_cross_wired_facts() {
        let create = CreateSession::new(command_id(1), user_initiated_empty(), defaults(2));
        let matching = CreationFacts::matching(create, session_id(3));
        let second_version = SessionConfigurationDefaultsVersion::first()
            .checked_next()
            .expect("version two exists");
        let fork = SessionCreationProvenance::new(
            SessionCreationCause::Interactive,
            TranscriptAncestry::SingleSource {
                source_session: session_id(10),
                source_frontier: test_frontier(11),
            },
        );
        let fork_create = CreateSession::new(command_id(1), fork, defaults(2));

        let cross_wired_result = creation_reconstitution_failure(CreationFacts {
            result_session: session_id(4),
            ..matching.clone()
        });
        assert_eq!(
            cross_wired_result,
            CreateSessionReconstitutionFailure::SessionResultMismatch
        );

        let replaced_provenance = creation_reconstitution_failure(CreationFacts {
            provenance: user_initiated_empty(),
            ..CreationFacts::matching(fork_create.clone(), session_id(3))
        });
        assert_eq!(
            replaced_provenance,
            CreateSessionReconstitutionFailure::ProvenanceMismatch
        );

        let unvalidated_ancestry =
            creation_reconstitution_failure(CreationFacts::matching(fork_create, session_id(3)));
        assert_eq!(
            unvalidated_ancestry,
            CreateSessionReconstitutionFailure::TranscriptAncestryUnavailable
        );

        let cross_wired_defaults_owner = creation_reconstitution_failure(CreationFacts {
            defaults_session: session_id(9),
            ..matching.clone()
        });
        assert_eq!(
            cross_wired_defaults_owner,
            CreateSessionReconstitutionFailure::DefaultsSessionMismatch
        );

        let later_defaults_version = creation_reconstitution_failure(CreationFacts {
            defaults_version: second_version,
            ..matching.clone()
        });
        assert_eq!(
            later_defaults_version,
            CreateSessionReconstitutionFailure::DefaultsVersionIsNotFirst
        );

        let replaced_defaults = creation_reconstitution_failure(CreationFacts {
            defaults: defaults(5),
            ..matching.clone()
        });
        assert_eq!(
            replaced_defaults,
            CreateSessionReconstitutionFailure::DefaultsMismatch
        );

        expect![[r#"
            ┌────────────────────────────────────┬───────────────────────────────┐
            │ perturbed_stored_fact              │ failure                       │
            ├────────────────────────────────────┼───────────────────────────────┤
            │ result session cross-wired         │ SessionResultMismatch         │
            │ stored provenance replaced         │ ProvenanceMismatch            │
            │ single-source ancestry unvalidated │ TranscriptAncestryUnavailable │
            │ defaults owner cross-wired         │ DefaultsSessionMismatch       │
            │ defaults version is not first      │ DefaultsVersionIsNotFirst     │
            │ stored defaults differ             │ DefaultsMismatch              │
            └────────────────────────────────────┴───────────────────────────────┘
        "#]]
        .assert_eq(&table([
            ReconstitutionFailureRow {
                perturbed_stored_fact: "result session cross-wired",
                failure: format!("{cross_wired_result:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "stored provenance replaced",
                failure: format!("{replaced_provenance:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "single-source ancestry unvalidated",
                failure: format!("{unvalidated_ancestry:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "defaults owner cross-wired",
                failure: format!("{cross_wired_defaults_owner:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "defaults version is not first",
                failure: format!("{later_defaults_version:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "stored defaults differ",
                failure: format!("{replaced_defaults:?}"),
            },
        ]));
    }
}
