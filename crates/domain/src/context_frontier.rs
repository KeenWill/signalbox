//! Immutable identified context-frontier snapshot values.
//!
//! docs/spec/turn-lifecycle-and-scheduling.md is normative. This module
//! separates cheap frontier identity from explicit semantic-content
//! comparison, represents source-qualified semantic transcript-entry
//! references, rejects duplicate references in a resolved snapshot, and
//! offers only prefix-preserving append derivation from an already-resolved
//! snapshot.
//!
//! These are pure domain values, not lifecycle or commit authority. The
//! accepted-input scheduling seam establishes entry existence and
//! eligibility when it reconstructs or prepares a start. Later call
//! preparation must separately correlate a call with its turn and attempt.
//! Persistence commits every new entry, snapshot, disposition, and lifecycle
//! fact atomically.

#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "later eligibility and call-preparation slices consume the sealed candidate seams"
    )
)]

use std::{
    collections::BTreeSet,
    fmt,
    sync::{Arc, OnceLock},
};

use rpds::{RedBlackTreeMapSync, RedBlackTreeSetSync, VectorSync};

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
    pub(crate) const fn new(owning_session: SessionId, snapshot: ContextFrontierId) -> Self {
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
pub struct ResolvedContextFrontierSnapshot {
    frontier: ContextFrontier,
    content: FrontierContent,
}

/// Complete checked values supplied for one stored context snapshot.
///
/// This input cannot independently construct a resolved snapshot. The
/// scheduling reconstitution seam validates complete membership together with
/// every semantic entry and lifecycle correlation that authorizes a start.
pub struct ResolvedContextFrontierReconstitutionInput {
    owning_session: SessionId,
    snapshot: ContextFrontierId,
    content: ReconstitutionContent,
    materialized_entries: OnceLock<Box<[SemanticTranscriptEntryRef]>>,
}

#[derive(Clone)]
struct FrontierContent {
    ordered_entries: VectorSync<SemanticTranscriptEntryRef>,
    membership: RedBlackTreeSetSync<SemanticTranscriptEntryRef>,
    lineage: RedBlackTreeMapSync<ContextFrontierId, Arc<ContentToken>>,
    token: Arc<ContentToken>,
    validation_nodes: VectorSync<Arc<FrontierContentNode>>,
    immediate_prefix: Option<ContextFrontier>,
    appended_entry_count: usize,
}

struct FrontierContentNode {
    ordered_entries: VectorSync<SemanticTranscriptEntryRef>,
    appended_start: usize,
    token: Arc<ContentToken>,
}

#[derive(Clone)]
struct ReconstitutionContent {
    frontier: FrontierContent,
    first_duplicate: Option<SemanticTranscriptEntryRef>,
}

#[derive(Debug)]
struct ContentToken;

#[derive(Default)]
pub(crate) struct ContextFrontierEntryValidationCache {
    validated_nodes: BTreeSet<*const ContentToken>,
}

impl ResolvedContextFrontierReconstitutionInput {
    /// Supplies one snapshot's complete stored identity and membership.
    pub fn new(
        owning_session: SessionId,
        snapshot: ContextFrontierId,
        ordered_entries: Vec<SemanticTranscriptEntryRef>,
    ) -> Self {
        let content = ReconstitutionContent::from_complete(snapshot, ordered_entries);
        Self {
            owning_session,
            snapshot,
            content,
            materialized_entries: OnceLock::new(),
        }
    }

    /// Supplies a complete stored successor by sharing this input's exact
    /// prefix and appending the stored suffix.
    ///
    /// The values remain inert: only a complete aggregate reconstitution seam
    /// can validate their storage and lifecycle correlations. This operation
    /// preserves the first repeated exact reference, if any, for that later
    /// validation.
    pub fn derive_appending(
        &self,
        snapshot: ContextFrontierId,
        appended_entries: Vec<SemanticTranscriptEntryRef>,
    ) -> Self {
        Self {
            owning_session: self.owning_session,
            snapshot,
            content: self.content.derive_appending(
                self.owning_session,
                self.snapshot,
                snapshot,
                appended_entries,
            ),
            materialized_entries: OnceLock::new(),
        }
    }

    /// Returns the stored owning session.
    pub const fn owning_session(&self) -> SessionId {
        self.owning_session
    }

    /// Returns the stored session-scoped snapshot identity.
    pub const fn snapshot(&self) -> ContextFrontierId {
        self.snapshot
    }

    /// Returns the complete stored member count without materializing a slice.
    pub fn entry_count(&self) -> usize {
        self.content.frontier.ordered_entries.len()
    }

    /// Returns the complete stored ordered membership.
    pub fn ordered_entries(&self) -> &[SemanticTranscriptEntryRef] {
        self.materialized_entries
            .get_or_init(|| {
                self.content
                    .frontier
                    .ordered_entries
                    .iter()
                    .copied()
                    .collect()
            })
            .as_ref()
    }

    /// Validates this complete stored membership for an aggregate that owns
    /// its database existence and eligibility checks.
    ///
    /// Persistence adapters use this after loading the declared member count,
    /// every contiguous member, and every referenced semantic entry. The
    /// returned value proves only snapshot shape; the consuming aggregate
    /// remains responsible for ownership and boundary correlation.
    pub fn reconstitute(self) -> Option<ResolvedContextFrontierSnapshot> {
        ResolvedContextFrontierSnapshot::try_from_reconstitution_input(self).ok()
    }

    pub(crate) fn first_missing_entry(
        &self,
        validation: &mut ContextFrontierEntryValidationCache,
        mut contains: impl FnMut(&SemanticTranscriptEntryRef) -> bool,
    ) -> Option<SemanticTranscriptEntryRef> {
        let mut unvalidated_nodes = Vec::new();
        for node in self.content.frontier.validation_nodes.iter().rev() {
            let token = Arc::as_ptr(&node.token);
            if validation.validated_nodes.contains(&token) {
                break;
            }
            unvalidated_nodes.push(node.as_ref());
        }

        for node in unvalidated_nodes.into_iter().rev() {
            for entry in FrontierEntryRange::new(
                &node.ordered_entries,
                node.appended_start,
                node.ordered_entries.len(),
            ) {
                if !contains(&entry) {
                    return Some(entry);
                }
            }
            validation.validated_nodes.insert(Arc::as_ptr(&node.token));
        }
        None
    }
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
            content: FrontierContent::from_distinct(snapshot, ordered_entries),
        })
    }

    pub(crate) fn try_from_reconstitution_input(
        input: ResolvedContextFrontierReconstitutionInput,
    ) -> Result<Self, ContextFrontierSnapshotConstructionError> {
        if let Some(duplicate) = input.content.first_duplicate {
            return Err(ContextFrontierSnapshotConstructionError::new(
                input.owning_session,
                input.snapshot,
                input
                    .content
                    .frontier
                    .ordered_entries
                    .iter()
                    .copied()
                    .collect(),
                ContextFrontierSnapshotConstructionRejection::DuplicateEntry { entry: duplicate },
            ));
        }

        Ok(Self {
            frontier: ContextFrontier::new(input.owning_session, input.snapshot),
            content: input.content.frontier,
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

        let mut membership = self.content.membership.clone();
        let duplicate = appended_entries.iter().copied().find(|entry| {
            if membership.contains(entry) {
                true
            } else {
                membership.insert_mut(*entry);
                false
            }
        });
        if let Some(entry) = duplicate {
            return Err(ContextFrontierSnapshotDerivationError::new(
                next_snapshot,
                appended_entries,
                ContextFrontierSnapshotDerivationRejection::DuplicateEntry { entry },
            ));
        }

        let appended_entry_count = appended_entries.len();
        let mut ordered_entries = self.content.ordered_entries.clone();
        ordered_entries.extend(appended_entries);
        let token = Arc::new(ContentToken);
        let mut lineage = self.content.lineage.clone();
        lineage.insert_mut(next_snapshot, token.clone());
        let node = Arc::new(FrontierContentNode {
            ordered_entries: ordered_entries.clone(),
            appended_start: ordered_entries.len() - appended_entry_count,
            token: token.clone(),
        });
        let mut validation_nodes = self.content.validation_nodes.clone();
        validation_nodes.push_back_mut(node);

        Ok(Self {
            frontier: ContextFrontier::new(self.frontier.owning_session, next_snapshot),
            content: FrontierContent {
                ordered_entries,
                membership,
                lineage,
                token,
                validation_nodes,
                immediate_prefix: Some(self.frontier),
                appended_entry_count,
            },
        })
    }

    /// Returns the exact identified frontier this value resolves.
    pub const fn frontier(&self) -> ContextFrontier {
        self.frontier
    }

    /// Returns the number of exact source-qualified semantic entries.
    pub fn entry_count(&self) -> usize {
        self.content.ordered_entries.len()
    }

    /// Iterates over the complete semantic entries in their exact order.
    pub fn ordered_entries(
        &self,
    ) -> impl ExactSizeIterator<Item = SemanticTranscriptEntryRef> + DoubleEndedIterator + '_ {
        self.content.ordered_entries.iter().copied()
    }

    /// Explicitly compares complete ordered semantic contents while ignoring
    /// frontier identity.
    pub fn same_semantic_content(&self, other: &Self) -> bool {
        (self.entry_count() == other.entry_count()
            && (self.content.is_shared_prefix_of(
                self.frontier.snapshot,
                &self.content.token,
                &other.content,
            ) || other.content.is_shared_prefix_of(
                other.frontier.snapshot,
                &other.content.token,
                &self.content,
            )))
            || self.content.ordered_entries == other.content.ordered_entries
    }

    /// Returns whether this complete ordered content is a prefix of `later`.
    ///
    /// This is a content relationship only. It does not prove that `later`
    /// was selected or committed by an accepted lifecycle transition.
    pub fn is_semantic_prefix_of(&self, later: &Self) -> bool {
        if self.content.is_shared_prefix_of(
            self.frontier.snapshot,
            &self.content.token,
            &later.content,
        ) {
            return true;
        }
        if self.entry_count() > later.entry_count() {
            return false;
        }
        self.ordered_entries()
            .eq(later.ordered_entries().take(self.entry_count()))
    }

    /// Returns the structurally shared immediate semantic prefix, when this
    /// snapshot was derived by exact append.
    ///
    /// This is a representation hint only. It does not prove that either
    /// snapshot exists durably or that a lifecycle transition selected it.
    pub fn immediate_semantic_prefix(&self) -> Option<ContextFrontier> {
        self.content.immediate_prefix
    }

    /// Iterates over the exact suffix appended to the immediate shared prefix.
    ///
    /// A snapshot constructed without a shared prefix reports its complete
    /// membership as the suffix.
    pub fn appended_entries(
        &self,
    ) -> impl ExactSizeIterator<Item = SemanticTranscriptEntryRef> + DoubleEndedIterator + '_ {
        let start = self
            .entry_count()
            .saturating_sub(self.content.appended_entry_count);
        FrontierEntryRange::new(&self.content.ordered_entries, start, self.entry_count())
    }

    pub(crate) fn has_semantic_prefix_and_suffix(
        &self,
        prefix: &Self,
        suffix: impl ExactSizeIterator<Item = SemanticTranscriptEntryRef>,
    ) -> bool {
        if !prefix.is_semantic_prefix_of(self)
            || self.entry_count() != prefix.entry_count() + suffix.len()
        {
            return false;
        }
        suffix.enumerate().all(|(offset, expected)| {
            self.content
                .ordered_entries
                .get(prefix.entry_count() + offset)
                .is_some_and(|actual| *actual == expected)
        })
    }

    pub(crate) fn ordered_entries_range(
        &self,
        start: usize,
        end: usize,
    ) -> impl ExactSizeIterator<Item = SemanticTranscriptEntryRef> + DoubleEndedIterator + '_ {
        FrontierEntryRange::new(&self.content.ordered_entries, start, end)
    }
}

