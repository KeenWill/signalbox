//! Fixed-input fixtures and measured functions shared by `instruction_counts.rs`
//! and `wall_clock.rs`.
//!
//! Each pair (a `*_fixture` constructor and its measured function) supplies one
//! deterministic, pure-CPU domain workload — scheduling reconstruction, deep
//! frontier construction, shared-prefix proof, interrupt total ordering,
//! compaction frontier projection, and tool-argument canonicalization. Exact
//! shapes and counts are documented beside each constructor; see `README.md`
//! for how a change here shifts the baseline.

use signalbox_domain::{
    AcceptedInputDisposition, AcceptedInputLifecycle, AcceptedInputQueueOrder,
    AcceptedInputQueueWork, AcceptedInputSchedulingProjection,
    AcceptedInputSchedulingReconstitutionInput, AcceptedInputTurnSchedulingRecord,
    AcceptedInputTurnSchedulingRecordState, ActiveTurnSchedulingReconstitutionInput,
    ContextFrontierId, ContextFrontierProjection, DeliveryRequest, DirectModelSelection,
    ModelSelectionOverride, ModelSelectionRequest, NormalizedToolArguments, OriginConfiguration,
    PerInputConfigurationChoices, ResolvedContextFrontierReconstitutionInput,
    ResolvedContextFrontierSnapshot, SemanticTranscriptEntry, SemanticTranscriptEntryId,
    SemanticTranscriptEntryReconstitutionInput, SemanticTranscriptEntryRef, Session,
    SessionAcceptanceTailEntryReconstitutionInput, SessionAcceptanceTailReconstitutionInput,
    SessionConfigurationDefaults, SessionConfigurationDefaultsVersion, SessionCreationCause,
    SessionCreationProvenance, SessionId, SessionInputPosition, SessionReconstitutionInput,
    ToolArgumentsKind, TranscriptAncestry, TurnAttemptId, TurnId,
    derive_accepted_input_total_order,
};
use uuid::Uuid;

const SCHEDULING_SESSION_ID_SEED: u128 = 1;
const SESSION_DEFAULT_MODEL_ID_SEED: u128 = 2;
const SCHEDULING_TURN_ID_SEED: u128 = 10_000;
const SCHEDULING_ACCEPTED_INPUT_ID_SEED: u128 = 20_000;
const SCHEDULING_ORIGIN_ENTRY_ID_SEED: u128 = 30_000;
const SCHEDULING_FAILURE_ENTRY_ID_SEED: u128 = 30_001;
const SCHEDULING_STARTING_FRONTIER_ID_SEED: u128 = 40_000;
const SCHEDULING_TERMINAL_FRONTIER_ID_SEED: u128 = 40_001;
const ACTIVE_TURN_ATTEMPT_ID_SEED: u128 = 50_000;
const FRONTIER_SESSION_ID_SEED: u128 = 60_000;
const FRONTIER_ENTRY_ID_SEED: u128 = 61_000;
const FRONTIER_SNAPSHOT_ID_SEED: u128 = 62_000;
const TOTAL_ORDER_SESSION_ID_SEED: u128 = 70_000;
const TOTAL_ORDER_TURN_ID_SEED: u128 = 80_000;
const TERMINAL_TURN_COUNT: u64 = 48;
const QUEUED_TURN_COUNT: u64 = 15;
const FRONTIER_LAYER_COUNT: u128 = 64;
const FRONTIER_ENTRIES_PER_LAYER: u128 = 8;
const PREFIX_LAYER_COUNT: usize = 48;
const TOTAL_ORDER_CHAIN_COUNT: u64 = 64;
const TOTAL_ORDER_CHAIN_LENGTH: u64 = 4;
const TOOL_ARGUMENT_MEMBER_COUNT: u32 = 128;
const TOOL_ARGUMENT_CAPACITY_HINT_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug)]
pub enum FixtureError {
    Session,
    Configuration,
    Position,
    Frontier,
    SemanticEntryMissing,
    ToolArguments,
}

pub type SchedulingFixture = AcceptedInputSchedulingReconstitutionInput;
pub type SchedulingResult = AcceptedInputSchedulingProjection;
pub type ProjectionFixture = Vec<SemanticTranscriptEntry>;
pub type ProjectionResult = ContextFrontierProjection;

