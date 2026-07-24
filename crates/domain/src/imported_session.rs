//! Checked imported-frontier session construction and reconstitution.
//!
//! `docs/spec/sessions-and-transcript.md` is normative. This module keeps the
//! imported aggregate as content authority while materializing an exact,
//! separately identified Signalbox context frontier.

use std::collections::BTreeSet;

use crate::{
    ContextFrontierId, CreateSessionFromImportedFrontier, ImportedConversation,
    ImportedSessionSeed, InitialSession, ResolvedContextFrontierReconstitutionInput,
    ResolvedContextFrontierSnapshot, SemanticTranscriptEntry, SemanticTranscriptEntryId,
    SemanticTranscriptEntryPayload, SemanticTranscriptEntryReconstitutionInput, Session,
    SessionConfigurationDefaults, SessionConfigurationDefaultsVersion, SessionCreationCause,
    SessionCreationProvenance, SessionId, TranscriptAncestry,
    VersionedSessionConfigurationDefaults,
};

/// The applied result recorded for imported-frontier session creation.
///
/// This is distinct from the baseline creation result so durable command kinds
/// cannot be confused at typed boundaries.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CreateSessionFromImportedFrontierAppliedResult {
    session: SessionId,
}

impl CreateSessionFromImportedFrontierAppliedResult {
    /// Returns the exact created session.
    pub const fn session(&self) -> SessionId {
        self.session
    }
}

/// One complete imported-frontier creation candidate for an atomic
/// transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedCreateSessionFromImportedFrontier {
    command: CreateSessionFromImportedFrontier,
    session: InitialSession,
    semantic_entries: Box<[SemanticTranscriptEntry]>,
    seed_snapshot: ResolvedContextFrontierSnapshot,
    imported_seed: ImportedSessionSeed,
    applied_result: CreateSessionFromImportedFrontierAppliedResult,
}

impl PreparedCreateSessionFromImportedFrontier {
    /// Borrows the exact canonical command.
    pub const fn command(&self) -> &CreateSessionFromImportedFrontier {
        &self.command
    }

    /// Borrows the complete initial session.
    pub const fn session(&self) -> &InitialSession {
        &self.session
    }

    /// Borrows the exact imported semantic prefix in imported order.
    pub fn semantic_entries(&self) -> &[SemanticTranscriptEntry] {
        &self.semantic_entries
    }

    /// Borrows the exact generated seed snapshot.
    pub const fn seed_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.seed_snapshot
    }

    /// Returns the one-to-one session-to-frontier link.
    pub const fn imported_seed(&self) -> ImportedSessionSeed {
        self.imported_seed
    }

    /// Returns the correlated applied result.
    pub const fn applied_result(&self) -> CreateSessionFromImportedFrontierAppliedResult {
        self.applied_result
    }

    /// Consumes the candidate into its complete correlated transaction facts.
    #[allow(clippy::type_complexity)]
    pub fn into_parts(
        self,
    ) -> (
        CreateSessionFromImportedFrontier,
        InitialSession,
        Box<[SemanticTranscriptEntry]>,
        ResolvedContextFrontierSnapshot,
        ImportedSessionSeed,
        CreateSessionFromImportedFrontierAppliedResult,
    ) {
        (
            self.command,
            self.session,
            self.semantic_entries,
            self.seed_snapshot,
            self.imported_seed,
            self.applied_result,
        )
    }
}

/// Why an imported-frontier command cannot form a complete seed candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CreateSessionFromImportedFrontierPreparationFailure {
    /// The supplied aggregate differs from the command's selected conversation.
    ImportedConversationMismatch,
    /// The selected frontier is not a boundary in the supplied aggregate.
    ImportedFrontierNotFound,
    /// The semantic-entry identity producer repeated an identity.
    DuplicateSemanticEntryIdentity {
        /// The repeated generated semantic-entry identity.
        entry: SemanticTranscriptEntryId,
    },
}

/// Failed imported-frontier preparation retaining all fixed caller inputs.
#[derive(Clone, Debug)]
pub struct CreateSessionFromImportedFrontierPreparationError {
    rejected: Box<(
        CreateSessionFromImportedFrontier,
        SessionId,
        ContextFrontierId,
        CreateSessionFromImportedFrontierPreparationFailure,
    )>,
}

impl CreateSessionFromImportedFrontierPreparationError {
    /// Borrows the unchanged canonical command.
    pub const fn command(&self) -> &CreateSessionFromImportedFrontier {
        &self.rejected.0
    }

    /// Returns the unchanged session candidate.
    pub const fn session(&self) -> SessionId {
        self.rejected.1
    }

    /// Returns the unchanged seed-frontier candidate.
    pub const fn seed_frontier(&self) -> ContextFrontierId {
        self.rejected.2
    }

    /// Returns why preparation failed.
    pub const fn failure(&self) -> CreateSessionFromImportedFrontierPreparationFailure {
        self.rejected.3
    }

    /// Returns all unchanged fixed inputs and the failure.
    pub fn into_parts(
        self,
    ) -> (
        CreateSessionFromImportedFrontier,
        SessionId,
        ContextFrontierId,
        CreateSessionFromImportedFrontierPreparationFailure,
    ) {
        *self.rejected
    }
}

impl CreateSessionFromImportedFrontier {
    /// Checks and materializes the exact selected imported prefix.
    ///
    /// The caller supplies already minted session and frontier candidates plus
    /// an application-owned semantic-entry identity producer. After target
    /// resolution succeeds, the producer is invoked exactly once per imported
    /// prefix member in order.
    pub fn prepare<NextSemanticEntryId>(
        self,
        imported_conversation: &ImportedConversation,
        session: SessionId,
        seed_frontier: ContextFrontierId,
        mut next_semantic_entry_id: NextSemanticEntryId,
    ) -> Result<
        PreparedCreateSessionFromImportedFrontier,
        CreateSessionFromImportedFrontierPreparationError,
    >
    where
        NextSemanticEntryId: FnMut() -> SemanticTranscriptEntryId,
    {
        let fail = |failure| CreateSessionFromImportedFrontierPreparationError {
            rejected: Box::new((self, session, seed_frontier, failure)),
        };
        if imported_conversation.id() != self.imported_conversation() {
            return Err(fail(
                CreateSessionFromImportedFrontierPreparationFailure::ImportedConversationMismatch,
            ));
        }
        let Some(prefix) = imported_conversation.prefix(self.imported_frontier()) else {
            return Err(fail(
                CreateSessionFromImportedFrontierPreparationFailure::ImportedFrontierNotFound,
            ));
        };

        let generated_identities = prefix
            .iter()
            .map(|_| next_semantic_entry_id())
            .collect::<Vec<_>>();
        let mut seen = BTreeSet::new();
        if let Some(entry) = generated_identities
            .iter()
            .copied()
            .find(|entry| !seen.insert(*entry))
        {
            return Err(fail(
                CreateSessionFromImportedFrontierPreparationFailure::DuplicateSemanticEntryIdentity {
                    entry,
                },
            ));
        }

        let semantic_entries = prefix
            .iter()
            .zip(generated_identities)
            .map(|(imported, identity)| {
                SemanticTranscriptEntry::from_validated_parts(
                    identity,
                    session,
                    SemanticTranscriptEntryPayload::Imported {
                        imported_entry: imported.identity(),
                        source_speaker: imported.source_speaker().clone(),
                        content: imported.content().clone(),
                    },
                )
            })
            .collect::<Vec<_>>();
        let ordered_entries = semantic_entries
            .iter()
            .map(SemanticTranscriptEntry::reference)
            .collect();
        let seed_snapshot = match ResolvedContextFrontierSnapshot::try_from_candidate(
            session,
            seed_frontier,
            ordered_entries,
        ) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let crate::context_frontier::ContextFrontierSnapshotConstructionRejection::DuplicateEntry {
                    entry,
                } = error.rejection();
                return Err(fail(
                    CreateSessionFromImportedFrontierPreparationFailure::DuplicateSemanticEntryIdentity {
                        entry: entry.entry(),
                    },
                ));
            }
        };
        let provenance = SessionCreationProvenance::new(
            SessionCreationCause::OwnerInitiated,
            TranscriptAncestry::ImportedConversation {
                source_frontier: self.imported_frontier(),
                relationship: self.relationship(),
            },
        );
        let initial_session = InitialSession::from_validated_imported_creation(
            session,
            provenance,
            self.establish_initial_defaults(),
        );

        Ok(PreparedCreateSessionFromImportedFrontier {
            command: self,
            session: initial_session,
            semantic_entries: semantic_entries.into_boxed_slice(),
            seed_snapshot,
            imported_seed: ImportedSessionSeed::from_validated_parts(session, seed_frontier),
            applied_result: CreateSessionFromImportedFrontierAppliedResult { session },
        })
    }
}

