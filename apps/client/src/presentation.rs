use std::{
    collections::HashSet,
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
    str::FromStr,
};

use rust_decimal::Decimal;
use signalbox_process_protocol::{
    BoundChildAction, CanonicalBlobDigest, CanonicalUuid, CurrentModelCallState,
    DelegationMessageDirection, DelegationOutcome, DelegationPolicy, DelegationProvenance,
    DelegationReason, DelegationWaitMode, DescendantTerminationScope, FailedModelCallCause,
    FailedModelCallDisposition, GoalBlockedProvenance, GoalBlockedReason, GoalHistoryEvent,
    GoalLifecycleState, ImportedContentKind, ImportedSourceSpeaker, ImportedSpeaker,
    ImportedTextPreview, LifecycleActorClass, MAX_RATE_VERSION_UTF8_BYTES, MetadataActor,
    MetadataLastWriter, ModelCallCostLabel, ModelCallDisposition, ModelCallState,
    OperatorStatusLifecycleDeadlineViolationMessage, OperatorStatusLifecycleState,
    OperatorStatusLifecycleWeekMessage, OperatorStatusMessage, ReviewDiffSide,
    ReviewFindingSnapshot, ReviewFindingStatus, ReviewOrchestrationConcernStatus,
    ReviewOrchestrationSnapshot, ReviewOrchestrationState, ReviewPassKind, ReviewPassLifecycle,
    ReviewRunLifecycle, ReviewRunSnapshot, ReviewSeverity, ReviewTargetSnapshot,
    ReviewTargetSubject, ReviewWorkflow, RunnerConnectionHealth, RunnerProjection,
    RunnerProjectionSelector, RunnerProjectionState, RunnerSandboxProfile,
    RunnerStateTransitionState, ServerMessage, SessionClosureOutcome, SessionEvent,
    ToolApprovalEventDecider, ToolApprovalEventDecision, ToolBatchState, ToolDecision,
    TranscriptEntry, TranscriptTextEntry, TurnState, UsageProvenance, UserInputContent,
    UserInputPart,
};

use crate::{
    ImportScanSummary,
    error::ClientError,
    transcript::{
        SnapshotEntry, SnapshotEntryKind, SnapshotIdentitySet, SnapshotRecord, TranscriptSnapshot,
        TranscriptTurn,
    },
};

pub(crate) struct ChildResultPresentation<'a> {
    pub(crate) await_request_id: CanonicalUuid,
    pub(crate) spawning_request_id: CanonicalUuid,
    pub(crate) child_session_id: CanonicalUuid,
    pub(crate) outcome: DelegationOutcome,
    pub(crate) content: Option<&'a String>,
    pub(crate) reason: DelegationReason,
    pub(crate) provenance: DelegationProvenance,
}

pub(crate) struct SessionSpawnedPresentation {
    pub(crate) tool_request_id: CanonicalUuid,
    pub(crate) child_session_id: CanonicalUuid,
    pub(crate) relationship: DelegationPolicy,
}

pub(crate) struct SessionAwaitRegisteredPresentation {
    pub(crate) tool_request_id: CanonicalUuid,
    pub(crate) child_session_id: CanonicalUuid,
    pub(crate) mode: DelegationWaitMode,
}

pub(crate) struct SessionMessageSentPresentation {
    pub(crate) tool_request_id: CanonicalUuid,
    pub(crate) peer_session_id: CanonicalUuid,
    pub(crate) message_id: CanonicalUuid,
    pub(crate) direction: DelegationMessageDirection,
    pub(crate) ordinal: u64,
    pub(crate) delivery_sequence: u64,
}

pub(crate) struct OperatorStatusPresentationCounts {
    pub(crate) lifecycle_weeks: u64,
    pub(crate) lifecycle_deadline_violations: u64,
}

pub(crate) enum BlobUploadPresentation {
    AlreadyPresent,
    Committed,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PresentTokenTotal {
    tokens: u128,
    present_calls: u64,
}

impl PresentTokenTotal {
    fn add(
        &mut self,
        value: Option<signalbox_process_protocol::CanonicalU64>,
    ) -> Result<(), ClientError> {
        let Some(value) = value else {
            return Ok(());
        };
        self.tokens = self
            .tokens
            .checked_add(u128::from(value.value()))
            .ok_or(ClientError::Protocol("token usage total overflowed"))?;
        self.present_calls = self
            .present_calls
            .checked_add(1)
            .ok_or(ClientError::Protocol("token usage coverage overflowed"))?;
        Ok(())
    }

    fn label(self) -> String {
        if self.present_calls == 0 {
            String::from("unreported")
        } else {
            self.tokens.to_string()
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TokenUsageTotal {
    terminal_calls: u64,
    input: PresentTokenTotal,
    output: PresentTokenTotal,
    cache_creation_input: PresentTokenTotal,
    cache_read_input: PresentTokenTotal,
}

impl TokenUsageTotal {
    fn add(
        &mut self,
        usage: signalbox_process_protocol::ModelCallTokenUsage,
    ) -> Result<(), ClientError> {
        self.terminal_calls = self
            .terminal_calls
            .checked_add(1)
            .ok_or(ClientError::Protocol(
                "terminal model-call count overflowed",
            ))?;
        self.input.add(usage.input_tokens)?;
        self.output.add(usage.output_tokens)?;
        self.cache_creation_input
            .add(usage.cache_creation_input_tokens)?;
        self.cache_read_input.add(usage.cache_read_input_tokens)
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CostAggregateKey {
    provenance: UsageProvenance,
    label: ModelCallCostLabel,
    rate_version: String,
}

const COST_KEY_WIDTH: usize = 2 + MAX_RATE_VERSION_UTF8_BYTES;
const COST_TOTAL_WIDTH: usize = 16 + 8;
const COST_SLOT_WIDTH: usize = 1 + COST_KEY_WIDTH + COST_TOTAL_WIDTH;

struct DiskCostTotals {
    file: File,
    len: u64,
    capacity: u64,
}

impl DiskCostTotals {
    fn new() -> io::Result<Self> {
        Self::with_capacity(16)
    }

    fn with_capacity(capacity: u64) -> io::Result<Self> {
        let file = tempfile::tempfile()?;
        file.set_len(cost_slot_offset(capacity)?)?;
        Ok(Self {
            file,
            len: 0,
            capacity,
        })
    }

    fn add(&mut self, key: &CostAggregateKey, amount: Decimal) -> Result<(), ClientError> {
        let next_len = self
            .len
            .checked_add(1)
            .ok_or(ClientError::Protocol("cost aggregate count overflowed"))?;
        if next_len
            .checked_mul(10)
            .is_none_or(|scaled| scaled >= self.capacity.saturating_mul(7))
        {
            self.grow()?;
        }
        let encoded = encode_cost_key(key);
        let start = stable_cost_hash(&encoded) % self.capacity;
        let mut candidate = [0_u8; COST_KEY_WIDTH];
        for displacement in 0..self.capacity {
            let index = (start + displacement) % self.capacity;
            let Some(mut total) = self.read_slot(index, &mut candidate)? else {
                self.write_slot(
                    index,
                    &encoded,
                    CostTotal {
                        amount_usd: amount,
                        calls: 1,
                    },
                )?;
                self.len = next_len;
                return Ok(());
            };
            if candidate == encoded {
                let next_amount = total
                    .amount_usd
                    .checked_add(amount)
                    .ok_or(ClientError::Protocol("dollar cost total overflowed"))?;
                if next_amount.checked_sub(total.amount_usd) != Some(amount)
                    || next_amount.checked_sub(amount) != Some(total.amount_usd)
                {
                    return Err(ClientError::Protocol("dollar cost total was inexact"));
                }
                total.amount_usd = next_amount;
                total.calls = total
                    .calls
                    .checked_add(1)
                    .ok_or(ClientError::Protocol("dollar cost coverage overflowed"))?;
                self.write_slot(index, &encoded, total)?;
                return Ok(());
            }
        }
        Err(ClientError::Io(io::Error::other(
            "disk cost aggregate was unexpectedly full",
        )))
    }

    fn grow(&mut self) -> io::Result<()> {
        let new_capacity = self
            .capacity
            .checked_mul(2)
            .ok_or_else(|| io::Error::other("disk cost capacity overflowed"))?;
        let mut replacement = Self::with_capacity(new_capacity)?;
        let mut key = [0_u8; COST_KEY_WIDTH];
        for index in 0..self.capacity {
            if let Some(total) = self.read_slot(index, &mut key)? {
                replacement.insert_stored(key, total)?;
            }
        }
        *self = replacement;
        Ok(())
    }

    fn insert_stored(&mut self, key: [u8; COST_KEY_WIDTH], total: CostTotal) -> io::Result<()> {
        let start = stable_cost_hash(&key) % self.capacity;
        let mut candidate = [0_u8; COST_KEY_WIDTH];
        for displacement in 0..self.capacity {
            let index = (start + displacement) % self.capacity;
            if self.read_slot(index, &mut candidate)?.is_none() {
                self.write_slot(index, &key, total)?;
                self.len = self
                    .len
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("disk cost count overflowed"))?;
                return Ok(());
            }
        }
        Err(io::Error::other(
            "replacement disk cost aggregate was unexpectedly full",
        ))
    }

    #[cfg(test)]
    fn get(&mut self, key: &CostAggregateKey) -> io::Result<Option<CostTotal>> {
        let encoded = encode_cost_key(key);
        let start = stable_cost_hash(&encoded) % self.capacity;
        let mut candidate = [0_u8; COST_KEY_WIDTH];
        for displacement in 0..self.capacity {
            let index = (start + displacement) % self.capacity;
            let Some(total) = self.read_slot(index, &mut candidate)? else {
                return Ok(None);
            };
            if candidate == encoded {
                return Ok(Some(total));
            }
        }
        Ok(None)
    }

    fn entry_at(&mut self, index: u64) -> io::Result<Option<(CostAggregateKey, CostTotal)>> {
        let mut encoded = [0_u8; COST_KEY_WIDTH];
        self.read_slot(index, &mut encoded)?
            .map(|total| Ok((decode_cost_key(&encoded)?, total)))
            .transpose()
    }

    fn read_slot(
        &mut self,
        index: u64,
        key: &mut [u8; COST_KEY_WIDTH],
    ) -> io::Result<Option<CostTotal>> {
        self.file.seek(SeekFrom::Start(cost_slot_offset(index)?))?;
        let mut occupied = [0_u8; 1];
        self.file.read_exact(&mut occupied)?;
        if occupied[0] == 0 {
            return Ok(None);
        }
        self.file.read_exact(key)?;
        let mut amount = [0_u8; 16];
        self.file.read_exact(&mut amount)?;
        let mut calls = [0_u8; 8];
        self.file.read_exact(&mut calls)?;
        Ok(Some(CostTotal {
            amount_usd: Decimal::deserialize(amount),
            calls: u64::from_le_bytes(calls),
        }))
    }

    fn write_slot(
        &mut self,
        index: u64,
        key: &[u8; COST_KEY_WIDTH],
        total: CostTotal,
    ) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(cost_slot_offset(index)?))?;
        self.file.write_all(&[1])?;
        self.file.write_all(key)?;
        self.file.write_all(&total.amount_usd.serialize())?;
        self.file.write_all(&total.calls.to_le_bytes())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CostTotal {
    amount_usd: Decimal,
    calls: u64,
}

struct UsageAggregate {
    reported: TokenUsageTotal,
    estimated: TokenUsageTotal,
    costs: DiskCostTotals,
}

impl UsageAggregate {
    fn new() -> Result<Self, ClientError> {
        Ok(Self {
            reported: TokenUsageTotal::default(),
            estimated: TokenUsageTotal::default(),
            costs: DiskCostTotals::new()?,
        })
    }

    fn add(
        &mut self,
        evidence: &crate::transcript::SnapshotModelCallUsage,
    ) -> Result<(), ClientError> {
        match evidence.usage_provenance {
            UsageProvenance::Reported => self.reported.add(evidence.usage)?,
            UsageProvenance::Estimated => self.estimated.add(evidence.usage)?,
        }
        let Some(cost) = evidence.cost.as_ref() else {
            return Ok(());
        };
        let amount = Decimal::from_str(cost.amount_usd.as_str())
            .map_err(|_| ClientError::Protocol("dollar cost was not representable"))?;
        let key = CostAggregateKey {
            provenance: evidence.usage_provenance,
            label: cost.label,
            rate_version: cost.rate_version.as_str().to_owned(),
        };
        self.costs.add(&key, amount)
    }
}

fn encode_cost_key(key: &CostAggregateKey) -> [u8; COST_KEY_WIDTH] {
    let mut encoded = [0_u8; COST_KEY_WIDTH];
    encoded[0] = match key.provenance {
        UsageProvenance::Reported => 0,
        UsageProvenance::Estimated => 1,
    };
    encoded[1] = match key.label {
        ModelCallCostLabel::Real => 0,
        ModelCallCostLabel::MeteredEquivalent => 1,
    };
    let version = key.rate_version.as_bytes();
    debug_assert!(version.len() <= MAX_RATE_VERSION_UTF8_BYTES);
    encoded[2..2 + version.len()].copy_from_slice(version);
    encoded
}

fn decode_cost_key(encoded: &[u8; COST_KEY_WIDTH]) -> io::Result<CostAggregateKey> {
    let provenance = match encoded[0] {
        0 => UsageProvenance::Reported,
        1 => UsageProvenance::Estimated,
        _ => return Err(io::Error::other("disk cost provenance was invalid")),
    };
    let label = match encoded[1] {
        0 => ModelCallCostLabel::Real,
        1 => ModelCallCostLabel::MeteredEquivalent,
        _ => return Err(io::Error::other("disk cost label was invalid")),
    };
    let version_end = encoded[2..]
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(MAX_RATE_VERSION_UTF8_BYTES);
    let rate_version = String::from_utf8(encoded[2..2 + version_end].to_vec())
        .map_err(|_| io::Error::other("disk rate version was not UTF-8"))?;
    Ok(CostAggregateKey {
        provenance,
        label,
        rate_version,
    })
}

fn cost_slot_offset(index: u64) -> io::Result<u64> {
    index
        .checked_mul(
            u64::try_from(COST_SLOT_WIDTH)
                .map_err(|_| io::Error::other("disk cost slot width overflowed"))?,
        )
        .ok_or_else(|| io::Error::other("disk cost slot offset overflowed"))
}

fn stable_cost_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

const fn usage_provenance_label(provenance: UsageProvenance) -> &'static str {
    match provenance {
        UsageProvenance::Reported => "reported",
        UsageProvenance::Estimated => "estimated",
    }
}

const fn cost_label(label: ModelCallCostLabel) -> &'static str {
    match label {
        ModelCallCostLabel::Real => "real",
        ModelCallCostLabel::MeteredEquivalent => "metered_equivalent",
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SnapshotSelection {
    All,
    Completed {
        turn_id: CanonicalUuid,
        model_call_id: CanonicalUuid,
        terminal_entry_id: CanonicalUuid,
    },
    Failed {
        turn_id: CanonicalUuid,
        terminal_entry_id: CanonicalUuid,
    },
    Cancelled {
        turn_id: CanonicalUuid,
        terminal_entry_id: CanonicalUuid,
    },
    ToolBatchProposed {
        turn_id: CanonicalUuid,
        model_call_id: CanonicalUuid,
    },
    ToolBatchResults {
        turn_id: CanonicalUuid,
        model_call_id: CanonicalUuid,
    },
    ToolReconciliation {
        turn_id: CanonicalUuid,
        tool_attempt_id: CanonicalUuid,
        terminal_frontier_id: CanonicalUuid,
    },
}

#[derive(Default)]
struct SnapshotSelectionContext {
    requests: HashSet<CanonicalUuid>,
}

/// One imported entry as the imported verb presents it.
pub(crate) struct ImportedEntryRow<'a> {
    pub(crate) position: u64,
    pub(crate) imported_entry_id: CanonicalUuid,
    pub(crate) source_speaker: ImportedSourceSpeaker,
    pub(crate) content_kind: ImportedContentKind,
    pub(crate) text_preview: Option<&'a ImportedTextPreview>,
}

/// One complete metadata summary as the search verb presents it.
pub(crate) struct SessionMetadataRow<'a> {
    pub(crate) session_id: CanonicalUuid,
    pub(crate) defaults_version: u64,
    pub(crate) selection: &'a str,
    pub(crate) dangerous_tool_auto_approval: bool,
    pub(crate) archived: bool,
    pub(crate) last_writer: Option<MetadataLastWriter>,
    pub(crate) tags: &'a [String],
    pub(crate) title: Option<&'a str>,
}

/// One unified conversation summary as the conversations verb presents it.
pub(crate) enum ConversationRow<'a> {
    /// One native session line.
    Native {
        session_id: CanonicalUuid,
        archived: bool,
        defaults_version: u64,
        title: Option<&'a str>,
    },
    /// One imported conversation line; the entry count is the greatest
    /// `--through-position` a continuation may select.
    Imported {
        imported_conversation_id: CanonicalUuid,
        format: &'static str,
        entry_count: u64,
        title: Option<&'a str>,
    },
}

/// What one process-derived text field may carry unescaped, given where it
/// sits in the output that carries it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TextField {
    /// Flowing text that owns the lines it is written to, so U+000A is its
    /// content rather than a delimiter.
    Flowing,
    /// The last named value on its line: a line feed inside it would forge a
    /// following line, and nothing else delimits it.
    TrailingOnLine,
    /// A value delimited within its line: the space that ends its field and
    /// the comma that separates it from a sibling are escaped too, so the
    /// field states its exact values.
    DelimitedOnLine,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChatTurnStatus {
    Queued(CanonicalUuid),
    Active(CanonicalUuid),
    AwaitingApproval {
        turn_id: CanonicalUuid,
        tool_request_id: CanonicalUuid,
    },
}

pub(crate) struct Output<'a> {
    stdout: &'a mut dyn Write,
    stderr: &'a mut dyn Write,
    raw: bool,
}

impl<'a> Output<'a> {
    pub(crate) fn new(stdout: &'a mut dyn Write, stderr: &'a mut dyn Write, raw: bool) -> Self {
        Self {
            stdout,
            stderr,
            raw,
        }
    }

    pub(crate) fn flush(&mut self) -> io::Result<()> {
        self.stdout.flush()
    }

    pub(crate) fn blob_metadata(
        &mut self,
        digest: CanonicalBlobDigest,
        byte_length: u64,
        replica_count: u64,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "digest={digest} byte_length={byte_length} replica_count={replica_count}"
        )
    }

    pub(crate) fn chat_started(
        &mut self,
        session_id: CanonicalUuid,
        status: Option<ChatTurnStatus>,
        commands: &str,
    ) -> io::Result<()> {
        match status {
            Some(ChatTurnStatus::Active(turn_id)) => writeln!(
                self.stdout,
                "chat session={session_id} state=following turn={turn_id} commands={commands}"
            )?,
            Some(ChatTurnStatus::AwaitingApproval {
                turn_id,
                tool_request_id,
            }) => writeln!(
                self.stdout,
                "chat session={session_id} state=awaiting_approval turn={turn_id} request={tool_request_id} commands={commands}"
            )?,
            Some(ChatTurnStatus::Queued(turn_id)) => writeln!(
                self.stdout,
                "chat session={session_id} state=queued turn={turn_id} commands={commands}"
            )?,
            None => writeln!(
                self.stdout,
                "chat session={session_id} state=ready commands={commands}"
            )?,
        }
        self.stdout.flush()
    }

    pub(crate) fn chat_ready(&mut self, session_id: CanonicalUuid) -> io::Result<()> {
        writeln!(self.stdout, "chat session={session_id} state=ready")?;
        self.stdout.flush()
    }

    pub(crate) fn chat_queued(&mut self, turn_id: CanonicalUuid) -> io::Result<()> {
        writeln!(self.stdout, "chat state=queued turn={turn_id}")?;
        self.stdout.flush()
    }

    pub(crate) fn chat_activated(&mut self, turn_id: CanonicalUuid) -> io::Result<()> {
        writeln!(self.stdout, "chat state=streaming turn={turn_id}")?;
        self.stdout.flush()
    }