impl FrontierContent {
    fn from_distinct(
        snapshot: ContextFrontierId,
        ordered_entries: Vec<SemanticTranscriptEntryRef>,
    ) -> Self {
        let membership = ordered_entries.iter().copied().collect();
        let appended_entry_count = ordered_entries.len();
        let ordered_entries: VectorSync<SemanticTranscriptEntryRef> =
            ordered_entries.into_iter().collect();
        let token = Arc::new(ContentToken);
        let mut lineage = RedBlackTreeMapSync::new_sync();
        lineage.insert_mut(snapshot, token.clone());
        let node = Arc::new(FrontierContentNode {
            ordered_entries: ordered_entries.clone(),
            appended_start: 0,
            token: token.clone(),
        });
        Self {
            ordered_entries,
            membership,
            lineage,
            token,
            validation_nodes: VectorSync::new_sync().push_back(node),
            immediate_prefix: None,
            appended_entry_count,
        }
    }

    fn is_shared_prefix_of(
        &self,
        snapshot: ContextFrontierId,
        token: &Arc<ContentToken>,
        later: &Self,
    ) -> bool {
        self.ordered_entries.len() <= later.ordered_entries.len()
            && later
                .lineage
                .get(&snapshot)
                .is_some_and(|later_token| Arc::ptr_eq(token, later_token))
    }
}