pub struct FrontierBuildInput {
    session: SessionId,
    first_snapshot: ContextFrontierId,
    first_entries: Vec<SemanticTranscriptEntryRef>,
    later_layers: Vec<(ContextFrontierId, Vec<SemanticTranscriptEntryRef>)>,
}

pub struct PrefixProofFixture {
    prefix: ResolvedContextFrontierSnapshot,
    later: ResolvedContextFrontierSnapshot,
}

fn fixture_or_exit<T, Failure>(result: Result<T, Failure>) -> T
where
    Failure: std::fmt::Debug,
{
    match result {
        Ok(value) => value,
        Err(failure) => {
            eprintln!("domain benchmark fixture is invalid: {failure:?}");
            std::process::exit(2);
        }
    }
}

fn session_id(seed: u128) -> SessionId {
    SessionId::from_uuid(Uuid::from_u128(seed))
}

fn turn_id(seed: u128) -> TurnId {
    TurnId::from_uuid(Uuid::from_u128(seed))
}

fn accepted_input_id(seed: u128) -> signalbox_domain::AcceptedInputId {
    signalbox_domain::AcceptedInputId::from_uuid(Uuid::from_u128(seed))
}

fn semantic_entry_id(seed: u128) -> SemanticTranscriptEntryId {
    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed))
}

fn frontier_id(seed: u128) -> ContextFrontierId {
    ContextFrontierId::from_uuid(Uuid::from_u128(seed))
}

fn attempt_id(seed: u128) -> TurnAttemptId {
    TurnAttemptId::from_uuid(Uuid::from_u128(seed))
}

fn position(ordinal: u64) -> Result<SessionInputPosition, FixtureError> {
    SessionInputPosition::try_from_u64(ordinal).ok_or(FixtureError::Position)
}

fn current_session() -> Result<Session, FixtureError> {
    let session = session_id(SCHEDULING_SESSION_ID_SEED);
    let version = SessionConfigurationDefaultsVersion::first();
    let defaults = SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
        DirectModelSelection::from_uuid(Uuid::from_u128(SESSION_DEFAULT_MODEL_ID_SEED)),
    ));
    SessionReconstitutionInput::new(
        session,
        session,
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None),
        session,
        version,
        session,
        version,
        defaults,
        signalbox_domain::SessionPlacementReconstitutionFacts {
            current_pointer_session: session,
            current_pointer_version: signalbox_domain::SessionPlacementVersion::INITIAL,
            selected_event_session: session,
            selected_event: signalbox_domain::VersionedSessionPlacement::initial(
                signalbox_domain::SessionPlacement::pathless(),
            ),
        },
    )
    .reconstitute()
    .map_err(|_| FixtureError::Session)
}

fn configuration(session: &Session) -> Result<OriginConfiguration, FixtureError> {
    let checked = session
        .current_configuration_defaults()
        .derive_request(
            SessionConfigurationDefaultsVersion::first(),
            ModelSelectionOverride::UseSessionDefault,
        )
        .map_err(|_| FixtureError::Configuration)?;
    OriginConfiguration::freeze(checked, |_| None).map_err(|_| FixtureError::Configuration)
}

fn choices() -> PerInputConfigurationChoices {
    PerInputConfigurationChoices::new(
        SessionConfigurationDefaultsVersion::first(),
        ModelSelectionOverride::UseSessionDefault,
    )
}

fn delivery(predecessor: Option<TurnId>) -> DeliveryRequest {
    match predecessor {
        Some(expected_active_turn) => DeliveryRequest::AfterCurrentTurn {
            expected_active_turn,
            configuration: choices(),
        },
        None => DeliveryRequest::StartWhenNoActiveTurn {
            configuration: choices(),
        },
    }
}

fn turn_for_ordinal(ordinal: u64) -> TurnId {
    turn_id(SCHEDULING_TURN_ID_SEED + u128::from(ordinal))
}

fn accepted_input_for_ordinal(ordinal: u64) -> signalbox_domain::AcceptedInputId {
    accepted_input_id(SCHEDULING_ACCEPTED_INPUT_ID_SEED + u128::from(ordinal))
}

fn origin_entry_for_ordinal(ordinal: u64) -> SemanticTranscriptEntryId {
    semantic_entry_id(SCHEDULING_ORIGIN_ENTRY_ID_SEED + u128::from(ordinal) * 2)
}

fn failure_entry_for_ordinal(ordinal: u64) -> SemanticTranscriptEntryId {
    semantic_entry_id(SCHEDULING_FAILURE_ENTRY_ID_SEED + u128::from(ordinal) * 2)
}