    pub(crate) fn chat_stopped(
        &mut self,
        stopped_turn_id: CanonicalUuid,
        successor_turn_id: CanonicalUuid,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "chat state=queued stopped_turn={stopped_turn_id} successor_turn={successor_turn_id}"
        )?;
        self.stdout.flush()
    }

    pub(crate) fn chat_awaiting_approval(
        &mut self,
        turn_id: CanonicalUuid,
        tool_request_id: CanonicalUuid,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "chat state=awaiting_approval turn={turn_id} request={tool_request_id}"
        )?;
        self.stdout.flush()
    }

    pub(crate) fn chat_usage(&mut self, message: &str, commands: &str) -> io::Result<()> {
        let message = self.render_field(message, TextField::TrailingOnLine);
        writeln!(self.stderr, "chat: {message}; commands: {commands}")?;
        self.stderr.flush()
    }

    pub(crate) fn chat_interrupt_offered(&mut self, commands: &str) -> io::Result<()> {
        writeln!(
            self.stderr,
            "chat: turn still running; use :stop TEXT to stop and continue, or press Ctrl-C again to exit leaving it running; commands: {commands}"
        )?;
        self.stderr.flush()
    }

    pub(crate) fn chat_approval_interrupt_offered(
        &mut self,
        tool_request_id: CanonicalUuid,
        commands: &str,
    ) -> io::Result<()> {
        writeln!(
            self.stderr,
            "chat: turn awaits approval request {tool_request_id}; use :approve ID or :deny ID REASON, or press Ctrl-C again to exit leaving it running; commands: {commands}"
        )?;
        self.stderr.flush()
    }

    pub(crate) fn chat_mutation_abandoned(&mut self) -> io::Result<()> {
        writeln!(
            self.stderr,
            "chat: exiting with an in-flight mutation whose outcome may be ambiguous; use the printed recovery values for any exact standalone retry"
        )?;
        self.stderr.flush()
    }

    pub(crate) fn chat_exiting(&mut self, status: Option<ChatTurnStatus>) -> io::Result<()> {
        match status {
            Some(
                ChatTurnStatus::Active(turn_id) | ChatTurnStatus::AwaitingApproval { turn_id, .. },
            ) => writeln!(
                self.stderr,
                "chat: exiting; turn {turn_id} remains running in the daemon"
            ),
            Some(ChatTurnStatus::Queued(turn_id)) => writeln!(
                self.stderr,
                "chat: exiting; turn {turn_id} remains queued in the daemon"
            ),
            None => writeln!(self.stderr, "chat: exiting; no turn is queued or running"),
        }?;
        self.stderr.flush()
    }

    pub(crate) fn recovery_value(&mut self, name: &str, value: &str) -> io::Result<()> {
        writeln!(self.stderr, "{name}={value}")?;
        self.stderr.flush()
    }

    pub(crate) fn error(&mut self, error: &ClientError) -> io::Result<()> {
        let message = format!("error: {error}");
        self.stderr.write_all(self.render(&message).as_bytes())?;
        self.stderr.write_all(b"\n")
    }

    pub(crate) fn session_created(&mut self, session_id: CanonicalUuid) -> io::Result<()> {
        writeln!(self.stdout, "{session_id}")
    }

    pub(crate) fn session_spawned(
        &mut self,
        receipt: SessionSpawnedPresentation,
    ) -> io::Result<()> {
        let SessionSpawnedPresentation {
            tool_request_id,
            child_session_id,
            relationship,
        } = receipt;
        match relationship {
            DelegationPolicy::Background {} => writeln!(
                self.stdout,
                "spawn_request={tool_request_id} child_session={child_session_id} relationship=background"
            ),
            DelegationPolicy::Bound {
                on_parent_stopped,
                on_parent_cancelled,
            } => writeln!(
                self.stdout,
                "spawn_request={tool_request_id} child_session={child_session_id} \
                 relationship=bound on_parent_stopped={} on_parent_cancelled={}",
                bound_child_action(on_parent_stopped),
                bound_child_action(on_parent_cancelled)
            ),
        }
    }

    pub(crate) fn session_await_registered(
        &mut self,
        receipt: SessionAwaitRegisteredPresentation,
    ) -> io::Result<()> {
        let SessionAwaitRegisteredPresentation {
            tool_request_id,
            child_session_id,
            mode,
        } = receipt;
        writeln!(
            self.stdout,
            "await_request={tool_request_id} child_session={child_session_id} mode={}",
            delegation_wait_mode(mode)
        )
    }

    pub(crate) fn child_result(&mut self, result: ChildResultPresentation<'_>) -> io::Result<()> {
        let ChildResultPresentation {
            await_request_id,
            spawning_request_id,
            child_session_id,
            outcome,
            content,
            reason,
            provenance,
        } = result;
        let content = content.map_or_else(
            || String::from("null"),
            |content| self.render_field(content, TextField::TrailingOnLine),
        );
        writeln!(
            self.stdout,
            "await_request={await_request_id} spawning_request={spawning_request_id} \
             child_session={child_session_id} delivery=foreground outcome={} reason={} \
             provenance={} content={content}",
            delegation_outcome(outcome),
            delegation_reason(reason),
            delegation_provenance(&provenance)
        )
    }

    pub(crate) fn session_message_sent(
        &mut self,
        receipt: SessionMessageSentPresentation,
    ) -> io::Result<()> {
        let SessionMessageSentPresentation {
            tool_request_id,
            peer_session_id,
            message_id,
            direction,
            ordinal,
            delivery_sequence,
        } = receipt;
        writeln!(
            self.stdout,
            "message_request={tool_request_id} peer_session={peer_session_id} \
             message={message_id} direction={} ordinal={ordinal} delivery_sequence={delivery_sequence}",
            delegation_message_direction(direction)
        )
    }

    pub(crate) fn goal_transition_applied(
        &mut self,
        session_id: CanonicalUuid,
        event_ordinal: u64,
        generation: u64,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "session={session_id} goal_event={event_ordinal} generation={generation}"
        )
    }

    pub(crate) fn goal_current(
        &mut self,
        session_id: CanonicalUuid,
        generation: u64,
        statement: &str,
        state: &GoalLifecycleState,
    ) -> io::Result<()> {
        match state {
            GoalLifecycleState::Pursuing {} => writeln!(
                self.stdout,
                "goal session={session_id} generation={generation} state=pursuing"
            )?,
            GoalLifecycleState::Blocked { reason, .. } => writeln!(
                self.stdout,
                "goal session={session_id} generation={generation} state=blocked reason={}",
                goal_blocked_reason_label(*reason)
            )?,
            GoalLifecycleState::Achieved {
                turn_id,
                tool_request_id,
            } => writeln!(
                self.stdout,
                "goal session={session_id} generation={generation} state=achieved turn={turn_id} request={tool_request_id}"
            )?,
            GoalLifecycleState::UserStopped {} => writeln!(
                self.stdout,
                "goal session={session_id} generation={generation} state=user_stopped"
            )?,
            GoalLifecycleState::Superseded { by_generation } => writeln!(
                self.stdout,
                "goal session={session_id} generation={generation} state=superseded by_generation={}",
                by_generation.value()
            )?,
            GoalLifecycleState::SessionClosed { outcome } => writeln!(
                self.stdout,
                "goal session={session_id} generation={generation} state=session_closed outcome={}",
                session_closure_outcome_label(*outcome)
            )?,
        }
        self.goal_text_field("statement", statement)?;
        match state {
            GoalLifecycleState::Blocked { need, .. } => self.goal_text_field("need", need)?,
            GoalLifecycleState::Pursuing {}
            | GoalLifecycleState::Achieved { .. }
            | GoalLifecycleState::UserStopped {}
            | GoalLifecycleState::Superseded { .. }
            | GoalLifecycleState::SessionClosed { .. } => {}
        }
        Ok(())
    }

    pub(crate) fn goal_history_event(
        &mut self,
        ordinal: u64,
        generation: u64,
        event: &GoalHistoryEvent,
    ) -> io::Result<()> {
        match event {
            GoalHistoryEvent::Commissioned {
                statement,
                command_id,
            } => {
                writeln!(
                    self.stdout,
                    "event={ordinal} generation={generation} type=commissioned command={}",
                    command_id.into_uuid().hyphenated()
                )?;
                self.goal_text_field("statement", statement)
            }
            GoalHistoryEvent::Blocked {
                reason,
                need,
                provenance,
            } => {
                match provenance {
                    GoalBlockedProvenance::Model {
                        turn_id,
                        tool_request_id,
                    } => writeln!(
                        self.stdout,
                        "event={ordinal} generation={generation} type=blocked reason={} source=model turn={turn_id} request={tool_request_id}",
                        goal_blocked_reason_label(*reason)
                    )?,
                    GoalBlockedProvenance::ExecutionFailure { turn_id } => writeln!(
                        self.stdout,
                        "event={ordinal} generation={generation} type=blocked reason={} source=scheduler turn={turn_id}",
                        goal_blocked_reason_label(*reason)
                    )?,
                }
                self.goal_text_field("need", need)
            }
            GoalHistoryEvent::Resumed {
                guidance,
                command_id,
            } => {
                writeln!(
                    self.stdout,
                    "event={ordinal} generation={generation} type=resumed command={} guidance_present={}",
                    command_id.into_uuid().hyphenated(),
                    guidance.is_some()
                )?;
                match guidance {
                    Some(guidance) => self.goal_text_field("guidance", guidance),
                    None => Ok(()),
                }
            }
            GoalHistoryEvent::Achieved {
                report,
                turn_id,
                tool_request_id,
            } => {
                writeln!(
                    self.stdout,
                    "event={ordinal} generation={generation} type=achieved turn={turn_id} request={tool_request_id}"
                )?;
                self.goal_text_field("report", report)
            }
            GoalHistoryEvent::UserStopped { command_id } => writeln!(
                self.stdout,
                "event={ordinal} generation={generation} type=user_stopped command={}",
                command_id.into_uuid().hyphenated()
            ),
            GoalHistoryEvent::Superseded {
                replacement_statement,
                command_id,
            } => {
                writeln!(
                    self.stdout,
                    "event={ordinal} generation={generation} type=superseded command={}",
                    command_id.into_uuid().hyphenated()
                )?;
                self.goal_text_field("replacement_statement", replacement_statement)
            }
            GoalHistoryEvent::SessionClosed { outcome, actor } => writeln!(
                self.stdout,
                "event={ordinal} generation={generation} type=session_closed outcome={} actor={}",
                session_closure_outcome_label(*outcome),
                lifecycle_actor_label(*actor)
            ),
        }
    }

    fn goal_text_field(&mut self, name: &str, value: &str) -> io::Result<()> {
        write!(self.stdout, "{name}=")?;
        self.stdout.write_all(
            self.render_field(value, TextField::TrailingOnLine)
                .as_bytes(),
        )?;
        self.stdout.write_all(b"\n")
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn session_compacted(
        &mut self,
        session_id: CanonicalUuid,
        context_compaction_id: CanonicalUuid,
        model_call_id: CanonicalUuid,
        through_position: u64,
        summary_entry_id: CanonicalUuid,
        result_frontier_id: CanonicalUuid,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "session={session_id} compaction={context_compaction_id} call={model_call_id} \
             through_position={through_position} summary_entry={summary_entry_id} \
             result_frontier={result_frontier_id}"
        )
    }

    pub(crate) fn template_summary(&mut self, name: &str, version: u64) -> io::Result<()> {
        writeln!(self.stdout, "name={name} version={version}")
    }

    pub(crate) fn steering_submitted(
        &mut self,
        accepted_input_id: CanonicalUuid,
        acceptance_position: u64,
        source_turn_id: CanonicalUuid,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "accepted_input={accepted_input_id} position={acceptance_position} source_turn={source_turn_id}"
        )
    }

    pub(crate) fn session_defaults_replaced(
        &mut self,
        session_id: CanonicalUuid,
        defaults_version: u64,
        model_selection: &str,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "session={session_id} defaults_version={defaults_version} {model_selection}"
        )
    }

    pub(crate) fn conversation_import_inserted(
        &mut self,
        imported_conversation_id: CanonicalUuid,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "inserted imported_conversation_id={imported_conversation_id}"
        )
    }

    pub(crate) fn blob_uploaded(
        &mut self,
        digest: CanonicalBlobDigest,
        byte_length: u64,
        outcome: BlobUploadPresentation,
    ) -> io::Result<()> {
        let status = match outcome {
            BlobUploadPresentation::AlreadyPresent => "already_present",
            BlobUploadPresentation::Committed => "committed",
        };
        writeln!(
            self.stdout,
            "{status} digest={digest} byte_length={byte_length}"
        )
    }

    pub(crate) fn conversation_import_already_imported(
        &mut self,
        imported_conversation_id: CanonicalUuid,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "already_imported imported_conversation_id={imported_conversation_id}"
        )
    }

    pub(crate) fn conversation_import_scan_inserted(
        &mut self,
        path: &Path,
        imported_conversation_id: CanonicalUuid,
    ) -> io::Result<()> {
        let path = self.render(&format!("{path:?}"));
        writeln!(
            self.stdout,
            "imported path={path} imported_conversation_id={imported_conversation_id}"
        )
    }

    pub(crate) fn conversation_import_scan_already_imported(
        &mut self,
        path: &Path,
        imported_conversation_id: CanonicalUuid,
    ) -> io::Result<()> {
        let path = self.render(&format!("{path:?}"));
        writeln!(
            self.stdout,
            "already_imported path={path} imported_conversation_id={imported_conversation_id}"
        )
    }

    pub(crate) fn conversation_import_scan_skipped(
        &mut self,
        path: &Path,
        error: &ClientError,
    ) -> io::Result<()> {
        let path = self.render(&format!("{path:?}"));
        let reason = self.render_field(&error.to_string(), TextField::TrailingOnLine);
        writeln!(self.stdout, "skipped path={path} reason={reason}")
    }

    pub(crate) fn conversation_import_scan_summary(
        &mut self,
        summary: &ImportScanSummary,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "scan_summary imported={} already_imported={} skipped={}",
            summary.imported, summary.already_imported, summary.skipped
        )
    }

    /// Prints one selectable imported position with its attestation, kind, and
    /// bounded text preview.
    ///
    /// The preview is the last field on its line and the truncation marker
    /// precedes it, so preview text cannot forge either. An entry carrying no
    /// exact attested text omits both fields rather than printing a placeholder
    /// that empty attested text could not be told apart from.
    pub(crate) fn imported_conversation_entry(
        &mut self,
        row: &ImportedEntryRow<'_>,
    ) -> io::Result<()> {
        let prefix = format!(
            "position={} imported_entry={} speaker={} kind={}",
            row.position,
            row.imported_entry_id,
            imported_speaker_attestation_label(row.source_speaker),
            imported_content_kind(row.content_kind),
        );
        match row.text_preview {
            Some(preview) => {
                let text = self.render_field(preview.preview(), TextField::TrailingOnLine);
                writeln!(
                    self.stdout,
                    "{prefix} truncated={} text={text}",
                    preview.truncated()
                )
            }
            None => writeln!(self.stdout, "{prefix}"),
        }
    }

    /// Prints the imported conversation's total entry count, which is also its
    /// greatest selectable position.
    pub(crate) fn imported_conversation_entry_count(&mut self, entry_count: u64) -> io::Result<()> {
        writeln!(self.stdout, "entry_count={entry_count}")
    }

    /// Prints the concrete position a `latest` selection resolved to, before
    /// the durable command that consumes it can become ambiguous.
    pub(crate) fn resolved_through_position(&mut self, position: u64) -> io::Result<()> {
        self.recovery_value("through_position", &position.to_string())
    }

    pub(crate) fn operator_status_counts(
        &mut self,
        counts: OperatorStatusPresentationCounts,
    ) -> io::Result<()> {
        let OperatorStatusPresentationCounts {
            lifecycle_weeks,
            lifecycle_deadline_violations,
        } = counts;
        writeln!(
            self.stdout,
            "status lifecycle_weeks={lifecycle_weeks} \
             nonterminal_past_deadline={lifecycle_deadline_violations}"
        )
    }

    pub(crate) fn operator_status_item(
        &mut self,
        message: &ServerMessage,
    ) -> Result<(), ClientError> {
        let ServerMessage::OperatorStatus(message) = message else {
            return Err(ClientError::Protocol(
                "operator-status spool contained an unexpected frame",
            ));
        };
        match message.as_ref() {
            OperatorStatusMessage::LifecycleWeek(item) => {
                let OperatorStatusLifecycleWeekMessage {
                    week_start_date,
                    completion_failure_numerator,
                    completion_failure_denominator,
                    failed_unknown_count,
                    overflow_numerator,
                    overflow_denominator,
                    finish_given_overflow_numerator,
                    wall_numerator,
                    wall_denominator,
                    wall_occurrence_count,
                    classified_terminal_turn_count,
                    terminal_turn_count,
                    classified_known_failed_call_count,
                    known_failed_call_count,
                } = item.as_ref();
                writeln!(
                    self.stdout,
                    "lifecycle_week week={week_start_date} completion_failure={} \
                     failed_unknown={} overflow={} finish_given_overflow={} wall={} \
                     wall_occurrences={} \
                     turn_cause_completeness={} model_call_cause_completeness={}",
                    rate_label(RateCounts {
                        numerator: completion_failure_numerator.value(),
                        denominator: completion_failure_denominator.value(),
                    }),
                    rate_label(RateCounts {
                        numerator: failed_unknown_count.value(),
                        denominator: completion_failure_denominator.value(),
                    }),
                    rate_label(RateCounts {
                        numerator: overflow_numerator.value(),
                        denominator: overflow_denominator.value(),
                    }),
                    rate_label(RateCounts {
                        numerator: finish_given_overflow_numerator.value(),
                        denominator: overflow_numerator.value(),
                    }),
                    rate_label(RateCounts {
                        numerator: wall_numerator.value(),
                        denominator: wall_denominator.value(),
                    }),
                    wall_occurrence_count.value(),
                    rate_label(RateCounts {
                        numerator: classified_terminal_turn_count.value(),
                        denominator: terminal_turn_count.value(),
                    }),
                    rate_label(RateCounts {
                        numerator: classified_known_failed_call_count.value(),
                        denominator: known_failed_call_count.value(),
                    }),
                )?;
                Ok(())
            }
            OperatorStatusMessage::LifecycleDeadlineViolation(item) => {
                let OperatorStatusLifecycleDeadlineViolationMessage {
                    session_id,
                    state,
                    deadline_missing,
                    expired_for_seconds,
                } = item.as_ref();
                writeln!(
                    self.stdout,
                    "nonterminal_past_deadline session={session_id} state={} deadline={} \
                     expired={}",
                    operator_status_lifecycle_state_label(*state),
                    if *deadline_missing {
                        "missing"
                    } else {
                        "armed"
                    },
                    expired_for_seconds
                        .map_or_else(|| "n/a".to_owned(), |value| duration_label(value.value())),
                )?;
                Ok(())
            }
            OperatorStatusMessage::Start {} | OperatorStatusMessage::End(_) => Err(
                ClientError::Protocol("operator-status spool contained an unexpected frame"),
            ),
        }
    }

    pub(crate) fn operator_status_model_usage_omitted(&mut self) -> io::Result<()> {
        writeln!(
            self.stdout,
            "model_usage=omitted reason=no_cheap_status_aggregate"
        )
    }

    pub(crate) fn session_summary(
        &mut self,
        session_id: CanonicalUuid,
        defaults_version: u64,
        selection: &str,
        placement_version: u64,
        placement: &str,
        runner: Option<&RunnerProjection>,
    ) -> io::Result<()> {
        write!(
            self.stdout,
            "{session_id} defaults_version={defaults_version} {selection} \
             placement_version={placement_version} {placement}"
        )?;
        if let Some(runner) = runner {
            write!(self.stdout, " runner_selector=")?;
            match runner.selector() {
                RunnerProjectionSelector::Runner { runner_id } => {
                    write!(self.stdout, "runner runner_selector_runner={runner_id}")?;
                }
                RunnerProjectionSelector::CapabilityClass { name } => write!(
                    self.stdout,
                    "capability_class runner_selector_capability={}",
                    self.render_field(name.as_str(), TextField::DelimitedOnLine)
                )?,
            }
            if let Some(runner_id) = runner.runner_id() {
                write!(self.stdout, " runner={runner_id}")?;
            }
            write!(
                self.stdout,
                " runner_placement_revision={} runner_sandbox={}",
                runner.placement_revision().value(),
                runner_sandbox_profile(runner.sandbox_profile())
            )?;
            if let Some(profile) = runner.credential_profile() {
                write!(
                    self.stdout,
                    " runner_credential_profile={}",
                    self.render_field(profile.as_str(), TextField::DelimitedOnLine)
                )?;
            }
            if let Some(repository) = runner.repository() {
                write!(
                    self.stdout,
                    " runner_repository={}",
                    self.render_field(repository.as_str(), TextField::DelimitedOnLine)
                )?;
            }
            if let Some(directory) = runner.working_directory() {
                write!(
                    self.stdout,
                    " runner_working_directory={}",
                    self.render_field(directory.as_str(), TextField::DelimitedOnLine)
                )?;
            }
            if let Some(health) = runner.connection_health() {
                write!(
                    self.stdout,
                    " runner_connection_health={}",
                    runner_connection_health(health)
                )?;
            }
            write!(
                self.stdout,
                " runner_state={}",
                runner_projection_state(runner.state())
            )?;
        }
        writeln!(self.stdout)
    }

    pub(crate) fn session_placement_updated(
        &mut self,
        session_id: CanonicalUuid,
        placement_version: u64,
        placement: &str,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "session={session_id} placement_version={placement_version} {placement}"
        )
    }

    pub(crate) fn review_acknowledgement(&mut self, line: &str) -> io::Result<()> {
        self.stdout.write_all(self.render(line).as_bytes())?;
        self.stdout.write_all(b"\n")
    }

    pub(crate) fn review_orchestration(
        &mut self,
        snapshot: &ReviewOrchestrationSnapshot,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "attempt={} target={} state={} concerns={} findings={} judgment_members={} \
             judgment_effects_applied={} repairs_fixed={} publications_published={}",
            snapshot.attempt_id,
            snapshot.target_id,
            review_orchestration_state_label(snapshot.state),
            snapshot.concerns.len(),
            snapshot.counts.finding_count.value(),
            snapshot.counts.judgment_member_count.value(),
            snapshot.counts.judgment_effect_applied_count.value(),
            snapshot.counts.repair_fixed_count.value(),
            snapshot.counts.publication_published_count.value(),
        )?;
        self.text_field("concern_set_version", &snapshot.concern_set_version)?;
        writeln!(
            self.stdout,
            "template_import_digest={} template_judgment_digest={} template_repair_digest={} \
             template_publication_digest={}",
            snapshot.stage_template_digests.import.as_str(),
            snapshot.stage_template_digests.judgment.as_str(),
            snapshot.stage_template_digests.repair.as_str(),
            snapshot.stage_template_digests.publication.as_str(),
        )?;
        for (index, concern) in snapshot.concerns.iter().enumerate() {
            writeln!(
                self.stdout,
                "concern_index={index} status={} pass={} template_digest={}",
                review_orchestration_concern_status_label(concern.status),
                concern
                    .pass_id
                    .map_or_else(|| String::from("-"), |id| id.to_string()),
                concern.template_digest.as_str(),
            )?;
            self.text_field("concern_key", &concern.key)?;
        }
        Ok(())
    }

    pub(crate) fn review_target(&mut self, target: &ReviewTargetSnapshot) -> io::Result<()> {
        let subject = match target.subject {
            ReviewTargetSubject::ChangeRequest { number } => {
                format!("change_request:{}", number.value())
            }
            ReviewTargetSubject::Commit {} => String::from("commit"),
        };
        writeln!(
            self.stdout,
            "target={} subject={} parent={}",
            target.target_id,
            subject,
            target
                .stack_parent_target_id
                .map_or_else(|| String::from("-"), |id| id.to_string()),
        )?;
        self.text_field("provider", &target.provider)?;
        self.text_field("repository", &target.repository)?;
        self.text_field("head_revision", &target.head_revision)?;
        match target.base_revision.as_deref() {
            Some(base_revision) => {
                writeln!(self.stdout, "base_revision_present=true")?;
                self.text_field("base_revision", base_revision)
            }
            None => writeln!(self.stdout, "base_revision_present=false"),
        }
    }

    pub(crate) fn review_run(
        &mut self,
        run: &ReviewRunSnapshot,
        pass: Option<&signalbox_process_protocol::ReviewPassSnapshot>,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "run={} target={} workflow={} policy_version={} minimum_judge_confidence={} \
             minimum_publication_confidence={} state={} pass={}",
            run.run_id,
            run.target_id,
            review_workflow_label(run.workflow),
            run.policy_version.value(),
            run.minimum_judge_confidence.value(),
            run.minimum_publication_confidence.value(),
            review_run_state_label(run.state),
            run.pass_id
                .map_or_else(|| String::from("-"), |id| id.to_string()),
        )?;
        if let Some(pass) = pass {
            writeln!(
                self.stdout,
                "pass={} kind={} state={} session={} input={} origin_turn={} turn={} frontier={}",
                pass.pass_id,
                review_pass_kind_label(pass.kind),
                review_pass_state_label(pass.state),
                pass.session_id,
                pass.accepted_input_id,
                pass.origin_turn_id,
                pass.turn_id
                    .map_or_else(|| String::from("-"), |id| id.to_string()),
                pass.output_frontier_id
                    .map_or_else(|| String::from("-"), |id| id.to_string()),
            )?;
        }
        Ok(())
    }

    pub(crate) fn review_finding(&mut self, finding: &ReviewFindingSnapshot) -> io::Result<()> {
        writeln!(
            self.stdout,
            "finding={} target={} run={} pass={} status={} events={} line_start={} line_end={} \
             diff_side={} severity={} is_real_confidence={} severity_label_confidence={}",
            finding.finding.finding_id,
            finding.target_id,
            finding.run_id,
            finding.producing_pass_id,
            review_finding_status_label(finding.status),
            finding.event_count.value(),
            finding
                .finding
                .line_start
                .map_or_else(|| String::from("none"), |line| line.value().to_string()),
            finding
                .finding
                .line_end
                .map_or_else(|| String::from("none"), |line| line.value().to_string()),
            finding
                .finding
                .diff_side
                .map_or("none", review_diff_side_label),
            review_severity_label(finding.finding.severity),
            finding.finding.is_real_confidence.value(),
            finding.finding.severity_label_confidence.value(),
        )?;
        self.text_field("file_path", &finding.finding.file_path)?;
        self.text_field("title", &finding.finding.title)?;
        self.text_field("body", &finding.finding.body)?;
        self.text_field("category", &finding.finding.category)?;
        match finding.finding.recommended_fix.as_deref() {
            Some(recommended_fix) => {
                writeln!(self.stdout, "recommended_fix_present=true")?;
                self.text_field("recommended_fix", recommended_fix)
            }
            None => writeln!(self.stdout, "recommended_fix_present=false"),
        }
    }

    fn text_field(&mut self, name: &str, value: &str) -> io::Result<()> {
        write!(self.stdout, "{name}=")?;
        self.stdout.write_all(
            self.render_field(value, TextField::TrailingOnLine)
                .as_bytes(),
        )?;
        self.stdout.write_all(b"\n")
    }

    pub(crate) fn tool_request_decided(
        &mut self,
        tool_request_id: CanonicalUuid,
        decision: &ToolDecision,
    ) -> io::Result<()> {
        let decision = match decision {
            ToolDecision::Approve {} => "approve",
            ToolDecision::Deny { .. } => "deny",
        };
        writeln!(
            self.stdout,
            "tool_request={tool_request_id} decision={decision}"
        )
    }

    pub(crate) fn session_metadata_summary(
        &mut self,
        row: &SessionMetadataRow<'_>,
    ) -> io::Result<()> {
        let tags = row
            .tags
            .iter()
            .map(|tag| self.render_field(tag, TextField::DelimitedOnLine))
            .collect::<Vec<_>>()
            .join(",");
        let title = row
            .title
            .map(|title| self.render_field(title, TextField::TrailingOnLine));
        writeln!(
            self.stdout,
            "{} archived={} defaults_version={} {} dangerous_tool_auto_approval={} \
             last_writer={} updated_at_unix_micros={} tags={tags} title={}",
            row.session_id,
            row.archived,
            row.defaults_version,
            row.selection,
            dangerous_tool_auto_approval_label(row.dangerous_tool_auto_approval),
            last_writer_actor_label(row.last_writer),
            last_writer_micros_label(row.last_writer),
            title.unwrap_or_default()
        )
    }

    pub(crate) fn next_page_cursor(
        &mut self,
        next_after_session_id: CanonicalUuid,
    ) -> io::Result<()> {
        writeln!(self.stderr, "next_after_session_id={next_after_session_id}")?;
        self.stderr.flush()
    }

    pub(crate) fn conversation_summary(&mut self, row: &ConversationRow<'_>) -> io::Result<()> {
        match row {
            ConversationRow::Native {
                session_id,
                archived,
                defaults_version,
                title,
            } => {
                let title = title.map(|title| self.render_field(title, TextField::TrailingOnLine));
                writeln!(
                    self.stdout,
                    "origin=native session_id={session_id} archived={archived} \
                     defaults_version={defaults_version} title={}",
                    title.unwrap_or_default()
                )
            }
            ConversationRow::Imported {
                imported_conversation_id,
                format,
                entry_count,
                title,
            } => {
                let title = title.map(|title| self.render_field(title, TextField::TrailingOnLine));
                writeln!(
                    self.stdout,
                    "origin=imported imported_conversation_id={imported_conversation_id} \
                     format={format} entry_count={entry_count} title={}",
                    title.unwrap_or_default()
                )
            }
        }
    }

    pub(crate) fn next_conversation_cursor(
        &mut self,
        origin_label: &str,
        conversation_id: CanonicalUuid,
    ) -> io::Result<()> {
        writeln!(self.stderr, "next_after={origin_label}:{conversation_id}")?;
        self.stderr.flush()
    }

    pub(crate) fn snapshot(
        &mut self,
        snapshot: &mut TranscriptSnapshot,
    ) -> Result<(), ClientError> {
        let mut rendered_snapshot = tempfile::tempfile()?;
        {
            let mut staged = Output::new(&mut rendered_snapshot, &mut *self.stderr, self.raw);
            staged.snapshot_runner(snapshot.runner())?;
            staged.render_snapshot(snapshot, None, SnapshotSelection::All, true)?;
            staged.render_usage(snapshot)?;
        }
        rendered_snapshot.seek(SeekFrom::Start(0))?;
        io::copy(&mut rendered_snapshot, &mut self.stdout)?;
        Ok(())
    }

    pub(crate) fn followed_snapshot(
        &mut self,
        snapshot: &mut TranscriptSnapshot,
        displayed: &mut SnapshotIdentitySet,
    ) -> Result<(), ClientError> {
        self.snapshot_runner(snapshot.runner())?;
        self.render_snapshot(snapshot, Some(displayed), SnapshotSelection::All, true)
    }

    fn snapshot_runner(&mut self, runner: Option<&RunnerProjection>) -> io::Result<()> {
        let Some(runner) = runner else {
            return Ok(());
        };
        write!(self.stdout, "runner_snapshot selector=")?;
        match runner.selector() {
            RunnerProjectionSelector::Runner { runner_id } => {
                write!(self.stdout, "runner selector_runner={runner_id}")?;
            }
            RunnerProjectionSelector::CapabilityClass { name } => write!(
                self.stdout,
                "capability_class selector_capability={}",
                self.render_field(name.as_str(), TextField::DelimitedOnLine)
            )?,
        }
        if let Some(runner_id) = runner.runner_id() {
            write!(self.stdout, " runner={runner_id}")?;
        }
        write!(
            self.stdout,
            " placement_revision={} sandbox={}",
            runner.placement_revision().value(),
            runner_sandbox_profile(runner.sandbox_profile())
        )?;
        if let Some(profile) = runner.credential_profile() {
            write!(
                self.stdout,
                " credential_profile={}",
                self.render_field(profile.as_str(), TextField::DelimitedOnLine)
            )?;
        }
        if let Some(repository) = runner.repository() {
            write!(
                self.stdout,
                " repository={}",
                self.render_field(repository.as_str(), TextField::DelimitedOnLine)
            )?;
        }
        if let Some(directory) = runner.working_directory() {
            write!(
                self.stdout,
                " working_directory={}",
                self.render_field(directory.as_str(), TextField::DelimitedOnLine)
            )?;
        }
        if let Some(health) = runner.connection_health() {
            write!(
                self.stdout,
                " connection_health={}",
                runner_connection_health(health)
            )?;
        }
        writeln!(
            self.stdout,
            " state={}",
            runner_projection_state(runner.state())
        )
    }

    pub(crate) fn terminal_material(
        &mut self,
        snapshot: &mut TranscriptSnapshot,
        displayed: &mut SnapshotIdentitySet,
        selection: SnapshotSelection,
    ) -> Result<(), ClientError> {
        self.render_snapshot(snapshot, Some(displayed), selection, false)
    }

    fn render_snapshot(
        &mut self,
        snapshot: &mut TranscriptSnapshot,
        mut displayed: Option<&mut SnapshotIdentitySet>,
        selection: SnapshotSelection,
        render_turns: bool,
    ) -> Result<(), ClientError> {
        let selection_context = selection.context(snapshot)?;
        let mut render_content = false;
        for record in snapshot.replay()? {
            match record? {
                SnapshotRecord::Turn(turn) if render_turns => self.snapshot_turn(&turn)?,
                SnapshotRecord::Turn(_) => {}
                SnapshotRecord::ModelCallUsage(_) => {}
                SnapshotRecord::Entry(entry) => {
                    render_content = false;
                    let selected = selection.includes(&entry, &selection_context);
                    let undisplayed = if selected {
                        match displayed.as_deref_mut() {
                            Some(identities) => {
                                identities.insert(entry.source_session_id, entry.entry_id)?
                            }
                            None => true,
                        }
                    } else {
                        false
                    };
                    if undisplayed {
                        render_content = matches!(entry.kind, SnapshotEntryKind::Text(_));
                        self.snapshot_entry(&entry)?;
                    }
                }
                SnapshotRecord::Content(content) if render_content => {
                    let content_ends_with_newline = content.content.as_str().ends_with('\n');
                    self.text_fragment(
                        content.content.as_str(),
                        content.final_fragment,
                        content_ends_with_newline,
                    )?;
                    if content.final_fragment {
                        render_content = false;
                    }
                }
                SnapshotRecord::Content(_) => {}
            }
        }
        Ok(())
    }

    fn render_usage(&mut self, snapshot: &mut TranscriptSnapshot) -> Result<(), ClientError> {
        let mut rendered_usage = tempfile::tempfile()?;
        let mut current_turn: Option<(CanonicalUuid, UsageAggregate)> = None;
        let mut session_total = UsageAggregate::new()?;
        for record in snapshot.replay()? {
            let SnapshotRecord::ModelCallUsage(evidence) = record? else {
                continue;
            };
            if current_turn
                .as_ref()
                .is_some_and(|(turn, _)| *turn != evidence.turn_id)
            {
                let (turn, mut total) = current_turn.take().ok_or(ClientError::Protocol(
                    "token usage turn grouping was invalid",
                ))?;
                self.usage_lines(&mut rendered_usage, Some(turn), &mut total)?;
            }
            if current_turn.is_none() {
                current_turn = Some((evidence.turn_id, UsageAggregate::new()?));
            }
            let (_, turn_total) = current_turn.as_mut().ok_or(ClientError::Protocol(
                "token usage turn grouping was invalid",
            ))?;
            turn_total.add(&evidence)?;
            session_total.add(&evidence)?;
        }
        if let Some((turn, mut total)) = current_turn {
            self.usage_lines(&mut rendered_usage, Some(turn), &mut total)?;
        }
        self.usage_lines(&mut rendered_usage, None, &mut session_total)?;
        rendered_usage.seek(SeekFrom::Start(0))?;
        io::copy(&mut rendered_usage, &mut self.stdout)?;
        Ok(())
    }

    fn usage_lines<OutputWriter: Write>(
        &self,
        stdout: &mut OutputWriter,
        turn: Option<CanonicalUuid>,
        total: &mut UsageAggregate,
    ) -> Result<(), ClientError> {
        Self::usage_line(stdout, turn, UsageProvenance::Reported, total.reported)?;
        Self::usage_line(stdout, turn, UsageProvenance::Estimated, total.estimated)?;
        for index in 0..total.costs.capacity {
            let Some((key, cost)) = total.costs.entry_at(index)? else {
                continue;
            };
            let prefix = turn.map_or_else(
                || String::from("cost_total scope=session"),
                |turn| format!("cost turn={turn}"),
            );
            let rate_version = self.render_field(&key.rate_version, TextField::DelimitedOnLine);
            writeln!(
                stdout,
                "{prefix} usage_provenance={} label={} rate_version={} usd={} costed_calls={}",
                usage_provenance_label(key.provenance),
                cost_label(key.label),
                rate_version,
                cost.amount_usd.normalize(),
                cost.calls,
            )?;
        }
        Ok(())
    }

    fn usage_line<OutputWriter: Write>(
        stdout: &mut OutputWriter,
        turn: Option<CanonicalUuid>,
        provenance: UsageProvenance,
        total: TokenUsageTotal,
    ) -> io::Result<()> {
        let prefix = turn.map_or_else(
            || String::from("usage_total scope=session"),
            |turn| format!("usage turn={turn}"),
        );
        writeln!(
            stdout,
            "{prefix} usage_provenance={} terminal_calls={} input_tokens={} \
             input_tokens_present_calls={}/{} \
             output_tokens={} output_tokens_present_calls={}/{} \
             cache_creation_input_tokens={} \
             cache_creation_input_tokens_present_calls={}/{} cache_read_input_tokens={} \
             cache_read_input_tokens_present_calls={}/{}",
            usage_provenance_label(provenance),
            total.terminal_calls,
            total.input.label(),
            total.input.present_calls,
            total.terminal_calls,
            total.output.label(),
            total.output.present_calls,
            total.terminal_calls,
            total.cache_creation_input.label(),
            total.cache_creation_input.present_calls,
            total.terminal_calls,
            total.cache_read_input.label(),
            total.cache_read_input.present_calls,
            total.terminal_calls,
        )
    }

    pub(crate) fn assistant_text_fragment(
        &mut self,
        fragment: &str,
        final_fragment: bool,
        content_ends_with_newline: bool,
    ) -> io::Result<()> {
        self.text_fragment(fragment, final_fragment, content_ends_with_newline)
    }

    pub(crate) fn event(
        &mut self,
        cursor: u64,
        session_id: CanonicalUuid,
        event: &SessionEvent,
    ) -> io::Result<()> {
        match event {
            SessionEvent::SessionCreated {} => {
                writeln!(
                    self.stdout,
                    "event={cursor} session={session_id} session_created"
                )
            }
            SessionEvent::SessionModelSettingsChanged {
                command_id,
                prior_defaults_version,
                installed_defaults_version,
                adjustments,
                ..
            } => writeln!(
                self.stdout,
                "event={cursor} session={session_id} session_model_settings_changed \
                 command={command_id} prior_defaults_version={} \
                 installed_defaults_version={} adjustment_count={}",
                prior_defaults_version.value(),
                installed_defaults_version.value(),
                adjustments.len()
            ),
            SessionEvent::TurnModelSettingsResolved {
                accepted_input_id,
                turn_id,
                defaults_version,
                adjustments,
                ..
            } => writeln!(
                self.stdout,
                "event={cursor} session={session_id} turn_model_settings_resolved \
                 accepted_input={accepted_input_id} turn={turn_id} defaults_version={} \
                 adjustment_count={}",
                defaults_version.value(),
                adjustments.len()
            ),
            SessionEvent::InputAccepted {
                accepted_input_id,
                turn_id,
                acceptance_position,
                content,
            } => {
                writeln!(
                    self.stdout,
                    "event={cursor} session={session_id} input_accepted \
                     accepted_input={accepted_input_id} turn={turn_id} position={}",
                    acceptance_position.value()
                )?;
                self.user_content(content)
            }
            SessionEvent::GoalTurnRetired { turn_id } => writeln!(
                self.stdout,
                "event={cursor} session={session_id} goal_turn_retired turn={turn_id}"
            ),
            SessionEvent::TurnActivated {
                turn_id,
                current_attempt_id,
            } => writeln!(
                self.stdout,
                "event={cursor} session={session_id} turn_activated \
                 turn={turn_id} attempt={current_attempt_id}"
            ),
            SessionEvent::ModelCallTransition {
                turn_id,
                model_call_id,
                state,
            } => writeln!(
                self.stdout,
                "event={cursor} session={session_id} model_call_transition \
                 turn={turn_id} call={model_call_id} state={}",
                model_call_state(*state)
            ),
            SessionEvent::ToolBatchTransition {
                turn_id,
                model_call_id,
                state,
            } => match state {
                ToolBatchState::Proposed { frontier_id } => writeln!(
                    self.stdout,
                    "event={cursor} session={session_id} tool_batch_transition \
                     turn={turn_id} call={model_call_id} state=proposed frontier={frontier_id}"
                ),
                ToolBatchState::ResultsProjected { frontier_id } => writeln!(
                    self.stdout,
                    "event={cursor} session={session_id} tool_batch_transition \
                     turn={turn_id} call={model_call_id} state=results_projected \
                     frontier={frontier_id}"
                ),
                ToolBatchState::RecoveryRequired { tool_attempt_id } => writeln!(
                    self.stdout,
                    "event={cursor} session={session_id} tool_batch_transition \
                     turn={turn_id} call={model_call_id} state=recovery_required \
                     tool_attempt={tool_attempt_id}"
                ),
            },
            SessionEvent::RunnerStateTransition {
                runner_id,
                placement_revision,
                sandbox_profile,
                working_directory,
                state,
            } => match working_directory {
                Some(working_directory) => writeln!(
                    self.stdout,
                    "event={cursor} session={session_id} runner_state_transition \
                     runner={runner_id} placement_revision={} sandbox={} \
                     working_directory={} state={}",
                    placement_revision.value(),
                    runner_sandbox_profile(*sandbox_profile),
                    self.render_field(working_directory.as_str(), TextField::DelimitedOnLine,),
                    runner_state_transition_state(*state),
                ),
                None => writeln!(
                    self.stdout,
                    "event={cursor} session={session_id} runner_state_transition \
                     runner={runner_id} placement_revision={} sandbox={} state={}",
                    placement_revision.value(),
                    runner_sandbox_profile(*sandbox_profile),
                    runner_state_transition_state(*state),
                ),
            },
            SessionEvent::ToolApprovalDecided {
                turn_id,
                tool_request_id,
                decision,
                decider,
                rationale,
            } => {
                let (decision, denial_reason) = match decision {
                    ToolApprovalEventDecision::Approve {} => ("approve", None),
                    ToolApprovalEventDecision::Deny { reason } => ("deny", reason.as_deref()),
                };
                match decider {
                    ToolApprovalEventDecider::User { command_id } => writeln!(
                        self.stdout,
                        "event={cursor} session={session_id} tool_approval_decided \
                         turn={turn_id} request={tool_request_id} decision={decision} \
                         decider=user command={command_id}"
                    )?,
                    ToolApprovalEventDecider::Delegate {
                        model_selection_id,
                        model_call_id,
                    } => writeln!(
                        self.stdout,
                        "event={cursor} session={session_id} tool_approval_decided \
                         turn={turn_id} request={tool_request_id} decision={decision} \
                         decider=delegate model_selection={model_selection_id} \
                         call={model_call_id}"
                    )?,
                    ToolApprovalEventDecider::UserOverride {
                        command_id,
                        overridden_tool_request_id,
                    } => writeln!(
                        self.stdout,
                        "event={cursor} session={session_id} tool_approval_decided \
                         turn={turn_id} request={tool_request_id} decision={decision} \
                         decider=user_override command={command_id} \
                         overridden_request={overridden_tool_request_id}"
                    )?,
                }
                if let Some(reason) = denial_reason {
                    self.text_field("denial_reason", reason)?;
                }
                if let Some(rationale) = rationale {
                    self.text_field("rationale", rationale)?;
                }
                Ok(())
            }
            SessionEvent::ContextCompacted {
                context_compaction_id,
                model_call_id,
                through_position,
                summary_entry_id,
                result_frontier_id,
            } => writeln!(
                self.stdout,
                "event={cursor} session={session_id} context_compacted \
                 compaction={context_compaction_id} call={model_call_id} \
                 through={} summary_entry={summary_entry_id} \
                 frontier={result_frontier_id}",
                through_position.value()
            ),
            SessionEvent::TurnCompleted {
                turn_id,
                model_call_id,
                completion_entry_id,
                terminal_frontier_id,
            } => writeln!(
                self.stdout,
                "event={cursor} session={session_id} turn_completed turn={turn_id} \
                 call={model_call_id} entry={completion_entry_id} \
                 frontier={terminal_frontier_id}"
            ),
            SessionEvent::TurnFailed {
                turn_id,
                failure_entry_id,
                terminal_frontier_id,
            } => writeln!(
                self.stdout,
                "event={cursor} session={session_id} turn_failed turn={turn_id} \
                 entry={failure_entry_id} frontier={terminal_frontier_id}"
            ),
            SessionEvent::TurnRefused {
                turn_id,
                model_call_id,
                terminal_frontier_id,
            } => writeln!(
                self.stdout,
                "event={cursor} session={session_id} turn_refused turn={turn_id} \
                 call={model_call_id} frontier={terminal_frontier_id}"
            ),
            SessionEvent::TurnCancelled {
                turn_id,
                cancellation_entry_id,
                terminal_frontier_id,
            } => writeln!(
                self.stdout,
                "event={cursor} session={session_id} turn_cancelled turn={turn_id} \
                 entry={cancellation_entry_id} frontier={terminal_frontier_id}"
            ),
            SessionEvent::TurnReconciliationRequired {
                turn_id,
                model_call_id,
                terminal_frontier_id,
            } => writeln!(
                self.stdout,
                "event={cursor} session={session_id} turn_reconciliation_required \
                 turn={turn_id} operation=model_call operation_id={model_call_id} \
                 frontier={terminal_frontier_id}"
            ),
            SessionEvent::TurnToolReconciliationRequired {
                turn_id,
                tool_attempt_id,
                terminal_frontier_id,
            } => writeln!(
                self.stdout,
                "event={cursor} session={session_id} turn_tool_reconciliation_required \
                 turn={turn_id} operation=tool_attempt operation_id={tool_attempt_id} \
                 frontier={terminal_frontier_id}"
            ),
            SessionEvent::ChildSpawned {
                spawning_request_id,
                child_session_id,
                relationship,
            } => match relationship {
                DelegationPolicy::Background {} => writeln!(
                    self.stdout,
                    "event={cursor} session={session_id} delegation_child_spawned \
                     spawning_request={spawning_request_id} child={child_session_id} \
                     policy=background"
                ),
                DelegationPolicy::Bound {
                    on_parent_stopped,
                    on_parent_cancelled,
                } => writeln!(
                    self.stdout,
                    "event={cursor} session={session_id} delegation_child_spawned \
                     spawning_request={spawning_request_id} child={child_session_id} \
                     policy=bound on_parent_stopped={} on_parent_cancelled={}",
                    bound_child_action(*on_parent_stopped),
                    bound_child_action(*on_parent_cancelled)
                ),
            },
            SessionEvent::ChildWaiting {
                await_request_id,
                spawning_request_id,
                child_session_id,
                mode,
            } => writeln!(
                self.stdout,
                "event={cursor} session={session_id} delegation_child_waiting \
                 spawning_request={spawning_request_id} child={child_session_id} \
                 await_request={await_request_id} mode={}",
                match mode {
                    signalbox_process_protocol::DelegationWaitMode::Foreground => "foreground",
                    signalbox_process_protocol::DelegationWaitMode::Background => "background",
                }
            ),
            SessionEvent::ChildLifecycleDisposition {
                spawning_request_id,
                child_session_id,
                outcome,
                reason,
                provenance,
            } => writeln!(
                self.stdout,
                "event={cursor} session={session_id} delegation_child_lifecycle_disposition \
                 spawning_request={spawning_request_id} child={child_session_id} \
                 outcome={} reason={} provenance={}",
                delegation_outcome(*outcome),
                delegation_reason(*reason),
                delegation_provenance(provenance)
            ),
            SessionEvent::ChildResult {
                spawning_request_id,
                child_session_id,
                outcome,
                reason,
                provenance,
                content,
            } => {
                writeln!(
                    self.stdout,
                    "event={cursor} session={session_id} delegation_child_result \
                     spawning_request={spawning_request_id} child={child_session_id} \
                     outcome={} reason={} provenance={} content_present={}",
                    delegation_outcome(*outcome),
                    delegation_reason(*reason),
                    delegation_provenance(provenance),
                    content.is_some()
                )?;
                if let Some(content) = content {
                    self.text(content)
                } else {
                    Ok(())
                }
            }
            SessionEvent::SessionMessage {
                spawning_request_id,
                message_id,
                sender_session_id,
                recipient_session_id,
                ordinal,
                delivery_sequence,
                content,
            } => {
                writeln!(
                    self.stdout,
                    "event={cursor} session={session_id} delegation_session_message \
                     spawning_request={spawning_request_id} message={message_id} \
                     sender={sender_session_id} recipient={recipient_session_id} \
                     ordinal={} delivery_sequence={}",
                    ordinal.value(),
                    delivery_sequence.value()
                )?;
                self.text(content)
            }
        }
    }

    pub(crate) fn provider_text_delta(
        &mut self,
        session_id: CanonicalUuid,
        turn_id: CanonicalUuid,
        model_call_id: CanonicalUuid,
        part_index: u64,
        content: &str,
    ) -> io::Result<()> {
        let content = self.render_field(content, TextField::TrailingOnLine);
        writeln!(
            self.stdout,
            "provider_text_delta session={session_id} turn={turn_id} call={model_call_id} \
             part={part_index} content={content}"
        )?;
        self.stdout.flush()
    }

    fn text(&mut self, text: &str) -> io::Result<()> {
        self.text_fragment(text, true, text.ends_with('\n'))
    }

    fn user_content(&mut self, content: &UserInputContent) -> io::Result<()> {
        match content.parts() {
            [UserInputPart::Text { text }] => self.text(text),
            parts => {
                self.user_content_parts_json(parts)?;
                writeln!(self.stdout)?;
                if self.raw {
                    self.stdout.flush()?;
                }
                Ok(())
            }
        }
    }

    fn user_content_parts_json(&mut self, parts: &[UserInputPart]) -> io::Result<()> {
        let serialized = serde_json::to_string(parts)?;
        if self.raw {
            return self.stdout.write_all(serialized.as_bytes());
        }
        for character in serialized.chars() {
            let code = character as u32;
            if (0x7f..=0x9f).contains(&code) {
                write!(self.stdout, "\\u{code:04x}")?;
            } else {
                write!(self.stdout, "{character}")?;
            }
        }
        Ok(())
    }

    fn text_fragment(
        &mut self,
        fragment: &str,
        final_fragment: bool,
        content_ends_with_newline: bool,
    ) -> io::Result<()> {
        if self.raw {
            self.stdout.write_all(fragment.as_bytes())?;
            if final_fragment {
                self.stdout.flush()?;
            }
            return Ok(());
        }
        self.stdout.write_all(self.render(fragment).as_bytes())?;
        if final_fragment && !content_ends_with_newline {
            self.stdout.write_all(b"\n")?;
        }
        Ok(())
    }

    fn snapshot_turn(&mut self, turn: &TranscriptTurn) -> io::Result<()> {
        let turn_id = turn.turn_id;
        let position = turn.acceptance_position;
        match &turn.state {
            TurnState::Queued {
                accepted_input_id,
                content,
            } => {
                writeln!(
                    self.stdout,
                    "turn={turn_id} position={position} state=queued \
                     accepted_input={accepted_input_id}"
                )?;
                self.user_content(content)
            }
            TurnState::QueuedDelegated {
                spawning_request_id,
                parent_session_id,
                parent_turn_id,
                content,
            } => {
                writeln!(
                    self.stdout,
                    "turn={turn_id} position={position} state=queued_delegated \
                     spawning_request={spawning_request_id} \
                     parent_session={parent_session_id} parent_turn={parent_turn_id}"
                )?;
                self.text(content.as_str())
            }
            TurnState::QueuedDelegationWake {
                first_delivery_sequence,
                through_delivery_sequence,
            } => writeln!(
                self.stdout,
                "turn={turn_id} position={position} state=queued_delegation_wake \
                 deliveries={}-{}",
                first_delivery_sequence.value(),
                through_delivery_sequence.value()
            ),
            TurnState::DelegationTerminated {
                spawning_request_id,
                outcome,
                reason,
                provenance,
            } => writeln!(
                self.stdout,
                "turn={turn_id} position={position} state=delegation_terminated \
                 spawning_request={spawning_request_id} outcome={} reason={} provenance={}",
                delegation_outcome(*outcome),
                delegation_reason(*reason),
                delegation_provenance(provenance)
            ),
            TurnState::ActiveRunning {
                current_attempt_id,
                current_model_call,
            } => match current_model_call {
                Some(call) => writeln!(
                    self.stdout,
                    "turn={turn_id} position={position} state=active_running \
                     attempt={current_attempt_id} call={} call_state={}",
                    call.model_call_id(),
                    current_model_call_state(call.state())
                ),
                None => writeln!(
                    self.stdout,
                    "turn={turn_id} position={position} state=active_running \
                     attempt={current_attempt_id} call=none"
                ),
            },
            TurnState::ActiveAwaitingModelCallRecovery {
                ended_attempt_id,
                recovery_model_call_id,
                automatic_reconciliation_attempts,
                operator_action_required,
            } => {
                let recovery = if *operator_action_required {
                    "operator_required"
                } else {
                    "automatic"
                };
                writeln!(
                    self.stdout,
                    "turn={turn_id} position={position} \
                     state=active_awaiting_model_call_recovery \
                     attempt={ended_attempt_id} call={recovery_model_call_id} \
                     recovery={recovery} recovery_attempts={}",
                    automatic_reconciliation_attempts.value()
                )
            }
            TurnState::ActiveAwaitingToolApproval { tool_request_id } => writeln!(
                self.stdout,
                "turn={turn_id} position={position} state=active_awaiting_tool_approval \
                 request={tool_request_id}"
            ),
            TurnState::ActiveAwaitingChild {
                await_request_id,
                spawning_request_id,
                child_session_id,
            } => writeln!(
                self.stdout,
                "turn={turn_id} position={position} state=active_awaiting_child \
                 request={await_request_id} spawning_request={spawning_request_id} \
                 child={child_session_id}"
            ),
            TurnState::ActiveAwaitingToolRecovery {
                ended_attempt_id,
                recovery_tool_attempt_id,
                automatic_reconciliation_attempts,
                operator_action_required,
            } => {
                let recovery = if *operator_action_required {
                    "operator_required"
                } else {
                    "automatic"
                };
                writeln!(
                    self.stdout,
                    "turn={turn_id} position={position} state=active_awaiting_tool_recovery \
                     attempt={ended_attempt_id} tool_attempt={recovery_tool_attempt_id} \
                     recovery={recovery} recovery_attempts={}",
                    automatic_reconciliation_attempts.value()
                )
            }
            TurnState::ActiveAwaitingRunnerRecovery {
                runner_id,
                placement_revision,
                tool_attempt_id,
            } => match tool_attempt_id {
                Some(tool_attempt_id) => writeln!(
                    self.stdout,
                    "turn={turn_id} position={position} state=active_awaiting_runner_recovery \
                     runner={runner_id} placement_revision={} tool_attempt={tool_attempt_id}",
                    placement_revision.value()
                ),
                None => writeln!(
                    self.stdout,
                    "turn={turn_id} position={position} state=active_awaiting_runner_recovery \
                     runner={runner_id} placement_revision={} tool_attempt=none",
                    placement_revision.value()
                ),
            },
            TurnState::Failed {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call,
            } => match (terminal_attempt_id, terminal_model_call) {
                (None, None) => writeln!(
                    self.stdout,
                    "turn={turn_id} position={position} state=failed \
                     frontier={terminal_frontier_id} attempt=none call=none"
                ),
                (Some(attempt_id), None) => writeln!(
                    self.stdout,
                    "turn={turn_id} position={position} state=failed \
                     frontier={terminal_frontier_id} attempt={attempt_id} call=none"
                ),
                (Some(attempt_id), Some(call)) => writeln!(
                    self.stdout,
                    "turn={turn_id} position={position} state=failed \
                     frontier={terminal_frontier_id} attempt={attempt_id} call={} \
                     call_disposition={} call_cause={}",
                    call.model_call_id(),
                    failed_model_call_disposition(call.disposition()),
                    call.cause().map_or("none", failed_model_call_cause)
                ),
                (None, Some(_)) => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "failed turn carried terminal call evidence without an attempt",
                )),
            },
            TurnState::Completed {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            } => writeln!(
                self.stdout,
                "turn={turn_id} position={position} state=completed \
                 frontier={terminal_frontier_id} attempt={terminal_attempt_id} \
                 call={terminal_model_call_id}"
            ),
            TurnState::Refused {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            } => writeln!(
                self.stdout,
                "turn={turn_id} position={position} state=refused \
                 frontier={terminal_frontier_id} attempt={terminal_attempt_id} \
                 call={terminal_model_call_id}"
            ),
            TurnState::Cancelled {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            } => match terminal_model_call_id {
                Some(model_call_id) => writeln!(
                    self.stdout,
                    "turn={turn_id} position={position} state=cancelled \
                     frontier={terminal_frontier_id} attempt={terminal_attempt_id} \
                     call={model_call_id}"
                ),
                None => writeln!(
                    self.stdout,
                    "turn={turn_id} position={position} state=cancelled \
                     frontier={terminal_frontier_id} attempt={terminal_attempt_id} call=none"
                ),
            },
            TurnState::ReconciliationRequired {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            } => writeln!(
                self.stdout,
                "turn={turn_id} position={position} state=reconciliation_required \
                 frontier={terminal_frontier_id} attempt={terminal_attempt_id} \
                 operation=model_call operation_id={terminal_model_call_id}"
            ),
            TurnState::ToolReconciliationRequired {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_tool_attempt_id,
            } => writeln!(
                self.stdout,
                "turn={turn_id} position={position} state=tool_reconciliation_required \
                 frontier={terminal_frontier_id} attempt={terminal_attempt_id} \
                 operation=tool_attempt operation_id={terminal_tool_attempt_id}"
            ),
        }
    }

    fn snapshot_entry(&mut self, entry: &SnapshotEntry) -> io::Result<()> {
        match &entry.kind {
            SnapshotEntryKind::User {
                accepted_input_id,
                turn_id,
                content,
            } => {
                write!(
                    self.stdout,
                    "user_content source_session={} entry={} accepted_input={accepted_input_id} turn={turn_id} parts=",
                    entry.source_session_id, entry.entry_id
                )?;
                self.user_content_parts_json(content.parts())?;
                writeln!(self.stdout)?;
                if self.raw {
                    self.stdout.flush()?;
                }
                Ok(())
            }
            SnapshotEntryKind::Text(metadata) => {
                let label = match metadata {
                    TranscriptTextEntry::Assistant { turn_id, .. } => {
                        format!("assistant turn={turn_id}")
                    }
                    TranscriptTextEntry::ContextSummary {
                        model_call_id,
                        first_source_session_id,
                        first_entry_id,
                        through_source_session_id,
                        through_entry_id,
                    } => format!(
                        "context_summary model_call={model_call_id} range={first_source_session_id}/{first_entry_id}..={through_source_session_id}/{through_entry_id}"
                    ),
                    TranscriptTextEntry::Imported {
                        imported_conversation_id,
                        imported_entry_id,
                        source_speaker,
                    } => format!(
                        "imported_{} imported_conversation={imported_conversation_id} \
                         imported_entry={imported_entry_id}",
                        imported_speaker_label(*source_speaker)
                    ),
                };
                writeln!(
                    self.stdout,
                    "{label} source={} entry={}",
                    entry.source_session_id, entry.entry_id
                )
            }
            SnapshotEntryKind::Marker(TranscriptEntry::TurnCompleted { turn_id }) => {
                writeln!(
                    self.stdout,
                    "turn_completed turn={turn_id} source={} entry={}",
                    entry.source_session_id, entry.entry_id
                )
            }
            SnapshotEntryKind::Marker(TranscriptEntry::TurnFailed { turn_id }) => {
                writeln!(
                    self.stdout,
                    "turn_failed turn={turn_id} source={} entry={}",
                    entry.source_session_id, entry.entry_id
                )
            }
            SnapshotEntryKind::Marker(TranscriptEntry::TurnCancelled { turn_id }) => {
                writeln!(
                    self.stdout,
                    "turn_cancelled turn={turn_id} source={} entry={}",
                    entry.source_session_id, entry.entry_id
                )
            }
            SnapshotEntryKind::Marker(TranscriptEntry::DelegatedTask {
                spawning_request_id,
                parent_session_id,
                parent_turn_id,
                content,
            }) => writeln!(
                self.stdout,
                "delegated_task spawning_request={spawning_request_id} \
                 parent_session={parent_session_id} parent_turn={parent_turn_id} \
                 content={} source={} entry={}",
                self.render(content),
                entry.source_session_id,
                entry.entry_id
            ),
            SnapshotEntryKind::Marker(TranscriptEntry::DelegationMessage {
                spawning_request_id,
                message_id,
                sender_session_id,
                recipient_session_id,
                ordinal,
                delivery_sequence,
                content,
            }) => writeln!(
                self.stdout,
                "delegation_message spawning_request={spawning_request_id} message={message_id} \
                 sender={sender_session_id} recipient={recipient_session_id} \
                 ordinal={} delivery_sequence={} content={} source={} entry={}",
                ordinal.value(),
                delivery_sequence.value(),
                self.render(content),
                entry.source_session_id,
                entry.entry_id
            ),
            SnapshotEntryKind::Marker(TranscriptEntry::DelegationResult {
                await_request_id,
                spawning_request_id,
                child_session_id,
                mode,
                delivery_sequence,
                outcome,
                content,
                reason,
                provenance,
            }) => writeln!(
                self.stdout,
                "delegation_result await_request={await_request_id} \
                 spawning_request={spawning_request_id} child={child_session_id} mode={} \
                 delivery_sequence={} outcome={} content={} reason={} provenance={} \
                 source={} entry={}",
                delegation_wait_mode(*mode),
                delivery_sequence
                    .map(|sequence| sequence.value().to_string())
                    .unwrap_or_else(|| String::from("none")),
                delegation_outcome(*outcome),
                content
                    .as_deref()
                    .map(|value| self.render(value))
                    .unwrap_or_else(|| String::from("none")),
                delegation_reason(*reason),
                delegation_provenance(provenance),
                entry.source_session_id,
                entry.entry_id
            ),
            SnapshotEntryKind::Marker(TranscriptEntry::ModelIdentityChanged {
                turn_id,
                defaults_version,
                selected_model_id,
            }) => writeln!(
                self.stdout,
                "model_identity_changed turn={turn_id} defaults_version={} model={selected_model_id} \
                 source={} entry={}",
                defaults_version.value(),
                entry.source_session_id,
                entry.entry_id
            ),
            SnapshotEntryKind::Marker(TranscriptEntry::AssistantToolUse {
                turn_id,
                model_call_id,
                tool_request_id,
                tool_name,
                arguments,
                approval,
            }) => {
                writeln!(
                    self.stdout,
                    "assistant_tool_use turn={turn_id} call={model_call_id} \
                     request={tool_request_id} name={} arguments={} source={} entry={}",
                    self.render(tool_name),
                    self.render(arguments),
                    entry.source_session_id,
                    entry.entry_id
                )?;
                if let Some(approval) = approval {
                    let (decision, reason) = match &approval.decision {
                        ToolApprovalEventDecision::Approve {} => ("approve", None),
                        ToolApprovalEventDecision::Deny { reason } => ("deny", reason.as_deref()),
                    };
                    match &approval.decider {
                        ToolApprovalEventDecider::User { command_id } => writeln!(
                            self.stdout,
                            "tool_approval request={tool_request_id} decision={decision} \
                             decider=user command={command_id}"
                        )?,
                        ToolApprovalEventDecider::Delegate {
                            model_selection_id,
                            model_call_id,
                        } => writeln!(
                            self.stdout,
                            "tool_approval request={tool_request_id} decision={decision} \
                             decider=delegate model_selection={model_selection_id} \
                             call={model_call_id}"
                        )?,
                        ToolApprovalEventDecider::UserOverride {
                            command_id,
                            overridden_tool_request_id,
                        } => writeln!(
                            self.stdout,
                            "tool_approval request={tool_request_id} decision={decision} \
                             decider=user_override command={command_id} \
                             overridden_request={overridden_tool_request_id}"
                        )?,
                    }
                    if let Some(reason) = reason {
                        self.text_field("denial_reason", reason)?;
                    }
                    if let Some(rationale) = &approval.rationale {
                        self.text_field("rationale", rationale)?;
                    }
                }
                Ok(())
            }
            SnapshotEntryKind::Marker(TranscriptEntry::ToolExecutionResult {
                tool_request_id,
                tool_attempt_id,
                content,
            }) => writeln!(
                self.stdout,
                "tool_execution_result request={tool_request_id} attempt={tool_attempt_id} \
                 content={} source={} entry={}",
                self.render(content),
                entry.source_session_id,
                entry.entry_id
            ),
            SnapshotEntryKind::Marker(TranscriptEntry::ToolDenied {
                tool_request_id,
                content,
            }) => writeln!(
                self.stdout,
                "tool_denied request={tool_request_id} content={} source={} entry={}",
                self.render(content),
                entry.source_session_id,
                entry.entry_id
            ),
            SnapshotEntryKind::Marker(TranscriptEntry::ToolClosed {
                tool_request_id,
                content,
            }) => writeln!(
                self.stdout,
                "tool_closed request={tool_request_id} content={} source={} entry={}",
                self.render(content),
                entry.source_session_id,
                entry.entry_id
            ),
            SnapshotEntryKind::Marker(TranscriptEntry::Imported {
                imported_conversation_id,
                imported_entry_id,
                source_speaker,
                content_kind,
            }) => writeln!(
                self.stdout,
                "imported_{} kind={} imported_conversation={imported_conversation_id} \
                 imported_entry={imported_entry_id} source={} entry={}",
                imported_speaker_label(*source_speaker),
                imported_content_kind(*content_kind),
                entry.source_session_id,
                entry.entry_id
            ),
        }
    }

    fn render(&self, value: &str) -> String {
        self.render_field(value, TextField::Flowing)
    }

    fn render_field(&self, value: &str, field: TextField) -> String {
        if self.raw {
            value.to_owned()
        } else {
            control_safe(value, field)
        }
    }
}

