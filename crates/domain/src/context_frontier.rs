//! Immutable identified context-frontier snapshot values.
//!
//! ADR-0030 is normative. This module separates cheap frontier identity from
//! explicit semantic-content comparison, represents source-qualified semantic
//! transcript-entry references, rejects duplicate references in a resolved
//! snapshot, and offers only prefix-preserving append derivation from an
//! already-resolved snapshot.
//!
//! These are pure domain values, not lifecycle or commit authority. The later
//! turn aggregate must establish entry existence and eligibility, derive a
//! starting lineage with its frontier, correlate a call with its turn and
//! attempt, and commit every new entry, snapshot, disposition, and lifecycle
//! fact atomically. Persistence rehydration also remains a separate validated
//! boundary.

#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "later eligibility and call-preparation slices consume the sealed candidate seams"
    )
)]

use std::collections::BTreeSet;

use crate::SessionId;

crate::define_identity!(
    /// Identifies one immutable context-frontier snapshot within its owning
    /// session.
    ///
    /// The initial Rust backing follows Signalbox's private UUID-newtype
    /// convention. A raw identifier is not proof that a snapshot exists,
    /// resolves immutably, belongs to a session, or is correct for a lifecycle
    /// transition.
    ContextFrontierId
);

crate::define_identity!(
    /// Identifies one immutable semantic transcript entry.
    ///
    /// A complete frontier reference qualifies this identity with its source
    /// session. Payload variants, commit granularity, and rendering remain
    /// separate open questions.
    SemanticTranscriptEntryId
);

/// One exact immutable context-frontier reference.
///
/// Ordinary equality compares both the consuming session and its
/// session-scoped snapshot identity. Raw parts cannot construct a valid
/// frontier:
///
/// ```compile_fail
/// use signalbox_domain::{ContextFrontier, ContextFrontierId, SessionId};
///
/// fn raw_parts_are_not_a_frontier(
///     owning_session: SessionId,
///     snapshot: ContextFrontierId,
/// ) {
///     let _ = ContextFrontier {
///         owning_session,
///         snapshot,
///     };
/// }
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ContextFrontier {
    owning_session: SessionId,
    snapshot: ContextFrontierId,
}

impl ContextFrontier {
    const fn new(owning_session: SessionId, snapshot: ContextFrontierId) -> Self {
        Self {
            owning_session,
            snapshot,
        }
    }

    /// Returns the session that owns and consumes this snapshot.
    pub const fn owning_session(&self) -> SessionId {
        self.owning_session
    }

    /// Returns the session-scoped immutable snapshot identity.
    pub const fn snapshot(&self) -> ContextFrontierId {
        self.snapshot
    }
}

/// One exact immutable semantic-history entry qualified by its source session.
///
/// Constructing this reference does not prove that the entry exists or is
/// eligible for a frontier. It only prevents a session-scoped entry identity
/// from losing its semantic source while pure domain values are compared.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SemanticTranscriptEntryRef {
    source_session: SessionId,
    entry: SemanticTranscriptEntryId,
}

impl SemanticTranscriptEntryRef {
    /// Qualifies one semantic entry with the session that created it.
    pub const fn from_source(source_session: SessionId, entry: SemanticTranscriptEntryId) -> Self {
        Self {
            source_session,
            entry,
        }
    }

    /// Returns the session that created the immutable semantic entry.
    pub const fn source_session(&self) -> SessionId {
        self.source_session
    }

    /// Returns the immutable semantic-entry identity.
    pub const fn entry(&self) -> SemanticTranscriptEntryId {
        self.entry
    }
}

/// One identified context frontier resolved to its complete ordered contents.
///
/// The entry sequence is exact and contains no duplicate
/// [`SemanticTranscriptEntryRef`]. Repeated or equal rendered content remains
/// representable through distinct semantic-entry identities.
///
/// Identity equality stays on [`ContextFrontier`]. Use
/// [`Self::same_semantic_content`] when complete ordered-entry equality is the
/// intended comparison.
///
/// Raw identifiers and a plausible list cannot construct a resolved snapshot:
///
/// ```compile_fail
/// use signalbox_domain::{
///     ContextFrontier, ResolvedContextFrontierSnapshot, SemanticTranscriptEntryRef,
/// };
///
/// fn raw_values_are_not_a_resolved_snapshot(
///     frontier: ContextFrontier,
///     ordered_entries: Box<[SemanticTranscriptEntryRef]>,
/// ) {
///     let _ = ResolvedContextFrontierSnapshot {
///         frontier,
///         ordered_entries,
///     };
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedContextFrontierSnapshot {
    frontier: ContextFrontier,
    ordered_entries: Box<[SemanticTranscriptEntryRef]>,
}

