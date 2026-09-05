//! Append-only context compaction and deterministic frontier projection.
//!
//! docs/spec/sessions-and-transcript.md and
//! docs/spec/model-call-execution.md are normative. A compaction preserves the
//! complete durable frontier and adds one summary entry. Projection changes
//! only the ordered entries rendered for later model calls.

use crate::{
    ContextFrontier, ContextFrontierId, DirectModelSelection, ModelCallDisposition, ModelCallId,
    ResolvedContextFrontierSnapshot, ResolvedProviderTarget, SemanticTranscriptEntry,
    SemanticTranscriptEntryId, SemanticTranscriptEntryPayload, SemanticTranscriptEntryRef,
    SessionId,
};

crate::define_identity!(
    /// Identifies one immutable context-compaction record.
    ContextCompactionId
);

/// Token usage exactly as reported for one dedicated compaction call.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ContextCompactionTokenUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
}

impl ContextCompactionTokenUsage {
    /// Returns usage with every field unreported.
    pub const fn unreported() -> Self {
        Self {
            input_tokens: None,
            output_tokens: None,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        }
    }
    /// Retains the provider's input-token field exactly.
    pub const fn with_input_tokens(mut self, value: Option<u64>) -> Self {
        self.input_tokens = value;
        self
    }
    /// Retains the provider's output-token field exactly.
    pub const fn with_output_tokens(mut self, value: Option<u64>) -> Self {
        self.output_tokens = value;
        self
    }
    /// Retains the provider's cache-creation input-token field exactly.
    pub const fn with_cache_creation_input_tokens(mut self, value: Option<u64>) -> Self {
        self.cache_creation_input_tokens = value;
        self
    }
    /// Retains the provider's cache-read input-token field exactly.
    pub const fn with_cache_read_input_tokens(mut self, value: Option<u64>) -> Self {
        self.cache_read_input_tokens = value;
        self
    }
    /// Returns the provider's input-token field.
    pub const fn input_tokens(self) -> Option<u64> {
        self.input_tokens
    }
    /// Returns the provider's output-token field.
    pub const fn output_tokens(self) -> Option<u64> {
        self.output_tokens
    }
    /// Returns the provider's cache-creation input-token field.
    pub const fn cache_creation_input_tokens(self) -> Option<u64> {
        self.cache_creation_input_tokens
    }
    /// Returns the provider's cache-read input-token field.
    pub const fn cache_read_input_tokens(self) -> Option<u64> {
        self.cache_read_input_tokens
    }
    const fn is_unreported(self) -> bool {
        self.input_tokens.is_none()
            && self.output_tokens.is_none()
            && self.cache_creation_input_tokens.is_none()
            && self.cache_read_input_tokens.is_none()
    }
}

/// Stored lifecycle state of one dedicated compaction model call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextCompactionModelCallState {
    /// The call is durable but provider interaction is not authorized.
    Prepared,
    /// Provider interaction is durably authorized.
    InFlight,
    /// The immutable terminal disposition is recorded.
    Terminal(ModelCallDisposition),
}

/// One fully correlated dedicated compaction model call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextCompactionModelCall {
    id: ModelCallId,
    session: SessionId,
    selection: DirectModelSelection,
    target: ResolvedProviderTarget,
    source_frontier: ContextFrontier,
    state: ContextCompactionModelCallState,
    usage: ContextCompactionTokenUsage,
}

impl ContextCompactionModelCall {
    /// Returns the physical model-call identity.
    pub const fn id(&self) -> ModelCallId {
        self.id
    }
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }
    /// Returns the current direct selection used for the call.
    pub const fn selection(&self) -> DirectModelSelection {
        self.selection
    }
    /// Returns the exact resolved provider target.
    pub const fn target(&self) -> ResolvedProviderTarget {
        self.target
    }
    /// Returns the complete source frontier whose range is summarized.
    pub const fn source_frontier(&self) -> ContextFrontier {
        self.source_frontier
    }
    /// Returns the stored lifecycle state.
    pub const fn state(&self) -> ContextCompactionModelCallState {
        self.state
    }
    /// Returns exact provider-reported usage fields.
    pub const fn usage(&self) -> ContextCompactionTokenUsage {
        self.usage
    }
}

/// Complete independently stored facts for one compaction model call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextCompactionModelCallReconstitutionInput {
    call: ContextCompactionModelCall,
}