const fn operator_status_lifecycle_state_label(
    state: OperatorStatusLifecycleState,
) -> &'static str {
    match state {
        OperatorStatusLifecycleState::Created => "created",
        OperatorStatusLifecycleState::Dispatched => "dispatched",
        OperatorStatusLifecycleState::Active => "active",
        OperatorStatusLifecycleState::Waiting => "waiting",
        OperatorStatusLifecycleState::Recovering => "recovering",
        OperatorStatusLifecycleState::Blocked => "blocked",
        OperatorStatusLifecycleState::Parked => "parked",
    }
}

/// One metric's two counts: how much of a population the metric names.
#[derive(Clone, Copy)]
struct RateCounts {
    numerator: u64,
    denominator: u64,
}

/// Renders one metric as its exact counts beside its parts-per-million rate.
///
/// An empty population prints no rate rather than a zero.
fn rate_label(counts: RateCounts) -> String {
    let RateCounts {
        numerator,
        denominator,
    } = counts;
    if denominator == 0 {
        return format!("{numerator}/0");
    }
    let ppm = u128::from(numerator) * 1_000_000 / u128::from(denominator);
    format!("{numerator}/{denominator}@{ppm}ppm")
}

fn duration_label(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = seconds % 86_400 / 3_600;
    let minutes = seconds % 3_600 / 60;
    let seconds = seconds % 60;
    if days > 0 {
        format!("{days}d{hours}h{minutes}m{seconds}s")
    } else if hours > 0 {
        format!("{hours}h{minutes}m{seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m{seconds}s")
    } else {
        format!("{seconds}s")
    }
}