/// One stored imported-session-seed row supplied to checked reconstitution.
///
/// This is inert storage input rather than a canonical [`ImportedSessionSeed`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImportedSessionSeedReconstitutionInput {
    session: SessionId,
    seed_frontier: ContextFrontierId,
}

impl ImportedSessionSeedReconstitutionInput {
    /// Supplies the two typed stored fields.
    pub const fn new(session: SessionId, seed_frontier: ContextFrontierId) -> Self {
        Self {
            session,
            seed_frontier,
        }
    }

    /// Returns the stored owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the stored seed-frontier identity.
    pub const fn seed_frontier(&self) -> ContextFrontierId {
        self.seed_frontier
    }
}

/// Why stored imported seed facts do not prove one exact imported prefix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportedSessionSeedReconstitutionFailure {
    /// The stored session ancestry is not imported ancestry.
    AncestryNotImported,
    /// The supplied imported aggregate differs from the ancestry source.
    ImportedConversationMismatch,
    /// The ancestry boundary is not a member of the supplied aggregate.
    ImportedFrontierNotFound,
    /// No one-to-one seed record was supplied.
    MissingSeedRecord,
    /// More than one seed record was supplied.
    DuplicateSeedRecord,
    /// The seed record belongs to another session.
    SeedSessionMismatch,
    /// No resolved snapshot was supplied for the linked frontier.
    MissingSeedSnapshot,
    /// More than one candidate seed snapshot was supplied.
    DuplicateSeedSnapshot,
    /// The resolved seed snapshot belongs to another session.
    SeedSnapshotSessionMismatch,
    /// The resolved snapshot identity differs from the linked seed identity.
    SeedSnapshotIdentityMismatch,
    /// The semantic-entry count differs from the imported-prefix count.
    SemanticEntryCountMismatch {
        /// Required imported-prefix length.
        expected: usize,
        /// Supplied semantic-entry length.
        actual: usize,
    },
    /// One semantic entry belongs to another session.
    SemanticEntrySourceSessionMismatch {
        /// The offending semantic-entry identity.
        entry: SemanticTranscriptEntryId,
    },
    /// One semantic-entry identity occurred more than once.
    DuplicateSemanticEntry {
        /// The repeated semantic-entry identity.
        entry: SemanticTranscriptEntryId,
    },
    /// One prefix member does not carry imported provenance.
    SemanticEntryNotImported {
        /// The offending semantic-entry identity.
        entry: SemanticTranscriptEntryId,
    },
    /// One semantic entry names a different imported-entry identity.
    ImportedEntryIdentityMismatch {
        /// The offending semantic-entry identity.
        entry: SemanticTranscriptEntryId,
    },
    /// One semantic entry carries different source-speaker attestation.
    ImportedSpeakerMismatch {
        /// The offending semantic-entry identity.
        entry: SemanticTranscriptEntryId,
    },
    /// One semantic entry carries different normalized imported content.
    ImportedContentMismatch {
        /// The offending semantic-entry identity.
        entry: SemanticTranscriptEntryId,
    },
    /// The stored seed snapshot contains a duplicate reference.
    SeedSnapshotMalformed,
    /// Snapshot membership differs from the exact semantic prefix in order.
    SeedSnapshotMembershipMismatch,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ValidatedImportedSeedProjection {
    seed: ImportedSessionSeed,
    snapshot: ResolvedContextFrontierSnapshot,
    semantic_entries: Box<[SemanticTranscriptEntry]>,
}

fn validate_imported_seed_projection(
    session: SessionId,
    provenance: SessionCreationProvenance,
    imported_conversation: &ImportedConversation,
    seed_records: &[ImportedSessionSeedReconstitutionInput],
    seed_snapshots: &[ResolvedContextFrontierReconstitutionInput],
    semantic_inputs: &[SemanticTranscriptEntryReconstitutionInput],
) -> Result<ValidatedImportedSeedProjection, ImportedSessionSeedReconstitutionFailure> {
    let TranscriptAncestry::ImportedConversation {
        source_frontier, ..
    } = provenance.ancestry()
    else {
        return Err(ImportedSessionSeedReconstitutionFailure::AncestryNotImported);
    };
    if imported_conversation.id() != source_frontier.conversation() {
        return Err(ImportedSessionSeedReconstitutionFailure::ImportedConversationMismatch);
    }
    let Some(prefix) = imported_conversation.prefix(source_frontier) else {
        return Err(ImportedSessionSeedReconstitutionFailure::ImportedFrontierNotFound);
    };

    let [seed_record] = seed_records else {
        return Err(if seed_records.is_empty() {
            ImportedSessionSeedReconstitutionFailure::MissingSeedRecord
        } else {
            ImportedSessionSeedReconstitutionFailure::DuplicateSeedRecord
        });
    };
    if seed_record.session != session {
        return Err(ImportedSessionSeedReconstitutionFailure::SeedSessionMismatch);
    }
    let [seed_snapshot] = seed_snapshots else {
        return Err(if seed_snapshots.is_empty() {
            ImportedSessionSeedReconstitutionFailure::MissingSeedSnapshot
        } else {
            ImportedSessionSeedReconstitutionFailure::DuplicateSeedSnapshot
        });
    };
    if seed_snapshot.owning_session() != session {
        return Err(ImportedSessionSeedReconstitutionFailure::SeedSnapshotSessionMismatch);
    }
    if seed_snapshot.snapshot() != seed_record.seed_frontier {
        return Err(ImportedSessionSeedReconstitutionFailure::SeedSnapshotIdentityMismatch);
    }
    if semantic_inputs.len() != prefix.len() {
        return Err(
            ImportedSessionSeedReconstitutionFailure::SemanticEntryCountMismatch {
                expected: prefix.len(),
                actual: semantic_inputs.len(),
            },
        );
    }

    let mut seen = BTreeSet::new();
    let mut semantic_entries = Vec::with_capacity(prefix.len());
    for (semantic, imported) in semantic_inputs.iter().zip(prefix) {
        if semantic.source_session() != session {
            return Err(
                ImportedSessionSeedReconstitutionFailure::SemanticEntrySourceSessionMismatch {
                    entry: semantic.identity(),
                },
            );
        }
        if !seen.insert(semantic.identity()) {
            return Err(
                ImportedSessionSeedReconstitutionFailure::DuplicateSemanticEntry {
                    entry: semantic.identity(),
                },
            );
        }
        let SemanticTranscriptEntryPayload::Imported {
            imported_entry,
            source_speaker,
            content,
        } = semantic.payload()
        else {
            return Err(
                ImportedSessionSeedReconstitutionFailure::SemanticEntryNotImported {
                    entry: semantic.identity(),
                },
            );
        };
        if *imported_entry != imported.identity() {
            return Err(
                ImportedSessionSeedReconstitutionFailure::ImportedEntryIdentityMismatch {
                    entry: semantic.identity(),
                },
            );
        }
        if source_speaker != imported.source_speaker() {
            return Err(
                ImportedSessionSeedReconstitutionFailure::ImportedSpeakerMismatch {
                    entry: semantic.identity(),
                },
            );
        }
        if content != imported.content() {
            return Err(
                ImportedSessionSeedReconstitutionFailure::ImportedContentMismatch {
                    entry: semantic.identity(),
                },
            );
        }
        semantic_entries.push(SemanticTranscriptEntry::from_validated_parts(
            semantic.identity(),
            semantic.source_session(),
            semantic.payload().clone(),
        ));
    }

    let snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
        seed_snapshot.owning_session(),
        seed_snapshot.snapshot(),
        seed_snapshot.ordered_entries().to_vec(),
    )
    .map_err(|_| ImportedSessionSeedReconstitutionFailure::SeedSnapshotMalformed)?;
    if semantic_entries
        .iter()
        .map(SemanticTranscriptEntry::reference)
        .ne(snapshot.ordered_entries())
    {
        return Err(ImportedSessionSeedReconstitutionFailure::SeedSnapshotMembershipMismatch);
    }

    Ok(ValidatedImportedSeedProjection {
        seed: ImportedSessionSeed::from_validated_parts(session, seed_record.seed_frontier),
        snapshot,
        semantic_entries: semantic_entries.into_boxed_slice(),
    })
}