impl ContextCompactionModelCallReconstitutionInput {
    /// Supplies inert stored facts; [`Self::reconstitute`] checks the frontier.
    pub const fn new(
        id: ModelCallId,
        session: SessionId,
        selection: DirectModelSelection,
        target: ResolvedProviderTarget,
        source_frontier: ContextFrontierId,
        state: ContextCompactionModelCallState,
        usage: ContextCompactionTokenUsage,
    ) -> Self {
        Self {
            call: ContextCompactionModelCall {
                id,
                session,
                selection,
                target,
                source_frontier: ContextFrontier::new(session, source_frontier),
                state,
                usage,
            },
        }
    }

    /// Returns the stored call identity.
    pub const fn id(&self) -> ModelCallId {
        self.call.id
    }

    /// Returns the stored source-frontier snapshot identity.
    pub const fn source_snapshot(&self) -> ContextFrontierId {
        self.call.source_frontier.snapshot()
    }

    /// Reconstitutes only against the exact source snapshot and usage shape.
    pub fn reconstitute(
        self,
        source: &ResolvedContextFrontierSnapshot,
    ) -> Result<ContextCompactionModelCall, ContextCompactionModelCallReconstitutionFailure> {
        if self.call.source_frontier.owning_session() != self.call.session
            || source.frontier() != self.call.source_frontier
        {
            return Err(ContextCompactionModelCallReconstitutionFailure::FrontierMismatch);
        }
        if !matches!(
            self.call.state,
            ContextCompactionModelCallState::Terminal(_)
        ) && !self.call.usage.is_unreported()
        {
            return Err(ContextCompactionModelCallReconstitutionFailure::UsageBeforeTerminal);
        }
        Ok(self.call)
    }
}

/// Why stored dedicated-call facts cannot form one canonical record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextCompactionModelCallReconstitutionFailure {
    /// The source snapshot identity or session differs.
    FrontierMismatch,
    /// Provider usage appeared before a terminal observation.
    UsageBeforeTerminal,
}

/// The exact inclusive entry range summarized by one compaction call.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ContextCompactionRange {
    first: SemanticTranscriptEntryRef,
    through: SemanticTranscriptEntryRef,
}

impl ContextCompactionRange {
    /// Names the exact inclusive source-qualified entry range.
    pub const fn inclusive(
        first: SemanticTranscriptEntryRef,
        through: SemanticTranscriptEntryRef,
    ) -> Self {
        Self { first, through }
    }

    /// Returns the first summarized entry.
    pub const fn first(&self) -> SemanticTranscriptEntryRef {
        self.first
    }

    /// Returns the final summarized entry and projection boundary.
    pub const fn through(&self) -> SemanticTranscriptEntryRef {
        self.through
    }
}

/// One immutable, fully correlated context-compaction record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextCompaction {
    id: ContextCompactionId,
    session: SessionId,
    predecessor: Option<ContextCompactionId>,
    source_frontier: ContextFrontier,
    result_frontier: ContextFrontier,
    producing_call: ModelCallId,
    range: ContextCompactionRange,
    summary_entry: SemanticTranscriptEntryId,
}

impl ContextCompaction {
    /// Returns this compaction's distinct identity.
    pub const fn id(&self) -> ContextCompactionId {
        self.id
    }

    /// Returns the session whose model-input projection this changes.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the preceding compaction in this session, when one exists.
    pub const fn predecessor(&self) -> Option<ContextCompactionId> {
        self.predecessor
    }

    /// Returns the complete durable frontier supplied to summary production.
    pub const fn source_frontier(&self) -> ContextFrontier {
        self.source_frontier
    }

    /// Returns the complete durable frontier after appending the summary.
    pub const fn result_frontier(&self) -> ContextFrontier {
        self.result_frontier
    }

    /// Returns the dedicated model call that produced the summary.
    pub const fn producing_call(&self) -> ModelCallId {
        self.producing_call
    }

    /// Returns the exact inclusive summarized entry range.
    pub const fn range(&self) -> ContextCompactionRange {
        self.range
    }

    /// Returns the semantic entry carrying the resulting summary.
    pub const fn summary_entry(&self) -> SemanticTranscriptEntryId {
        self.summary_entry
    }
}

/// Complete independently stored facts for one compaction.
///
/// Construction is inert. Reconstitution validates the source frontier and
/// summary payload together before producing [`ContextCompaction`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextCompactionReconstitutionInput {
    id: ContextCompactionId,
    session: SessionId,
    predecessor: Option<ContextCompactionId>,
    source_frontier: ContextFrontier,
    result_frontier: ContextFrontier,
    producing_call: ModelCallId,
    range: ContextCompactionRange,
    summary_entry: SemanticTranscriptEntryId,
}