fn starting_frontier_for_ordinal(ordinal: u64) -> ContextFrontierId {
    frontier_id(SCHEDULING_STARTING_FRONTIER_ID_SEED + u128::from(ordinal) * 2)
}

fn terminal_frontier_for_ordinal(ordinal: u64) -> ContextFrontierId {
    frontier_id(SCHEDULING_TERMINAL_FRONTIER_ID_SEED + u128::from(ordinal) * 2)
}

fn lifecycle(ordinal: u64) -> AcceptedInputLifecycle {
    let turn = turn_for_ordinal(ordinal);
    AcceptedInputLifecycle::new(
        accepted_input_for_ordinal(ordinal),
        AcceptedInputDisposition::OriginOf(turn),
    )
}

/// Builds the fixed scheduling input shared by both suites.
///
/// Shape: 64 turns in one session: a 48-turn failed-terminal prefix, one
/// prepared active turn, and 15 queued turns. The transcript has 97 entries
/// (an origin and failure marker per terminal turn plus the active origin) and
/// 97 structurally shared snapshots. The active acceptance tail has 16
/// entries, covering the active origin and all 15 later queued origins.
fn try_scheduling_fixture() -> Result<SchedulingFixture, FixtureError> {
    let session = current_session()?;
    let configuration = configuration(&session)?;
    let mut turns = Vec::with_capacity(
        usize::try_from(TERMINAL_TURN_COUNT + 1 + QUEUED_TURN_COUNT)
            .map_err(|_| FixtureError::Position)?,
    );
    let mut semantic_entries = Vec::with_capacity(
        usize::try_from(TERMINAL_TURN_COUNT * 2 + 1).map_err(|_| FixtureError::Position)?,
    );
    let mut snapshots = Vec::with_capacity(
        usize::try_from(TERMINAL_TURN_COUNT * 2 + 1).map_err(|_| FixtureError::Position)?,
    );
    let mut prior_snapshot: Option<ResolvedContextFrontierReconstitutionInput> = None;
    let mut prior_turn = None;

    for ordinal in 1..=TERMINAL_TURN_COUNT {
        let turn = turn_for_ordinal(ordinal);
        let origin_entry = origin_entry_for_ordinal(ordinal);
        let failure_entry = failure_entry_for_ordinal(ordinal);
        let origin_reference = SemanticTranscriptEntryRef::from_source(session.id(), origin_entry);
        let failure_reference =
            SemanticTranscriptEntryRef::from_source(session.id(), failure_entry);
        let starting_frontier = starting_frontier_for_ordinal(ordinal);
        let terminal_frontier = terminal_frontier_for_ordinal(ordinal);
        let starting_snapshot = match prior_snapshot.as_ref() {
            Some(previous) => previous.derive_appending(starting_frontier, vec![origin_reference]),
            None => ResolvedContextFrontierReconstitutionInput::new(
                session.id(),
                starting_frontier,
                vec![origin_reference],
            ),
        };
        let terminal_snapshot =
            starting_snapshot.derive_appending(terminal_frontier, vec![failure_reference]);
        let starting_lineage = match prior_turn {
            Some(immediate_predecessor) => signalbox_domain::AcceptedInputStartingLineage::After {
                immediate_predecessor,
            },
            None => signalbox_domain::AcceptedInputStartingLineage::FirstInSession,
        };

        turns.push(AcceptedInputTurnSchedulingRecord::new(
            session.id(),
            turn,
            session.id(),
            lifecycle(ordinal),
            session.id(),
            turn,
            AcceptedInputQueueOrder::ordinary(position(ordinal)?),
            delivery(prior_turn),
            configuration.clone(),
            AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                starting_lineage,
                starting_frontier,
                terminal_execution: None,
                terminal_frontier,
            },
        ));
        semantic_entries.push(SemanticTranscriptEntryReconstitutionInput::new(
            origin_entry,
            session.id(),
            signalbox_domain::SemanticTranscriptEntryPayload::OriginAcceptedInput {
                accepted_input: accepted_input_for_ordinal(ordinal),
            },
        ));
        semantic_entries.push(SemanticTranscriptEntryReconstitutionInput::new(
            failure_entry,
            session.id(),
            signalbox_domain::SemanticTranscriptEntryPayload::TurnFailed { turn },
        ));
        snapshots.push(starting_snapshot);
        snapshots.push(terminal_snapshot.clone());
        prior_snapshot = Some(terminal_snapshot);
        prior_turn = Some(turn);
    }

    let active_ordinal = TERMINAL_TURN_COUNT + 1;
    let active_turn = turn_for_ordinal(active_ordinal);
    let active_origin = origin_entry_for_ordinal(active_ordinal);
    let active_reference = SemanticTranscriptEntryRef::from_source(session.id(), active_origin);
    let active_frontier = starting_frontier_for_ordinal(active_ordinal);
    let active_snapshot = match prior_snapshot.as_ref() {
        Some(previous) => previous.derive_appending(active_frontier, vec![active_reference]),
        None => ResolvedContextFrontierReconstitutionInput::new(
            session.id(),
            active_frontier,
            vec![active_reference],
        ),
    };
    turns.push(AcceptedInputTurnSchedulingRecord::new(
        session.id(),
        active_turn,
        session.id(),
        lifecycle(active_ordinal),
        session.id(),
        active_turn,
        AcceptedInputQueueOrder::ordinary(position(active_ordinal)?),
        delivery(prior_turn),
        configuration.clone(),
        AcceptedInputTurnSchedulingRecordState::Active {
            starting_lineage: signalbox_domain::AcceptedInputStartingLineage::After {
                immediate_predecessor: prior_turn.ok_or(FixtureError::Position)?,
            },
            starting_frontier: active_frontier,
            phase: ActiveTurnSchedulingReconstitutionInput::prepared(
                active_turn,
                attempt_id(ACTIVE_TURN_ATTEMPT_ID_SEED),
            ),
        },
    ));
    semantic_entries.push(SemanticTranscriptEntryReconstitutionInput::new(
        active_origin,
        session.id(),
        signalbox_domain::SemanticTranscriptEntryPayload::OriginAcceptedInput {
            accepted_input: accepted_input_for_ordinal(active_ordinal),
        },
    ));
    snapshots.push(active_snapshot);

    let mut tail_entries = Vec::with_capacity(
        usize::try_from(QUEUED_TURN_COUNT + 1).map_err(|_| FixtureError::Position)?,
    );
    tail_entries.push(SessionAcceptanceTailEntryReconstitutionInput::new(
        session.id(),
        lifecycle(active_ordinal),
        position(active_ordinal)?,
        delivery(prior_turn),
    ));
    for offset in 1..=QUEUED_TURN_COUNT {
        let ordinal = active_ordinal + offset;
        let queued_turn = turn_for_ordinal(ordinal);
        let queued_delivery = delivery(Some(active_turn));
        turns.push(AcceptedInputTurnSchedulingRecord::new(
            session.id(),
            queued_turn,
            session.id(),
            lifecycle(ordinal),
            session.id(),
            queued_turn,
            AcceptedInputQueueOrder::ordinary(position(ordinal)?),
            queued_delivery,
            configuration.clone(),
            AcceptedInputTurnSchedulingRecordState::Queued,
        ));
        tail_entries.push(SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            lifecycle(ordinal),
            position(ordinal)?,
            queued_delivery,
        ));
    }
    let final_ordinal = active_ordinal + QUEUED_TURN_COUNT;
    let tail = SessionAcceptanceTailReconstitutionInput::new(
        session.id(),
        accepted_input_for_ordinal(active_ordinal),
        position(final_ordinal)?,
        tail_entries,
    );

    Ok(AcceptedInputSchedulingReconstitutionInput::new(
        session,
        turns,
        semantic_entries,
        snapshots,
        Some(tail),
    ))
}