impl SnapshotSelection {
    fn context(
        self,
        snapshot: &mut TranscriptSnapshot,
    ) -> Result<SnapshotSelectionContext, ClientError> {
        if matches!(self, Self::All) {
            return Ok(SnapshotSelectionContext::default());
        }
        let mut proposals = HashSet::new();
        let mut results = HashSet::new();
        let mut trailing_results = HashSet::new();
        let mut terminal_results = HashSet::new();
        let mut reconciliation_call = None;
        let mut reconciliation_proposals = HashSet::new();
        let mut anchor_found = false;
        for record in snapshot.replay()? {
            let record = record?;
            if let SnapshotRecord::Turn(turn) = &record {
                if matches!(
                    (self, &turn.state),
                    (
                        Self::ToolReconciliation {
                            turn_id,
                            tool_attempt_id,
                            terminal_frontier_id,
                        },
                        TurnState::ToolReconciliationRequired {
                            terminal_frontier_id: stored_frontier,
                            terminal_tool_attempt_id: stored_attempt,
                            ..
                        },
                    ) if turn_id == turn.turn_id
                        && tool_attempt_id == *stored_attempt
                        && terminal_frontier_id == *stored_frontier
                ) {
                    anchor_found = true;
                }
                continue;
            }
            let SnapshotRecord::Entry(entry) = record else {
                continue;
            };
            match &entry.kind {
                SnapshotEntryKind::Marker(TranscriptEntry::AssistantToolUse {
                    turn_id,
                    model_call_id,
                    tool_request_id,
                    ..
                }) => {
                    trailing_results.clear();
                    if self.matches_tool_batch(*turn_id, *model_call_id) {
                        proposals.insert(*tool_request_id);
                        if matches!(self, Self::ToolBatchProposed { .. }) {
                            anchor_found = true;
                        }
                    }
                    if matches!(
                        self,
                        Self::ToolReconciliation {
                            turn_id: selected_turn,
                            ..
                        } if selected_turn == *turn_id
                    ) {
                        if reconciliation_call != Some(*model_call_id) {
                            reconciliation_call = Some(*model_call_id);
                            reconciliation_proposals.clear();
                        }
                        reconciliation_proposals.insert(*tool_request_id);
                    }
                }
                SnapshotEntryKind::Marker(
                    TranscriptEntry::ToolExecutionResult {
                        tool_request_id, ..
                    }
                    | TranscriptEntry::ToolDenied {
                        tool_request_id, ..
                    }
                    | TranscriptEntry::ToolClosed {
                        tool_request_id, ..
                    },
                ) => {
                    results.insert(*tool_request_id);
                    trailing_results.insert(*tool_request_id);
                }
                SnapshotEntryKind::Marker(TranscriptEntry::DelegationResult {
                    await_request_id,
                    mode: DelegationWaitMode::Foreground,
                    ..
                }) => {
                    results.insert(*await_request_id);
                    trailing_results.insert(*await_request_id);
                }
                _ if self.includes_terminal_marker(&entry) => {
                    anchor_found = true;
                    terminal_results.clone_from(&trailing_results);
                    trailing_results.clear();
                }
                _ => trailing_results.clear(),
            }
        }
        match self {
            Self::ToolBatchResults { .. } => {
                if proposals.is_empty()
                    || proposals.iter().any(|request| !results.contains(request))
                {
                    return Err(ClientError::Protocol(
                        "tool-result reread omitted the event's exact proposal or result set",
                    ));
                }
                Ok(SnapshotSelectionContext {
                    requests: proposals,
                })
            }
            Self::ToolBatchProposed { .. } if anchor_found => {
                Ok(SnapshotSelectionContext::default())
            }
            Self::Completed { .. } | Self::Failed { .. } | Self::Cancelled { .. }
                if anchor_found =>
            {
                Ok(SnapshotSelectionContext {
                    requests: terminal_results,
                })
            }
            Self::ToolReconciliation { .. }
                if anchor_found
                    && !reconciliation_proposals.is_empty()
                    && reconciliation_proposals
                        .iter()
                        .all(|request| results.contains(request)) =>
            {
                Ok(SnapshotSelectionContext {
                    requests: reconciliation_proposals,
                })
            }
            Self::ToolBatchProposed { .. } => Err(ClientError::Protocol(
                "tool-proposal reread omitted the event's exact proposal",
            )),
            Self::Completed { .. } | Self::Failed { .. } | Self::Cancelled { .. } => Err(
                ClientError::Protocol("terminal reread omitted the event's exact marker"),
            ),
            Self::ToolReconciliation { .. } => Err(ClientError::Protocol(
                "tool reconciliation reread omitted its exact terminal result suffix",
            )),
            Self::All => Ok(SnapshotSelectionContext::default()),
        }
    }