#[allow(
    clippy::too_many_arguments,
    reason = "the inert input names every independently stored compaction fact"
)]
impl ContextCompactionReconstitutionInput {
    /// Supplies one complete stored compaction candidate.
    pub const fn new(
        id: ContextCompactionId,
        session: SessionId,
        predecessor: Option<ContextCompactionId>,
        source_frontier: ContextFrontierId,
        result_frontier: ContextFrontierId,
        producing_call: ModelCallId,
        range: ContextCompactionRange,
        summary_entry: SemanticTranscriptEntryId,
    ) -> Self {
        Self {
            id,
            session,
            predecessor,
            source_frontier: ContextFrontier::new(session, source_frontier),
            result_frontier: ContextFrontier::new(session, result_frontier),
            producing_call,
            range,
            summary_entry,
        }
    }

    /// Returns the stored compaction identity.
    pub const fn id(&self) -> ContextCompactionId {
        self.id
    }

    /// Returns the stored source-frontier snapshot identity.
    pub const fn source_snapshot(&self) -> ContextFrontierId {
        self.source_frontier.snapshot()
    }

    /// Returns the stored result-frontier snapshot identity.
    pub const fn result_snapshot(&self) -> ContextFrontierId {
        self.result_frontier.snapshot()
    }

    /// Returns the stored producing-call identity.
    pub const fn producing_call(&self) -> ModelCallId {
        self.producing_call
    }

    /// Returns the stored summary-entry identity.
    pub const fn summary_entry(&self) -> SemanticTranscriptEntryId {
        self.summary_entry
    }

    /// Reconstitutes only when the exact frontier range and summary agree.
    pub fn reconstitute(
        self,
        source: &ResolvedContextFrontierSnapshot,
        result: &ResolvedContextFrontierSnapshot,
        source_entries: &[SemanticTranscriptEntry],
        result_entries: &[SemanticTranscriptEntry],
        summary: &SemanticTranscriptEntry,
        call: &ContextCompactionModelCall,
    ) -> Result<ContextCompaction, ContextCompactionReconstitutionFailure> {
        if self.source_frontier.owning_session() != self.session
            || self.result_frontier.owning_session() != self.session
        {
            return Err(ContextCompactionReconstitutionFailure::FrontierSessionMismatch);
        }
        if source.frontier() != self.source_frontier || result.frontier() != self.result_frontier {
            return Err(ContextCompactionReconstitutionFailure::FrontierIdentityMismatch);
        }
        if call.id() != self.producing_call
            || call.session() != self.session
            || call.source_frontier() != self.source_frontier
            || call.state()
                != ContextCompactionModelCallState::Terminal(ModelCallDisposition::Completed)
        {
            return Err(ContextCompactionReconstitutionFailure::ProducingCallMismatch);
        }
        if summary.source_session() != self.session || summary.identity() != self.summary_entry {
            return Err(ContextCompactionReconstitutionFailure::SummaryEntryMismatch);
        }
        let SemanticTranscriptEntryPayload::ContextSummary {
            producing_call,
            summarized,
            ..
        } = summary.payload()
        else {
            return Err(ContextCompactionReconstitutionFailure::SummaryPayloadMismatch);
        };
        if *producing_call != self.producing_call || *summarized != self.range {
            return Err(ContextCompactionReconstitutionFailure::SummaryPayloadMismatch);
        }
        if source_entries.len() != source.entry_count()
            || result_entries.len() != result.entry_count()
            || !source_entries
                .iter()
                .map(SemanticTranscriptEntry::reference)
                .eq(source.ordered_entries())
            || !result_entries
                .iter()
                .map(SemanticTranscriptEntry::reference)
                .eq(result.ordered_entries())
        {
            return Err(ContextCompactionReconstitutionFailure::FrontierEntryMismatch);
        }
        let source_projection = ContextFrontierProjection::from_complete_entries(source_entries)
            .map_err(|_| ContextCompactionReconstitutionFailure::SourceProjectionInvalid)?;
        let entries_by_reference = source_entries
            .iter()
            .map(|entry| (entry.reference(), entry))
            .collect::<std::collections::BTreeMap<_, _>>();
        let visible_entries = source_projection
            .ordered_entries()
            .map(|reference| entries_by_reference[&reference])
            .collect::<Vec<_>>();
        let first = visible_entries
            .iter()
            .position(|entry| entry.reference() == self.range.first())
            .ok_or(ContextCompactionReconstitutionFailure::RangeEndpointMissing)?;
        let through = visible_entries
            .iter()
            .position(|entry| entry.reference() == self.range.through())
            .ok_or(ContextCompactionReconstitutionFailure::RangeEndpointMissing)?;
        if first != 0 {
            return Err(ContextCompactionReconstitutionFailure::RangeStartMismatch);
        }
        if first > through {
            return Err(ContextCompactionReconstitutionFailure::RangeOrderInvalid);
        }
        if !range_closes_tool_exchanges(&visible_entries[first..=through]) {
            return Err(ContextCompactionReconstitutionFailure::UnsafeToolExchangeBoundary);
        }
        if result_entries.len() != source_entries.len() + 1
            || result_entries[..source_entries.len()] != *source_entries
            || result_entries.last() != Some(summary)
        {
            return Err(ContextCompactionReconstitutionFailure::ResultIsNotSummaryAppend);
        }
        Ok(ContextCompaction {
            id: self.id,
            session: self.session,
            predecessor: self.predecessor,
            source_frontier: self.source_frontier,
            result_frontier: self.result_frontier,
            producing_call: self.producing_call,
            range: self.range,
            summary_entry: self.summary_entry,
        })
    }
}