impl ResolvedContextFrontierSnapshot {
    /// Validates one complete candidate projection without claiming that its
    /// entries exist, are eligible, or have committed.
    pub(crate) fn try_from_candidate(
        owning_session: SessionId,
        snapshot: ContextFrontierId,
        ordered_entries: Vec<SemanticTranscriptEntryRef>,
    ) -> Result<Self, ContextFrontierSnapshotConstructionError> {
        if let Some(duplicate) = first_duplicate(&ordered_entries) {
            return Err(ContextFrontierSnapshotConstructionError::new(
                owning_session,
                snapshot,
                ordered_entries,
                ContextFrontierSnapshotConstructionRejection::DuplicateEntry { entry: duplicate },
            ));
        }

        Ok(Self {
            frontier: ContextFrontier::new(owning_session, snapshot),
            ordered_entries: ordered_entries.into_boxed_slice(),
        })
    }

    /// Derives a candidate with the same owner and an identity different from
    /// the source solely by retaining the complete source prefix and appending
    /// exact new entries.
    ///
    /// The borrowed source remains unchanged on success or rejection. The
    /// later aggregate and persistence boundary must still establish that the
    /// candidate identity is fresh among all authoritative session snapshots.
    pub(crate) fn derive_appending_candidate(
        &self,
        next_snapshot: ContextFrontierId,
        appended_entries: Vec<SemanticTranscriptEntryRef>,
    ) -> Result<Self, ContextFrontierSnapshotDerivationError> {
        if next_snapshot == self.frontier.snapshot {
            return Err(ContextFrontierSnapshotDerivationError::new(
                next_snapshot,
                appended_entries,
                ContextFrontierSnapshotDerivationRejection::ReusedSourceSnapshotIdentity,
            ));
        }

        let mut seen = self
            .ordered_entries
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let duplicate = appended_entries
            .iter()
            .copied()
            .find(|entry| !seen.insert(*entry));
        if let Some(entry) = duplicate {
            return Err(ContextFrontierSnapshotDerivationError::new(
                next_snapshot,
                appended_entries,
                ContextFrontierSnapshotDerivationRejection::DuplicateEntry { entry },
            ));
        }

        let mut ordered_entries =
            Vec::with_capacity(self.ordered_entries.len() + appended_entries.len());
        ordered_entries.extend_from_slice(&self.ordered_entries);
        ordered_entries.extend_from_slice(&appended_entries);

        Ok(Self {
            frontier: ContextFrontier::new(self.frontier.owning_session, next_snapshot),
            ordered_entries: ordered_entries.into_boxed_slice(),
        })
    }

    /// Returns the exact identified frontier this value resolves.
    pub const fn frontier(&self) -> ContextFrontier {
        self.frontier
    }

    /// Returns the number of exact source-qualified semantic entries.
    pub fn entry_count(&self) -> usize {
        self.ordered_entries.len()
    }

    /// Iterates over the complete semantic entries in their exact order.
    pub fn ordered_entries(
        &self,
    ) -> impl ExactSizeIterator<Item = SemanticTranscriptEntryRef> + DoubleEndedIterator + '_ {
        self.ordered_entries.iter().copied()
    }

    /// Explicitly compares complete ordered semantic contents while ignoring
    /// frontier identity.
    pub fn same_semantic_content(&self, other: &Self) -> bool {
        self.ordered_entries == other.ordered_entries
    }

    /// Returns whether this complete ordered content is a prefix of `later`.
    ///
    /// This is a content relationship only. It does not prove that `later`
    /// was selected or committed by an accepted lifecycle transition.
    pub fn is_semantic_prefix_of(&self, later: &Self) -> bool {
        later.ordered_entries.starts_with(&self.ordered_entries)
    }
}