/// Complete stored facts for one current imported-seeded session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedSessionReconstitutionInput {
    requested_session: SessionId,
    stored_session: SessionId,
    provenance: SessionCreationProvenance,
    current_defaults_session: SessionId,
    current_defaults_version: SessionConfigurationDefaultsVersion,
    defaults_session: SessionId,
    defaults_version: SessionConfigurationDefaultsVersion,
    defaults: SessionConfigurationDefaults,
    imported_conversation: ImportedConversation,
    seed_records: Vec<ImportedSessionSeedReconstitutionInput>,
    seed_snapshots: Vec<ResolvedContextFrontierReconstitutionInput>,
    semantic_entries: Vec<SemanticTranscriptEntryReconstitutionInput>,
}

impl ImportedSessionReconstitutionInput {
    /// Supplies every independently stored fact required by this seam.
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
        imported_conversation: ImportedConversation,
        seed_records: Vec<ImportedSessionSeedReconstitutionInput>,
        seed_snapshots: Vec<ResolvedContextFrontierReconstitutionInput>,
        semantic_entries: Vec<SemanticTranscriptEntryReconstitutionInput>,
    ) -> Self {
        Self {
            requested_session,
            stored_session,
            provenance,
            current_defaults_session,
            current_defaults_version,
            defaults_session,
            defaults_version,
            defaults,
            imported_conversation,
            seed_records,
            seed_snapshots,
            semantic_entries,
        }
    }

    /// Reconstructs one complete current imported-seeded session.
    pub fn reconstitute(
        self,
    ) -> Result<ReconstitutedImportedSession, ImportedSessionReconstitutionError> {
        let fail = |input, failure| ImportedSessionReconstitutionError {
            input: Box::new(input),
            failure,
        };
        if self.requested_session != self.stored_session {
            return Err(fail(
                self,
                ImportedSessionReconstitutionFailure::RequestedSessionMismatch,
            ));
        }
        if self.current_defaults_session != self.stored_session {
            return Err(fail(
                self,
                ImportedSessionReconstitutionFailure::CurrentDefaultsSessionMismatch,
            ));
        }
        if self.defaults_session != self.stored_session {
            return Err(fail(
                self,
                ImportedSessionReconstitutionFailure::DefaultsSessionMismatch,
            ));
        }
        if self.current_defaults_version != self.defaults_version {
            return Err(fail(
                self,
                ImportedSessionReconstitutionFailure::CurrentDefaultsVersionMismatch,
            ));
        }
        let projection = match validate_imported_seed_projection(
            self.stored_session,
            self.provenance,
            &self.imported_conversation,
            &self.seed_records,
            &self.seed_snapshots,
            &self.semantic_entries,
        ) {
            Ok(projection) => projection,
            Err(failure) => {
                return Err(fail(
                    self,
                    ImportedSessionReconstitutionFailure::Seed(failure),
                ));
            }
        };
        let session = Session::from_validated_imported_reconstitution(
            self.stored_session,
            self.provenance,
            VersionedSessionConfigurationDefaults::reconstitute(
                self.defaults_version,
                self.defaults,
            ),
        );
        Ok(ReconstitutedImportedSession {
            session,
            imported_seed: projection.seed,
            seed_snapshot: projection.snapshot,
            semantic_entries: projection.semantic_entries,
        })
    }

    /// Returns the requested session identity.
    pub const fn requested_session(&self) -> SessionId {
        self.requested_session
    }

    /// Returns the stored session identity.
    pub const fn stored_session(&self) -> SessionId {
        self.stored_session
    }

    /// Returns the stored immutable provenance.
    pub const fn provenance(&self) -> SessionCreationProvenance {
        self.provenance
    }

    /// Returns the current-defaults pointer owner.
    pub const fn current_defaults_session(&self) -> SessionId {
        self.current_defaults_session
    }

    /// Returns the current-defaults pointer version.
    pub const fn current_defaults_version(&self) -> SessionConfigurationDefaultsVersion {
        self.current_defaults_version
    }

    /// Returns the selected defaults-row owner.
    pub const fn defaults_session(&self) -> SessionId {
        self.defaults_session
    }

    /// Returns the selected defaults-row version.
    pub const fn defaults_version(&self) -> SessionConfigurationDefaultsVersion {
        self.defaults_version
    }

    /// Returns the selected complete defaults.
    pub const fn defaults(&self) -> SessionConfigurationDefaults {
        self.defaults
    }

    /// Borrows the supplied immutable imported aggregate.
    pub const fn imported_conversation(&self) -> &ImportedConversation {
        &self.imported_conversation
    }

    /// Borrows all supplied candidate seed rows.
    pub fn seed_records(&self) -> &[ImportedSessionSeedReconstitutionInput] {
        &self.seed_records
    }

    /// Borrows all supplied candidate seed snapshots.
    pub fn seed_snapshots(&self) -> &[ResolvedContextFrontierReconstitutionInput] {
        &self.seed_snapshots
    }

    /// Borrows the supplied semantic prefix.
    pub fn semantic_entries(&self) -> &[SemanticTranscriptEntryReconstitutionInput] {
        &self.semantic_entries
    }
}

/// Why a complete current imported-session projection is inconsistent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportedSessionReconstitutionFailure {
    /// The requested session differs from the stored session.
    RequestedSessionMismatch,
    /// The current-defaults pointer belongs to another session.
    CurrentDefaultsSessionMismatch,
    /// The selected defaults row belongs to another session.
    DefaultsSessionMismatch,
    /// The current pointer and selected row name different versions.
    CurrentDefaultsVersionMismatch,
    /// The imported seed projection is inconsistent.
    Seed(ImportedSessionSeedReconstitutionFailure),
}

/// Failed current imported-session reconstitution retaining every input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedSessionReconstitutionError {
    input: Box<ImportedSessionReconstitutionInput>,
    failure: ImportedSessionReconstitutionFailure,
}

impl ImportedSessionReconstitutionError {
    /// Returns why reconstitution failed.
    pub const fn failure(&self) -> ImportedSessionReconstitutionFailure {
        self.failure
    }

    /// Borrows the complete unchanged input.
    pub const fn input(&self) -> &ImportedSessionReconstitutionInput {
        &self.input
    }