/// Why stored compaction facts cannot form one canonical record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextCompactionReconstitutionFailure {
    /// The source frontier belongs to another session.
    FrontierSessionMismatch,
    /// A supplied snapshot does not have the stored frontier identity.
    FrontierIdentityMismatch,
    /// Supplied entries do not exactly resolve their respective snapshots.
    FrontierEntryMismatch,
    /// Existing summary entries cannot form one valid visible source frontier.
    SourceProjectionInvalid,
    /// The named summary identity or source session differs.
    SummaryEntryMismatch,
    /// The named entry does not carry matching compaction provenance.
    SummaryPayloadMismatch,
    /// At least one exact range endpoint is absent from the source frontier.
    RangeEndpointMissing,
    /// The range does not begin at the current model-visible frontier start.
    RangeStartMismatch,
    /// The first endpoint occurs after the through endpoint.
    RangeOrderInvalid,
    /// The summarized range leaves a correlated tool exchange open.
    UnsafeToolExchangeBoundary,
    /// The result frontier is not exactly the source plus its summary.
    ResultIsNotSummaryAppend,
    /// The producing call is not the matching completed dedicated call.
    ProducingCallMismatch,
}

fn range_closes_tool_exchanges(entries: &[&SemanticTranscriptEntry]) -> bool {
    let mut open_requests = 0usize;
    for entry in entries {
        match entry.payload() {
            SemanticTranscriptEntryPayload::AssistantToolUse { .. } => {
                open_requests = open_requests.saturating_add(1);
            }
            SemanticTranscriptEntryPayload::ToolExecutionResult { .. }
            | SemanticTranscriptEntryPayload::ToolDenied { .. }
            | SemanticTranscriptEntryPayload::ToolClosed { .. } => {
                let Some(remaining) = open_requests.checked_sub(1) else {
                    return false;
                };
                open_requests = remaining;
            }
            _ => {}
        }
    }
    open_requests == 0
}

/// The exact source-qualified entries visible after applying compaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextFrontierProjection {
    ordered_entries: Box<[SemanticTranscriptEntryRef]>,
}