fn first_duplicate(entries: &[SemanticTranscriptEntryRef]) -> Option<SemanticTranscriptEntryRef> {
    let mut seen = BTreeSet::new();
    entries.iter().copied().find(|entry| !seen.insert(*entry))
}

/// Why a complete snapshot candidate could not construct an ordered-distinct
/// resolved value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ContextFrontierSnapshotConstructionRejection {
    /// The exact source-session and entry-identity pair occurred twice.
    DuplicateEntry {
        /// The duplicated exact semantic-entry reference.
        entry: SemanticTranscriptEntryRef,
    },
}

/// Rejected complete snapshot candidate with every input unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ContextFrontierSnapshotConstructionError {
    rejected: Box<(
        SessionId,
        ContextFrontierId,
        Vec<SemanticTranscriptEntryRef>,
        ContextFrontierSnapshotConstructionRejection,
    )>,
}

impl ContextFrontierSnapshotConstructionError {
    fn new(
        owning_session: SessionId,
        snapshot: ContextFrontierId,
        ordered_entries: Vec<SemanticTranscriptEntryRef>,
        rejection: ContextFrontierSnapshotConstructionRejection,
    ) -> Self {
        Self {
            rejected: Box::new((owning_session, snapshot, ordered_entries, rejection)),
        }
    }

    pub(crate) const fn owning_session(&self) -> SessionId {
        self.rejected.0
    }

    pub(crate) const fn snapshot(&self) -> ContextFrontierId {
        self.rejected.1
    }

    pub(crate) fn ordered_entries(&self) -> &[SemanticTranscriptEntryRef] {
        &self.rejected.2
    }

    pub(crate) const fn rejection(&self) -> ContextFrontierSnapshotConstructionRejection {
        self.rejected.3
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        SessionId,
        ContextFrontierId,
        Vec<SemanticTranscriptEntryRef>,
        ContextFrontierSnapshotConstructionRejection,
    ) {
        *self.rejected
    }
}

/// Why an append-only derivation candidate was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ContextFrontierSnapshotDerivationRejection {
    /// A derivation candidate must differ from its source snapshot identity.
    ReusedSourceSnapshotIdentity,
    /// An appended reference duplicated the source prefix or an earlier
    /// appended reference.
    DuplicateEntry {
        /// The duplicated exact semantic-entry reference.
        entry: SemanticTranscriptEntryRef,
    },
}

/// Rejected append-only derivation inputs.
///
/// The resolved source was only borrowed and therefore remains unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ContextFrontierSnapshotDerivationError {
    rejected: Box<(
        ContextFrontierId,
        Vec<SemanticTranscriptEntryRef>,
        ContextFrontierSnapshotDerivationRejection,
    )>,
}

impl ContextFrontierSnapshotDerivationError {
    fn new(
        next_snapshot: ContextFrontierId,
        appended_entries: Vec<SemanticTranscriptEntryRef>,
        rejection: ContextFrontierSnapshotDerivationRejection,
    ) -> Self {
        Self {
            rejected: Box::new((next_snapshot, appended_entries, rejection)),
        }
    }

    pub(crate) const fn next_snapshot(&self) -> ContextFrontierId {
        self.rejected.0
    }

    pub(crate) fn appended_entries(&self) -> &[SemanticTranscriptEntryRef] {
        &self.rejected.1
    }