impl ReconstitutionContent {
    fn from_complete(
        snapshot: ContextFrontierId,
        ordered_entries: Vec<SemanticTranscriptEntryRef>,
    ) -> Self {
        let mut membership = RedBlackTreeSetSync::new_sync();
        let mut first_duplicate = None;
        for entry in &ordered_entries {
            if first_duplicate.is_none() && membership.contains(entry) {
                first_duplicate = Some(*entry);
            } else {
                membership.insert_mut(*entry);
            }
        }
        let mut frontier = FrontierContent::from_distinct(snapshot, ordered_entries);
        frontier.membership = membership;
        Self {
            frontier,
            first_duplicate,
        }
    }

    fn derive_appending(
        &self,
        owning_session: SessionId,
        source_snapshot: ContextFrontierId,
        snapshot: ContextFrontierId,
        appended_entries: Vec<SemanticTranscriptEntryRef>,
    ) -> Self {
        let appended_entry_count = appended_entries.len();
        let mut membership = self.frontier.membership.clone();
        let mut first_duplicate = self.first_duplicate;
        for entry in &appended_entries {
            if first_duplicate.is_none() && membership.contains(entry) {
                first_duplicate = Some(*entry);
            } else {
                membership.insert_mut(*entry);
            }
        }
        let mut ordered_entries = self.frontier.ordered_entries.clone();
        ordered_entries.extend(appended_entries);
        let token = Arc::new(ContentToken);
        let mut lineage = self.frontier.lineage.clone();
        lineage.insert_mut(snapshot, token.clone());
        let node = Arc::new(FrontierContentNode {
            ordered_entries: ordered_entries.clone(),
            appended_start: ordered_entries.len() - appended_entry_count,
            token: token.clone(),
        });
        let mut validation_nodes = self.frontier.validation_nodes.clone();
        validation_nodes.push_back_mut(node);
        Self {
            frontier: FrontierContent {
                ordered_entries,
                membership,
                lineage,
                token,
                validation_nodes,
                immediate_prefix: Some(ContextFrontier::new(owning_session, source_snapshot)),
                appended_entry_count,
            },
            first_duplicate,
        }
    }
}