impl ContextFrontierProjection {
    /// Projects the latest summary plus every entry after its boundary.
    ///
    /// With no summary entry, the complete frontier remains visible.
    pub fn from_complete_entries(
        entries: &[SemanticTranscriptEntry],
    ) -> Result<Self, ContextFrontierProjectionFailure> {
        let physical_positions = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.reference(), index))
            .collect::<std::collections::BTreeMap<_, _>>();
        let mut visible = entries.iter().collect::<Vec<_>>();
        for (summary_index, summary) in entries.iter().enumerate() {
            let SemanticTranscriptEntryPayload::ContextSummary { summarized, .. } =
                summary.payload()
            else {
                continue;
            };
            let first = visible
                .iter()
                .position(|entry| entry.reference() == summarized.first())
                .ok_or(ContextFrontierProjectionFailure::RangeEndpointMissing)?;
            if first != 0 {
                return Err(ContextFrontierProjectionFailure::RangeStartMismatch);
            }
            let through = visible
                .iter()
                .position(|entry| entry.reference() == summarized.through())
                .ok_or(ContextFrontierProjectionFailure::RangeEndpointMissing)?;
            if first > through {
                return Err(ContextFrontierProjectionFailure::RangeOrderInvalid);
            }
            if !range_closes_tool_exchanges(&visible[first..=through]) {
                return Err(ContextFrontierProjectionFailure::UnsafeToolExchangeBoundary);
            }
            let physical_through = physical_positions[&summarized.through()];
            let visible_summary = visible
                .iter()
                .position(|entry| entry.reference() == summary.reference())
                .ok_or(ContextFrontierProjectionFailure::RangeEndpointMissing)?;
            if summary_index <= physical_through || visible_summary <= through {
                return Err(ContextFrontierProjectionFailure::SummaryNotAfterBoundary);
            }
            visible = std::iter::once(summary)
                .chain(
                    visible[through + 1..]
                        .iter()
                        .copied()
                        .filter(|entry| entry.reference() != summary.reference()),
                )
                .collect();
        }
        Ok(Self {
            ordered_entries: visible
                .into_iter()
                .map(SemanticTranscriptEntry::reference)
                .collect(),
        })
    }

    /// Iterates the exact model-visible entry order.
    pub fn ordered_entries(
        &self,
    ) -> impl ExactSizeIterator<Item = SemanticTranscriptEntryRef> + '_ {
        self.ordered_entries.iter().copied()
    }
}