    /// Returns the complete unchanged input and failure.
    pub fn into_parts(
        self,
    ) -> (
        ImportedSessionReconstitutionInput,
        ImportedSessionReconstitutionFailure,
    ) {
        (*self.input, self.failure)
    }
}

/// One complete current imported-seeded session reconstructed from durable
/// facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconstitutedImportedSession {
    session: Session,
    imported_seed: ImportedSessionSeed,
    seed_snapshot: ResolvedContextFrontierSnapshot,
    semantic_entries: Box<[SemanticTranscriptEntry]>,
}

impl ReconstitutedImportedSession {
    /// Borrows the complete current session.
    pub const fn session(&self) -> &Session {
        &self.session
    }

    /// Returns the exact one-to-one seed link.
    pub const fn imported_seed(&self) -> ImportedSessionSeed {
        self.imported_seed
    }

    /// Borrows the exact resolved seed snapshot.
    pub const fn seed_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.seed_snapshot
    }

    /// Borrows the exact imported semantic prefix.
    pub fn semantic_entries(&self) -> &[SemanticTranscriptEntry] {
        &self.semantic_entries
    }

    /// Consumes the projection into its complete parts.
    pub fn into_parts(
        self,
    ) -> (
        Session,
        ImportedSessionSeed,
        ResolvedContextFrontierSnapshot,
        Box<[SemanticTranscriptEntry]>,
    ) {
        (
            self.session,
            self.imported_seed,
            self.seed_snapshot,
            self.semantic_entries,
        )
    }
}

/// Complete stored facts for one applied imported-frontier creation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateSessionFromImportedFrontierReconstitutionInput {
    command: CreateSessionFromImportedFrontier,
    result_session: SessionId,
    session: SessionId,
    provenance: SessionCreationProvenance,
    defaults_session: SessionId,
    defaults_version: SessionConfigurationDefaultsVersion,
    defaults: SessionConfigurationDefaults,
    imported_conversation: ImportedConversation,
    seed_records: Vec<ImportedSessionSeedReconstitutionInput>,
    seed_snapshots: Vec<ResolvedContextFrontierReconstitutionInput>,
    semantic_entries: Vec<SemanticTranscriptEntryReconstitutionInput>,
}

impl CreateSessionFromImportedFrontierReconstitutionInput {
    /// Supplies every independently stored creation fact.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        command: CreateSessionFromImportedFrontier,
        result_session: SessionId,
        session: SessionId,
        provenance: SessionCreationProvenance,
        defaults_session: SessionId,
        defaults_version: SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
        imported_conversation: ImportedConversation,
        seed_records: Vec<ImportedSessionSeedReconstitutionInput>,
        seed_snapshots: Vec<ResolvedContextFrontierReconstitutionInput>,
        semantic_entries: Vec<SemanticTranscriptEntryReconstitutionInput>,
    ) -> Self {
        Self {
            command,
            result_session,
            session,
            provenance,
            defaults_session,
            defaults_version,
            defaults,
            imported_conversation,
            seed_records,
            seed_snapshots,
            semantic_entries,
        }
    }

    /// Reconstructs the complete applied creation without replaying effects.
    pub fn reconstitute(
        self,
    ) -> Result<
        ReconstitutedSessionCreationFromImportedFrontier,
        CreateSessionFromImportedFrontierReconstitutionError,
    > {
        let fail = |input, failure| CreateSessionFromImportedFrontierReconstitutionError {
            input: Box::new(input),
            failure,
        };
        if self.session != self.result_session {
            return Err(fail(
                self,
                CreateSessionFromImportedFrontierReconstitutionFailure::SessionResultMismatch,
            ));
        }
        let expected_provenance = SessionCreationProvenance::new(
            SessionCreationCause::OwnerInitiated,
            TranscriptAncestry::ImportedConversation {
                source_frontier: self.command.imported_frontier(),
                relationship: self.command.relationship(),
            },
        );
        if self.provenance != expected_provenance {
            return Err(fail(
                self,
                CreateSessionFromImportedFrontierReconstitutionFailure::ProvenanceMismatch,
            ));
        }
        if self.defaults_session != self.session {
            return Err(fail(
                self,
                CreateSessionFromImportedFrontierReconstitutionFailure::DefaultsSessionMismatch,
            ));
        }
        if self.defaults_version != SessionConfigurationDefaultsVersion::first() {
            return Err(fail(
                self,
                CreateSessionFromImportedFrontierReconstitutionFailure::DefaultsVersionIsNotFirst,
            ));
        }
        if self.command.initial_configuration_defaults() != self.defaults {
            return Err(fail(
                self,
                CreateSessionFromImportedFrontierReconstitutionFailure::DefaultsMismatch,
            ));
        }
        let projection = match validate_imported_seed_projection(
            self.session,
            self.provenance,
            &self.imported_conversation,
            &self.seed_records,
            &self.seed_snapshots,
            &self.semantic_entries,
        ) {
            Ok(projection) => projection,
            Err(failure) => {
                return Err(fail(
                    self,
                    CreateSessionFromImportedFrontierReconstitutionFailure::Seed(failure),
                ));
            }
        };
        let initial_session = InitialSession::from_validated_imported_creation(
            self.session,
            self.provenance,
            VersionedSessionConfigurationDefaults::establish(self.defaults),
        );
        Ok(ReconstitutedSessionCreationFromImportedFrontier {
            command: self.command,
            session: initial_session,
            semantic_entries: projection.semantic_entries,
            seed_snapshot: projection.snapshot,
            imported_seed: projection.seed,
            applied_result: CreateSessionFromImportedFrontierAppliedResult {
                session: self.result_session,
            },
        })
    }

    /// Borrows the reconstructed canonical command.
    pub const fn command(&self) -> &CreateSessionFromImportedFrontier {
        &self.command
    }

    /// Returns the result session identity.
    pub const fn result_session(&self) -> SessionId {
        self.result_session
    }

    /// Returns the stored session identity.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the stored immutable provenance.
    pub const fn provenance(&self) -> SessionCreationProvenance {
        self.provenance
    }

    /// Returns the stored defaults-row owner.
    pub const fn defaults_session(&self) -> SessionId {
        self.defaults_session
    }

    /// Returns the stored initial defaults version.
    pub const fn defaults_version(&self) -> SessionConfigurationDefaultsVersion {
        self.defaults_version
    }

    /// Returns the stored initial defaults.
    pub const fn defaults(&self) -> SessionConfigurationDefaults {
        self.defaults
    }

    /// Borrows the supplied immutable imported aggregate.
    pub const fn imported_conversation(&self) -> &ImportedConversation {
        &self.imported_conversation
    }

    /// Borrows all supplied candidate seed rows.
    pub fn seed_records(&self) -> &[ImportedSessionSeedReconstitutionInput] {
        &self.seed_records
    }

    /// Borrows all supplied candidate seed snapshots.
    pub fn seed_snapshots(&self) -> &[ResolvedContextFrontierReconstitutionInput] {
        &self.seed_snapshots
    }

    /// Borrows the supplied semantic prefix.
    pub fn semantic_entries(&self) -> &[SemanticTranscriptEntryReconstitutionInput] {
        &self.semantic_entries
    }
}

/// Why complete stored facts cannot reconstruct an applied imported creation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CreateSessionFromImportedFrontierReconstitutionFailure {
    /// The result names a different session.
    SessionResultMismatch,
    /// Stored provenance differs from the command-selected source.
    ProvenanceMismatch,
    /// The initial defaults row belongs to another session.
    DefaultsSessionMismatch,
    /// Imported session creation did not establish defaults version one.
    DefaultsVersionIsNotFirst,
    /// Stored initial defaults differ from the command payload.
    DefaultsMismatch,
    /// The imported seed projection is inconsistent.
    Seed(ImportedSessionSeedReconstitutionFailure),
}