    fn includes(self, entry: &SnapshotEntry, context: &SnapshotSelectionContext) -> bool {
        match (self, &entry.kind) {
            (Self::All, _) => true,
            (
                Self::Completed {
                    turn_id,
                    model_call_id,
                    ..
                }
                | Self::ToolBatchProposed {
                    turn_id,
                    model_call_id,
                },
                SnapshotEntryKind::Text(TranscriptTextEntry::Assistant {
                    turn_id: entry_turn,
                    model_call_id: entry_call,
                }),
            ) => turn_id == *entry_turn && model_call_id == *entry_call,
            (
                Self::ToolBatchProposed {
                    turn_id,
                    model_call_id,
                },
                SnapshotEntryKind::Marker(TranscriptEntry::AssistantToolUse {
                    turn_id: entry_turn,
                    model_call_id: entry_call,
                    ..
                }),
            ) => turn_id == *entry_turn && model_call_id == *entry_call,
            (
                Self::ToolBatchResults { .. }
                | Self::ToolReconciliation { .. }
                | Self::Failed { .. }
                | Self::Cancelled { .. },
                SnapshotEntryKind::Marker(
                    TranscriptEntry::ToolExecutionResult {
                        tool_request_id, ..
                    }
                    | TranscriptEntry::ToolDenied {
                        tool_request_id, ..
                    }
                    | TranscriptEntry::ToolClosed {
                        tool_request_id, ..
                    },
                ),
            ) => context.requests.contains(tool_request_id),
            (
                Self::ToolBatchResults { .. }
                | Self::ToolReconciliation { .. }
                | Self::Failed { .. }
                | Self::Cancelled { .. },
                SnapshotEntryKind::Marker(TranscriptEntry::DelegationResult {
                    await_request_id,
                    mode: DelegationWaitMode::Foreground,
                    ..
                }),
            ) => context.requests.contains(await_request_id),
            (
                Self::Completed { .. } | Self::Failed { .. } | Self::Cancelled { .. },
                SnapshotEntryKind::Marker(_),
            ) => self.includes_terminal_marker(entry),
            (
                Self::Completed { .. }
                | Self::Failed { .. }
                | Self::Cancelled { .. }
                | Self::ToolBatchProposed { .. }
                | Self::ToolBatchResults { .. }
                | Self::ToolReconciliation { .. },
                SnapshotEntryKind::User { .. } | SnapshotEntryKind::Text(_),
            ) => false,
            (
                Self::ToolBatchProposed { .. }
                | Self::ToolBatchResults { .. }
                | Self::ToolReconciliation { .. },
                SnapshotEntryKind::Marker(_),
            ) => false,
        }
    }

    fn matches_tool_batch(self, entry_turn: CanonicalUuid, entry_call: CanonicalUuid) -> bool {
        matches!(
            self,
            Self::ToolBatchProposed {
                turn_id,
                model_call_id,
            } | Self::ToolBatchResults {
                turn_id,
                model_call_id,
            } if turn_id == entry_turn && model_call_id == entry_call
        )
    }

    fn includes_terminal_marker(self, entry: &SnapshotEntry) -> bool {
        match (self, &entry.kind) {
            (
                Self::Completed {
                    turn_id,
                    terminal_entry_id,
                    ..
                },
                SnapshotEntryKind::Marker(TranscriptEntry::TurnCompleted {
                    turn_id: entry_turn,
                }),
            )
            | (
                Self::Failed {
                    turn_id,
                    terminal_entry_id,
                },
                SnapshotEntryKind::Marker(TranscriptEntry::TurnFailed {
                    turn_id: entry_turn,
                }),
            )
            | (
                Self::Cancelled {
                    turn_id,
                    terminal_entry_id,
                },
                SnapshotEntryKind::Marker(TranscriptEntry::TurnCancelled {
                    turn_id: entry_turn,
                }),
            ) => turn_id == *entry_turn && terminal_entry_id == entry.entry_id,
            (
                Self::All
                | Self::Completed { .. }
                | Self::Failed { .. }
                | Self::Cancelled { .. }
                | Self::ToolBatchProposed { .. }
                | Self::ToolBatchResults { .. }
                | Self::ToolReconciliation { .. },
                SnapshotEntryKind::User { .. }
                | SnapshotEntryKind::Text(_)
                | SnapshotEntryKind::Marker(
                    TranscriptEntry::ModelIdentityChanged { .. }
                    | TranscriptEntry::DelegatedTask { .. }
                    | TranscriptEntry::DelegationMessage { .. }
                    | TranscriptEntry::DelegationResult { .. }
                    | TranscriptEntry::AssistantToolUse { .. }
                    | TranscriptEntry::ToolExecutionResult { .. }
                    | TranscriptEntry::ToolDenied { .. }
                    | TranscriptEntry::ToolClosed { .. }
                    | TranscriptEntry::TurnCompleted { .. }
                    | TranscriptEntry::TurnFailed { .. }
                    | TranscriptEntry::TurnCancelled { .. }
                    | TranscriptEntry::Imported { .. },
                ),
            ) => false,
        }
    }
}

const fn imported_speaker_label(source: ImportedSourceSpeaker) -> &'static str {
    match source {
        ImportedSourceSpeaker::NotAttested {} => "speaker_unattested",
        ImportedSourceSpeaker::AttestedAbsent {} => "speaker_absent",
        ImportedSourceSpeaker::Attested {
            speaker: ImportedSpeaker::User,
        } => "user",
        ImportedSourceSpeaker::Attested {
            speaker: ImportedSpeaker::Assistant,
        } => "assistant",
    }
}

/// Names the speaker attestation as a standalone field value, where the
/// transcript's `imported_<suffix>` composition does not supply the noun.
const fn imported_speaker_attestation_label(source: ImportedSourceSpeaker) -> &'static str {
    match source {
        ImportedSourceSpeaker::NotAttested {} => "unattested",
        ImportedSourceSpeaker::AttestedAbsent {} => "absent",
        ImportedSourceSpeaker::Attested {
            speaker: ImportedSpeaker::User,
        } => "user",
        ImportedSourceSpeaker::Attested {
            speaker: ImportedSpeaker::Assistant,
        } => "assistant",
    }
}

const fn imported_content_kind(kind: ImportedContentKind) -> &'static str {
    match kind {
        ImportedContentKind::SourceEvent => "source_event",
        ImportedContentKind::SourceMessageBlock => "source_message_block",
        ImportedContentKind::Text => "text",
        ImportedContentKind::ToolCall => "tool_call",
        ImportedContentKind::ToolResult => "tool_result",
        ImportedContentKind::Thinking => "thinking",
        ImportedContentKind::RedactedThinking => "redacted_thinking",
        ImportedContentKind::Document => "document",
        ImportedContentKind::MessageContentAbsent => "message_content_absent",
    }
}

fn model_call_state(state: ModelCallState) -> &'static str {
    match state {
        ModelCallState::Prepared {} => "prepared",
        ModelCallState::InFlight {} => "in_flight",
        ModelCallState::CancellationRequested {} => "cancellation_requested",
        ModelCallState::Terminal { disposition } => match disposition {
            ModelCallDisposition::Completed => "terminal:completed",
            ModelCallDisposition::KnownFailed => "terminal:known_failed",
            ModelCallDisposition::Refused => "terminal:refused",
            ModelCallDisposition::Cancelled => "terminal:cancelled",
            ModelCallDisposition::Ambiguous => "terminal:ambiguous",
        },
    }
}

const fn runner_sandbox_profile(profile: RunnerSandboxProfile) -> &'static str {
    match profile {
        RunnerSandboxProfile::Ambient => "ambient",
        RunnerSandboxProfile::WorkspaceRestricted => "workspace_restricted",
    }
}

const fn runner_connection_health(health: RunnerConnectionHealth) -> &'static str {
    match health {
        RunnerConnectionHealth::Connected => "connected",
        RunnerConnectionHealth::Suspect => "suspect",
        RunnerConnectionHealth::Shutdown => "shutdown",
        RunnerConnectionHealth::Lost => "lost",
    }
}

const fn runner_projection_state(state: RunnerProjectionState) -> &'static str {
    match state {
        RunnerProjectionState::Unpinned => "unpinned",
        RunnerProjectionState::Pinned => "pinned",
        RunnerProjectionState::RunnerLostBeforePin => "runner_lost_before_pin",
        RunnerProjectionState::RunnerLost => "runner_lost",
        RunnerProjectionState::RunnerAbandoned => "runner_abandoned",
    }
}

const fn runner_state_transition_state(state: RunnerStateTransitionState) -> &'static str {
    match state {
        RunnerStateTransitionState::Pinned => "pinned",
        RunnerStateTransitionState::Suspect => "suspect",
        RunnerStateTransitionState::Connected => "connected",
        RunnerStateTransitionState::RunnerLostBeforePin => "runner_lost_before_pin",
        RunnerStateTransitionState::RunnerLost => "runner_lost",
        RunnerStateTransitionState::Replaced => "replaced",
        RunnerStateTransitionState::WorkingDirectoryChanged => "working_directory_changed",
        RunnerStateTransitionState::Abandoned => "abandoned",
    }
}

const fn current_model_call_state(state: CurrentModelCallState) -> &'static str {
    match state {
        CurrentModelCallState::Prepared {} => "prepared",
        CurrentModelCallState::InFlight {} => "in_flight",
        CurrentModelCallState::CancellationRequested {} => "cancellation_requested",
    }
}

const fn failed_model_call_disposition(disposition: FailedModelCallDisposition) -> &'static str {
    match disposition {
        FailedModelCallDisposition::KnownFailed => "known_failed",
        FailedModelCallDisposition::Cancelled => "cancelled",
    }
}

const fn failed_model_call_cause(cause: FailedModelCallCause) -> &'static str {
    match cause {
        FailedModelCallCause::CredentialRejected => "credential_rejected",
        FailedModelCallCause::AttachmentTooLarge => "attachment_too_large",
        FailedModelCallCause::AttachmentMissing => "attachment_missing",
        FailedModelCallCause::AttachmentCorrupt => "attachment_corrupt",
        FailedModelCallCause::PermissionDenied => "permission_denied",
        FailedModelCallCause::InvalidRequest => "invalid_request",
        FailedModelCallCause::TargetNotFound => "target_not_found",
        FailedModelCallCause::RequestTooLarge => "request_too_large",
        FailedModelCallCause::RateLimited => "rate_limited",
        FailedModelCallCause::QuotaExhausted => "quota_exhausted",
        FailedModelCallCause::Overloaded => "overloaded",
        FailedModelCallCause::ProviderInternal => "provider_internal",
        FailedModelCallCause::Unrecognized => "unrecognized",
    }
}

const fn bound_child_action(action: BoundChildAction) -> &'static str {
    match action {
        BoundChildAction::KeepRunning => "keep_running",
        BoundChildAction::Stop => "stop",
        BoundChildAction::Cancel => "cancel",
    }
}

const fn delegation_outcome(outcome: DelegationOutcome) -> &'static str {
    match outcome {
        DelegationOutcome::Returned => "returned",
        DelegationOutcome::Failed => "failed",
        DelegationOutcome::Stopped => "stopped",
        DelegationOutcome::Cancelled => "cancelled",
        DelegationOutcome::ContinueRunning => "continue_running",
        DelegationOutcome::AlreadyTerminal => "already_terminal",
    }
}

const fn delegation_reason(reason: DelegationReason) -> &'static str {
    match reason {
        DelegationReason::ChildCompleted => "child_completed",
        DelegationReason::ChildExecutionFailed => "child_execution_failed",
        DelegationReason::ChildResultUnavailable => "child_result_unavailable",
        DelegationReason::ChildCancelled => "child_cancelled",
        DelegationReason::ParentStopped => "parent_stopped",
        DelegationReason::ParentCancelled => "parent_cancelled",
    }
}

const fn delegation_wait_mode(mode: DelegationWaitMode) -> &'static str {
    match mode {
        DelegationWaitMode::Foreground => "foreground",
        DelegationWaitMode::Background => "background",
    }
}

const fn delegation_message_direction(direction: DelegationMessageDirection) -> &'static str {
    match direction {
        DelegationMessageDirection::ParentToChild => "parent_to_child",
        DelegationMessageDirection::ChildToParent => "child_to_parent",
    }
}

fn delegation_provenance(provenance: &DelegationProvenance) -> String {
    match provenance {
        DelegationProvenance::ChildTurn {
            child_session_id,
            child_turn_id,
        } => format!("child_turn:{child_session_id}:{child_turn_id}"),
        DelegationProvenance::ParentTurnCommand {
            parent_session_id,
            parent_turn_id,
            command_id,
            descendant_scope,
        } => format!(
            "parent_turn_command:{parent_session_id}:{parent_turn_id}:{command_id}:{}",
            descendant_scope_label(*descendant_scope)
        ),
        DelegationProvenance::ParentGoalCommand {
            parent_session_id,
            goal_generation,
            command_id,
            descendant_scope,
        } => format!(
            "parent_goal_command:{parent_session_id}:{}:{command_id}:{}",
            goal_generation.value(),
            descendant_scope_label(*descendant_scope)
        ),
        DelegationProvenance::ParentLifecycleCommand {
            parent_session_id,
            command_id,
            descendant_scope,
        } => format!(
            "parent_lifecycle_command:{parent_session_id}:{command_id}:{}",
            descendant_scope_label(*descendant_scope)
        ),
    }
}

const fn descendant_scope_label(scope: DescendantTerminationScope) -> &'static str {
    match scope {
        DescendantTerminationScope::ParentAlone => "parent_alone",
        DescendantTerminationScope::ParentAndDescendants => "parent_and_descendants",
    }
}

const fn goal_blocked_reason_label(reason: GoalBlockedReason) -> &'static str {
    match reason {
        GoalBlockedReason::UserInputRequired => "user_input_required",
        GoalBlockedReason::ExternalChangeRequired => "external_change_required",
        GoalBlockedReason::AuthorizationRequired => "authorization_required",
        GoalBlockedReason::ExecutionFailure => "execution_failure",
        GoalBlockedReason::FinishCheckFailed => "finish_check_failed",
    }
}

const fn session_closure_outcome_label(outcome: SessionClosureOutcome) -> &'static str {
    match outcome {
        SessionClosureOutcome::FailedRetryable => "failed_retryable",
        SessionClosureOutcome::FailedStructural => "failed_structural",
        SessionClosureOutcome::FailedUnknown => "failed_unknown",
        SessionClosureOutcome::Superseded => "superseded",
        SessionClosureOutcome::Stopped => "stopped",
        SessionClosureOutcome::Abandoned => "abandoned",
        SessionClosureOutcome::Retired => "retired",
    }
}

const fn lifecycle_actor_label(actor: LifecycleActorClass) -> &'static str {
    match actor {
        LifecycleActorClass::Core => "core",
        LifecycleActorClass::Operator => "operator",
        LifecycleActorClass::Module => "module",
        LifecycleActorClass::Watchdog => "watchdog",
    }
}

const fn review_orchestration_state_label(state: ReviewOrchestrationState) -> &'static str {
    match state {
        ReviewOrchestrationState::AwaitingImport => "awaiting_import",
        ReviewOrchestrationState::ImportIncomplete => "import_incomplete",
        ReviewOrchestrationState::AwaitingConcerns => "awaiting_concerns",
        ReviewOrchestrationState::FanoutIncomplete => "fanout_incomplete",
        ReviewOrchestrationState::AwaitingJudgment => "awaiting_judgment",
        ReviewOrchestrationState::AwaitingJudgmentEffects => "awaiting_judgment_effects",
        ReviewOrchestrationState::JudgmentIncomplete => "judgment_incomplete",
        ReviewOrchestrationState::AwaitingRepair => "awaiting_repair",
        ReviewOrchestrationState::RepairIncomplete => "repair_incomplete",
        ReviewOrchestrationState::AwaitingPublication => "awaiting_publication",
        ReviewOrchestrationState::PublicationIncomplete => "publication_incomplete",
        ReviewOrchestrationState::Complete => "complete",
    }
}

const fn review_orchestration_concern_status_label(
    status: ReviewOrchestrationConcernStatus,
) -> &'static str {
    match status {
        ReviewOrchestrationConcernStatus::Pending => "pending",
        ReviewOrchestrationConcernStatus::Succeeded => "succeeded",
        ReviewOrchestrationConcernStatus::Failed => "failed",
        ReviewOrchestrationConcernStatus::Blocked => "blocked",
        ReviewOrchestrationConcernStatus::Cancelled => "cancelled",
        ReviewOrchestrationConcernStatus::Superseded => "superseded",
    }
}

const fn review_workflow_label(workflow: ReviewWorkflow) -> &'static str {
    match workflow {
        ReviewWorkflow::ImportExternalContext => "import_external_context",
        ReviewWorkflow::ReadOnlyReview => "read_only_review",
        ReviewWorkflow::JudgeFindings => "judge_findings",
        ReviewWorkflow::DedupeFindings => "dedupe_findings",
        ReviewWorkflow::PublishReview => "publish_review",
        ReviewWorkflow::FixFindings => "fix_findings",
        ReviewWorkflow::PropagateStack => "propagate_stack",
    }
}

const fn review_run_state_label(state: ReviewRunLifecycle) -> &'static str {
    match state {
        ReviewRunLifecycle::Queued => "queued",
        ReviewRunLifecycle::Running => "running",
        ReviewRunLifecycle::Succeeded => "succeeded",
        ReviewRunLifecycle::Failed => "failed",
        ReviewRunLifecycle::Blocked => "blocked",
        ReviewRunLifecycle::Cancelled => "cancelled",
    }
}

const fn review_pass_kind_label(kind: ReviewPassKind) -> &'static str {
    match kind {
        ReviewPassKind::ImportExternalContext => "import_external_context",
        ReviewPassKind::ReadOnlyReview => "read_only_review",
        ReviewPassKind::Judge => "judge",
        ReviewPassKind::Dedupe => "dedupe",
        ReviewPassKind::Publish => "publish",
        ReviewPassKind::Fix => "fix",
        ReviewPassKind::PropagateStack => "propagate_stack",
    }
}

const fn review_pass_state_label(state: ReviewPassLifecycle) -> &'static str {
    match state {
        ReviewPassLifecycle::Queued => "queued",
        ReviewPassLifecycle::Running => "running",
        ReviewPassLifecycle::Succeeded => "succeeded",
        ReviewPassLifecycle::Failed => "failed",
        ReviewPassLifecycle::Blocked => "blocked",
        ReviewPassLifecycle::Cancelled => "cancelled",
    }
}

const fn review_diff_side_label(side: ReviewDiffSide) -> &'static str {
    match side {
        ReviewDiffSide::Left => "left",
        ReviewDiffSide::Right => "right",
    }
}

const fn review_severity_label(severity: ReviewSeverity) -> &'static str {
    match severity {
        ReviewSeverity::Info => "info",
        ReviewSeverity::Low => "low",
        ReviewSeverity::Medium => "medium",
        ReviewSeverity::High => "high",
        ReviewSeverity::Critical => "critical",
    }
}

const fn review_finding_status_label(status: ReviewFindingStatus) -> &'static str {
    match status {
        ReviewFindingStatus::Open => "open",
        ReviewFindingStatus::Accepted => "accepted",
        ReviewFindingStatus::Rejected => "rejected",
        ReviewFindingStatus::Duplicate => "duplicate",
        ReviewFindingStatus::Superseded => "superseded",
        ReviewFindingStatus::Stale => "stale",
        ReviewFindingStatus::Posted => "posted",
        ReviewFindingStatus::Fixed => "fixed",
        ReviewFindingStatus::BlockedWithReason => "blocked_with_reason",
    }
}

const fn dangerous_tool_auto_approval_label(dangerous_tool_auto_approval: bool) -> &'static str {
    if dangerous_tool_auto_approval {
        "approve-all"
    } else {
        "disabled"
    }
}

const fn last_writer_actor_label(last_writer: Option<MetadataLastWriter>) -> &'static str {
    match last_writer {
        Some(last_writer) => match last_writer.actor() {
            MetadataActor::User {} => "user",
            MetadataActor::Core {} => "core",
            MetadataActor::Model { .. } => "model",
            MetadataActor::Recovery {} => "recovery",
            MetadataActor::Tool { .. } => "tool",
        },
        None => "none",
    }
}