pub fn scheduling_fixture() -> SchedulingFixture {
    fixture_or_exit(try_scheduling_fixture())
}

/// Measures complete scheduling reconstruction, including `reconstitute_inner`.
pub fn reconstitute_scheduling(input: SchedulingFixture) -> SchedulingResult {
    fixture_or_exit(input.reconstitute())
}

/// Supplies a fixed 64-layer, 512-entry frontier construction workload.
///
/// Every layer appends eight distinct source-qualified entries. Snapshot and
/// entry identities are fixed synthetic UUIDs, and each layer shares the
/// complete prefix built by the prior layer.
pub fn frontier_build_input() -> FrontierBuildInput {
    let session = session_id(FRONTIER_SESSION_ID_SEED);
    let mut layers = (0..FRONTIER_LAYER_COUNT).map(|layer| {
        let entries = (0..FRONTIER_ENTRIES_PER_LAYER)
            .map(|offset| {
                SemanticTranscriptEntryRef::from_source(
                    session,
                    semantic_entry_id(
                        FRONTIER_ENTRY_ID_SEED + layer * FRONTIER_ENTRIES_PER_LAYER + offset,
                    ),
                )
            })
            .collect::<Vec<_>>();
        (frontier_id(FRONTIER_SNAPSHOT_ID_SEED + layer), entries)
    });
    let (first_snapshot, first_entries) =
        fixture_or_exit(layers.next().ok_or(FixtureError::Frontier));
    FrontierBuildInput {
        session,
        first_snapshot,
        first_entries,
        later_layers: layers.collect(),
    }
}