/// Failed creation reconstitution retaining every typed input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateSessionFromImportedFrontierReconstitutionError {
    input: Box<CreateSessionFromImportedFrontierReconstitutionInput>,
    failure: CreateSessionFromImportedFrontierReconstitutionFailure,
}

impl CreateSessionFromImportedFrontierReconstitutionError {
    /// Returns why reconstitution failed.
    pub const fn failure(&self) -> CreateSessionFromImportedFrontierReconstitutionFailure {
        self.failure
    }

    /// Borrows every unchanged typed input.
    pub const fn input(&self) -> &CreateSessionFromImportedFrontierReconstitutionInput {
        &self.input
    }

    /// Returns the complete unchanged input and failure.
    pub fn into_parts(
        self,
    ) -> (
        CreateSessionFromImportedFrontierReconstitutionInput,
        CreateSessionFromImportedFrontierReconstitutionFailure,
    ) {
        (*self.input, self.failure)
    }
}

/// One applied imported-frontier creation reconstructed from complete durable
/// facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconstitutedSessionCreationFromImportedFrontier {
    command: CreateSessionFromImportedFrontier,
    session: InitialSession,
    semantic_entries: Box<[SemanticTranscriptEntry]>,
    seed_snapshot: ResolvedContextFrontierSnapshot,
    imported_seed: ImportedSessionSeed,
    applied_result: CreateSessionFromImportedFrontierAppliedResult,
}

impl ReconstitutedSessionCreationFromImportedFrontier {
    /// Borrows the reconstructed command.
    pub const fn command(&self) -> &CreateSessionFromImportedFrontier {
        &self.command
    }

    /// Borrows the reconstructed initial session.
    pub const fn session(&self) -> &InitialSession {
        &self.session
    }

    /// Borrows the exact imported semantic prefix.
    pub fn semantic_entries(&self) -> &[SemanticTranscriptEntry] {
        &self.semantic_entries
    }