struct FrontierEntryRange<'a> {
    entries: &'a VectorSync<SemanticTranscriptEntryRef>,
    front: usize,
    back: usize,
}

impl<'a> FrontierEntryRange<'a> {
    fn new(entries: &'a VectorSync<SemanticTranscriptEntryRef>, front: usize, back: usize) -> Self {
        Self {
            entries,
            front,
            back,
        }
    }
}

impl Iterator for FrontierEntryRange<'_> {
    type Item = SemanticTranscriptEntryRef;

    fn next(&mut self) -> Option<Self::Item> {
        if self.front == self.back {
            return None;
        }
        let entry = self.entries.get(self.front).copied();
        if entry.is_some() {
            self.front += 1;
        }
        entry
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.back - self.front;
        (remaining, Some(remaining))
    }
}

impl DoubleEndedIterator for FrontierEntryRange<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.front == self.back {
            return None;
        }
        self.back -= 1;
        self.entries.get(self.back).copied()
    }
}

impl ExactSizeIterator for FrontierEntryRange<'_> {}

impl Clone for ResolvedContextFrontierReconstitutionInput {
    fn clone(&self) -> Self {
        Self {
            owning_session: self.owning_session,
            snapshot: self.snapshot,
            content: self.content.clone(),
            materialized_entries: OnceLock::new(),
        }
    }
}

impl fmt::Debug for ResolvedContextFrontierReconstitutionInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedContextFrontierReconstitutionInput")
            .field("owning_session", &self.owning_session)
            .field("snapshot", &self.snapshot)
            .field("ordered_entries", &self.ordered_entries())
            .finish()
    }
}