/// Builds all 64 structurally shared frontier layers.
pub fn build_deep_frontier(
    input: FrontierBuildInput,
) -> ResolvedContextFrontierReconstitutionInput {
    let mut frontier = ResolvedContextFrontierReconstitutionInput::new(
        input.session,
        input.first_snapshot,
        input.first_entries,
    );
    for (snapshot, entries) in input.later_layers {
        frontier = frontier.derive_appending(snapshot, entries);
    }
    frontier
}

/// Builds the fixed prefix-proof operands outside either measurement.
///
/// The prefix is layer 48 with 384 entries; the later frontier is layer 64
/// with 512 entries. Both retain the same structural-sharing lineage, so the
/// measured proof exercises the shared-prefix lookup without scanning entries.
pub fn prefix_proof_fixture() -> PrefixProofFixture {
    let input = frontier_build_input();
    let mut frontier = ResolvedContextFrontierReconstitutionInput::new(
        input.session,
        input.first_snapshot,
        input.first_entries,
    );
    let mut prefix = None;
    for (index, (snapshot, entries)) in input.later_layers.into_iter().enumerate() {
        frontier = frontier.derive_appending(snapshot, entries);
        if index + 2 == PREFIX_LAYER_COUNT {
            prefix = Some(frontier.clone());
        }
    }
    let operands = fixture_or_exit(
        prefix
            .and_then(ResolvedContextFrontierReconstitutionInput::reconstitute)
            .zip(frontier.reconstitute())
            .map(|(prefix, later)| PrefixProofFixture { prefix, later })
            .ok_or(FixtureError::Frontier),
    );
    fixture_or_exit(
        operands
            .prefix
            .is_semantic_prefix_of(&operands.later)
            .then_some(())
            .ok_or(FixtureError::Frontier),
    );
    operands
}

/// Proves the fixed 384-entry frontier is a semantic prefix of the 512-entry
/// frontier.
pub fn prove_shared_prefix(input: &PrefixProofFixture) -> bool {
    input.prefix.is_semantic_prefix_of(&input.later)
}

/// Supplies a fixed total-order workload with realistic interrupt chaining.
///
/// Shape: 256 turns in 64 groups. Each group has one ordinary root followed
/// by three `InterruptImmediatelyAfter` successors. Acceptance positions are
/// 1 through 256 and identities deliberately descend as positions increase,
/// preventing identity order from coinciding with durable acceptance order.
pub fn total_order_fixture() -> Vec<AcceptedInputQueueWork> {
    let session = session_id(TOTAL_ORDER_SESSION_ID_SEED);
    let capacity = fixture_or_exit(
        usize::try_from(TOTAL_ORDER_CHAIN_COUNT * TOTAL_ORDER_CHAIN_LENGTH)
            .map_err(|_| FixtureError::Position),
    );
    let mut work = Vec::with_capacity(capacity);
    for chain in 0..TOTAL_ORDER_CHAIN_COUNT {
        let root_ordinal = chain * TOTAL_ORDER_CHAIN_LENGTH + 1;
        let root = turn_id(TOTAL_ORDER_TURN_ID_SEED + u128::from(u64::MAX - root_ordinal));
        let root_position = fixture_or_exit(position(root_ordinal));
        work.push(AcceptedInputQueueWork::new(
            session,
            root,
            AcceptedInputQueueOrder::ordinary(root_position),
        ));
        let mut predecessor = root;
        for offset in 1..TOTAL_ORDER_CHAIN_LENGTH {
            let ordinal = root_ordinal + offset;
            let turn = turn_id(TOTAL_ORDER_TURN_ID_SEED + u128::from(u64::MAX - ordinal));
            let accepted_position = fixture_or_exit(position(ordinal));
            work.push(AcceptedInputQueueWork::new(
                session,
                turn,
                AcceptedInputQueueOrder::interrupt_immediately_after(
                    accepted_position,
                    predecessor,
                ),
            ));
            predecessor = turn;
        }
    }
    work
}