    /// Borrows the exact reconstructed seed snapshot.
    pub const fn seed_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.seed_snapshot
    }

    /// Returns the exact reconstructed one-to-one seed link.
    pub const fn imported_seed(&self) -> ImportedSessionSeed {
        self.imported_seed
    }

    /// Returns the reconstructed applied result.
    pub const fn applied_result(&self) -> CreateSessionFromImportedFrontierAppliedResult {
        self.applied_result
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::test_support::{
        accepted_input_id, command_id, context_frontier_id, direct, imported_conversation_id,
        imported_transcript_entry_id, semantic_transcript_entry_id, session_id,
    };
    use crate::{
        ImportedConversationFormat, ImportedRawRecordPosition, ImportedRawSourceRecord,
        ImportedRecordEntryPosition, ImportedSourceAttestation, ImportedSourceMetadata,
        ImportedStructuredObjectMember, ImportedStructuredValue, ImportedText,
        ImportedTranscriptContent, ImportedTranscriptEntryInput, ImportedTranscriptPosition,
        ModelSelectionRequest, SemanticTranscriptEntryRef,
    };

    fn defaults(value: u128) -> SessionConfigurationDefaults {
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(direct(value)))
    }

    fn position(value: u64) -> ImportedTranscriptPosition {
        ImportedTranscriptPosition::try_from_u64(value).expect("test position is positive")
    }

    fn raw_position(value: u64) -> ImportedRawRecordPosition {
        ImportedRawRecordPosition::try_from_u64(value).expect("test position is positive")
    }

    fn source_event(
        conversation: crate::ImportedConversationId,
        identity: u128,
        ordinal: u64,
        source_type: &str,
    ) -> (ImportedRawSourceRecord, ImportedTranscriptEntryInput) {
        let source_type = ImportedText::new(source_type.to_owned());
        let normalized = ImportedStructuredValue::Object(
            vec![ImportedStructuredObjectMember::new(
                ImportedText::new("type".to_owned()),
                ImportedStructuredValue::String(source_type.clone()),
            )]
            .into_boxed_slice(),
        );
        let raw = ImportedRawSourceRecord::from_converted(
            format!("synthetic-record-{ordinal}").into_bytes(),
            normalized,
        );
        let source = ImportedSourceMetadata::new(
            ImportedSourceAttestation::NotAttested,
            ImportedSourceAttestation::NotAttested,
            ImportedSourceAttestation::NotAttested,
            ImportedSourceAttestation::NotAttested,
            ImportedSourceAttestation::NotAttested,
            ImportedSourceAttestation::NotAttested,
            ImportedSourceAttestation::NotAttested,
        );
        let entry = ImportedTranscriptEntryInput::new(
            imported_transcript_entry_id(identity),
            conversation,
            position(ordinal),
            raw_position(ordinal),
            ImportedRecordEntryPosition::first(),
            ImportedSourceAttestation::NotAttested,
            ImportedTranscriptContent::SourceEvent {
                source_type: ImportedSourceAttestation::Attested(source_type),
            },
            source,
        );
        (raw, entry)
    }

    fn conversation(id: u128) -> ImportedConversation {
        let conversation = imported_conversation_id(id);
        let (first_raw, first_entry) = source_event(conversation, id * 10 + 1, 1, "summary");
        let (second_raw, second_entry) = source_event(conversation, id * 10 + 2, 2, "system");
        ImportedConversation::from_converted_records(
            conversation,
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
            vec![first_raw, second_raw],
            vec![first_entry, second_entry],
        )
        .expect("synthetic source events form a checked imported conversation")
    }

    fn alternate_conversation_with_same_identity(
        original: &ImportedConversation,
    ) -> ImportedConversation {
        let conversation = original.id();
        let (first_raw, first_entry) = source_event(conversation, 201, 1, "other-summary");
        let (second_raw, second_entry) = source_event(conversation, 202, 2, "other-system");
        ImportedConversation::from_converted_records(
            conversation,
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
            vec![first_raw, second_raw],
            vec![first_entry, second_entry],
        )
        .expect("alternate synthetic history remains internally checked")
    }

    fn command_for(conversation: &ImportedConversation) -> CreateSessionFromImportedFrontier {
        CreateSessionFromImportedFrontier::new(
            command_id(1),
            conversation
                .frontiers()
                .last()
                .expect("fixture has two entries"),
            crate::ImportedSessionRelationship::Resume,
            defaults(2),
        )
    }

    fn prepared_fixture() -> (
        ImportedConversation,
        CreateSessionFromImportedFrontier,
        PreparedCreateSessionFromImportedFrontier,
    ) {
        let conversation = conversation(1);
        let command = command_for(&conversation);
        let mut next = 10_u128;
        let prepared = command
            .prepare(&conversation, session_id(3), context_frontier_id(4), || {
                let identity = semantic_transcript_entry_id(next);
                next += 1;
                identity
            })
            .expect("matching imported prefix prepares");
        (conversation, command, prepared)
    }

    fn projection_inputs(
        prepared: &PreparedCreateSessionFromImportedFrontier,
    ) -> (
        Vec<ImportedSessionSeedReconstitutionInput>,
        Vec<ResolvedContextFrontierReconstitutionInput>,
        Vec<SemanticTranscriptEntryReconstitutionInput>,
    ) {
        let seed = prepared.imported_seed();
        let snapshot = prepared.seed_snapshot();
        (
            vec![ImportedSessionSeedReconstitutionInput::new(
                seed.session(),
                seed.seed_frontier(),
            )],
            vec![ResolvedContextFrontierReconstitutionInput::new(
                snapshot.frontier().owning_session(),
                snapshot.frontier().snapshot(),
                snapshot.ordered_entries().collect(),
            )],
            prepared
                .semantic_entries()
                .iter()
                .map(|entry| {
                    SemanticTranscriptEntryReconstitutionInput::new(
                        entry.identity(),
                        entry.source_session(),
                        entry.payload().clone(),
                    )
                })
                .collect(),
        )
    }

    fn creation_input(
        conversation: &ImportedConversation,
        command: CreateSessionFromImportedFrontier,
        prepared: &PreparedCreateSessionFromImportedFrontier,
    ) -> CreateSessionFromImportedFrontierReconstitutionInput {
        let (seeds, snapshots, semantic_entries) = projection_inputs(prepared);
        CreateSessionFromImportedFrontierReconstitutionInput::new(
            command,
            prepared.applied_result().session(),
            prepared.session().id(),
            prepared.session().provenance(),
            prepared.session().id(),
            SessionConfigurationDefaultsVersion::first(),
            command.initial_configuration_defaults(),
            conversation.clone(),
            seeds,
            snapshots,
            semantic_entries,
        )
    }

    fn current_input(
        conversation: &ImportedConversation,
        prepared: &PreparedCreateSessionFromImportedFrontier,
    ) -> ImportedSessionReconstitutionInput {
        let (seeds, snapshots, semantic_entries) = projection_inputs(prepared);
        ImportedSessionReconstitutionInput::new(
            prepared.session().id(),
            prepared.session().id(),
            prepared.session().provenance(),
            prepared.session().id(),
            SessionConfigurationDefaultsVersion::first(),
            prepared.session().id(),
            SessionConfigurationDefaultsVersion::first(),
            prepared.command().initial_configuration_defaults(),
            conversation.clone(),
            seeds,
            snapshots,
            semantic_entries,
        )
    }

    /// S28 / INV-015 / INV-038 / INV-039: preparation projects every exact
    /// imported prefix member once, in order, and couples it to one exact
    /// separately identified seed frontier.
    #[test]
    fn s28_inv015_inv038_inv039_preparation_materializes_exact_imported_seed() {
        let conversation = conversation(1);
        let command = command_for(&conversation);
        let calls = Cell::new(0_u128);

        let prepared = command
            .prepare(&conversation, session_id(3), context_frontier_id(4), || {
                let next = calls.get() + 1;
                calls.set(next);
                semantic_transcript_entry_id(next)
            })
            .expect("the selected imported prefix is valid");

        assert_eq!(calls.get(), 2);
        assert_eq!(prepared.semantic_entries().len(), 2);
        assert_eq!(prepared.imported_seed().session(), session_id(3));
        assert_eq!(
            prepared.imported_seed().seed_frontier(),
            context_frontier_id(4)
        );
        assert_eq!(
            prepared.seed_snapshot().frontier().snapshot(),
            context_frontier_id(4)
        );
        assert_eq!(
            prepared
                .seed_snapshot()
                .ordered_entries()
                .collect::<Vec<_>>(),
            prepared
                .semantic_entries()
                .iter()
                .map(SemanticTranscriptEntry::reference)
                .collect::<Vec<_>>()
        );
        for (semantic, imported) in prepared
            .semantic_entries()
            .iter()
            .zip(conversation.entries())
        {
            assert_eq!(semantic.source_session(), session_id(3));
            assert_eq!(
                semantic.payload(),
                &SemanticTranscriptEntryPayload::Imported {
                    imported_entry: imported.identity(),
                    source_speaker: imported.source_speaker().clone(),
                    content: imported.content().clone(),
                }
            );
        }
    }

    /// S28 / INV-012 / INV-039: mismatched target identities fail before any
    /// semantic identity is generated or command identity is claimed.
    #[test]
    fn s28_inv012_inv039_target_mismatch_precedes_projection() {
        let selected = conversation(1);
        let supplied = conversation(2);
        let command = command_for(&selected);
        let calls = Cell::new(0);

        let error = command
            .prepare(&supplied, session_id(3), context_frontier_id(4), || {
                calls.set(calls.get() + 1);
                semantic_transcript_entry_id(5)
            })
            .expect_err("the supplied aggregate differs from the command target");

        assert_eq!(
            error.failure(),
            CreateSessionFromImportedFrontierPreparationFailure::ImportedConversationMismatch
        );
        assert_eq!(calls.get(), 0);

        let same_identity_different_history = alternate_conversation_with_same_identity(&selected);
        let mismatched_frontier = CreateSessionFromImportedFrontier::new(
            command_id(6),
            selected.frontiers().next().expect("fixture frontier"),
            crate::ImportedSessionRelationship::Fork,
            defaults(7),
        );
        let error = mismatched_frontier
            .prepare(
                &same_identity_different_history,
                session_id(8),
                context_frontier_id(9),
                || {
                    calls.set(calls.get() + 1);
                    semantic_transcript_entry_id(10)
                },
            )
            .expect_err("a frontier from another conversation is not a member");
        assert_eq!(
            error.failure(),
            CreateSessionFromImportedFrontierPreparationFailure::ImportedFrontierNotFound
        );
        assert_eq!(calls.get(), 0);
    }

    /// S28 / INV-001 / INV-039: a faulty generator is called exactly once per
    /// prefix member, then duplicate semantic identity fails closed.
    #[test]
    fn s28_inv001_inv039_duplicate_generated_identity_fails_closed() {
        let conversation = conversation(1);
        let command = command_for(&conversation);
        let calls = Cell::new(0);

        let error = command
            .prepare(&conversation, session_id(3), context_frontier_id(4), || {
                calls.set(calls.get() + 1);
                semantic_transcript_entry_id(5)
            })
            .expect_err("a generated identity cannot name two prefix members");

        assert_eq!(calls.get(), 2);
        assert_eq!(
            error.failure(),
            CreateSessionFromImportedFrontierPreparationFailure::DuplicateSemanticEntryIdentity {
                entry: semantic_transcript_entry_id(5),
            }
        );
    }

    /// S28 / INV-003 / INV-008 / INV-012 / INV-039: complete matching
    /// creation facts reconstruct the exact prepared session seed.
    #[test]
    fn s28_inv003_inv008_inv012_inv039_creation_reconstitutes_complete_seed() {
        let (conversation, command, prepared) = prepared_fixture();
        let input = creation_input(&conversation, command, &prepared);

        let reconstituted = input
            .reconstitute()
            .expect("complete matching creation facts reconstruct");

        assert_eq!(reconstituted.command(), &command);
        assert_eq!(reconstituted.session(), prepared.session());
        assert_eq!(
            reconstituted.semantic_entries(),
            prepared.semantic_entries()
        );
        assert_eq!(reconstituted.seed_snapshot(), prepared.seed_snapshot());
        assert_eq!(reconstituted.imported_seed(), prepared.imported_seed());
        assert_eq!(reconstituted.applied_result(), prepared.applied_result());
    }

    /// S28 / INV-002 / INV-003 / INV-015 / INV-039: current-session
    /// reconstitution requires and returns the exact seed identity and prefix.
    #[test]
    fn s28_inv002_inv003_inv015_inv039_current_session_reconstitutes_seed() {
        let (conversation, _, prepared) = prepared_fixture();
        let input = current_input(&conversation, &prepared);

        let reconstituted = input
            .reconstitute()
            .expect("complete current imported session reconstructs");

        assert_eq!(reconstituted.session().id(), prepared.session().id());
        assert_eq!(
            reconstituted.session().creation_provenance(),
            prepared.session().provenance()
        );
        assert_eq!(reconstituted.imported_seed(), prepared.imported_seed());
        assert_eq!(reconstituted.seed_snapshot(), prepared.seed_snapshot());
        assert_eq!(
            reconstituted.semantic_entries(),
            prepared.semantic_entries()
        );
    }

    /// S28 / INV-015 / INV-039: missing, duplicate, cross-session, and
    /// equal-content-but-different-identity seed facts are typed corruption.
    #[test]
    fn s28_inv015_inv039_seed_record_and_identity_corruption_is_typed() {
        let (conversation, _, prepared) = prepared_fixture();

        let mut missing = current_input(&conversation, &prepared);
        missing.seed_records.clear();
        assert_eq!(
            missing
                .reconstitute()
                .expect_err("seed row is required")
                .failure(),
            ImportedSessionReconstitutionFailure::Seed(
                ImportedSessionSeedReconstitutionFailure::MissingSeedRecord
            )
        );

        let mut duplicate = current_input(&conversation, &prepared);
        duplicate.seed_records.push(duplicate.seed_records[0]);
        assert_eq!(
            duplicate
                .reconstitute()
                .expect_err("one-to-one seed row cannot repeat")
                .failure(),
            ImportedSessionReconstitutionFailure::Seed(
                ImportedSessionSeedReconstitutionFailure::DuplicateSeedRecord
            )
        );

        let mut cross_session = current_input(&conversation, &prepared);
        cross_session.seed_records[0] = ImportedSessionSeedReconstitutionInput::new(
            session_id(99),
            prepared.imported_seed().seed_frontier(),
        );
        assert_eq!(
            cross_session
                .reconstitute()
                .expect_err("seed row must belong to the imported session")
                .failure(),
            ImportedSessionReconstitutionFailure::Seed(
                ImportedSessionSeedReconstitutionFailure::SeedSessionMismatch
            )
        );

        let mut reminted = current_input(&conversation, &prepared);
        let members = reminted.seed_snapshots[0].ordered_entries().to_vec();
        reminted.seed_snapshots[0] = ResolvedContextFrontierReconstitutionInput::new(
            prepared.session().id(),
            context_frontier_id(100),
            members,
        );
        assert_eq!(
            reminted
                .reconstitute()
                .expect_err("equal membership cannot substitute another identity")
                .failure(),
            ImportedSessionReconstitutionFailure::Seed(
                ImportedSessionSeedReconstitutionFailure::SeedSnapshotIdentityMismatch
            )
        );
    }

    /// S28 / INV-038 / INV-039: imported identity, speaker, content, and
    /// ordered snapshot membership are independently checked.
    #[test]
    fn s28_inv038_inv039_semantic_prefix_corruption_is_typed() {
        let (conversation, _, prepared) = prepared_fixture();

        let mut wrong_imported_entry = current_input(&conversation, &prepared);
        let first = wrong_imported_entry.semantic_entries[0].clone();
        let SemanticTranscriptEntryPayload::Imported {
            source_speaker,
            content,
            ..
        } = first.payload().clone()
        else {
            panic!("fixture is imported");
        };
        wrong_imported_entry.semantic_entries[0] = SemanticTranscriptEntryReconstitutionInput::new(
            first.identity(),
            first.source_session(),
            SemanticTranscriptEntryPayload::Imported {
                imported_entry: imported_transcript_entry_id(999),
                source_speaker,
                content,
            },
        );
        assert_eq!(
            wrong_imported_entry
                .reconstitute()
                .expect_err("equal content under another imported identity is invalid")
                .failure(),
            ImportedSessionReconstitutionFailure::Seed(
                ImportedSessionSeedReconstitutionFailure::ImportedEntryIdentityMismatch {
                    entry: first.identity(),
                }
            )
        );

        let mut wrong_content = current_input(&conversation, &prepared);
        let first = wrong_content.semantic_entries[0].clone();
        let SemanticTranscriptEntryPayload::Imported {
            imported_entry,
            source_speaker,
            ..
        } = first.payload().clone()
        else {
            panic!("fixture is imported");
        };
        wrong_content.semantic_entries[0] = SemanticTranscriptEntryReconstitutionInput::new(
            first.identity(),
            first.source_session(),
            SemanticTranscriptEntryPayload::Imported {
                imported_entry,
                source_speaker,
                content: conversation.entries()[1].content().clone(),
            },
        );
        assert_eq!(
            wrong_content
                .reconstitute()
                .expect_err("changed normalized content is invalid")
                .failure(),
            ImportedSessionReconstitutionFailure::Seed(
                ImportedSessionSeedReconstitutionFailure::ImportedContentMismatch {
                    entry: first.identity(),
                }
            )
        );

        let mut reordered = current_input(&conversation, &prepared);
        let mut members = reordered.seed_snapshots[0].ordered_entries().to_vec();
        members.swap(0, 1);
        reordered.seed_snapshots[0] = ResolvedContextFrontierReconstitutionInput::new(
            prepared.session().id(),
            prepared.imported_seed().seed_frontier(),
            members,
        );
        assert_eq!(
            reordered
                .reconstitute()
                .expect_err("snapshot order must equal imported order")
                .failure(),
            ImportedSessionReconstitutionFailure::Seed(
                ImportedSessionSeedReconstitutionFailure::SeedSnapshotMembershipMismatch
            )
        );
    }

    /// S28 / INV-002 / INV-003 / INV-015 / INV-038 / INV-039: every
    /// constructible imported-seed corruption branch retains its complete
    /// input and reports one exact typed cause.
    #[test]
    fn s28_inv002_inv003_inv015_inv038_inv039_seed_corruption_matrix_is_complete() {
        let (imported_conversation, _, prepared) = prepared_fixture();
        let other_conversation = conversation(2);
        let mut cases = Vec::new();

        let mut ancestry_not_imported = current_input(&imported_conversation, &prepared);
        ancestry_not_imported.provenance = SessionCreationProvenance::new(
            SessionCreationCause::OwnerInitiated,
            TranscriptAncestry::None,
        );
        cases.push((
            "ancestry not imported",
            ancestry_not_imported,
            ImportedSessionSeedReconstitutionFailure::AncestryNotImported,
        ));

        let mut conversation_mismatch = current_input(&imported_conversation, &prepared);
        conversation_mismatch.provenance = SessionCreationProvenance::new(
            SessionCreationCause::OwnerInitiated,
            TranscriptAncestry::ImportedConversation {
                source_frontier: other_conversation
                    .frontiers()
                    .last()
                    .expect("other fixture frontier"),
                relationship: crate::ImportedSessionRelationship::Resume,
            },
        );
        cases.push((
            "imported conversation mismatch",
            conversation_mismatch,
            ImportedSessionSeedReconstitutionFailure::ImportedConversationMismatch,
        ));

        let mut frontier_not_found = current_input(&imported_conversation, &prepared);
        frontier_not_found.imported_conversation =
            alternate_conversation_with_same_identity(&imported_conversation);
        cases.push((
            "imported frontier not found",
            frontier_not_found,
            ImportedSessionSeedReconstitutionFailure::ImportedFrontierNotFound,
        ));

        let mut missing_snapshot = current_input(&imported_conversation, &prepared);
        missing_snapshot.seed_snapshots.clear();
        cases.push((
            "missing seed snapshot",
            missing_snapshot,
            ImportedSessionSeedReconstitutionFailure::MissingSeedSnapshot,
        ));

        let mut duplicate_snapshot = current_input(&imported_conversation, &prepared);
        duplicate_snapshot
            .seed_snapshots
            .push(duplicate_snapshot.seed_snapshots[0].clone());
        cases.push((
            "duplicate seed snapshot",
            duplicate_snapshot,
            ImportedSessionSeedReconstitutionFailure::DuplicateSeedSnapshot,
        ));

        let mut snapshot_session_mismatch = current_input(&imported_conversation, &prepared);
        let snapshot = &snapshot_session_mismatch.seed_snapshots[0];
        snapshot_session_mismatch.seed_snapshots[0] =
            ResolvedContextFrontierReconstitutionInput::new(
                session_id(99),
                snapshot.snapshot(),
                snapshot.ordered_entries().to_vec(),
            );
        cases.push((
            "seed snapshot session mismatch",
            snapshot_session_mismatch,
            ImportedSessionSeedReconstitutionFailure::SeedSnapshotSessionMismatch,
        ));

        let mut semantic_session_mismatch = current_input(&imported_conversation, &prepared);
        let semantic = semantic_session_mismatch.semantic_entries[0].clone();
        semantic_session_mismatch.semantic_entries[0] =
            SemanticTranscriptEntryReconstitutionInput::new(
                semantic.identity(),
                session_id(99),
                semantic.payload().clone(),
            );
        cases.push((
            "semantic entry source session mismatch",
            semantic_session_mismatch,
            ImportedSessionSeedReconstitutionFailure::SemanticEntrySourceSessionMismatch {
                entry: semantic.identity(),
            },
        ));

        let mut duplicate_semantic = current_input(&imported_conversation, &prepared);
        let first_identity = duplicate_semantic.semantic_entries[0].identity();
        let second = duplicate_semantic.semantic_entries[1].clone();
        duplicate_semantic.semantic_entries[1] = SemanticTranscriptEntryReconstitutionInput::new(
            first_identity,
            second.source_session(),
            second.payload().clone(),
        );
        cases.push((
            "duplicate semantic entry",
            duplicate_semantic,
            ImportedSessionSeedReconstitutionFailure::DuplicateSemanticEntry {
                entry: first_identity,
            },
        ));

        let mut semantic_not_imported = current_input(&imported_conversation, &prepared);
        let semantic = semantic_not_imported.semantic_entries[0].clone();
        semantic_not_imported.semantic_entries[0] = SemanticTranscriptEntryReconstitutionInput::new(
            semantic.identity(),
            semantic.source_session(),
            SemanticTranscriptEntryPayload::OriginAcceptedInput {
                accepted_input: accepted_input_id(99),
            },
        );
        cases.push((
            "semantic entry not imported",
            semantic_not_imported,
            ImportedSessionSeedReconstitutionFailure::SemanticEntryNotImported {
                entry: semantic.identity(),
            },
        ));

        let mut speaker_mismatch = current_input(&imported_conversation, &prepared);
        let semantic = speaker_mismatch.semantic_entries[0].clone();
        let SemanticTranscriptEntryPayload::Imported {
            imported_entry,
            content,
            ..
        } = semantic.payload().clone()
        else {
            panic!("fixture is imported");
        };
        speaker_mismatch.semantic_entries[0] = SemanticTranscriptEntryReconstitutionInput::new(
            semantic.identity(),
            semantic.source_session(),
            SemanticTranscriptEntryPayload::Imported {
                imported_entry,
                source_speaker: ImportedSourceAttestation::Attested(crate::ImportedSpeaker::User),
                content,
            },
        );
        cases.push((
            "imported speaker mismatch",
            speaker_mismatch,
            ImportedSessionSeedReconstitutionFailure::ImportedSpeakerMismatch {
                entry: semantic.identity(),
            },
        ));

        let mut malformed_snapshot = current_input(&imported_conversation, &prepared);
        let snapshot = &malformed_snapshot.seed_snapshots[0];
        let duplicate = snapshot.ordered_entries()[0];
        malformed_snapshot.seed_snapshots[0] = ResolvedContextFrontierReconstitutionInput::new(
            snapshot.owning_session(),
            snapshot.snapshot(),
            vec![duplicate, duplicate],
        );
        cases.push((
            "malformed seed snapshot",
            malformed_snapshot,
            ImportedSessionSeedReconstitutionFailure::SeedSnapshotMalformed,
        ));

        for (name, input, expected) in cases {
            let unchanged = input.clone();
            let error = input.reconstitute().expect_err(name);
            assert_eq!(
                error.failure(),
                ImportedSessionReconstitutionFailure::Seed(expected),
                "{name}"
            );
            assert_eq!(error.input(), &unchanged, "{name}");
        }
    }

    /// S28 / INV-002 / INV-003 / INV-008 / INV-012 / INV-039: every
    /// constructible top-level creation mismatch returns the complete
    /// unchanged reconstitution input.
    #[test]
    fn s28_inv002_inv003_inv008_inv012_inv039_creation_corruption_matrix_is_complete() {
        let (conversation, command, prepared) = prepared_fixture();
        let mut cases = Vec::new();

        let mut result_mismatch = creation_input(&conversation, command, &prepared);
        result_mismatch.result_session = session_id(99);
        cases.push((
            "session result mismatch",
            result_mismatch,
            CreateSessionFromImportedFrontierReconstitutionFailure::SessionResultMismatch,
        ));

        let mut provenance_mismatch = creation_input(&conversation, command, &prepared);
        provenance_mismatch.provenance = SessionCreationProvenance::new(
            SessionCreationCause::OwnerInitiated,
            TranscriptAncestry::ImportedConversation {
                source_frontier: command.imported_frontier(),
                relationship: crate::ImportedSessionRelationship::Fork,
            },
        );
        cases.push((
            "provenance mismatch",
            provenance_mismatch,
            CreateSessionFromImportedFrontierReconstitutionFailure::ProvenanceMismatch,
        ));

        let mut defaults_session_mismatch = creation_input(&conversation, command, &prepared);
        defaults_session_mismatch.defaults_session = session_id(99);
        cases.push((
            "defaults session mismatch",
            defaults_session_mismatch,
            CreateSessionFromImportedFrontierReconstitutionFailure::DefaultsSessionMismatch,
        ));

        let mut defaults_version_mismatch = creation_input(&conversation, command, &prepared);
        defaults_version_mismatch.defaults_version = SessionConfigurationDefaultsVersion::first()
            .checked_next()
            .expect("version one has a successor");
        cases.push((
            "defaults version is not first",
            defaults_version_mismatch,
            CreateSessionFromImportedFrontierReconstitutionFailure::DefaultsVersionIsNotFirst,
        ));

        let mut defaults_mismatch = creation_input(&conversation, command, &prepared);
        defaults_mismatch.defaults = defaults(99);
        cases.push((
            "defaults mismatch",
            defaults_mismatch,
            CreateSessionFromImportedFrontierReconstitutionFailure::DefaultsMismatch,
        ));

        for (name, input, expected) in cases {
            let unchanged = input.clone();
            let error = input.reconstitute().expect_err(name);
            assert_eq!(error.failure(), expected, "{name}");
            assert_eq!(error.input(), &unchanged, "{name}");
        }
    }

    /// S28 / INV-039: a different selected imported boundary cannot
    /// reconstruct the semantic prefix of another boundary.
    #[test]
    fn s28_inv039_mismatched_boundary_fails_closed() {
        let (conversation, _, prepared) = prepared_fixture();
        let mut input = current_input(&conversation, &prepared);
        input.provenance = SessionCreationProvenance::new(
            SessionCreationCause::OwnerInitiated,
            TranscriptAncestry::ImportedConversation {
                source_frontier: conversation.frontiers().next().expect("first frontier"),
                relationship: crate::ImportedSessionRelationship::Resume,
            },
        );

        assert_eq!(
            input
                .reconstitute()
                .expect_err("a one-entry boundary cannot own a two-entry seed")
                .failure(),
            ImportedSessionReconstitutionFailure::Seed(
                ImportedSessionSeedReconstitutionFailure::SemanticEntryCountMismatch {
                    expected: 1,
                    actual: 2,
                }
            )
        );
    }

    #[test]
    fn imported_semantic_entry_reference_remains_session_qualified() {
        let (_, _, prepared) = prepared_fixture();
        let entry = &prepared.semantic_entries()[0];
        assert_eq!(
            entry.reference(),
            SemanticTranscriptEntryRef::from_source(entry.source_session(), entry.identity())
        );
    }
}