    pub(crate) const fn rejection(&self) -> ContextFrontierSnapshotDerivationRejection {
        self.rejected.2
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        ContextFrontierId,
        Vec<SemanticTranscriptEntryRef>,
        ContextFrontierSnapshotDerivationRejection,
    ) {
        *self.rejected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{context_frontier_id, semantic_transcript_entry_id, session_id};

    /// One semantic entry created in the canonical source session for tests
    /// that do not care about cross-session sources.
    fn entry(entry: u128) -> SemanticTranscriptEntryRef {
        entry_from(session_id(1), entry)
    }

    fn entry_from(source_session: SessionId, entry: u128) -> SemanticTranscriptEntryRef {
        SemanticTranscriptEntryRef::from_source(source_session, semantic_transcript_entry_id(entry))
    }

    fn snapshot(
        owning_session: SessionId,
        snapshot: u128,
        ordered_entries: impl IntoIterator<Item = SemanticTranscriptEntryRef>,
    ) -> ResolvedContextFrontierSnapshot {
        ResolvedContextFrontierSnapshot::try_from_candidate(
            owning_session,
            context_frontier_id(snapshot),
            ordered_entries.into_iter().collect(),
        )
        .expect("test snapshot entries are ordered and distinct")
    }

    /// INV-001: even equal UUID bytes retain distinct semantic identity kinds,
    /// and a complete context-frontier identity includes its owning session.
    #[test]
    fn inv001_frontier_and_entry_identity_kinds_remain_distinct() {
        let frontier_id = context_frontier_id(1);
        let entry_id = semantic_transcript_entry_id(1);
        assert_eq!(frontier_id.as_uuid(), entry_id.as_uuid());

        let owner = session_id(1);
        let first = snapshot(owner, 1, []);
        let same = snapshot(owner, 1, []);
        let different_owner = snapshot(session_id(2), 1, []);
        let different_snapshot = snapshot(owner, 2, []);

        assert_eq!(first.frontier(), same.frontier());
        assert_ne!(first.frontier(), different_owner.frontier());
        assert_ne!(first.frontier(), different_snapshot.frontier());
        assert_eq!(first.frontier().owning_session(), owner);
        assert_eq!(first.frontier().snapshot(), frontier_id);
    }

    /// INV-015: ordinary frontier identity and explicit complete semantic
    /// content equality remain separate comparisons; independently identified
    /// equal-content snapshots are legal.
    #[test]
    fn inv015_identity_and_semantic_content_equality_are_explicitly_distinct() {
        let owner = session_id(1);
        let entries = [entry(1), entry(2)];
        let first = snapshot(owner, 1, entries);
        let independent = snapshot(owner, 2, entries);
        let reordered = snapshot(owner, 3, [entries[1], entries[0]]);

        assert_ne!(first.frontier(), independent.frontier());
        assert_ne!(first, independent);
        assert!(first.same_semantic_content(&independent));
        assert!(!first.same_semantic_content(&reordered));
        assert_eq!(
            first.ordered_entries().collect::<Vec<_>>(),
            entries.to_vec()
        );
        assert_eq!(first.entry_count(), entries.len());
    }

    /// INV-015 / INV-030: exact duplicate references are rejected unchanged,
    /// while matching entry identifiers from distinct source sessions remain
    /// distinct semantic references.
    #[test]
    fn inv015_inv030_resolved_contents_are_ordered_and_exactly_distinct() {
        let first_source = session_id(1);
        let first = entry_from(first_source, 1);
        let same_entry_other_source = entry_from(session_id(2), 1);
        let ordered_entries = vec![first, same_entry_other_source, first];

        let error = ResolvedContextFrontierSnapshot::try_from_candidate(
            session_id(3),
            context_frontier_id(1),
            ordered_entries.clone(),
        )
        .expect_err("the exact repeated source-qualified reference is invalid");

        assert_eq!(error.owning_session(), session_id(3));
        assert_eq!(error.snapshot(), context_frontier_id(1));
        assert_eq!(error.ordered_entries(), ordered_entries);
        assert_eq!(
            error.rejection(),
            ContextFrontierSnapshotConstructionRejection::DuplicateEntry { entry: first }
        );
        assert_eq!(
            error.into_parts(),
            (
                session_id(3),
                context_frontier_id(1),
                ordered_entries,
                ContextFrontierSnapshotConstructionRejection::DuplicateEntry { entry: first },
            )
        );

        let valid = snapshot(session_id(3), 1, [first, same_entry_other_source]);
        assert_eq!(valid.entry_count(), 2);
        assert_ne!(first, same_entry_other_source);
        assert_eq!(first.source_session(), first_source);
        assert_eq!(first.entry(), semantic_transcript_entry_id(1));
    }

    /// S09 / INV-015: later candidate derivation retains the complete earlier
    /// prefix in order and only appends exact new semantic entries.
    #[test]
    fn s09_inv015_derivation_is_prefix_preserving_and_append_only() {
        let owner = session_id(1);
        let source_entries = [entry(1), entry(2)];
        let appended_entries = vec![entry(3), entry(4)];
        let source = snapshot(owner, 1, source_entries);
        let derived = source
            .derive_appending_candidate(context_frontier_id(2), appended_entries.clone())
            .expect("distinct entries and a fresh identity derive a candidate");

        assert_eq!(source.ordered_entries().collect::<Vec<_>>(), source_entries);
        assert_eq!(
            derived.ordered_entries().collect::<Vec<_>>(),
            source_entries
                .into_iter()
                .chain(appended_entries)
                .collect::<Vec<_>>()
        );
        assert!(source.is_semantic_prefix_of(&derived));
        assert!(!derived.is_semantic_prefix_of(&source));
        assert_eq!(derived.frontier().owning_session(), owner);
        assert_eq!(derived.frontier().snapshot(), context_frontier_id(2));

        let no_new_entries = vec![];
        let equal_content = source
            .derive_appending_candidate(context_frontier_id(3), no_new_entries)
            .expect("a separately identified equal-content snapshot is legal");
        assert_ne!(source.frontier(), equal_content.frontier());
        assert!(source.same_semantic_content(&equal_content));
    }

    /// INV-015: derivation cannot reinterpret the source snapshot identity or
    /// duplicate an exact reference from either the retained prefix or the
    /// same append batch; every rejected append input is returned unchanged.
    #[test]
    fn inv015_invalid_derivations_preserve_source_and_inputs() {
        let source = snapshot(session_id(1), 1, [entry(1)]);

        assert_derivation_rejects_unchanged(
            &source,
            context_frontier_id(1),
            vec![entry(2)],
            ContextFrontierSnapshotDerivationRejection::ReusedSourceSnapshotIdentity,
        );
        assert_derivation_rejects_unchanged(
            &source,
            context_frontier_id(2),
            vec![entry(1)],
            ContextFrontierSnapshotDerivationRejection::DuplicateEntry { entry: entry(1) },
        );
        assert_derivation_rejects_unchanged(
            &source,
            context_frontier_id(3),
            vec![entry(2), entry(2)],
            ContextFrontierSnapshotDerivationRejection::DuplicateEntry { entry: entry(2) },
        );
    }

    #[track_caller]
    fn assert_derivation_rejects_unchanged(
        source: &ResolvedContextFrontierSnapshot,
        next_snapshot: ContextFrontierId,
        appended_entries: Vec<SemanticTranscriptEntryRef>,
        expected_rejection: ContextFrontierSnapshotDerivationRejection,
    ) {
        let unchanged_source = source.clone();
        let unchanged_appended_entries = appended_entries.clone();
        let error = source
            .derive_appending_candidate(next_snapshot, appended_entries)
            .expect_err("invalid append derivation must reject");
        assert_eq!(source, &unchanged_source);
        assert_eq!(error.next_snapshot(), next_snapshot);
        assert_eq!(error.appended_entries(), unchanged_appended_entries);
        assert_eq!(error.rejection(), expected_rejection);
        assert_eq!(
            error.into_parts(),
            (
                next_snapshot,
                unchanged_appended_entries,
                expected_rejection
            )
        );
    }

    /// S17 / INV-030: a new consuming session owns its own frontier while
    /// preserving inherited source-session and semantic-entry identities
    /// before appending its own origin entry.
    #[test]
    fn s17_inv030_inherited_entry_references_are_preserved_without_reminting() {
        let source_session = session_id(1);
        let consuming_session = session_id(2);
        let inherited = [entry_from(source_session, 1), entry_from(source_session, 2)];
        let origin = entry_from(consuming_session, 3);
        let fork = snapshot(consuming_session, 1, inherited.into_iter().chain([origin]));

        assert_eq!(fork.frontier().owning_session(), consuming_session);
        assert_eq!(
            fork.ordered_entries().collect::<Vec<_>>(),
            vec![inherited[0], inherited[1], origin]
        );
        assert_eq!(fork.ordered_entries().next(), Some(inherited[0]));
        assert_eq!(origin.source_session(), consuming_session);
    }
}