/// Why a complete durable frontier cannot be projected safely.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextFrontierProjectionFailure {
    /// A summary range starts after the model-visible frontier start.
    RangeStartMismatch,
    /// At least one exact summarized-range endpoint is absent.
    RangeEndpointMissing,
    /// The first summarized endpoint occurs after the through endpoint.
    RangeOrderInvalid,
    /// The summarized range leaves a correlated tool exchange open.
    UnsafeToolExchangeBoundary,
    /// The selected summary is not a later append after its boundary.
    SummaryNotAfterBoundary,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        direct, model_call_id, provider_model_identity, semantic_transcript_entry_id, session_id,
    };
    use crate::{AssistantText, InitialSemanticTranscriptEntryPayload};

    fn entry(value: u128) -> SemanticTranscriptEntry {
        SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(value),
            session_id(1),
            InitialSemanticTranscriptEntryPayload::TurnFailed {
                turn: crate::test_support::turn_id(value),
            },
        )
    }

    fn tool_use(value: u128, request: u128) -> SemanticTranscriptEntry {
        SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(value),
            session_id(1),
            InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call: model_call_id(8),
                request: crate::ToolRequestId::from_uuid(uuid::Uuid::from_u128(request)),
            },
        )
    }

    fn tool_denied(value: u128, request: u128) -> SemanticTranscriptEntry {
        SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(value),
            session_id(1),
            InitialSemanticTranscriptEntryPayload::ToolDenied {
                request: crate::ToolRequestId::from_uuid(uuid::Uuid::from_u128(request)),
            },
        )
    }

    fn summary(value: u128, summarized: ContextCompactionRange) -> SemanticTranscriptEntry {
        SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(value),
            session_id(1),
            InitialSemanticTranscriptEntryPayload::ContextSummary {
                producing_call: model_call_id(8),
                summarized,
                value: AssistantText::try_new(String::from("compact history"))
                    .expect("fixture summary is nonempty"),
            },
        )
    }

    /// projection changes model visibility without removing any
    /// durable entry from the complete frontier.
    #[test]
    fn projection_is_summary_plus_suffix_while_source_stays_complete() {
        let first = entry(1);
        let through = entry(2);
        let suffix = entry(3);
        let range = ContextCompactionRange::inclusive(first.reference(), through.reference());
        let summary = summary(4, range);
        let complete = vec![
            first.clone(),
            through.clone(),
            suffix.clone(),
            summary.clone(),
        ];

        let projection = ContextFrontierProjection::from_complete_entries(&complete)
            .expect("the appended summary projects");

        assert_eq!(complete.len(), 4);
        assert_eq!(complete[0], first);
        assert_eq!(complete[1], through);
        assert_eq!(complete[2], suffix);
        assert_eq!(complete[3], summary);
        assert_eq!(
            projection.ordered_entries().collect::<Vec<_>>(),
            vec![summary.reference(), suffix.reference()]
        );
    }

    /// stored compaction facts must agree with the summary payload
    /// and exact source-frontier range.
    #[test]
    fn projection_rejects_a_range_that_hides_unsummarized_prefix() {
        let first = entry(1);
        let hidden = entry(2);
        let through = entry(3);
        let range = ContextCompactionRange::inclusive(hidden.reference(), through.reference());
        let summary = summary(4, range);

        assert_eq!(
            ContextFrontierProjection::from_complete_entries(&[first, hidden, through, summary]),
            Err(ContextFrontierProjectionFailure::RangeStartMismatch)
        );
    }

    /// a boundary cannot separate a tool proposal from its correlated
    /// result because the suffix would be invalid provider conversation history.
    #[test]
    fn projection_rejects_open_tool_exchange_boundary() {
        let proposal = tool_use(1, 9);
        let result = tool_denied(2, 9);
        let range = ContextCompactionRange::inclusive(proposal.reference(), proposal.reference());
        let summary = summary(3, range);

        assert_eq!(
            ContextFrontierProjection::from_complete_entries(&[proposal, result, summary]),
            Err(ContextFrontierProjectionFailure::UnsafeToolExchangeBoundary)
        );
    }

    /// stored compaction facts must agree with the summary payload
    /// and exact source-frontier range.
    #[test]
    fn compaction_reconstitution_preserves_exact_provenance() {
        let first = entry(1);
        let through = entry(2);
        let range = ContextCompactionRange::inclusive(first.reference(), through.reference());
        let summary = summary(4, range);
        let source_snapshot = crate::ResolvedContextFrontierSnapshot::try_from_candidate(
            session_id(1),
            crate::ContextFrontierId::from_uuid(uuid::Uuid::from_u128(7)),
            vec![first.reference(), through.reference()],
        )
        .expect("fixture source frontier has unique entries");
        let source_frontier = source_snapshot.frontier();
        let result_snapshot = crate::ResolvedContextFrontierSnapshot::try_from_candidate(
            session_id(1),
            crate::ContextFrontierId::from_uuid(uuid::Uuid::from_u128(9)),
            vec![first.reference(), through.reference(), summary.reference()],
        )
        .expect("fixture result frontier appends the summary");
        let input = ContextCompactionReconstitutionInput::new(
            ContextCompactionId::from_uuid(uuid::Uuid::from_u128(6)),
            session_id(1),
            None,
            source_frontier.snapshot(),
            result_snapshot.frontier().snapshot(),
            model_call_id(8),
            range,
            summary.identity(),
        );
        let call = ContextCompactionModelCallReconstitutionInput::new(
            model_call_id(8),
            session_id(1),
            direct(10),
            ResolvedProviderTarget::naming(provider_model_identity(11)),
            source_frontier.snapshot(),
            ContextCompactionModelCallState::Terminal(ModelCallDisposition::Completed),
            ContextCompactionTokenUsage::unreported().with_input_tokens(Some(42)),
        )
        .reconstitute(&source_snapshot)
        .expect("fixture compaction call is completed against its source frontier");

        let compaction = input
            .reconstitute(
                &source_snapshot,
                &result_snapshot,
                &[first, through],
                &[entry(1), entry(2), summary.clone()],
                &summary,
                &call,
            )
            .expect("the exact stored compaction reconstructs");

        assert_eq!(compaction.session(), session_id(1));
        assert_eq!(compaction.predecessor(), None);
        assert_eq!(compaction.source_frontier(), source_frontier);
        assert_eq!(compaction.result_frontier(), result_snapshot.frontier());
        assert_eq!(compaction.producing_call(), model_call_id(8));
        assert_eq!(compaction.range(), range);
        assert_eq!(compaction.summary_entry(), summary.identity());
    }

    /// successor ranges are interpreted in the current model-visible
    /// order even when a retained suffix physically precedes the prior summary.
    #[test]
    fn successor_projection_uses_visible_order_across_prior_summary() {
        let first = entry(1);
        let through = entry(2);
        let retained_suffix = entry(3);
        let first_range = ContextCompactionRange::inclusive(first.reference(), through.reference());
        let first_summary = summary(4, first_range);
        let later = entry(5);
        let successor_range =
            ContextCompactionRange::inclusive(first_summary.reference(), later.reference());
        let successor_summary = summary(6, successor_range);
        let complete = vec![
            first,
            through,
            retained_suffix.clone(),
            first_summary,
            later,
            successor_summary.clone(),
        ];

        let projection = ContextFrontierProjection::from_complete_entries(&complete)
            .expect("the successor summarizes the projected predecessor frontier");

        assert_eq!(complete.len(), 6);
        assert_eq!(complete[2], retained_suffix);
        assert_eq!(
            projection.ordered_entries().collect::<Vec<_>>(),
            vec![successor_summary.reference()]
        );
    }
}