fn last_writer_micros_label(last_writer: Option<MetadataLastWriter>) -> String {
    match last_writer {
        Some(last_writer) => last_writer.updated_at_unix_micros().value().to_string(),
        None => String::from("none"),
    }
}

fn control_safe(value: &str, field: TextField) -> String {
    let mut rendered = String::with_capacity(value.len());
    for character in value.chars() {
        let code = character as u32;
        let preserved_line_feed = character == '\n' && field == TextField::Flowing;
        let control = code <= 0x1f || (0x7f..=0x9f).contains(&code);
        // A delimited field escapes the introducer too, so every backslash in
        // its output opens an escape this renderer wrote and the field decodes
        // back to the exact values it was given.
        let delimiter =
            matches!(character, ' ' | ',' | '\\') && field == TextField::DelimitedOnLine;
        if delimiter || (control && !preserved_line_feed) {
            rendered.push_str(&format!("\\u{{{code:x}}}"));
        } else {
            rendered.push(character);
        }
    }
    rendered
}

#[cfg(test)]
mod tests {
    use std::{
        io::{self, Write},
        path::Path,
        str::FromStr,
    };

    use expect_test::expect;
    use rust_decimal::Decimal;
    use signalbox_process_protocol::{
        BillingRateVersion, BoundChildAction, CanonicalDollarAmount, CanonicalU64, CanonicalUuid,
        ContentFragment, CurrentModelCall, CurrentModelCallState, DelegationOutcome,
        DelegationPolicy, DelegationProvenance, DelegationReason, DelegationWaitMode,
        DescendantTerminationScope, ErrorCode, ErrorDetail, FailedModelCallDisposition,
        FailedTerminalModelCall, ImportedContentKind, ImportedSourceSpeaker, ImportedSpeaker,
        ImportedTextPreview, InputContent, MetadataActor, MetadataLastWriter, ModelCallCostLabel,
        ModelCallDollarCost, ModelCallState, ModelCallTokenUsage,
        OperatorStatusLifecycleDeadlineViolationMessage, OperatorStatusLifecycleState,
        OperatorStatusLifecycleWeekMessage, OperatorStatusMessage, ReviewDiffSide,
        ReviewFindingInput, ReviewFindingSnapshot, ReviewFindingStatus, ReviewSeverity,
        ReviewTargetSnapshot, ReviewTargetSubject, RunnerCapabilityClass, RunnerConnectionHealth,
        RunnerCredentialProfileName, RunnerPlacementRevision, RunnerProjection,
        RunnerProjectionSelector, RunnerProjectionState, RunnerRepositoryKey, RunnerSandboxProfile,
        RunnerStateTransitionState, RunnerWorkingDirectory, ServerMessage, SessionEvent,
        ToolApprovalEventDecider, ToolApprovalEventDecision, TranscriptEntry, TranscriptTextEntry,
        TurnState, UsageProvenance, UserInputContent,
    };
    use uuid::Uuid;

    use super::{
        ConversationRow, CostAggregateKey, DiskCostTotals, ImportedEntryRow, Output,
        SessionMetadataRow, SnapshotSelection, TextField, control_safe, last_writer_actor_label,
    };
    use crate::{
        error::ClientError,
        transcript::{SnapshotIdentitySet, TranscriptSnapshot},
    };

    #[test]
    fn review_target_names_an_absent_base_revision() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .review_target(&review_target_snapshot(None))
            .expect("in-memory output cannot fail");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            target=00000000-0000-0000-0000-000000000001 subject=commit parent=-
            provider=example-host
            repository=example/repository
            head_revision=head
            base_revision_present=false
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn review_target_preserves_a_literal_dash_base_revision() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .review_target(&review_target_snapshot(Some(String::from("-"))))
            .expect("in-memory output cannot fail");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            target=00000000-0000-0000-0000-000000000001 subject=commit parent=-
            provider=example-host
            repository=example/repository
            head_revision=head
            base_revision_present=true
            base_revision=-
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn review_finding_renders_its_complete_snapshot() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .review_finding(&review_finding_snapshot())
            .expect("in-memory output cannot fail");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            finding=00000000-0000-0000-0000-000000000004 target=00000000-0000-0000-0000-000000000001 run=00000000-0000-0000-0000-000000000002 pass=00000000-0000-0000-0000-000000000003 status=open events=2 line_start=7 line_end=9 diff_side=right severity=high is_real_confidence=9000 severity_label_confidence=8500
            file_path=src/lib.rs
            title=Retain evidence
            body=First line\u{a}Second line
            category=correctness
            recommended_fix_present=true
            recommended_fix=Bind the exact\u{a}pass.
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn terminal_safe_text_preserves_line_feed_and_escapes_c0_del_and_c1() {
        assert_eq!(
            control_safe("a\n\t\u{1b}\u{7f}\u{85}z", TextField::Flowing),
            "a\n\\u{9}\\u{1b}\\u{7f}\\u{85}z"
        );
        assert_eq!(
            control_safe("café\u{1f980}", TextField::Flowing),
            "café\u{1f980}"
        );
    }

    #[test]
    fn terminal_safe_trailing_field_escapes_line_feed_and_keeps_its_spaces() {
        assert_eq!(
            control_safe("a\n\t\u{1b}\u{7f}\u{85}z", TextField::TrailingOnLine),
            "a\\u{a}\\u{9}\\u{1b}\\u{7f}\\u{85}z"
        );
        assert_eq!(
            control_safe("café, and a space\u{1f980}", TextField::TrailingOnLine),
            "café, and a space\u{1f980}"
        );
    }

    #[test]
    fn provider_text_delta_is_terminal_safe_and_flushed_immediately() {
        let session_id = wire_uuid(1);
        let turn_id = wire_uuid(2);
        let model_call_id = wire_uuid(3);
        let part_index = 4;
        let mut stdout = FlushWriter::default();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .provider_text_delta(
                session_id,
                turn_id,
                model_call_id,
                part_index,
                "first\nforged event\u{1b}",
            )
            .expect("in-memory output cannot fail");

        assert_eq!(
            String::from_utf8(stdout.bytes).expect("rendered output is UTF-8"),
            format!(
                "provider_text_delta session={session_id} turn={turn_id} \
                 call={model_call_id} part={part_index} \
                 content=first\\u{{a}}forged event\\u{{1b}}\n"
            )
        );
        assert_eq!(stdout.flushes, 1);
        assert!(stderr.is_empty());
    }

    #[test]
    fn operator_status_renders_all_sections_and_explains_omitted_usage() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        {
            let mut output = Output::new(&mut stdout, &mut stderr, false);
            output
                .operator_status_counts(super::OperatorStatusPresentationCounts {
                    lifecycle_weeks: 1,
                    lifecycle_deadline_violations: 1,
                })
                .expect("in-memory output cannot fail");
            output
                .operator_status_item(&ServerMessage::OperatorStatus(Box::new(
                    OperatorStatusMessage::LifecycleWeek(Box::new(
                        OperatorStatusLifecycleWeekMessage {
                            week_start_date: String::from("2026-08-31"),
                            completion_failure_numerator: CanonicalU64::new(3),
                            completion_failure_denominator: CanonicalU64::new(40),
                            failed_unknown_count: CanonicalU64::new(1),
                            overflow_numerator: CanonicalU64::new(5),
                            overflow_denominator: CanonicalU64::new(44),
                            finish_given_overflow_numerator: CanonicalU64::new(4),
                            wall_numerator: CanonicalU64::new(0),
                            wall_denominator: CanonicalU64::new(38),
                            wall_occurrence_count: CanonicalU64::new(0),
                            classified_terminal_turn_count: CanonicalU64::new(980),
                            terminal_turn_count: CanonicalU64::new(985),
                            classified_known_failed_call_count: CanonicalU64::new(91),
                            known_failed_call_count: CanonicalU64::new(95),
                        },
                    )),
                )))
                .expect("in-memory output cannot fail");
            output
                .operator_status_item(&ServerMessage::OperatorStatus(Box::new(
                    OperatorStatusMessage::LifecycleDeadlineViolation(Box::new(
                        OperatorStatusLifecycleDeadlineViolationMessage {
                            session_id: wire_uuid(6),
                            state: OperatorStatusLifecycleState::Parked,
                            deadline_missing: false,
                            expired_for_seconds: Some(CanonicalU64::new(90)),
                        },
                    )),
                )))
                .expect("in-memory output cannot fail");
            output
                .operator_status_model_usage_omitted()
                .expect("in-memory output cannot fail");
        }

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            status lifecycle_weeks=1 nonterminal_past_deadline=1
            lifecycle_week week=2026-08-31 completion_failure=3/40@75000ppm failed_unknown=1/40@25000ppm overflow=5/44@113636ppm finish_given_overflow=4/5@800000ppm wall=0/38@0ppm wall_occurrences=0 turn_cause_completeness=980/985@994923ppm model_call_cause_completeness=91/95@957894ppm
            nonterminal_past_deadline session=00000000-0000-0000-0000-000000000006 state=parked deadline=armed expired=1m30s
            model_usage=omitted reason=no_cheap_status_aggregate
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn terminal_safe_delimited_field_escapes_the_space_and_comma_that_delimit_it() {
        assert_eq!(
            control_safe("a\n\t\u{1b}\u{7f}\u{85}z", TextField::DelimitedOnLine),
            "a\\u{a}\\u{9}\\u{1b}\\u{7f}\\u{85}z"
        );
        assert_eq!(
            control_safe("café, and a space\u{1f980}", TextField::DelimitedOnLine),
            "café\\u{2c}\\u{20}and\\u{20}a\\u{20}space\u{1f980}"
        );
    }

    #[test]
    fn terminal_safe_delimited_field_distinguishes_written_escape_text_from_an_escape() {
        let comma = control_safe(",", TextField::DelimitedOnLine);
        let escape_text_for_a_comma = control_safe("\\u{2c}", TextField::DelimitedOnLine);

        assert_eq!(comma, "\\u{2c}");
        assert_eq!(escape_text_for_a_comma, "\\u{5c}u{2c}");
        assert_ne!(comma, escape_text_for_a_comma);
    }