impl PartialEq for ResolvedContextFrontierReconstitutionInput {
    fn eq(&self, other: &Self) -> bool {
        self.owning_session == other.owning_session
            && self.snapshot == other.snapshot
            && self.content.frontier.ordered_entries == other.content.frontier.ordered_entries
    }
}

impl Eq for ResolvedContextFrontierReconstitutionInput {}

impl Clone for ResolvedContextFrontierSnapshot {
    fn clone(&self) -> Self {
        Self {
            frontier: self.frontier,
            content: self.content.clone(),
        }
    }
}

impl fmt::Debug for ResolvedContextFrontierSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedContextFrontierSnapshot")
            .field("frontier", &self.frontier)
            .field(
                "ordered_entries",
                &self.ordered_entries().collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl PartialEq for ResolvedContextFrontierSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.frontier == other.frontier
            && self.content.ordered_entries == other.content.ordered_entries
    }
}

impl Eq for ResolvedContextFrontierSnapshot {}

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

    fn distinct_entries(count: u128) -> Vec<SemanticTranscriptEntryRef> {
        (1..=count).map(entry).collect()
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

    /// even equal UUID bytes retain distinct semantic identity kinds,
    /// and a complete context-frontier identity includes its owning session.
    #[test]
    fn frontier_and_entry_identity_kinds_remain_distinct() {
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

    /// ordinary frontier identity and explicit complete semantic
    /// content equality remain separate comparisons; independently identified
    /// equal-content snapshots are legal.
    #[test]
    fn identity_and_semantic_content_equality_are_explicitly_distinct() {
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

    /// exact duplicate references are rejected unchanged,
    /// while matching entry identifiers from distinct source sessions remain
    /// distinct semantic references.
    #[test]
    fn resolved_contents_are_ordered_and_exactly_distinct() {
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

    /// the persistence-facing seam admits complete distinct
    /// membership and rejects an exact duplicate without exposing unchecked
    /// snapshot construction.
    #[test]
    fn reconstitution_input_checks_complete_snapshot_shape() {
        let owner = session_id(1);
        let first = entry(1);
        let second = entry(2);
        let resolved = ResolvedContextFrontierReconstitutionInput::new(
            owner,
            context_frontier_id(1),
            vec![first, second],
        )
        .reconstitute()
        .expect("distinct complete membership reconstitutes");
        assert_eq!(resolved.frontier().owning_session(), owner);
        assert_eq!(
            resolved.ordered_entries().collect::<Vec<_>>(),
            vec![first, second]
        );
        assert!(
            ResolvedContextFrontierReconstitutionInput::new(
                owner,
                context_frontier_id(2),
                vec![first, first],
            )
            .reconstitute()
            .is_none()
        );
    }

    /// complete reconstitution preserves the exact order of a
    /// frontier large enough to exercise structurally shared tree nodes.
    #[test]
    fn long_frontier_reconstitution_preserves_complete_order() {
        let ordered_entries = distinct_entries(512);
        let input = ResolvedContextFrontierReconstitutionInput::new(
            session_id(1),
            context_frontier_id(1),
            ordered_entries.clone(),
        );

        let resolved = input
            .reconstitute()
            .expect("the long frontier contains distinct exact references");

        assert_eq!(resolved.entry_count(), ordered_entries.len());
        assert_eq!(
            resolved.ordered_entries().collect::<Vec<_>>(),
            ordered_entries
        );
    }

    /// S09: long-frontier derivation shares the exact source prefix,
    /// appends only the stated suffix, and leaves the source unchanged.
    #[test]
    fn s09_long_frontier_derivation_preserves_source_prefix() {
        let source_entries = distinct_entries(512);
        let appended_entries = vec![entry(513), entry(514)];
        let source = snapshot(session_id(1), 1, source_entries.clone());

        let derived = source
            .derive_appending_candidate(context_frontier_id(2), appended_entries.clone())
            .expect("the long frontier suffix is distinct");

        assert_eq!(source.ordered_entries().collect::<Vec<_>>(), source_entries);
        assert!(source.is_semantic_prefix_of(&derived));
        assert_eq!(
            derived.appended_entries().collect::<Vec<_>>(),
            appended_entries
        );
        assert_eq!(derived.entry_count(), 514);
    }

    /// S09: hundreds of one-entry derivations retain one exact
    /// ordered frontier without rebuilding or mutating any semantic prefix.
    #[test]
    fn s09_long_frontier_chain_preserves_every_append() {
        let root = snapshot(session_id(1), 1, vec![entry(1)]);
        let terminal = (2..=512).fold(root, |source, index| {
            source
                .derive_appending_candidate(context_frontier_id(index), vec![entry(index)])
                .expect("each fresh exact entry extends the retained prefix")
        });

        assert_eq!(terminal.entry_count(), 512);
        assert_eq!(
            terminal.ordered_entries().collect::<Vec<_>>(),
            distinct_entries(512)
        );
    }

    /// scheduling validation traverses a shared 512-entry chain once
    /// even when the complete descendant is presented before its root.
    #[test]
    fn long_shared_reconstitution_validates_each_entry_once() {
        let root = ResolvedContextFrontierReconstitutionInput::new(
            session_id(1),
            context_frontier_id(1),
            vec![entry(1)],
        );
        let terminal = (2..=512).fold(root.clone(), |source, index| {
            source.derive_appending(context_frontier_id(index), vec![entry(index)])
        });
        let mut validation = ContextFrontierEntryValidationCache::default();
        let mut visits = 0usize;

        assert_eq!(
            terminal.first_missing_entry(&mut validation, |_| {
                visits += 1;
                true
            }),
            None
        );
        assert_eq!(
            root.first_missing_entry(&mut validation, |_| {
                visits += 1;
                true
            }),
            None
        );
        assert_eq!(visits, 512);
    }

    /// a duplicate late in a long retained prefix is still rejected
    /// with the complete source and append inputs unchanged.
    #[test]
    fn long_frontier_derivation_rejects_retained_duplicate_unchanged() {
        let source = snapshot(session_id(1), 1, distinct_entries(512));
        let appended_entries = vec![entry(513), entry(512)];

        assert_derivation_rejects_unchanged(
            &source,
            context_frontier_id(2),
            appended_entries,
            ContextFrontierSnapshotDerivationRejection::DuplicateEntry { entry: entry(512) },
        );
    }

    /// S09: later candidate derivation retains the complete earlier
    /// prefix in order and only appends exact new semantic entries.
    #[test]
    fn s09_derivation_is_prefix_preserving_and_append_only() {
        let owner = session_id(1);
        let first = entry(1);
        let second = entry(2);
        let appended_first = entry(3);
        let appended_second = entry(4);
        let source_entries = [first, second];
        let appended_entries = vec![appended_first, appended_second];
        let expected_derived_entries = [first, second, appended_first, appended_second];
        let next_snapshot = context_frontier_id(2);
        let source = snapshot(owner, 1, source_entries);
        let derived = source
            .derive_appending_candidate(next_snapshot, appended_entries)
            .expect("distinct entries and a fresh identity derive a candidate");

        assert_eq!(source.ordered_entries().collect::<Vec<_>>(), source_entries);
        assert_eq!(
            derived.ordered_entries().collect::<Vec<_>>(),
            expected_derived_entries
        );
        assert!(source.is_semantic_prefix_of(&derived));
        assert!(!derived.is_semantic_prefix_of(&source));
        assert_eq!(derived.frontier().owning_session(), owner);
        assert_eq!(derived.frontier().snapshot(), next_snapshot);

        let no_new_entries = vec![];
        let equal_content = source
            .derive_appending_candidate(context_frontier_id(3), no_new_entries)
            .expect("a separately identified equal-content snapshot is legal");
        assert_ne!(source.frontier(), equal_content.frontier());
        assert!(source.same_semantic_content(&equal_content));
    }

    /// derivation cannot reinterpret the source snapshot identity or
    /// duplicate an exact reference from either the retained prefix or the
    /// same append batch; every rejected append input is returned unchanged.
    #[test]
    fn invalid_derivations_preserve_source_and_inputs() {
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

    /// S17: a new consuming session owns its own frontier while
    /// preserving inherited source-session and semantic-entry identities
    /// before appending its own origin entry.
    #[test]
    fn s17_inherited_entry_references_are_preserved_without_reminting() {
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