/// Derives total order for the fixed interrupt-chain inventory.
pub fn derive_interrupt_total_order(work: Vec<AcceptedInputQueueWork>) -> Vec<TurnId> {
    fixture_or_exit(derive_accepted_input_total_order(work))
}

/// Supplies 97 validated transcript entries from the shared scheduling input.
///
/// The entries contain 48 origin/failure pairs plus one active origin and no
/// summary yet. This measures the common pre-compaction projection of a
/// realistic complete frontier, including its reference-position index.
fn try_compaction_projection_fixture() -> Result<ProjectionFixture, FixtureError> {
    let projection = reconstitute_scheduling(scheduling_fixture());
    let session = projection.session().id();
    let capacity =
        usize::try_from(TERMINAL_TURN_COUNT * 2 + 1).map_err(|_| FixtureError::Position)?;
    let mut entries = Vec::with_capacity(capacity);
    for ordinal in 1..=TERMINAL_TURN_COUNT {
        let origin =
            SemanticTranscriptEntryRef::from_source(session, origin_entry_for_ordinal(ordinal));
        let failure =
            SemanticTranscriptEntryRef::from_source(session, failure_entry_for_ordinal(ordinal));
        entries.push(
            projection
                .semantic_entry(origin)
                .cloned()
                .ok_or(FixtureError::SemanticEntryMissing)?,
        );
        entries.push(
            projection
                .semantic_entry(failure)
                .cloned()
                .ok_or(FixtureError::SemanticEntryMissing)?,
        );
    }
    let active_origin = SemanticTranscriptEntryRef::from_source(
        session,
        origin_entry_for_ordinal(TERMINAL_TURN_COUNT + 1),
    );
    entries.push(
        projection
            .semantic_entry(active_origin)
            .cloned()
            .ok_or(FixtureError::SemanticEntryMissing)?,
    );
    Ok(entries)
}

pub fn compaction_projection_fixture() -> ProjectionFixture {
    fixture_or_exit(try_compaction_projection_fixture())
}

/// Projects the fixed complete transcript through the compaction visibility
/// algorithm.
pub fn project_compaction_frontier(input: &ProjectionFixture) -> ProjectionResult {
    fixture_or_exit(ContextFrontierProjection::from_complete_entries(input))
}

/// Supplies one fixed 20,169-byte JSON tool-argument document.
///
/// Shape: 128 object members presented in reverse lexical order. Each member
/// holds eight integer fields and a 74-byte synthetic string. The resulting
/// text is deterministic and exercises recursive parsing, lexical key
/// canonicalization, and compact serialization.
pub fn tool_arguments_fixture() -> String {
    let mut value = String::with_capacity(TOOL_ARGUMENT_CAPACITY_HINT_BYTES);
    value.push('{');
    for index in (0..TOOL_ARGUMENT_MEMBER_COUNT).rev() {
        if index + 1 != TOOL_ARGUMENT_MEMBER_COUNT {
            value.push(',');
        }
        value.push_str(&format!(
            "\"item_{index:03}\":{{\"z\":{index},\"y\":{},\"x\":{},\"w\":{},\"v\":{},\"u\":{},\"t\":{},\"s\":{},\"payload\":\"synthetic_{index:03}_abcdefghijklmnopqrstuvwxyz_ABCDEFGHIJKLMNOPQRSTUVWXYZ_012345\"}}",
            index + 1,
            index + 2,
            index + 3,
            index + 4,
            index + 5,
            index + 6,
            index + 7,
        ));
    }
    value.push('}');
    value
}

/// Canonicalizes the fixed provider tool-argument document.
pub fn canonicalize_tool_arguments(input: String) -> NormalizedToolArguments {
    let normalized = fixture_or_exit(NormalizedToolArguments::try_from_provider_text(input));
    match normalized.kind() {
        ToolArgumentsKind::Json => normalized,
        ToolArgumentsKind::Undecodable => fixture_or_exit(Err(FixtureError::ToolArguments)),
    }
}