    #[test]
    fn scan_failure_reason_cannot_forge_an_outcome_line() {
        let error = ClientError::remote(
            ErrorCode::Unavailable,
            String::from("first line\nscan_summary imported=99 already_imported=99 skipped=0"),
            ErrorDetail::none(),
        );
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .conversation_import_scan_skipped(Path::new("conversation.jsonl"), &error)
            .expect("in-memory output cannot fail");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            skipped path="conversation.jsonl" reason=unavailable: first line\u{a}scan_summary imported=99 already_imported=99 skipped=0
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn imported_renders_a_previewed_attested_text_entry() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .imported_conversation_entry(&ImportedEntryRow {
                position: 2,
                imported_entry_id: wire_uuid(7),
                source_speaker: ImportedSourceSpeaker::Attested {
                    speaker: ImportedSpeaker::Assistant,
                },
                content_kind: ImportedContentKind::Text,
                text_preview: Some(&ImportedTextPreview::of_exact_text("synthetic answer")),
            })
            .expect("in-memory output cannot fail");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            position=2 imported_entry=00000000-0000-0000-0000-000000000007 speaker=assistant kind=text truncated=false text=synthetic answer
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn imported_renders_a_nontext_entry_without_preview_fields() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .imported_conversation_entry(&ImportedEntryRow {
                position: 1,
                imported_entry_id: wire_uuid(7),
                source_speaker: ImportedSourceSpeaker::NotAttested {},
                content_kind: ImportedContentKind::SourceEvent,
                text_preview: None,
            })
            .expect("in-memory output cannot fail");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            position=1 imported_entry=00000000-0000-0000-0000-000000000007 speaker=unattested kind=source_event
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn imported_preview_text_cannot_forge_another_entry_row() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .imported_conversation_entry(&ImportedEntryRow {
                position: 3,
                imported_entry_id: wire_uuid(7),
                source_speaker: ImportedSourceSpeaker::Attested {
                    speaker: ImportedSpeaker::User,
                },
                content_kind: ImportedContentKind::Text,
                text_preview: Some(&ImportedTextPreview::of_exact_text(
                    "forged\nposition=9 imported_entry=00000000-0000-0000-0000-000000000008",
                )),
            })
            .expect("in-memory output cannot fail");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            position=3 imported_entry=00000000-0000-0000-0000-000000000007 speaker=user kind=text truncated=false text=forged\u{a}position=9 imported_entry=00000000-0000-0000-0000-000000000008
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn imported_names_its_entry_count_as_the_greatest_selectable_position() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .imported_conversation_entry_count(2)
            .expect("in-memory output cannot fail");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            entry_count=2
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn continue_prints_the_resolved_latest_position_before_its_command() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .resolved_through_position(2)
            .expect("in-memory output cannot fail");

        assert!(stdout.is_empty());
        expect![[r#"
            through_position=2
        "#]]
        .assert_eq(&String::from_utf8(stderr).expect("rendered output is UTF-8"));
    }

    #[test]
    fn search_renders_one_complete_written_metadata_row() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .session_metadata_summary(&SessionMetadataRow {
                session_id: wire_uuid(1),
                defaults_version: 2,
                selection: "model=00000000-0000-0000-0000-000000000003",
                dangerous_tool_auto_approval: true,
                archived: true,
                last_writer: Some(MetadataLastWriter::new(
                    CanonicalU64::new(1_753_484_400_000_000),
                    MetadataActor::User {},
                )),
                tags: &[String::from("daily"), String::from("plan")],
                title: Some("Active plan"),
            })
            .expect("in-memory output cannot fail");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            00000000-0000-0000-0000-000000000001 archived=true defaults_version=2 model=00000000-0000-0000-0000-000000000003 dangerous_tool_auto_approval=approve-all last_writer=user updated_at_unix_micros=1753484400000000 tags=daily,plan title=Active plan
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    /// One written last-writer stamp carrying the actor under test; the
    /// timestamp is fixture plumbing the label never reads.
    fn written_by(actor: MetadataActor) -> Option<MetadataLastWriter> {
        Some(MetadataLastWriter::new(CanonicalU64::new(1), actor))
    }

    #[test]
    fn search_names_every_last_writer_actor_the_wire_can_carry() {
        assert_eq!(
            last_writer_actor_label(written_by(MetadataActor::User {})),
            "user"
        );
        assert_eq!(
            last_writer_actor_label(written_by(MetadataActor::Model {
                turn_id: wire_uuid(2)
            })),
            "model"
        );
        assert_eq!(
            last_writer_actor_label(written_by(MetadataActor::Recovery {})),
            "recovery"
        );
        assert_eq!(
            last_writer_actor_label(written_by(MetadataActor::Tool {
                tool_request_id: wire_uuid(3)
            })),
            "tool"
        );
        assert_eq!(last_writer_actor_label(None), "none");
    }

    #[test]
    fn search_renders_the_unwritten_metadata_row_with_named_absences() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .session_metadata_summary(&SessionMetadataRow {
                session_id: wire_uuid(1),
                defaults_version: 1,
                selection: "alias=00000000-0000-0000-0000-000000000002",
                dangerous_tool_auto_approval: false,
                archived: false,
                last_writer: None,
                tags: &[],
                title: None,
            })
            .expect("in-memory output cannot fail");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            00000000-0000-0000-0000-000000000001 archived=false defaults_version=1 alias=00000000-0000-0000-0000-000000000002 dangerous_tool_auto_approval=disabled last_writer=none updated_at_unix_micros=none tags= title=
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn search_title_and_tags_cannot_forge_another_row() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .session_metadata_summary(&SessionMetadataRow {
                session_id: wire_uuid(1),
                defaults_version: 1,
                selection: "model=00000000-0000-0000-0000-000000000003",
                dangerous_tool_auto_approval: false,
                archived: false,
                last_writer: None,
                tags: &[String::from("first\nsecond")],
                title: Some("forged\n00000000-0000-0000-0000-000000000002 archived=false"),
            })
            .expect("in-memory output cannot fail");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            00000000-0000-0000-0000-000000000001 archived=false defaults_version=1 model=00000000-0000-0000-0000-000000000003 dangerous_tool_auto_approval=disabled last_writer=none updated_at_unix_micros=none tags=first\u{a}second title=forged\u{a}00000000-0000-0000-0000-000000000002 archived=false
        "#]]
        .assert_eq(&rendered);
        assert_eq!(rendered.lines().count(), 1);
        assert!(stderr.is_empty());
    }

    #[test]
    fn conversations_render_origin_tagged_native_and_imported_rows() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);
        output
            .conversation_summary(&ConversationRow::Native {
                session_id: wire_uuid(1),
                archived: true,
                defaults_version: 2,
                title: Some("Active plan"),
            })
            .expect("in-memory output cannot fail");
        output
            .conversation_summary(&ConversationRow::Imported {
                imported_conversation_id: wire_uuid(2),
                format: "codex-rollout-jsonl-v1",
                entry_count: 7,
                title: None,
            })
            .expect("in-memory output cannot fail");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            origin=native session_id=00000000-0000-0000-0000-000000000001 archived=true defaults_version=2 title=Active plan
            origin=imported imported_conversation_id=00000000-0000-0000-0000-000000000002 format=codex-rollout-jsonl-v1 entry_count=7 title=
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn conversation_title_cannot_forge_another_row() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .conversation_summary(&ConversationRow::Imported {
                imported_conversation_id: wire_uuid(1),
                format: "claude-code-session-jsonl-v2",
                entry_count: 1,
                title: Some(
                    "forged\norigin=native session_id=00000000-0000-0000-0000-000000000002",
                ),
            })
            .expect("in-memory output cannot fail");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            origin=imported imported_conversation_id=00000000-0000-0000-0000-000000000001 format=claude-code-session-jsonl-v2 entry_count=1 title=forged\u{a}origin=native session_id=00000000-0000-0000-0000-000000000002
        "#]]
        .assert_eq(&rendered);
        assert_eq!(rendered.lines().count(), 1);
        assert!(stderr.is_empty());
    }

    #[test]
    fn conversation_cursor_is_printed_to_standard_error() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .next_conversation_cursor("imported", wire_uuid(3))
            .expect("in-memory output cannot fail");

        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).expect("rendered output is UTF-8"),
            "next_after=imported:00000000-0000-0000-0000-000000000003\n"
        );
    }

    #[test]
    fn search_tags_state_their_exact_boundaries() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .session_metadata_summary(&SessionMetadataRow {
                session_id: wire_uuid(1),
                defaults_version: 1,
                selection: "model=00000000-0000-0000-0000-000000000003",
                dangerous_tool_auto_approval: false,
                archived: false,
                last_writer: None,
                tags: &[String::from("one,tag title=forged"), String::from("second")],
                title: Some("Active plan"),
            })
            .expect("in-memory output cannot fail");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            00000000-0000-0000-0000-000000000001 archived=false defaults_version=1 model=00000000-0000-0000-0000-000000000003 dangerous_tool_auto_approval=disabled last_writer=none updated_at_unix_micros=none tags=one\u{2c}tag\u{20}title=forged,second title=Active plan
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn search_prints_its_continuation_cursor_to_standard_error() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .next_page_cursor(wire_uuid(1))
            .expect("in-memory output cannot fail");

        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).expect("rendered output is UTF-8"),
            "next_after_session_id=00000000-0000-0000-0000-000000000001\n"
        );
    }

    #[test]
    fn raw_assistant_text_flushes_without_adding_a_delimiter() {
        let mut stdout = FlushWriter::default();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, true);
        output
            .assistant_text_fragment("ok", true, false)
            .expect("in-memory output cannot fail");
        assert_eq!(stdout.bytes, b"ok");
        assert_eq!(stdout.flushes, 1);
        assert!(stderr.is_empty());
    }

    #[test]
    fn followed_snapshot_renders_queued_content_before_adopting_its_cursor() {
        let turn_id = wire_uuid(1);
        let accepted_input_id = wire_uuid(2);
        let mut snapshot = TranscriptSnapshot::from_messages(
            9,
            [ServerMessage::TranscriptTurn {
                turn_id,
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state: TurnState::Queued {
                    accepted_input_id,
                    content: UserInputContent::text("queued user text".to_owned()),
                },
            }],
        )
        .expect("test snapshot must spool");
        let mut displayed = SnapshotIdentitySet::new().expect("identity spool must open");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .followed_snapshot(&mut snapshot, &mut displayed)
            .expect("queued snapshot must render");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        assert!(rendered.contains("state=queued"));
        assert!(rendered.contains("queued user text"));
        assert!(stderr.is_empty());
    }

    #[test]
    fn session_summary_renders_its_complete_runner_projection() {
        let projection = RunnerProjection::try_new(
            RunnerProjectionSelector::CapabilityClass {
                name: RunnerCapabilityClass::try_new(String::from("linux.workspace"))
                    .expect("the fixture capability class is valid"),
            },
            Some(wire_uuid(2)),
            RunnerPlacementRevision::try_new(3)
                .expect("the fixture placement revision is positive"),
            RunnerSandboxProfile::WorkspaceRestricted,
            Some(
                RunnerCredentialProfileName::try_new(String::from("readonly"))
                    .expect("the fixture credential profile is valid"),
            ),
            Some(
                RunnerRepositoryKey::try_new(String::from("signalbox"))
                    .expect("the fixture repository key is valid"),
            ),
            Some(
                RunnerWorkingDirectory::try_new(String::from("workspace root\nproject"))
                    .expect("the fixture working directory is valid"),
            ),
            Some(RunnerConnectionHealth::Suspect),
            RunnerProjectionState::Pinned,
        )
        .expect("the fixture projection is coherent");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        Output::new(&mut stdout, &mut stderr, false)
            .session_summary(
                wire_uuid(1),
                4,
                "model=alias alias=fast",
                2,
                "placement=pathless",
                Some(&projection),
            )
            .expect("in-memory output cannot fail");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            00000000-0000-0000-0000-000000000001 defaults_version=4 model=alias alias=fast placement_version=2 placement=pathless runner_selector=capability_class runner_selector_capability=linux.workspace runner=00000000-0000-0000-0000-000000000002 runner_placement_revision=3 runner_sandbox=workspace_restricted runner_credential_profile=readonly runner_repository=signalbox runner_working_directory=workspace\u{20}root\u{a}project runner_connection_health=suspect runner_state=pinned
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn followed_snapshot_renders_its_complete_authoritative_runner_projection() {
        let projection = RunnerProjection::try_new(
            RunnerProjectionSelector::CapabilityClass {
                name: RunnerCapabilityClass::try_new(String::from("linux.workspace"))
                    .expect("the fixture capability class is valid"),
            },
            Some(wire_uuid(2)),
            RunnerPlacementRevision::try_new(3)
                .expect("the fixture placement revision is positive"),
            RunnerSandboxProfile::WorkspaceRestricted,
            Some(
                RunnerCredentialProfileName::try_new(String::from("readonly"))
                    .expect("the fixture credential profile is valid"),
            ),
            Some(
                RunnerRepositoryKey::try_new(String::from("signalbox"))
                    .expect("the fixture repository key is valid"),
            ),
            Some(
                RunnerWorkingDirectory::try_new(String::from("workspace root\nproject"))
                    .expect("the fixture working directory is valid"),
            ),
            Some(RunnerConnectionHealth::Suspect),
            RunnerProjectionState::Pinned,
        )
        .expect("the fixture projection is coherent");
        let mut snapshot = TranscriptSnapshot::from_messages_with_runner(
            9,
            Some(projection),
            std::iter::empty::<ServerMessage>(),
        )
        .expect("test snapshot must spool");
        let mut displayed = SnapshotIdentitySet::new().expect("identity spool must open");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .followed_snapshot(&mut snapshot, &mut displayed)
            .expect("runner snapshot must render");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            runner_snapshot selector=capability_class selector_capability=linux.workspace runner=00000000-0000-0000-0000-000000000002 placement_revision=3 sandbox=workspace_restricted credential_profile=readonly repository=signalbox working_directory=workspace\u{20}root\u{a}project connection_health=suspect state=pinned
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn s28_imported_snapshot_renders_attested_text() {
        let mut snapshot = TranscriptSnapshot::from_messages(
            9,
            [
                ServerMessage::TranscriptTextEntry {
                    entry_index: CanonicalU64::new(0),
                    source_session_id: wire_uuid(1),
                    entry_id: wire_uuid(2),
                    entry: TranscriptTextEntry::Imported {
                        imported_conversation_id: wire_uuid(3),
                        imported_entry_id: wire_uuid(4),
                        source_speaker: ImportedSourceSpeaker::Attested {
                            speaker: ImportedSpeaker::User,
                        },
                    },
                },
                ServerMessage::TranscriptContent {
                    entry_index: CanonicalU64::new(0),
                    fragment_index: CanonicalU64::new(0),
                    final_fragment: true,
                    content_fragment: ContentFragment::try_new("exact imported text".to_owned())
                        .expect("short content is valid"),
                },
            ],
        )
        .expect("test snapshot must spool");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .snapshot(&mut snapshot)
            .expect("imported snapshot must render");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            imported_user imported_conversation=00000000-0000-0000-0000-000000000003 imported_entry=00000000-0000-0000-0000-000000000004 source=00000000-0000-0000-0000-000000000001 entry=00000000-0000-0000-0000-000000000002
            exact imported text
            usage_total scope=session usage_provenance=reported terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
            usage_total scope=session usage_provenance=estimated terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn snapshot_user_entry_renders_canonical_parts_on_one_line() {
        let mut snapshot = TranscriptSnapshot::from_messages(
            9,
            [ServerMessage::TranscriptUserEntry {
                entry_index: CanonicalU64::new(0),
                source_session_id: wire_uuid(1),
                entry_id: wire_uuid(2),
                accepted_input_id: wire_uuid(3),
                turn_id: wire_uuid(4),
                content: UserInputContent::text("first\nsecond".to_owned()),
            }],
        )
        .expect("test snapshot must spool");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .snapshot(&mut snapshot)
            .expect("user snapshot must render");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        assert!(rendered.starts_with(
            "user_content source_session=00000000-0000-0000-0000-000000000001 entry=00000000-0000-0000-0000-000000000002 accepted_input=00000000-0000-0000-0000-000000000003 turn=00000000-0000-0000-0000-000000000004 parts=[{\"type\":\"text\",\"text\":\"first\\nsecond\"}]\n"
        ));
        assert_eq!(
            rendered
                .lines()
                .filter(|line| line.starts_with("user_content "))
                .count(),
            1
        );
        assert!(stderr.is_empty());
    }

    #[test]
    fn s28_imported_snapshot_renders_conservative_nontext() {
        let mut snapshot = TranscriptSnapshot::from_messages(
            9,
            [ServerMessage::TranscriptEntry {
                entry_index: CanonicalU64::new(0),
                source_session_id: wire_uuid(1),
                entry_id: wire_uuid(5),
                entry: TranscriptEntry::Imported {
                    imported_conversation_id: wire_uuid(3),
                    imported_entry_id: wire_uuid(6),
                    source_speaker: ImportedSourceSpeaker::NotAttested {},
                    content_kind: ImportedContentKind::ToolCall,
                },
            }],
        )
        .expect("test snapshot must spool");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .snapshot(&mut snapshot)
            .expect("imported snapshot must render");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            imported_speaker_unattested kind=tool_call imported_conversation=00000000-0000-0000-0000-000000000003 imported_entry=00000000-0000-0000-0000-000000000006 source=00000000-0000-0000-0000-000000000001 entry=00000000-0000-0000-0000-000000000005
            usage_total scope=session usage_provenance=reported terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
            usage_total scope=session usage_provenance=estimated terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn delegation_snapshot_renders_task_message_and_background_result() {
        let mut snapshot = TranscriptSnapshot::from_messages(
            9,
            [
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(0),
                    source_session_id: wire_uuid(2),
                    entry_id: wire_uuid(3),
                    entry: TranscriptEntry::DelegatedTask {
                        spawning_request_id: wire_uuid(4),
                        parent_session_id: wire_uuid(1),
                        parent_turn_id: wire_uuid(5),
                        content: String::from("inspect the durable result"),
                    },
                },
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(1),
                    source_session_id: wire_uuid(2),
                    entry_id: wire_uuid(6),
                    entry: TranscriptEntry::DelegationMessage {
                        spawning_request_id: wire_uuid(4),
                        message_id: wire_uuid(7),
                        sender_session_id: wire_uuid(1),
                        recipient_session_id: wire_uuid(2),
                        ordinal: CanonicalU64::new(2),
                        delivery_sequence: CanonicalU64::new(1),
                        content: String::from("continue with the checked input"),
                    },
                },
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(2),
                    source_session_id: wire_uuid(1),
                    entry_id: wire_uuid(8),
                    entry: TranscriptEntry::DelegationResult {
                        await_request_id: wire_uuid(9),
                        spawning_request_id: wire_uuid(4),
                        child_session_id: wire_uuid(2),
                        mode: DelegationWaitMode::Background,
                        delivery_sequence: Some(CanonicalU64::new(2)),
                        outcome: DelegationOutcome::Returned,
                        content: Some(String::from("checked result")),
                        reason: DelegationReason::ChildCompleted,
                        provenance: DelegationProvenance::ChildTurn {
                            child_session_id: wire_uuid(2),
                            child_turn_id: wire_uuid(10),
                        },
                    },
                },
            ],
        )
        .expect("test snapshot must spool");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .snapshot(&mut snapshot)
            .expect("delegation snapshot must render");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            delegated_task spawning_request=00000000-0000-0000-0000-000000000004 parent_session=00000000-0000-0000-0000-000000000001 parent_turn=00000000-0000-0000-0000-000000000005 content=inspect the durable result source=00000000-0000-0000-0000-000000000002 entry=00000000-0000-0000-0000-000000000003
            delegation_message spawning_request=00000000-0000-0000-0000-000000000004 message=00000000-0000-0000-0000-000000000007 sender=00000000-0000-0000-0000-000000000001 recipient=00000000-0000-0000-0000-000000000002 ordinal=2 delivery_sequence=1 content=continue with the checked input source=00000000-0000-0000-0000-000000000002 entry=00000000-0000-0000-0000-000000000006
            delegation_result await_request=00000000-0000-0000-0000-000000000009 spawning_request=00000000-0000-0000-0000-000000000004 child=00000000-0000-0000-0000-000000000002 mode=background delivery_sequence=2 outcome=returned content=checked result reason=child_completed provenance=child_turn:00000000-0000-0000-0000-000000000002:00000000-0000-0000-0000-00000000000a source=00000000-0000-0000-0000-000000000001 entry=00000000-0000-0000-0000-000000000008
            usage_total scope=session usage_provenance=reported terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
            usage_total scope=session usage_provenance=estimated terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn terminal_reread_excludes_material_from_later_buffered_events() {
        let selected_turn = wire_uuid(1);
        let selected_call = wire_uuid(2);
        let later_turn = wire_uuid(3);
        let later_call = wire_uuid(4);
        let mut snapshot = TranscriptSnapshot::from_messages(
            12,
            [
                ServerMessage::TranscriptTextEntry {
                    entry_index: CanonicalU64::new(0),
                    source_session_id: wire_uuid(10),
                    entry_id: wire_uuid(11),
                    entry: TranscriptTextEntry::Assistant {
                        turn_id: selected_turn,
                        model_call_id: selected_call,
                    },
                },
                ServerMessage::TranscriptContent {
                    entry_index: CanonicalU64::new(0),
                    fragment_index: CanonicalU64::new(0),
                    final_fragment: true,
                    content_fragment: ContentFragment::try_new("selected reply".to_owned())
                        .expect("short content is valid"),
                },
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(1),
                    source_session_id: wire_uuid(10),
                    entry_id: wire_uuid(12),
                    entry: TranscriptEntry::TurnCompleted {
                        turn_id: selected_turn,
                    },
                },
                ServerMessage::TranscriptTextEntry {
                    entry_index: CanonicalU64::new(2),
                    source_session_id: wire_uuid(10),
                    entry_id: wire_uuid(13),
                    entry: TranscriptTextEntry::Assistant {
                        turn_id: later_turn,
                        model_call_id: later_call,
                    },
                },
                ServerMessage::TranscriptContent {
                    entry_index: CanonicalU64::new(2),
                    fragment_index: CanonicalU64::new(0),
                    final_fragment: true,
                    content_fragment: ContentFragment::try_new("later reply".to_owned())
                        .expect("short content is valid"),
                },
            ],
        )
        .expect("test snapshot must spool");
        let mut displayed = SnapshotIdentitySet::new().expect("identity spool must open");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .terminal_material(
                &mut snapshot,
                &mut displayed,
                SnapshotSelection::Completed {
                    turn_id: selected_turn,
                    model_call_id: selected_call,
                    terminal_entry_id: wire_uuid(12),
                },
            )
            .expect("selected terminal material must render");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        assert!(rendered.contains("selected reply"));
        assert!(rendered.contains("turn_completed"));
        assert!(!rendered.contains("later reply"));
        assert!(!rendered.contains(&later_turn.to_string()));
        assert!(stderr.is_empty());
    }

    #[test]
    fn tool_reconciliation_reread_uses_its_terminal_turn_batch() {
        let selected_turn = wire_uuid(1);
        let selected_call = wire_uuid(2);
        let selected_request = wire_uuid(3);
        let selected_attempt = wire_uuid(4);
        let selected_frontier = wire_uuid(5);
        let later_turn = wire_uuid(6);
        let later_call = wire_uuid(7);
        let later_request = wire_uuid(8);
        let mut snapshot = TranscriptSnapshot::from_messages(
            12,
            [
                ServerMessage::TranscriptTurn {
                    turn_id: selected_turn,
                    acceptance_position: CanonicalU64::new(1),
                    model_settings: None,
                    state: TurnState::ToolReconciliationRequired {
                        terminal_frontier_id: selected_frontier,
                        terminal_attempt_id: wire_uuid(9),
                        terminal_tool_attempt_id: selected_attempt,
                    },
                },
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(0),
                    source_session_id: wire_uuid(10),
                    entry_id: wire_uuid(11),
                    entry: TranscriptEntry::AssistantToolUse {
                        turn_id: selected_turn,
                        model_call_id: selected_call,
                        tool_request_id: selected_request,
                        tool_name: String::from("selected"),
                        arguments: String::from("{}"),
                        approval: None,
                    },
                },
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(1),
                    source_session_id: wire_uuid(10),
                    entry_id: wire_uuid(12),
                    entry: TranscriptEntry::ToolClosed {
                        tool_request_id: selected_request,
                        content: String::from("selected result"),
                    },
                },
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(2),
                    source_session_id: wire_uuid(10),
                    entry_id: wire_uuid(13),
                    entry: TranscriptEntry::AssistantToolUse {
                        turn_id: later_turn,
                        model_call_id: later_call,
                        tool_request_id: later_request,
                        tool_name: String::from("later"),
                        arguments: String::from("{}"),
                        approval: None,
                    },
                },
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(3),
                    source_session_id: wire_uuid(10),
                    entry_id: wire_uuid(14),
                    entry: TranscriptEntry::ToolClosed {
                        tool_request_id: later_request,
                        content: String::from("later result"),
                    },
                },
            ],
        )
        .expect("test snapshot must spool");
        let mut displayed = SnapshotIdentitySet::new().expect("identity spool must open");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .terminal_material(
                &mut snapshot,
                &mut displayed,
                SnapshotSelection::ToolReconciliation {
                    turn_id: selected_turn,
                    tool_attempt_id: selected_attempt,
                    terminal_frontier_id: selected_frontier,
                },
            )
            .expect("the exact terminal tool batch renders");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        assert!(rendered.contains("selected result"));
        assert!(!rendered.contains("later result"));
        assert!(stderr.is_empty());
    }

    #[test]
    fn terminal_reread_rejects_a_missing_exact_marker_before_output() {
        let selected_turn = wire_uuid(1);
        let selected_call = wire_uuid(2);
        let mut snapshot = TranscriptSnapshot::from_messages(
            12,
            [
                ServerMessage::TranscriptTextEntry {
                    entry_index: CanonicalU64::new(0),
                    source_session_id: wire_uuid(10),
                    entry_id: wire_uuid(11),
                    entry: TranscriptTextEntry::Assistant {
                        turn_id: selected_turn,
                        model_call_id: selected_call,
                    },
                },
                ServerMessage::TranscriptContent {
                    entry_index: CanonicalU64::new(0),
                    fragment_index: CanonicalU64::new(0),
                    final_fragment: true,
                    content_fragment: ContentFragment::try_new("untrusted reply".to_owned())
                        .expect("short content is valid"),
                },
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(1),
                    source_session_id: wire_uuid(10),
                    entry_id: wire_uuid(12),
                    entry: TranscriptEntry::TurnCompleted {
                        turn_id: selected_turn,
                    },
                },
            ],
        )
        .expect("test snapshot must spool");
        let mut displayed = SnapshotIdentitySet::new().expect("identity spool must open");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let error = Output::new(&mut stdout, &mut stderr, false)
            .terminal_material(
                &mut snapshot,
                &mut displayed,
                SnapshotSelection::Completed {
                    turn_id: selected_turn,
                    model_call_id: selected_call,
                    terminal_entry_id: wire_uuid(13),
                },
            )
            .expect_err("a side reread without the event marker must fail closed");

        assert!(matches!(
            error,
            ClientError::Protocol("terminal reread omitted the event's exact marker")
        ));
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
    }

    #[test]
    fn failed_terminal_reread_rejects_a_different_marker_identity() {
        let selected_turn = wire_uuid(1);
        let mut snapshot = TranscriptSnapshot::from_messages(
            12,
            [ServerMessage::TranscriptEntry {
                entry_index: CanonicalU64::new(0),
                source_session_id: wire_uuid(10),
                entry_id: wire_uuid(11),
                entry: TranscriptEntry::TurnFailed {
                    turn_id: selected_turn,
                },
            }],
        )
        .expect("test snapshot must spool");
        let mut displayed = SnapshotIdentitySet::new().expect("identity spool must open");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let error = Output::new(&mut stdout, &mut stderr, false)
            .terminal_material(
                &mut snapshot,
                &mut displayed,
                SnapshotSelection::Failed {
                    turn_id: selected_turn,
                    terminal_entry_id: wire_uuid(12),
                },
            )
            .expect_err("a failed reread must require the event marker");

        assert!(matches!(
            error,
            ClientError::Protocol("terminal reread omitted the event's exact marker")
        ));
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
    }

    #[test]
    fn snapshot_renders_cancellation_requested_call() {
        let rendered = render_snapshot_turn(TurnState::ActiveRunning {
            current_attempt_id: wire_uuid(2),
            current_model_call: Some(CurrentModelCall::new(
                wire_uuid(3),
                CurrentModelCallState::CancellationRequested {},
            )),
        });

        expect![[r#"
            turn=00000000-0000-0000-0000-000000000001 position=1 state=active_running attempt=00000000-0000-0000-0000-000000000002 call=00000000-0000-0000-0000-000000000003 call_state=cancellation_requested
            usage_total scope=session usage_provenance=reported terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
            usage_total scope=session usage_provenance=estimated terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
        "#]]
        .assert_eq(&rendered);
    }

    #[test]
    fn snapshot_renders_queued_delegated_origin() {
        let rendered = render_snapshot_turn(TurnState::QueuedDelegated {
            spawning_request_id: wire_uuid(2),
            parent_session_id: wire_uuid(3),
            parent_turn_id: wire_uuid(4),
            content: InputContent::new(String::from("delegated task")),
        });

        expect![[r#"
            turn=00000000-0000-0000-0000-000000000001 position=1 state=queued_delegated spawning_request=00000000-0000-0000-0000-000000000002 parent_session=00000000-0000-0000-0000-000000000003 parent_turn=00000000-0000-0000-0000-000000000004
            delegated task
            usage_total scope=session usage_provenance=reported terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
            usage_total scope=session usage_provenance=estimated terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
        "#]]
        .assert_eq(&rendered);
    }

    #[test]
    fn snapshot_renders_queued_delegation_wake_range() {
        let rendered = render_snapshot_turn(TurnState::QueuedDelegationWake {
            first_delivery_sequence: CanonicalU64::new(3),
            through_delivery_sequence: CanonicalU64::new(5),
        });

        expect![[r#"
            turn=00000000-0000-0000-0000-000000000001 position=1 state=queued_delegation_wake deliveries=3-5
            usage_total scope=session usage_provenance=reported terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
            usage_total scope=session usage_provenance=estimated terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
        "#]]
        .assert_eq(&rendered);
    }

    #[test]
    fn snapshot_renders_failed_call_evidence() {
        let rendered = render_snapshot_turn(TurnState::Failed {
            terminal_frontier_id: wire_uuid(2),
            terminal_attempt_id: Some(wire_uuid(3)),
            terminal_model_call: Some(FailedTerminalModelCall::new(
                wire_uuid(4),
                FailedModelCallDisposition::Cancelled,
            )),
        });

        expect![[r#"
            turn=00000000-0000-0000-0000-000000000001 position=1 state=failed frontier=00000000-0000-0000-0000-000000000002 attempt=00000000-0000-0000-0000-000000000003 call=00000000-0000-0000-0000-000000000004 call_disposition=cancelled call_cause=none
            usage_total scope=session usage_provenance=reported terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
            usage_total scope=session usage_provenance=estimated terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
        "#]]
        .assert_eq(&rendered);
    }

    #[test]
    fn snapshot_renders_cancelled_turn() {
        let rendered = render_snapshot_turn(TurnState::Cancelled {
            terminal_frontier_id: wire_uuid(2),
            terminal_attempt_id: wire_uuid(3),
            terminal_model_call_id: None,
        });

        expect![[r#"
            turn=00000000-0000-0000-0000-000000000001 position=1 state=cancelled frontier=00000000-0000-0000-0000-000000000002 attempt=00000000-0000-0000-0000-000000000003 call=none
            usage_total scope=session usage_provenance=reported terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
            usage_total scope=session usage_provenance=estimated terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
        "#]]
        .assert_eq(&rendered);
    }

    #[test]
    fn snapshot_renders_reconciliation_required_turn() {
        let rendered = render_snapshot_turn(TurnState::ReconciliationRequired {
            terminal_frontier_id: wire_uuid(2),
            terminal_attempt_id: wire_uuid(3),
            terminal_model_call_id: wire_uuid(4),
        });

        expect![[r#"
            turn=00000000-0000-0000-0000-000000000001 position=1 state=reconciliation_required frontier=00000000-0000-0000-0000-000000000002 attempt=00000000-0000-0000-0000-000000000003 operation=model_call operation_id=00000000-0000-0000-0000-000000000004
            usage_total scope=session usage_provenance=reported terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
            usage_total scope=session usage_provenance=estimated terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
        "#]]
        .assert_eq(&rendered);
    }

    #[test]
    fn transcript_without_terminal_calls_renders_a_session_usage_total() {
        let mut snapshot =
            TranscriptSnapshot::from_messages(1, std::iter::empty::<ServerMessage>())
                .expect("test snapshot must spool");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .snapshot(&mut snapshot)
            .expect("empty usage snapshot must render");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            usage_total scope=session usage_provenance=reported terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
            usage_total scope=session usage_provenance=estimated terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn transcript_usage_preserves_zero_absence_and_partial_coverage() {
        let first_turn = wire_uuid(1);
        let second_turn = wire_uuid(2);
        let mut snapshot = TranscriptSnapshot::from_messages(
            1,
            [
                ServerMessage::TranscriptModelCallUsage {
                    model_call_index: CanonicalU64::new(0),
                    turn_id: first_turn,
                    model_call_id: wire_uuid(11),
                    usage_provenance: UsageProvenance::Reported,
                    usage: ModelCallTokenUsage {
                        input_tokens: Some(CanonicalU64::new(10)),
                        output_tokens: Some(CanonicalU64::new(0)),
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: Some(CanonicalU64::new(4)),
                    },
                    cost: None,
                },
                ServerMessage::TranscriptModelCallUsage {
                    model_call_index: CanonicalU64::new(1),
                    turn_id: first_turn,
                    model_call_id: wire_uuid(12),
                    usage_provenance: UsageProvenance::Reported,
                    usage: ModelCallTokenUsage {
                        input_tokens: None,
                        output_tokens: None,
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: None,
                    },
                    cost: None,
                },
                ServerMessage::TranscriptModelCallUsage {
                    model_call_index: CanonicalU64::new(2),
                    turn_id: second_turn,
                    model_call_id: wire_uuid(13),
                    usage_provenance: UsageProvenance::Reported,
                    usage: ModelCallTokenUsage {
                        input_tokens: None,
                        output_tokens: None,
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: None,
                    },
                    cost: None,
                },
            ],
        )
        .expect("test snapshot must spool");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .snapshot(&mut snapshot)
            .expect("usage snapshot must render");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            usage turn=00000000-0000-0000-0000-000000000001 usage_provenance=reported terminal_calls=2 input_tokens=10 input_tokens_present_calls=1/2 output_tokens=0 output_tokens_present_calls=1/2 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/2 cache_read_input_tokens=4 cache_read_input_tokens_present_calls=1/2
            usage turn=00000000-0000-0000-0000-000000000001 usage_provenance=estimated terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
            usage turn=00000000-0000-0000-0000-000000000002 usage_provenance=reported terminal_calls=1 input_tokens=unreported input_tokens_present_calls=0/1 output_tokens=unreported output_tokens_present_calls=0/1 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/1 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/1
            usage turn=00000000-0000-0000-0000-000000000002 usage_provenance=estimated terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
            usage_total scope=session usage_provenance=reported terminal_calls=3 input_tokens=10 input_tokens_present_calls=1/3 output_tokens=0 output_tokens_present_calls=1/3 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/3 cache_read_input_tokens=4 cache_read_input_tokens_present_calls=1/3
            usage_total scope=session usage_provenance=estimated terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn transcript_costs_aggregate_only_with_matching_provenance_and_labels() {
        let turn = wire_uuid(1);
        let usage = ModelCallTokenUsage {
            input_tokens: Some(CanonicalU64::new(0)),
            output_tokens: None,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        };
        let mut snapshot = TranscriptSnapshot::from_messages(
            1,
            [
                ServerMessage::TranscriptModelCallUsage {
                    model_call_index: CanonicalU64::new(0),
                    turn_id: turn,
                    model_call_id: wire_uuid(11),
                    usage_provenance: UsageProvenance::Reported,
                    usage,
                    cost: Some(ModelCallDollarCost {
                        amount_usd: CanonicalDollarAmount::try_new(String::from("0.1"))
                            .expect("fixture dollar amount is canonical"),
                        rate_version: BillingRateVersion::try_new(String::from("rates-v1"))
                            .expect("fixture rate version is valid"),
                        label: ModelCallCostLabel::Real,
                    }),
                },
                ServerMessage::TranscriptModelCallUsage {
                    model_call_index: CanonicalU64::new(1),
                    turn_id: turn,
                    model_call_id: wire_uuid(12),
                    usage_provenance: UsageProvenance::Reported,
                    usage,
                    cost: Some(ModelCallDollarCost {
                        amount_usd: CanonicalDollarAmount::try_new(String::from("0.2"))
                            .expect("fixture dollar amount is canonical"),
                        rate_version: BillingRateVersion::try_new(String::from("rates-v1"))
                            .expect("fixture rate version is valid"),
                        label: ModelCallCostLabel::Real,
                    }),
                },
                ServerMessage::TranscriptModelCallUsage {
                    model_call_index: CanonicalU64::new(2),
                    turn_id: turn,
                    model_call_id: wire_uuid(13),
                    usage_provenance: UsageProvenance::Estimated,
                    usage,
                    cost: Some(ModelCallDollarCost {
                        amount_usd: CanonicalDollarAmount::try_new(String::from("0.4"))
                            .expect("fixture dollar amount is canonical"),
                        rate_version: BillingRateVersion::try_new(String::from("rates-v1"))
                            .expect("fixture rate version is valid"),
                        label: ModelCallCostLabel::MeteredEquivalent,
                    }),
                },
            ],
        )
        .expect("test snapshot must spool");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .snapshot(&mut snapshot)
            .expect("cost snapshot must render");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        expect![[r#"
            usage turn=00000000-0000-0000-0000-000000000001 usage_provenance=reported terminal_calls=2 input_tokens=0 input_tokens_present_calls=2/2 output_tokens=unreported output_tokens_present_calls=0/2 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/2 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/2
            usage turn=00000000-0000-0000-0000-000000000001 usage_provenance=estimated terminal_calls=1 input_tokens=0 input_tokens_present_calls=1/1 output_tokens=unreported output_tokens_present_calls=0/1 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/1 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/1
            cost turn=00000000-0000-0000-0000-000000000001 usage_provenance=reported label=real rate_version=rates-v1 usd=0.3 costed_calls=2
            cost turn=00000000-0000-0000-0000-000000000001 usage_provenance=estimated label=metered_equivalent rate_version=rates-v1 usd=0.4 costed_calls=1
            usage_total scope=session usage_provenance=reported terminal_calls=2 input_tokens=0 input_tokens_present_calls=2/2 output_tokens=unreported output_tokens_present_calls=0/2 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/2 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/2
            usage_total scope=session usage_provenance=estimated terminal_calls=1 input_tokens=0 input_tokens_present_calls=1/1 output_tokens=unreported output_tokens_present_calls=0/1 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/1 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/1
            cost_total scope=session usage_provenance=reported label=real rate_version=rates-v1 usd=0.3 costed_calls=2
            cost_total scope=session usage_provenance=estimated label=metered_equivalent rate_version=rates-v1 usd=0.4 costed_calls=1
        "#]]
        .assert_eq(&rendered);
        assert!(stderr.is_empty());
    }

    #[test]
    fn raw_transcript_cost_rate_version_is_unchanged() {
        let rate_version = String::from("rates v1");
        let mut snapshot = TranscriptSnapshot::from_messages(
            1,
            [ServerMessage::TranscriptModelCallUsage {
                model_call_index: CanonicalU64::new(0),
                turn_id: wire_uuid(1),
                model_call_id: wire_uuid(11),
                usage_provenance: UsageProvenance::Reported,
                usage: ModelCallTokenUsage {
                    input_tokens: Some(CanonicalU64::new(0)),
                    output_tokens: None,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                },
                cost: Some(ModelCallDollarCost {
                    amount_usd: CanonicalDollarAmount::try_new(String::from("0.1"))
                        .expect("fixture dollar amount is canonical"),
                    rate_version: BillingRateVersion::try_new(rate_version.clone())
                        .expect("fixture rate version is valid"),
                    label: ModelCallCostLabel::Real,
                }),
            }],
        )
        .expect("test snapshot must spool");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, true)
            .snapshot(&mut snapshot)
            .expect("raw cost snapshot must render");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        assert!(rendered.contains(&format!("rate_version={rate_version} ")));
        assert!(stderr.is_empty());
    }

    #[test]
    fn transcript_cost_totals_grow_on_disk_and_retain_values() {
        let later = CostAggregateKey {
            provenance: UsageProvenance::Reported,
            label: ModelCallCostLabel::Real,
            rate_version: String::from("rates-z"),
        };
        let earlier = CostAggregateKey {
            provenance: UsageProvenance::Reported,
            label: ModelCallCostLabel::Real,
            rate_version: String::from("rates-a"),
        };
        let mut totals = DiskCostTotals::with_capacity(2).expect("the test cost spool must open");
        totals
            .add(&later, Decimal::new(2, 1))
            .expect("the later key must spool");
        totals
            .add(&earlier, Decimal::new(1, 1))
            .expect("the earlier key must grow and spool");

        let later_total = totals
            .get(&later)
            .expect("the cost spool must read")
            .expect("the later key must exist");
        let earlier_total = totals
            .get(&earlier)
            .expect("the cost spool must read")
            .expect("the earlier key must exist");

        assert_eq!(totals.capacity, 4);
        assert_eq!(later_total.amount_usd, Decimal::new(2, 1));
        assert_eq!(later_total.calls, 1);
        assert_eq!(earlier_total.amount_usd, Decimal::new(1, 1));
        assert_eq!(earlier_total.calls, 1);
    }

    #[test]
    fn transcript_cost_totals_reject_inexact_decimal_addition() {
        let key = CostAggregateKey {
            provenance: UsageProvenance::Reported,
            label: ModelCallCostLabel::Real,
            rate_version: String::from("rates-v1"),
        };
        let large = Decimal::from_str("10000000000000000000000000000")
            .expect("fixture dollar amount is representable");
        let tiny = Decimal::from_str("0.0000000000000000000000000001")
            .expect("fixture dollar amount is representable");
        let mut totals = DiskCostTotals::with_capacity(2).expect("the test cost spool must open");
        totals
            .add(&key, large)
            .expect("the first exact amount must spool");

        let error = totals
            .add(&key, tiny)
            .expect_err("an inexact aggregate must be rejected");
        let retained = totals
            .get(&key)
            .expect("the cost spool must read")
            .expect("the original total must remain");

        assert!(matches!(
            error,
            ClientError::Protocol("dollar cost total was inexact")
        ));
        assert_eq!(retained.amount_usd, large);
        assert_eq!(retained.calls, 1);
    }

    #[test]
    fn transcript_is_not_partially_published_when_a_later_total_is_inexact() {
        let large = Decimal::from_str("10000000000000000000000000000")
            .expect("fixture dollar amount is representable");
        let tiny = Decimal::from_str("0.0000000000000000000000000001")
            .expect("fixture dollar amount is representable");
        let rate_version = BillingRateVersion::try_new(String::from("rates-v1"))
            .expect("fixture rate version is valid");
        let usage = ModelCallTokenUsage {
            input_tokens: Some(CanonicalU64::new(0)),
            output_tokens: None,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        };
        let mut snapshot = TranscriptSnapshot::from_messages(
            1,
            [
                ServerMessage::TranscriptTurn {
                    turn_id: wire_uuid(1),
                    acceptance_position: CanonicalU64::new(1),
                    model_settings: None,
                    state: TurnState::Queued {
                        accepted_input_id: wire_uuid(10),
                        content: UserInputContent::text("transcript content".to_owned()),
                    },
                },
                ServerMessage::TranscriptModelCallUsage {
                    model_call_index: CanonicalU64::new(0),
                    turn_id: wire_uuid(1),
                    model_call_id: wire_uuid(11),
                    usage_provenance: UsageProvenance::Reported,
                    usage,
                    cost: Some(ModelCallDollarCost {
                        amount_usd: CanonicalDollarAmount::try_new(large.to_string())
                            .expect("fixture dollar amount is canonical"),
                        rate_version: rate_version.clone(),
                        label: ModelCallCostLabel::Real,
                    }),
                },
                ServerMessage::TranscriptModelCallUsage {
                    model_call_index: CanonicalU64::new(1),
                    turn_id: wire_uuid(2),
                    model_call_id: wire_uuid(12),
                    usage_provenance: UsageProvenance::Reported,
                    usage,
                    cost: Some(ModelCallDollarCost {
                        amount_usd: CanonicalDollarAmount::try_new(tiny.to_string())
                            .expect("fixture dollar amount is canonical"),
                        rate_version,
                        label: ModelCallCostLabel::Real,
                    }),
                },
            ],
        )
        .expect("test snapshot must spool");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let error = Output::new(&mut stdout, &mut stderr, false)
            .snapshot(&mut snapshot)
            .expect_err("the inexact session total must be rejected");

        assert!(matches!(
            error,
            ClientError::Protocol("dollar cost total was inexact")
        ));
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
    }

    #[test]
    fn follow_event_renders_cancellation_requested_call() {
        let rendered = render_event(SessionEvent::ModelCallTransition {
            turn_id: wire_uuid(2),
            model_call_id: wire_uuid(3),
            state: ModelCallState::CancellationRequested {},
        });

        expect![[r#"
            event=1 session=00000000-0000-0000-0000-000000000001 model_call_transition turn=00000000-0000-0000-0000-000000000002 call=00000000-0000-0000-0000-000000000003 state=cancellation_requested
        "#]]
        .assert_eq(&rendered);
    }

    #[test]
    fn follow_event_renders_runner_working_directory_change() {
        let rendered = render_event(SessionEvent::RunnerStateTransition {
            runner_id: wire_uuid(2),
            placement_revision: RunnerPlacementRevision::try_new(3)
                .expect("the fixture placement revision is positive"),
            sandbox_profile: RunnerSandboxProfile::WorkspaceRestricted,
            working_directory: Some(
                RunnerWorkingDirectory::try_new(String::from("workspace root\nproject"))
                    .expect("the fixture working directory is valid"),
            ),
            state: RunnerStateTransitionState::WorkingDirectoryChanged,
        });

        expect![[r#"
            event=1 session=00000000-0000-0000-0000-000000000001 runner_state_transition runner=00000000-0000-0000-0000-000000000002 placement_revision=3 sandbox=workspace_restricted working_directory=workspace\u{20}root\u{a}project state=working_directory_changed
        "#]]
        .assert_eq(&rendered);
    }

    #[test]
    fn follow_event_distinguishes_default_from_literal_none_working_directory() {
        let default_directory = render_event(SessionEvent::RunnerStateTransition {
            runner_id: wire_uuid(2),
            placement_revision: RunnerPlacementRevision::try_new(3)
                .expect("the fixture placement revision is positive"),
            sandbox_profile: RunnerSandboxProfile::WorkspaceRestricted,
            working_directory: None,
            state: RunnerStateTransitionState::Pinned,
        });
        let literal_none = render_event(SessionEvent::RunnerStateTransition {
            runner_id: wire_uuid(2),
            placement_revision: RunnerPlacementRevision::try_new(3)
                .expect("the fixture placement revision is positive"),
            sandbox_profile: RunnerSandboxProfile::WorkspaceRestricted,
            working_directory: Some(
                RunnerWorkingDirectory::try_new(String::from("none"))
                    .expect("the fixture working directory is valid"),
            ),
            state: RunnerStateTransitionState::Pinned,
        });

        expect![[r#"
            event=1 session=00000000-0000-0000-0000-000000000001 runner_state_transition runner=00000000-0000-0000-0000-000000000002 placement_revision=3 sandbox=workspace_restricted state=pinned
            event=1 session=00000000-0000-0000-0000-000000000001 runner_state_transition runner=00000000-0000-0000-0000-000000000002 placement_revision=3 sandbox=workspace_restricted working_directory=none state=pinned
        "#]]
        .assert_eq(&format!("{default_directory}{literal_none}"));
    }

    #[test]
    fn follow_event_renders_delegate_tool_decision_and_rationale() {
        let rendered = render_event(SessionEvent::ToolApprovalDecided {
            turn_id: wire_uuid(2),
            tool_request_id: wire_uuid(3),
            decision: ToolApprovalEventDecision::Deny { reason: None },
            decider: ToolApprovalEventDecider::Delegate {
                model_selection_id: wire_uuid(4),
                model_call_id: wire_uuid(5),
            },
            rationale: Some(String::from(
                "request exceeds configured authority\nreview manually",
            )),
        });

        expect![[r#"
            event=1 session=00000000-0000-0000-0000-000000000001 tool_approval_decided turn=00000000-0000-0000-0000-000000000002 request=00000000-0000-0000-0000-000000000003 decision=deny decider=delegate model_selection=00000000-0000-0000-0000-000000000004 call=00000000-0000-0000-0000-000000000005
            rationale=request exceeds configured authority\u{a}review manually
        "#]]
        .assert_eq(&rendered);
    }

    #[test]
    fn follow_event_renders_user_tool_denial_and_reason() {
        let rendered = render_event(SessionEvent::ToolApprovalDecided {
            turn_id: wire_uuid(2),
            tool_request_id: wire_uuid(3),
            decision: ToolApprovalEventDecision::Deny {
                reason: Some(String::from("outside requested scope")),
            },
            decider: ToolApprovalEventDecider::User {
                command_id: wire_uuid(4),
            },
            rationale: None,
        });

        expect![[r#"
            event=1 session=00000000-0000-0000-0000-000000000001 tool_approval_decided turn=00000000-0000-0000-0000-000000000002 request=00000000-0000-0000-0000-000000000003 decision=deny decider=user command=00000000-0000-0000-0000-000000000004
            denial_reason=outside requested scope
        "#]]
        .assert_eq(&rendered);
    }

    #[test]
    fn follow_event_renders_cancelled_turn() {
        let rendered = render_event(SessionEvent::TurnCancelled {
            turn_id: wire_uuid(2),
            cancellation_entry_id: wire_uuid(3),
            terminal_frontier_id: wire_uuid(4),
        });

        expect![[r#"
            event=1 session=00000000-0000-0000-0000-000000000001 turn_cancelled turn=00000000-0000-0000-0000-000000000002 entry=00000000-0000-0000-0000-000000000003 frontier=00000000-0000-0000-0000-000000000004
        "#]]
        .assert_eq(&rendered);
    }

    #[test]
    fn follow_event_renders_reconciliation_required_turn() {
        let rendered = render_event(SessionEvent::TurnReconciliationRequired {
            turn_id: wire_uuid(2),
            model_call_id: wire_uuid(3),
            terminal_frontier_id: wire_uuid(4),
        });

        expect![[r#"
            event=1 session=00000000-0000-0000-0000-000000000001 turn_reconciliation_required turn=00000000-0000-0000-0000-000000000002 operation=model_call operation_id=00000000-0000-0000-0000-000000000003 frontier=00000000-0000-0000-0000-000000000004
        "#]]
        .assert_eq(&rendered);
    }

    #[test]
    fn follow_event_renders_bound_child_policy() {
        let rendered = render_event(SessionEvent::ChildSpawned {
            spawning_request_id: wire_uuid(2),
            child_session_id: wire_uuid(3),
            relationship: DelegationPolicy::Bound {
                on_parent_stopped: BoundChildAction::Stop,
                on_parent_cancelled: BoundChildAction::Cancel,
            },
        });

        expect![[r#"
            event=1 session=00000000-0000-0000-0000-000000000001 delegation_child_spawned spawning_request=00000000-0000-0000-0000-000000000002 child=00000000-0000-0000-0000-000000000003 policy=bound on_parent_stopped=stop on_parent_cancelled=cancel
        "#]]
        .assert_eq(&rendered);
    }

    #[test]
    fn follow_event_renders_returned_child_result_content() {
        let rendered = render_event(SessionEvent::ChildResult {
            spawning_request_id: wire_uuid(2),
            child_session_id: wire_uuid(3),
            outcome: DelegationOutcome::Returned,
            content: Some(String::from("delivered result")),
            reason: DelegationReason::ChildCompleted,
            provenance: DelegationProvenance::ChildTurn {
                child_session_id: wire_uuid(3),
                child_turn_id: wire_uuid(4),
            },
        });

        expect![[r#"
            event=1 session=00000000-0000-0000-0000-000000000001 delegation_child_result spawning_request=00000000-0000-0000-0000-000000000002 child=00000000-0000-0000-0000-000000000003 outcome=returned reason=child_completed provenance=child_turn:00000000-0000-0000-0000-000000000003:00000000-0000-0000-0000-000000000004 content_present=true
            delivered result
        "#]]
        .assert_eq(&rendered);
    }

    #[test]
    fn follow_event_renders_parent_cascade_child_result_without_content() {
        let rendered = render_event(SessionEvent::ChildResult {
            spawning_request_id: wire_uuid(2),
            child_session_id: wire_uuid(3),
            outcome: DelegationOutcome::Stopped,
            content: None,
            reason: DelegationReason::ParentStopped,
            provenance: DelegationProvenance::ParentTurnCommand {
                parent_session_id: wire_uuid(1),
                parent_turn_id: wire_uuid(4),
                command_id: wire_uuid(5),
                descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            },
        });

        expect![[r#"
            event=1 session=00000000-0000-0000-0000-000000000001 delegation_child_result spawning_request=00000000-0000-0000-0000-000000000002 child=00000000-0000-0000-0000-000000000003 outcome=stopped reason=parent_stopped provenance=parent_turn_command:00000000-0000-0000-0000-000000000001:00000000-0000-0000-0000-000000000004:00000000-0000-0000-0000-000000000005:parent_and_descendants content_present=false
        "#]]
        .assert_eq(&rendered);
    }

    #[test]
    fn cancelled_terminal_reread_selects_only_its_exact_marker() {
        let selected_turn = wire_uuid(1);
        let later_turn = wire_uuid(2);
        let mut snapshot = TranscriptSnapshot::from_messages(
            12,
            [
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(0),
                    source_session_id: wire_uuid(10),
                    entry_id: wire_uuid(11),
                    entry: TranscriptEntry::TurnCancelled {
                        turn_id: selected_turn,
                    },
                },
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(1),
                    source_session_id: wire_uuid(10),
                    entry_id: wire_uuid(12),
                    entry: TranscriptEntry::TurnCancelled {
                        turn_id: later_turn,
                    },
                },
            ],
        )
        .expect("test snapshot must spool");
        let mut displayed = SnapshotIdentitySet::new().expect("identity spool must open");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .terminal_material(
                &mut snapshot,
                &mut displayed,
                SnapshotSelection::Cancelled {
                    turn_id: selected_turn,
                    terminal_entry_id: wire_uuid(11),
                },
            )
            .expect("selected cancellation marker must render");

        let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
        assert!(rendered.contains(&selected_turn.to_string()));
        assert!(!rendered.contains(&later_turn.to_string()));
        assert!(stderr.is_empty());
    }

    #[derive(Default)]
    struct FlushWriter {
        bytes: Vec<u8>,
        flushes: usize,
    }

    impl Write for FlushWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    #[track_caller]
    fn render_snapshot_turn(state: TurnState) -> String {
        let mut snapshot = TranscriptSnapshot::from_messages(
            1,
            [ServerMessage::TranscriptTurn {
                turn_id: wire_uuid(1),
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state,
            }],
        )
        .expect("test snapshot must spool");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .snapshot(&mut snapshot)
            .expect("snapshot turn must render");
        assert!(stderr.is_empty());
        String::from_utf8(stdout).expect("rendered output is UTF-8")
    }

    #[track_caller]
    fn render_event(event: SessionEvent) -> String {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        Output::new(&mut stdout, &mut stderr, false)
            .event(1, wire_uuid(1), &event)
            .expect("event must render");
        assert!(stderr.is_empty());
        String::from_utf8(stdout).expect("rendered output is UTF-8")
    }

    fn wire_uuid(value: u128) -> CanonicalUuid {
        CanonicalUuid::from_uuid(Uuid::from_u128(value))
    }
    fn review_target_snapshot(base_revision: Option<String>) -> ReviewTargetSnapshot {
        ReviewTargetSnapshot {
            target_id: wire_uuid(1),
            provider: String::from("example-host"),
            repository: String::from("example/repository"),
            subject: ReviewTargetSubject::Commit {},
            head_revision: String::from("head"),
            base_revision,
            stack_parent_target_id: None,
        }
    }

    fn review_finding_snapshot() -> ReviewFindingSnapshot {
        ReviewFindingSnapshot {
            target_id: wire_uuid(1),
            run_id: wire_uuid(2),
            producing_pass_id: wire_uuid(3),
            finding: ReviewFindingInput {
                finding_id: wire_uuid(4),
                file_path: String::from("src/lib.rs"),
                line_start: Some(CanonicalU64::new(7)),
                line_end: Some(CanonicalU64::new(9)),
                diff_side: Some(ReviewDiffSide::Right),
                title: String::from("Retain evidence"),
                body: String::from("First line\nSecond line"),
                severity: ReviewSeverity::High,
                is_real_confidence: CanonicalU64::new(9_000),
                severity_label_confidence: CanonicalU64::new(8_500),
                category: String::from("correctness"),
                recommended_fix: Some(String::from("Bind the exact\npass.")),
            },
            status: ReviewFindingStatus::Open,
            event_count: CanonicalU64::new(2),
        }
    }
}
